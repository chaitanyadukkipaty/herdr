//! The Explorer's filesystem-watcher adapter.
//!
//! A thin wrapper over a `notify-debouncer-full` debouncer that watches the
//! currently-expanded directories **non-recursively** (the root always watched)
//! and forwards coalesced change events into the app event channel as
//! [`AppEvent::ExplorerFsChanged`]. The watcher thread never touches shared
//! state directly — the tree is only ever mutated on the main loop when that
//! event is handled, following the debouncing precedent set elsewhere in the
//! app. If the OS runs out of watch handles, the adapter logs a one-line hint
//! and degrades to manual refresh rather than crashing.
//!
//! The notify/debouncer glue here is a thin adapter over the OS and is verified
//! manually (consistent with the diff-pane spawn helper); the pure decisions it
//! relies on — which directories to watch and how to classify a failure — live
//! in [`crate::workspace::file_tree`] and in [`is_watch_limit_error`], which are
//! unit-tested.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify_debouncer_full::notify::{self, ErrorKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use tokio::sync::mpsc;

use crate::events::AppEvent;

/// Debounce window: on-disk changes are coalesced for this long before the tree
/// is re-read, matching the ~200ms debouncing precedent set elsewhere in the app.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// One-line hint logged once when the OS runs out of watch handles, after which
/// the Explorer degrades to manual refresh.
pub(crate) const WATCH_LIMIT_HINT: &str =
    "explorer file watcher hit the OS watch-handle limit; live updates are off — \
     use the refresh control, or raise the inotify/open-file limit to re-enable";

/// Whether a notify error means the OS ran out of watch handles — the inotify
/// `max_user_watches` limit on Linux (surfaced as `MaxFilesWatch` or `ENOSPC`)
/// or a per-process open-file-descriptor limit (`EMFILE`/`ENFILE`) — in which
/// case the Explorer must degrade to manual refresh rather than crash. Any other
/// error is transient and only skips the one path that failed.
pub(crate) fn is_watch_limit_error(err: &notify::Error) -> bool {
    match &err.kind {
        ErrorKind::MaxFilesWatch => true,
        ErrorKind::Io(io) => matches!(
            io.raw_os_error(),
            Some(libc::ENOSPC) | Some(libc::EMFILE) | Some(libc::ENFILE)
        ),
        _ => false,
    }
}

/// A live, non-recursive filesystem watcher over one Explorer tree's expanded
/// directories. Bound to the tree `root` so the coalesced events it emits are
/// tagged for the right workspace.
pub(crate) struct ExplorerWatcher {
    debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
    root: PathBuf,
    /// Directories currently under watch, so [`reconcile`](Self::reconcile) can
    /// diff against the desired set and add/drop only what changed.
    watched: HashSet<PathBuf>,
    /// Set once the OS rejects a watch for a handle-limit reason; thereafter the
    /// watcher stops trying to add watches and the Explorer relies on manual
    /// refresh.
    degraded: bool,
}

impl ExplorerWatcher {
    /// Create a watcher that forwards coalesced change events to `event_tx`,
    /// tagged with `root` so the main loop can find the workspace whose tree is
    /// rooted there. Returns `None` if a debouncer cannot be created at all (the
    /// Explorer then simply has no live updates and relies on manual refresh).
    pub(crate) fn new(event_tx: mpsc::Sender<AppEvent>, root: PathBuf) -> Option<Self> {
        let handler_root = root.clone();
        let debouncer = new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| {
            let Ok(events) = result else {
                // Errors arriving on the event stream are not fatal: drop them
                // and keep watching. Watch-handle-limit failures are handled at
                // `watch()` time in `reconcile`.
                return;
            };
            let paths: Vec<PathBuf> = events
                .into_iter()
                .flat_map(|event| event.event.paths.clone())
                .collect();
            if paths.is_empty() {
                return;
            }
            // The watcher thread only sends a message; the tree is mutated on the
            // main loop when this event is handled.
            let _ = event_tx.blocking_send(AppEvent::ExplorerFsChanged {
                root: handler_root.clone(),
                paths,
            });
        })
        .map_err(|err| {
            tracing::debug!(?err, "explorer file watcher could not be created");
            err
        })
        .ok()?;
        Some(Self {
            debouncer,
            root,
            watched: HashSet::new(),
            degraded: false,
        })
    }

    /// The tree root this watcher is bound to.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Drive the watch set toward `desired` (the root plus expanded folders, from
    /// [`crate::workspace::file_tree::watch_targets`]): add a watch for each newly
    /// expanded directory and drop the watch for each collapsed one, so the
    /// non-recursive watcher tracks exactly the expanded tree. Once degraded by a
    /// watch-handle-limit error this is a no-op — the Explorer relies on manual
    /// refresh.
    pub(crate) fn reconcile(&mut self, desired: &HashSet<PathBuf>) {
        if self.degraded {
            return;
        }
        let (to_add, to_drop) =
            crate::workspace::file_tree::reconcile_watches(desired, &self.watched);

        for path in to_drop {
            // A watch may already be gone if its directory was removed on disk;
            // dropping it is best-effort.
            let _ = self.debouncer.unwatch(&path);
            self.watched.remove(&path);
        }

        for path in to_add {
            match self.debouncer.watch(&path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(path);
                }
                Err(err) if is_watch_limit_error(&err) => {
                    // Out of watch handles: log once and stop watching. The tree
                    // still works, just without live updates.
                    tracing::warn!("{WATCH_LIMIT_HINT}");
                    self.degraded = true;
                    return;
                }
                Err(err) => {
                    // A transient per-path failure (e.g. the directory vanished
                    // between expand and watch): skip just this path.
                    tracing::debug!(?path, ?err, "explorer file watch skipped");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn watch_handle_limit_errors_trigger_the_manual_refresh_fallback() {
        // The dedicated inotify-limit kind degrades.
        assert!(is_watch_limit_error(&notify::Error::new(
            ErrorKind::MaxFilesWatch
        )));
        // inotify surfaces the watch-descriptor limit as ENOSPC, and FD
        // exhaustion as EMFILE/ENFILE — all mean "out of watch handles".
        for errno in [libc::ENOSPC, libc::EMFILE, libc::ENFILE] {
            let err = notify::Error::new(ErrorKind::Io(io::Error::from_raw_os_error(errno)));
            assert!(
                is_watch_limit_error(&err),
                "errno {errno} should degrade to manual refresh"
            );
        }
    }

    #[test]
    fn ordinary_errors_do_not_trigger_the_fallback() {
        // A missing watch or a generic error is not a handle-limit condition, so
        // the watcher keeps running with live updates.
        assert!(!is_watch_limit_error(&notify::Error::new(
            ErrorKind::WatchNotFound
        )));
        assert!(!is_watch_limit_error(&notify::Error::new(
            ErrorKind::PathNotFound
        )));
        let other_io =
            notify::Error::new(ErrorKind::Io(io::Error::from_raw_os_error(libc::EACCES)));
        assert!(!is_watch_limit_error(&other_io));
    }
}
