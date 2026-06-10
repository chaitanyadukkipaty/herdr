use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::PaneId;
#[cfg(test)]
use crate::layout::TileLayout;
use crate::pane::PaneState;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalRuntimeRegistry, TerminalState};

mod aggregate;
pub mod file_tree;
mod git;
mod tab;

#[cfg(test)]
use self::git::git_ahead_behind;
pub(crate) use self::git::{commit_files_for_cwd, DEFAULT_LOADED_COMMIT_COUNT};
pub use self::{
    file_tree::{
        editor_open_argv, resolve_editor_command, FileEntryKind, FileTreeEntry, FileTreeRow,
    },
    git::{
        changed_file_diff_argv, commit_file_diff_argv, commit_show_argv, derive_label_from_cwd,
        git_branch, git_space_metadata, git_status_cache_key, ChangeStatus, ChangedFile,
        CommitInfo, GitSpaceMetadata, GitStatusCacheEntry,
    },
    tab::Tab,
};

/// Which of the source panel's two mutually-exclusive modes is showing for a
/// workspace. `Source` keeps the Changes-over-Graph git layout; `Explorer`
/// renders the workspace's file tree. Runtime-only: resets to `Source` on
/// launch and is never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourcePanelMode {
    #[default]
    Source,
    Explorer,
}

impl SourcePanelMode {
    /// The other mode — used by the mode toggle.
    pub fn toggled(self) -> Self {
        match self {
            SourcePanelMode::Source => SourcePanelMode::Explorer,
            SourcePanelMode::Explorer => SourcePanelMode::Source,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorktreeSpaceMembership {
    pub key: String,
    pub label: String,
    pub repo_root: PathBuf,
    pub checkout_path: PathBuf,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatus {
    pub workspace_id: String,
    pub resolved_identity_cwd: PathBuf,
    pub branch: Option<String>,
    pub ahead_behind: Option<(usize, usize)>,
    pub space: Option<GitSpaceMetadata>,
    pub changes: Vec<ChangedFile>,
    pub log: Vec<CommitInfo>,
    pub log_has_more: bool,
    pub is_git_repo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatusSnapshot {
    pub branch: Option<String>,
    pub ahead_behind: Option<(usize, usize)>,
    pub space: Option<GitSpaceMetadata>,
    pub changes: Vec<ChangedFile>,
    pub log: Vec<CommitInfo>,
    pub log_has_more: bool,
    /// Whether `identity_cwd` resolved to a usable git repository. Drives the
    /// source-control panel's "not a git repository" empty state.
    pub is_git_repo: bool,
}

impl WorkspaceGitStatusSnapshot {
    pub fn into_workspace_status(
        self,
        workspace_id: String,
        resolved_identity_cwd: PathBuf,
    ) -> WorkspaceGitStatus {
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd,
            branch: self.branch,
            ahead_behind: self.ahead_behind,
            space: self.space,
            changes: self.changes,
            log: self.log,
            log_has_more: self.log_has_more,
            is_git_repo: self.is_git_repo,
        }
    }
}

static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn generate_workspace_id() -> String {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros())
        .unwrap_or(0);
    let counter = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
    format!("w{micros:x}{counter:x}")
}

/// A named workspace containing tabs.
pub struct Workspace {
    /// Stable public workspace identity, independent of display order.
    pub id: String,
    /// User-provided override. If set, auto-derived identity stops updating.
    pub custom_name: Option<String>,
    /// Fallback workspace identity source for tests, old snapshots, or missing runtimes.
    pub identity_cwd: PathBuf,
    /// Cached current git branch for the workspace repo.
    pub(crate) cached_git_branch: Option<String>,
    /// Cached ahead/behind counts for the workspace repo's current branch upstream.
    pub(crate) cached_git_ahead_behind: Option<(usize, usize)>,
    /// Cached derived Git repo metadata for worktree actions and status display.
    pub(crate) cached_git_space: Option<GitSpaceMetadata>,
    /// Cached working-tree changes for the source-control panel.
    pub(crate) cached_changes: Vec<ChangedFile>,
    /// Cached commit graph (most recent `loaded_commit_count`) for the panel.
    pub(crate) cached_log: Vec<CommitInfo>,
    /// Number of commits the graph section loads. Bumped by "load more".
    pub(crate) loaded_commit_count: usize,
    /// Whether the repository has history beyond `cached_log`.
    pub(crate) cached_log_has_more: bool,
    /// Whether `identity_cwd` resolved to a usable git repository. Drives the
    /// source-control panel's "not a git repository" empty state.
    pub(crate) cached_is_git_repo: bool,
    /// Explicit Herdr-managed worktree grouping provenance.
    pub worktree_space: Option<WorktreeSpaceMembership>,
    /// Stable-ish public pane numbers within this workspace.
    /// New panes append at the end; closing a pane compacts higher numbers down.
    pub public_pane_numbers: HashMap<PaneId, usize>,
    pub(crate) next_public_pane_number: usize,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    /// Which source-panel mode (Source / Explorer) is showing for this
    /// workspace. Runtime-only; resets to `Source` on launch (not persisted).
    pub(crate) source_panel_mode: SourcePanelMode,
    // Explorer mode's own scroll offset and selection, kept separate from the
    // Source mode scroll/selection so switching modes never disturbs the other's
    // place. Selection is the path of the last-clicked row, highlighted while it
    // stays visible.
    pub(crate) explorer_scroll: usize,
    pub(crate) explorer_selected: Option<PathBuf>,
    /// The directory the Explorer tree is rooted at — the workspace's resolved
    /// working directory, set the first time the tree is built. Runtime-only.
    pub(crate) explorer_root: Option<PathBuf>,
    /// Lazily-loaded children per directory (absolute path → sorted entries).
    /// The root's children load up front; a folder's children load the first
    /// time it is expanded. An unreadable directory caches as an empty list so
    /// its failed read is never retried. Runtime-only, per-workspace.
    pub(crate) explorer_cache: HashMap<PathBuf, Vec<FileTreeEntry>>,
    /// Folder paths the user has expanded. Runtime-only, per-workspace.
    pub(crate) explorer_expanded: std::collections::HashSet<PathBuf>,
    #[cfg(test)]
    pub(crate) test_runtimes: HashMap<PaneId, TerminalRuntime>,
}

impl Deref for Workspace {
    type Target = Tab;

    fn deref(&self) -> &Self::Target {
        self.active_tab()
            .expect("workspace must always have at least one active tab")
    }
}

impl DerefMut for Workspace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.active_tab_mut()
            .expect("workspace must always have at least one active tab")
    }
}

impl Workspace {
    pub fn new(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_tab(
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            events,
            render_notify,
            render_dirty,
            None,
        )
    }

    pub fn new_argv_command(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_tab(
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            events,
            render_notify,
            render_dirty,
            Some(argv),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_tab(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        argv: Option<&[String]>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        let (tab, terminal, runtime) = if let Some(argv) = argv {
            Tab::new_argv_command(
                1,
                initial_cwd.clone(),
                rows,
                cols,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
                events,
                render_notify,
                render_dirty,
            )?
        } else {
            Tab::new(
                1,
                initial_cwd.clone(),
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
                events,
                render_notify,
                render_dirty,
            )?
        };
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(tab.root_pane, 1);
        Ok((
            Self {
                id: generate_workspace_id(),
                custom_name: None,
                identity_cwd: initial_cwd.clone(),
                cached_git_branch: git_branch(&initial_cwd),
                cached_git_ahead_behind: None,
                cached_git_space: None,
                cached_changes: Vec::new(),
                cached_log: Vec::new(),
                loaded_commit_count: DEFAULT_LOADED_COMMIT_COUNT,
                cached_log_has_more: false,
                cached_is_git_repo: git_status_cache_key(&initial_cwd).is_some(),
                worktree_space: None,
                public_pane_numbers,
                next_public_pane_number: 2,
                tabs: vec![tab],
                active_tab: 0,
                source_panel_mode: SourcePanelMode::Source,
                explorer_scroll: 0,
                explorer_selected: None,
                explorer_root: None,
                explorer_cache: HashMap::new(),
                explorer_expanded: std::collections::HashSet::new(),
                #[cfg(test)]
                test_runtimes: HashMap::new(),
            },
            terminal,
            runtime,
        ))
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_tab_index(&self) -> usize {
        self.active_tab
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    pub fn active_tab_display_name(&self) -> Option<String> {
        self.active_tab().map(Tab::display_name)
    }

    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_tab = idx;
            if let Some(tab) = self.tabs.get_mut(idx) {
                for pane in tab.panes.values_mut() {
                    pane.seen = true;
                }
            }
        }
    }

    pub fn create_tab(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        self.create_tab_with_runtime(
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            None,
        )
    }

    fn create_tab_with_runtime(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        argv: Option<&[String]>,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        let number = self.tabs.len() + 1;
        let events = self
            .active_tab()
            .map(|tab| tab.events.clone())
            .expect("workspace must always have at least one tab");
        let render_notify = self
            .active_tab()
            .map(|tab| tab.render_notify.clone())
            .expect("workspace must always have at least one tab");
        let render_dirty = self
            .active_tab()
            .map(|tab| tab.render_dirty.clone())
            .expect("workspace must always have at least one tab");

        let (tab, terminal, runtime) = if let Some(argv) = argv {
            Tab::new_argv_command(
                number,
                cwd,
                rows,
                cols,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
                events,
                render_notify,
                render_dirty,
            )?
        } else {
            Tab::new(
                number,
                cwd,
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
                events,
                render_notify,
                render_dirty,
            )?
        };
        self.register_new_pane(tab.root_pane);
        self.tabs.push(tab);
        Ok((self.tabs.len() - 1, terminal, runtime))
    }

    pub fn close_tab(&mut self, idx: usize) -> bool {
        if self.tabs.len() <= 1 || idx >= self.tabs.len() {
            return false;
        }
        let tab = self.tabs.remove(idx);
        for pane_id in tab.panes.keys() {
            self.unregister_pane(*pane_id);
        }
        self.renumber_tabs();
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if idx <= self.active_tab && self.active_tab > 0 {
            self.active_tab -= 1;
        }
        true
    }

    pub fn move_tab(&mut self, source_idx: usize, insert_idx: usize) -> bool {
        if source_idx >= self.tabs.len() || insert_idx > self.tabs.len() {
            return false;
        }

        let target_idx = if source_idx < insert_idx {
            insert_idx.saturating_sub(1)
        } else {
            insert_idx
        }
        .min(self.tabs.len().saturating_sub(1));

        if source_idx == target_idx {
            return false;
        }

        let active_root_pane = self.tabs.get(self.active_tab).map(|tab| tab.root_pane);
        let tab = self.tabs.remove(source_idx);
        self.tabs.insert(target_idx, tab);
        self.renumber_tabs();
        self.active_tab = active_root_pane
            .and_then(|root_pane| self.tabs.iter().position(|tab| tab.root_pane == root_pane))
            .unwrap_or(target_idx);
        true
    }

    pub fn close_active_tab(&mut self) -> bool {
        self.close_tab(self.active_tab)
    }

    pub fn split_focused(
        &mut self,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
    ) -> std::io::Result<crate::workspace::tab::NewPane> {
        let new_pane = self
            .active_tab_mut()
            .expect("workspace must always have at least one tab")
            .split_focused(
                direction,
                rows,
                cols,
                cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
            )?;
        self.register_new_pane(new_pane.pane_id);
        Ok(new_pane)
    }

    pub fn split_pane(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            focus_new_pane,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_pane_with_ratio(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        ratio: f32,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            Some(ratio),
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            focus_new_pane,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_pane_argv_command(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            focus_new_pane,
            Some(argv),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn split_pane_with_runtime(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        ratio: Option<f32>,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        focus_new_pane: bool,
        argv: Option<&[String]>,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        let tab = &mut self.tabs[tab_idx];
        let previous_focus = tab.layout.focused();
        tab.layout.focus_pane(pane_id);
        let new_pane = match if let Some(argv) = argv {
            tab.split_focused_argv_command(
                direction,
                rows,
                cols,
                cwd,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
            )
        } else {
            match ratio {
                Some(ratio) => tab.split_focused_with_ratio(
                    direction,
                    ratio,
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    shell_config,
                ),
                None => tab.split_focused(
                    direction,
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    shell_config,
                ),
            }
        } {
            Ok(new_pane) => new_pane,
            Err(err) => {
                tab.layout.focus_pane(previous_focus);
                return Some(Err(err));
            }
        };
        if !focus_new_pane {
            tab.layout.focus_pane(previous_focus);
        }
        self.register_new_pane(new_pane.pane_id);
        Some(Ok((tab_idx, new_pane)))
    }

    /// Close the focused pane. Returns true if the workspace should close.
    pub fn close_focused(&mut self) -> bool {
        let pane_count = self
            .active_tab()
            .map(|tab| tab.layout.pane_count())
            .unwrap_or(0);
        let tab_count = self.tabs.len();
        if pane_count <= 1 {
            return tab_count <= 1 || self.close_active_tab_and_report();
        }

        if let Some((removed, _terminal_id)) = self.active_tab_mut().and_then(Tab::close_focused) {
            self.unregister_pane(removed);
        }
        false
    }

    /// Remove a specific pane from this workspace without terminating its runtime.
    /// Returns true if the workspace should close.
    pub fn remove_pane(&mut self, pane_id: PaneId) -> bool {
        let Some(tab_idx) = self.find_tab_index_for_pane(pane_id) else {
            return false;
        };
        let pane_count = self.tabs[tab_idx].layout.pane_count();
        let tab_count = self.tabs.len();
        if pane_count <= 1 {
            if tab_count <= 1 {
                return true;
            }
            self.tabs.remove(tab_idx);
            self.unregister_pane(pane_id);
            self.renumber_tabs();
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            } else if tab_idx <= self.active_tab && self.active_tab > 0 {
                self.active_tab -= 1;
            }
            return false;
        }

        if let Some((removed, _terminal_id)) = self.tabs[tab_idx].remove_pane(pane_id) {
            self.unregister_pane(removed);
        }
        false
    }

    pub fn public_pane_number(&self, pane_id: PaneId) -> Option<usize> {
        self.public_pane_numbers.get(&pane_id).copied()
    }

    pub fn set_custom_name(&mut self, name: String) {
        self.custom_name = Some(name);
    }

    pub fn resolved_identity_cwd(&self) -> Option<PathBuf> {
        Some(self.identity_cwd.clone())
    }

    pub fn resolved_identity_cwd_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        // Resolve from the *active* tab's root pane so the workspace's git
        // context (branch, changes, and commit graph, plus the derived label)
        // follows the tab the user is currently looking at. Falls back to the
        // first tab, then the stored identity cwd, when a runtime cwd is absent.
        self.active_tab()
            .or_else(|| self.tabs.first())
            .and_then(|tab| tab.cwd_for_pane(tab.root_pane, terminals, terminal_runtimes))
            .or_else(|| Some(self.identity_cwd.clone()))
    }

    pub fn display_name(&self) -> String {
        if let Some(name) = &self.custom_name {
            return name.clone();
        }

        self.resolved_identity_cwd()
            .map(|cwd| derive_label_from_cwd(&cwd))
            .unwrap_or_else(|| "workspace".into())
    }

    pub fn display_name_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> String {
        if let Some(name) = &self.custom_name {
            return name.clone();
        }

        self.resolved_identity_cwd_from(terminals, terminal_runtimes)
            .map(|cwd| derive_label_from_cwd(&cwd))
            .unwrap_or_else(|| "workspace".into())
    }

    /// The source panel mode currently showing for this workspace.
    pub fn source_panel_mode(&self) -> SourcePanelMode {
        self.source_panel_mode
    }

    /// Switch this workspace's source panel to `mode`.
    pub fn set_source_panel_mode(&mut self, mode: SourcePanelMode) {
        self.source_panel_mode = mode;
    }

    /// Flip this workspace's source panel between Source and Explorer.
    pub fn toggle_source_panel_mode(&mut self) {
        self.source_panel_mode = self.source_panel_mode.toggled();
    }

    /// Root the Explorer tree at `root` and load its immediate children up
    /// front. Re-rooting (e.g. the workspace cwd changed) resets the cache,
    /// expanded set, scroll, and selection so the tree reflects the new project.
    pub fn explorer_set_root(&mut self, root: PathBuf) {
        if self.explorer_root.as_deref() == Some(root.as_path()) {
            return;
        }
        self.explorer_cache.clear();
        self.explorer_expanded.clear();
        self.explorer_scroll = 0;
        self.explorer_selected = None;
        self.explorer_ensure_loaded(&root);
        self.explorer_root = Some(root);
    }

    /// Load a directory's children into the cache if they are not loaded yet. A
    /// failed read is cached as an empty list, so it is read exactly once and
    /// never retried in a loop.
    fn explorer_ensure_loaded(&mut self, path: &std::path::Path) {
        if !self.explorer_cache.contains_key(path) {
            let entries = file_tree::read_dir_sorted(path);
            self.explorer_cache.insert(path.to_path_buf(), entries);
        }
    }

    /// Toggle a folder's expansion. Expanding loads its children the first time
    /// (and only the first time); collapsing leaves the cache intact so
    /// re-expanding is instant.
    pub fn explorer_toggle_expand(&mut self, path: &std::path::Path) {
        if self.explorer_expanded.remove(path) {
            return;
        }
        self.explorer_ensure_loaded(path);
        self.explorer_expanded.insert(path.to_path_buf());
    }

    /// Force an immediate re-read of the loaded tree: re-stat the root and every
    /// currently-expanded directory so on-disk changes (entries added, removed,
    /// or renamed while the folder was open) are picked up on demand. Collapsed
    /// folders are intentionally left stale — they are re-read when next
    /// expanded. A pure view operation: it reads the filesystem but mutates
    /// nothing on disk, and leaves the `expanded` set untouched.
    pub fn explorer_refresh(&mut self) {
        let mut dirs: Vec<PathBuf> = self.explorer_root.iter().cloned().collect();
        dirs.extend(self.explorer_expanded.iter().cloned());
        for dir in dirs {
            let entries = file_tree::read_dir_sorted(&dir);
            self.explorer_cache.insert(dir, entries);
        }
    }

    /// The inputs the Explorer's non-recursive filesystem watcher needs to track
    /// this tree: the root and the currently-expanded folders. `None` until the
    /// tree has been rooted, so no watcher is created for a workspace that has
    /// never shown its Explorer.
    pub(crate) fn explorer_watch_state(
        &self,
    ) -> Option<(PathBuf, std::collections::HashSet<PathBuf>)> {
        let root = self.explorer_root.clone()?;
        Some((root, self.explorer_expanded.clone()))
    }

    /// Apply a filesystem-watcher event naming the `changed` paths to the loaded
    /// tree. For each changed path the directory that would contain it is
    /// re-read — the path itself when it is a loaded directory, otherwise its
    /// parent — but only when that directory is already in the cache (the root or
    /// a currently/previously expanded folder). Paths under directories that were
    /// never loaded are ignored, so a stray event for a collapsed or unrelated
    /// subtree does no work and never re-reads it into existence. Returns true if
    /// any cached directory was reloaded, so the caller can mark a redraw. The
    /// `expanded` set is left untouched — this only re-stats already-loaded dirs.
    pub fn explorer_apply_changed_paths(&mut self, changed: &[PathBuf]) -> bool {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for path in changed {
            for candidate in [Some(path.as_path()), path.parent()].into_iter().flatten() {
                if self.explorer_cache.contains_key(candidate)
                    && !dirs.iter().any(|d| d.as_path() == candidate)
                {
                    dirs.push(candidate.to_path_buf());
                }
            }
        }
        let reloaded = !dirs.is_empty();
        for dir in dirs {
            let entries = file_tree::read_dir_sorted(&dir);
            self.explorer_cache.insert(dir, entries);
        }
        reloaded
    }

    /// Collapse every expanded folder, returning the tree to just its roots.
    /// The directory cache is left intact so re-expanding is instant; only the
    /// `expanded` set is cleared. A pure view operation — touches no filesystem.
    pub fn explorer_collapse_all(&mut self) {
        self.explorer_expanded.clear();
    }

    /// Set the highlighted selection to `path` (the last-clicked row).
    pub fn explorer_select(&mut self, path: PathBuf) {
        self.explorer_selected = Some(path);
    }

    /// The currently selected (highlighted) row path, if any.
    pub fn explorer_selected(&self) -> Option<&std::path::Path> {
        self.explorer_selected.as_deref()
    }

    /// The flattened, ordered rows of the Explorer tree for the current cache and
    /// expanded set. Empty until the tree has been rooted.
    pub fn explorer_rows(&self) -> Vec<FileTreeRow> {
        let Some(root) = self.explorer_root.as_deref() else {
            return Vec::new();
        };
        file_tree::flatten_tree(root, &self.explorer_cache, &self.explorer_expanded)
    }

    /// Git-status decorations for the Explorer tree: the workspace's cached
    /// changed files rolled up onto the rooted tree (changed files colored,
    /// ancestor folders marked). Empty until the tree has been rooted. Pure over
    /// the cached state — reads no filesystem.
    pub fn explorer_decorations(&self) -> HashMap<PathBuf, file_tree::FileTreeDecoration> {
        let Some(root) = self.explorer_root.as_deref() else {
            return HashMap::new();
        };
        file_tree::rollup_git_status(root, &self.cached_changes)
    }

    pub fn branch(&self) -> Option<String> {
        self.cached_git_branch.clone()
    }

    pub fn git_ahead_behind(&self) -> Option<(usize, usize)> {
        self.cached_git_ahead_behind
    }

    pub fn git_space(&self) -> Option<&GitSpaceMetadata> {
        self.cached_git_space.as_ref()
    }

    pub fn changed_files(&self) -> &[ChangedFile] {
        &self.cached_changes
    }

    pub fn commits(&self) -> &[CommitInfo] {
        &self.cached_log
    }

    pub fn loaded_commit_count(&self) -> usize {
        self.loaded_commit_count
    }

    /// Whether the repository has commit history beyond `cached_log`, used to
    /// decide whether the Graph section shows a "load more" affordance.
    pub fn has_more_commits(&self) -> bool {
        self.cached_log_has_more
    }

    /// Whether the workspace's identity directory is a usable git repository.
    /// When false, the source-control panel shows its "not a git repository"
    /// empty state instead of the changes/graph sections.
    pub fn is_git_repo(&self) -> bool {
        self.cached_is_git_repo
    }

    pub fn worktree_space(&self) -> Option<&WorktreeSpaceMembership> {
        self.worktree_space.as_ref()
    }

    #[cfg(test)]
    pub fn refresh_git_ahead_behind(&mut self) {
        let cwd = self.resolved_identity_cwd();
        self.cached_git_branch = cwd.as_deref().and_then(git_branch);
        self.cached_git_ahead_behind = cwd.as_deref().and_then(git_ahead_behind);
        self.cached_git_space = cwd.as_deref().and_then(git_space_metadata);
    }

    pub fn git_status_snapshot_for_cwd_with_cache(
        resolved_identity_cwd: &std::path::Path,
        cached: Option<&GitStatusCacheEntry>,
        commit_count: usize,
    ) -> (WorkspaceGitStatusSnapshot, Option<GitStatusCacheEntry>) {
        self::git::git_status_snapshot_for_cwd(resolved_identity_cwd, cached, commit_count)
    }

    pub fn find_tab_index_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.panes.contains_key(&pane_id))
    }

    pub fn pane_state(&self, pane_id: PaneId) -> Option<&PaneState> {
        self.tabs.iter().find_map(|tab| tab.panes.get(&pane_id))
    }

    pub fn terminal_id(&self, pane_id: PaneId) -> Option<&TerminalId> {
        self.tabs.iter().find_map(|tab| tab.terminal_id(pane_id))
    }

    pub fn focused_pane_id(&self) -> Option<PaneId> {
        self.active_tab().map(|tab| tab.layout.focused())
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> bool {
        let tab_idx = match self.find_tab_index_for_pane(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        let pane_count = self.tabs[tab_idx].layout.pane_count();
        let tab_count = self.tabs.len();
        if pane_count <= 1 {
            if tab_count <= 1 {
                return true;
            }
            self.tabs.remove(tab_idx);
            self.unregister_pane(pane_id);
            self.renumber_tabs();
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            } else if tab_idx <= self.active_tab && self.active_tab > 0 {
                self.active_tab -= 1;
            }
            return false;
        }

        if let Some((removed, _terminal_id)) = self.tabs[tab_idx].close_pane(pane_id) {
            self.unregister_pane(removed);
        }
        false
    }

    fn register_new_pane(&mut self, pane_id: PaneId) {
        self.public_pane_numbers
            .insert(pane_id, self.next_public_pane_number);
        self.next_public_pane_number += 1;
    }

    fn unregister_pane(&mut self, pane_id: PaneId) {
        if let Some(removed_number) = self.public_pane_numbers.remove(&pane_id) {
            for number in self.public_pane_numbers.values_mut() {
                if *number > removed_number {
                    *number -= 1;
                }
            }
            self.next_public_pane_number = self.public_pane_numbers.len() + 1;
        }
    }

    fn renumber_tabs(&mut self) {
        for (idx, tab) in self.tabs.iter_mut().enumerate() {
            tab.number = idx + 1;
        }
    }

    fn close_active_tab_and_report(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return true;
        }
        self.close_active_tab();
        false
    }
}

#[cfg(test)]
impl Workspace {
    pub(crate) fn test_new(name: &str) -> Self {
        let (events, _) = mpsc::channel(64);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));
        let identity_cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
        let (layout, root_id) = TileLayout::new();
        let terminal_id = TerminalId::alloc();
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(terminal_id));
        let tab = Tab {
            custom_name: None,
            number: 1,
            root_pane: root_id,
            layout,
            panes,
            runtimes: HashMap::new(),
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        };
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(tab.root_pane, 1);
        Self {
            id: generate_workspace_id(),
            custom_name: Some(name.to_string()),
            identity_cwd: identity_cwd.clone(),
            cached_git_branch: git_branch(&identity_cwd),
            cached_git_ahead_behind: None,
            cached_git_space: None,
            cached_changes: Vec::new(),
            cached_log: Vec::new(),
            loaded_commit_count: DEFAULT_LOADED_COMMIT_COUNT,
            cached_log_has_more: false,
            cached_is_git_repo: true,
            worktree_space: None,
            public_pane_numbers,
            next_public_pane_number: 2,
            tabs: vec![tab],
            active_tab: 0,
            source_panel_mode: SourcePanelMode::Source,
            explorer_scroll: 0,
            explorer_selected: None,
            explorer_root: None,
            explorer_cache: HashMap::new(),
            explorer_expanded: std::collections::HashSet::new(),
            test_runtimes: HashMap::new(),
        }
    }

    pub(crate) fn insert_test_runtime(&mut self, pane_id: PaneId, runtime: TerminalRuntime) {
        self.test_runtimes.insert(pane_id, runtime);
    }

    pub(crate) fn test_split(&mut self, direction: Direction) -> PaneId {
        let tab = self.active_tab_mut().expect("workspace must have tab");
        let new_id = tab.layout.split_focused(direction);
        tab.panes
            .insert(new_id, PaneState::new(TerminalId::alloc()));
        self.register_new_pane(new_id);
        new_id
    }

    pub(crate) fn test_add_tab(&mut self, name: Option<&str>) -> usize {
        let (events, _) = mpsc::channel(64);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));
        let (layout, root_id) = TileLayout::new();
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(TerminalId::alloc()));
        let tab = Tab {
            custom_name: name.map(str::to_string),
            number: self.tabs.len() + 1,
            root_pane: root_id,
            layout,
            panes,
            runtimes: HashMap::new(),
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        };
        self.register_new_pane(root_id);
        self.tabs.push(tab);
        self.tabs.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_panel_mode_defaults_to_source_on_launch() {
        let ws = Workspace::test_new("ws");
        assert_eq!(ws.source_panel_mode(), SourcePanelMode::Source);
    }

    #[test]
    fn toggling_source_panel_mode_alternates_between_the_two_modes() {
        let mut ws = Workspace::test_new("ws");
        ws.toggle_source_panel_mode();
        assert_eq!(ws.source_panel_mode(), SourcePanelMode::Explorer);
        ws.toggle_source_panel_mode();
        assert_eq!(ws.source_panel_mode(), SourcePanelMode::Source);
    }

    /// Create a unique temp directory tree for Explorer state tests.
    fn temp_tree(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "herdr-ws-explorer-{tag}-{}-{}",
            std::process::id(),
            nanos,
        ));
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src").join("main.rs"), b"x").unwrap();
        std::fs::write(base.join("README.md"), b"x").unwrap();
        base
    }

    #[test]
    fn rooting_the_explorer_loads_the_roots_children_up_front() {
        let base = temp_tree("root");
        let mut ws = Workspace::test_new("ws");

        ws.explorer_set_root(base.clone());

        // The root's immediate children are listed without expanding anything.
        let names: Vec<String> = ws
            .explorer_rows()
            .into_iter()
            .filter_map(|r| match r {
                FileTreeRow::Node(n) => Some(n.name),
                FileTreeRow::Empty { .. } => None,
            })
            .collect();
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(names, vec!["src".to_string(), "README.md".to_string()]);
    }

    #[test]
    fn explorer_decorations_roll_cached_changes_up_the_tree() {
        let base = temp_tree("decorate");
        let mut ws = Workspace::test_new("ws");
        ws.explorer_set_root(base.clone());
        // A change recorded relative to the workspace cwd, as git status reports.
        ws.cached_changes = vec![ChangedFile {
            path: PathBuf::from("src/main.rs"),
            status: ChangeStatus::Modified,
        }];

        let decorations = ws.explorer_decorations();
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(
            decorations.get(&base.join("src/main.rs")),
            Some(&crate::workspace::file_tree::FileTreeDecoration::Changed(
                ChangeStatus::Modified
            )),
        );
        assert_eq!(
            decorations.get(&base.join("src")),
            Some(&crate::workspace::file_tree::FileTreeDecoration::ContainsChanges),
        );
    }

    #[test]
    fn refresh_restats_expanded_directories_and_updates_the_tree() {
        let base = temp_tree("refresh");
        let mut ws = Workspace::test_new("ws");
        ws.explorer_set_root(base.clone());
        let src = base.join("src");
        ws.explorer_toggle_expand(&src);

        // A file appears on disk inside the expanded folder after it was loaded.
        std::fs::write(src.join("added.rs"), b"x").unwrap();

        // Before a refresh the cached tree is stale: the new file is absent.
        let before: Vec<String> = ws
            .explorer_rows()
            .into_iter()
            .filter_map(|r| match r {
                FileTreeRow::Node(n) => Some(n.name),
                FileTreeRow::Empty { .. } => None,
            })
            .collect();
        assert!(!before.contains(&"added.rs".to_string()));

        // Refreshing re-stats the currently-expanded directories, so the new file
        // shows up while the folder stays expanded.
        ws.explorer_refresh();
        let after: Vec<String> = ws
            .explorer_rows()
            .into_iter()
            .filter_map(|r| match r {
                FileTreeRow::Node(n) => Some(n.name),
                FileTreeRow::Empty { .. } => None,
            })
            .collect();
        let _ = std::fs::remove_dir_all(&base);

        assert!(after.contains(&"added.rs".to_string()));
        assert!(ws.explorer_expanded.contains(&src));
    }

    #[test]
    fn applying_a_watcher_event_restats_only_the_affected_loaded_directory() {
        let base = temp_tree("watch-apply");
        let mut ws = Workspace::test_new("ws");
        ws.explorer_set_root(base.clone());
        let src = base.join("src");
        ws.explorer_toggle_expand(&src);

        // A new file lands inside the expanded (loaded) folder, as an agent pane
        // might create it; the watcher reports the created path.
        std::fs::write(src.join("added.rs"), b"x").unwrap();
        let reloaded = ws.explorer_apply_changed_paths(&[src.join("added.rs")]);

        // The affected directory was re-stated and the new file now shows, while
        // the folder stays expanded.
        assert!(reloaded);
        let names: Vec<String> = ws
            .explorer_rows()
            .into_iter()
            .filter_map(|r| match r {
                FileTreeRow::Node(n) => Some(n.name),
                FileTreeRow::Empty { .. } => None,
            })
            .collect();
        assert!(names.contains(&"added.rs".to_string()));
        assert!(ws.explorer_expanded.contains(&src));

        // An event whose containing directory was never loaded into the cache
        // touches nothing and reports no change — it never reads that subtree in.
        let untracked = base.join("ghost").join("x.rs");
        assert!(!ws.explorer_apply_changed_paths(&[untracked]));
        assert!(!ws.explorer_cache.contains_key(&base.join("ghost")));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expanding_a_folder_loads_its_children_only_on_first_expand() {
        let base = temp_tree("expand");
        let mut ws = Workspace::test_new("ws");
        ws.explorer_set_root(base.clone());
        let src = base.join("src");

        // Before expanding, src's children are not in the cache.
        assert!(!ws.explorer_cache.contains_key(&src));

        ws.explorer_toggle_expand(&src);
        assert!(ws.explorer_expanded.contains(&src));
        assert!(ws.explorer_cache.contains_key(&src));
        let expanded_names: Vec<String> = ws
            .explorer_rows()
            .into_iter()
            .filter_map(|r| match r {
                FileTreeRow::Node(n) => Some(n.name),
                FileTreeRow::Empty { .. } => None,
            })
            .collect();
        assert_eq!(
            expanded_names,
            vec![
                "src".to_string(),
                "main.rs".to_string(),
                "README.md".to_string()
            ]
        );

        // Collapsing keeps the cached children so re-expanding does not re-read.
        ws.explorer_toggle_expand(&src);
        assert!(!ws.explorer_expanded.contains(&src));
        assert!(ws.explorer_cache.contains_key(&src));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn each_workspace_has_independent_explorer_tree_state() {
        let base = temp_tree("indep");
        let mut a = Workspace::test_new("a");
        let mut b = Workspace::test_new("b");
        a.explorer_set_root(base.clone());
        b.explorer_set_root(base.clone());
        let src = base.join("src");

        a.explorer_toggle_expand(&src);
        a.explorer_select(base.join("README.md"));

        // Workspace b's tree state is untouched by interactions with a.
        assert!(a.explorer_expanded.contains(&src));
        assert!(!b.explorer_expanded.contains(&src));
        assert_eq!(
            a.explorer_selected(),
            Some(base.join("README.md").as_path())
        );
        assert_eq!(b.explorer_selected(), None);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn workspace_identity_follows_first_tab_root_pane_cwd() {
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        let root_pane = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0].terminal_id(root_pane).unwrap().clone();
        let mut terminals = HashMap::new();
        terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/herdr-test/pion")),
        );
        let terminal_runtimes = TerminalRuntimeRegistry::new();

        assert_eq!(ws.display_name_from(&terminals, &terminal_runtimes), "pion");
        assert_eq!(
            ws.resolved_identity_cwd_from(&terminals, &terminal_runtimes),
            Some(PathBuf::from("/herdr-test/pion"))
        );
    }

    #[test]
    fn moving_tab_keeps_active_identity_and_renumbers_auto_tabs() {
        let mut ws = Workspace::test_new("test");
        let moved_root = ws.tabs[0].root_pane;
        ws.test_add_tab(Some("foo"));
        let final_auto_idx = ws.test_add_tab(None);
        let active_root = ws.tabs[final_auto_idx].root_pane;
        ws.switch_tab(final_auto_idx);

        assert!(ws.move_tab(0, ws.tabs.len()));

        let labels: Vec<_> = ws.tabs.iter().map(|tab| tab.display_name()).collect();
        assert_eq!(labels, vec!["foo", "2", "3"]);
        assert_eq!(ws.tabs[0].custom_name.as_deref(), Some("foo"));
        assert!(ws.tabs[1].custom_name.is_none());
        assert!(ws.tabs[2].custom_name.is_none());
        assert_eq!(ws.tabs[2].root_pane, moved_root);
        assert_eq!(ws.tabs[ws.active_tab].root_pane, active_root);
    }
}
