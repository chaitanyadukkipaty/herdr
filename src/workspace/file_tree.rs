//! The Explorer mode's file-tree data layer: a directory-entry model plus pure
//! listing, sorting, and flattening functions. Mirrors the source panel's git
//! data layer (`source_panel.rs`) — the pure functions take their inputs as
//! parameters (entries, a loaded-directory cache, an expanded set) rather than
//! reading the filesystem internally, so they are unit-testable over in-memory
//! fixtures with no real FS. The single FS adapter, [`read_dir_sorted`], is a
//! thin wrapper over `std::fs::read_dir`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::workspace::{ChangeStatus, ChangedFile};

/// The git-status decoration the rollup attaches to a tree path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTreeDecoration {
    /// An exactly-changed file, carrying its precise git status so the row is
    /// colored via the source panel's `ChangeStatus` palette.
    Changed(ChangeStatus),
    /// A folder on the ancestry of one or more changed files. Collapsed or not,
    /// it shows a rolled-up status dot.
    ContainsChanges,
}

/// Roll the workspace's changed-files set up the Explorer tree. A pure
/// prefix-match over the paths, reading no filesystem: each change's path is
/// resolved against `root` and mapped to [`FileTreeDecoration::Changed`], and
/// every ancestor directory between that file and `root` (exclusive of `root`
/// itself, inclusive of the immediate parent) is marked
/// [`FileTreeDecoration::ContainsChanges`]. So a collapsed folder containing a
/// change shows a dot without being expanded, while folders off the changed
/// paths' ancestry get no entry at all. Change paths are matched as given
/// (joined onto `root`); they are never read from disk.
pub fn rollup_git_status(
    root: &Path,
    changes: &[ChangedFile],
) -> HashMap<PathBuf, FileTreeDecoration> {
    let mut decorations = HashMap::new();
    for change in changes {
        let path = root.join(&change.path);
        // Mark each ancestor up to (but not including) the root.
        let mut ancestors = path.ancestors();
        ancestors.next(); // skip the file path itself
        for ancestor in ancestors {
            if ancestor == root {
                break;
            }
            decorations
                .entry(ancestor.to_path_buf())
                .or_insert(FileTreeDecoration::ContainsChanges);
        }
        // The file's own precise status wins over any rolled-up marker.
        decorations.insert(path, FileTreeDecoration::Changed(change.status.clone()));
    }
    decorations
}

/// Resolve the editor command used to open a clicked Explorer file, following
/// the precedence `source_panel.editor` → `$VISUAL` → `$EDITOR` → `vi`. Each
/// candidate is taken in order and the first non-blank one wins; a blank or
/// whitespace-only value is treated as unset and skipped. Pure over its inputs
/// (the caller reads the config and environment) so it is testable without
/// touching the real environment.
pub fn resolve_editor_command(
    configured: Option<&str>,
    visual: Option<&str>,
    editor: Option<&str>,
) -> String {
    [configured, visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|candidate| !candidate.is_empty())
        .unwrap_or("vi")
        .to_string()
}

/// Build the argv that opens `path` in `editor` inside a terminal pane. Like the
/// diff/commit commands it runs through `sh -c` so an editor invocation carrying
/// flags (`code -w`, `emacsclient -t`) works, but — unlike them — it is never
/// piped into a pager, since an editor manages its own screen. The file is passed
/// as the `$1` positional parameter so paths with spaces or shell metacharacters
/// are never re-parsed by the shell.
pub fn editor_open_argv(editor: &str, path: &Path) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!("{editor} \"$1\""),
        "sh".into(),
        path.to_string_lossy().into_owned(),
    ]
}

/// The directories the Explorer's non-recursive filesystem watcher should be
/// watching: the tree root (always watched while a tree is rooted) plus every
/// currently-expanded folder. Changes inside a collapsed folder are picked up by
/// the re-read that happens when it is next expanded, so collapsed folders are
/// deliberately left unwatched. Pure over its inputs — reads no filesystem and
/// touches no watch handles.
pub fn watch_targets(root: Option<&Path>, expanded: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    let mut targets: HashSet<PathBuf> = expanded.iter().cloned().collect();
    if let Some(root) = root {
        targets.insert(root.to_path_buf());
    }
    targets
}

/// Diff the watcher's `currently_watched` set against the `desired` set
/// ([`watch_targets`]) to drive a non-recursive watcher so it tracks exactly the
/// expanded tree: a folder gains a watch when it is expanded and drops it when it
/// is collapsed. Returns `(to_add, to_drop)` — paths in `desired` but not yet
/// watched, and paths watched but no longer desired. Both vectors are sorted so
/// the reconciliation is deterministic. Pure over its inputs.
pub fn reconcile_watches(
    desired: &HashSet<PathBuf>,
    currently_watched: &HashSet<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut to_add: Vec<PathBuf> = desired.difference(currently_watched).cloned().collect();
    let mut to_drop: Vec<PathBuf> = currently_watched.difference(desired).cloned().collect();
    to_add.sort();
    to_drop.sort();
    (to_add, to_drop)
}

/// What kind of filesystem object a tree entry is. Symlinks are a distinct
/// variant — they are shown in the tree but never followed, so a symlink to a
/// directory is a non-expandable leaf and the tree never recurses into it
/// (preventing cycles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEntryKind {
    Directory,
    File,
    Symlink,
}

/// One entry (child) of a directory in the Explorer tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTreeEntry {
    /// The entry's basename, used for display and sorting.
    pub name: String,
    /// The entry's absolute path, used as the cache key when it is a directory
    /// expanded to load its own children.
    pub path: PathBuf,
    pub kind: FileEntryKind,
}

impl FileTreeEntry {
    /// Whether this entry is a real directory the tree may recurse into. A
    /// symlink — even one pointing at a directory — is never expandable, so the
    /// tree is never followed into it.
    pub fn is_expandable_dir(&self) -> bool {
        matches!(self.kind, FileEntryKind::Directory)
    }
}

/// Sort a directory's entries for display: directories first, then files and
/// symlinks, each group ordered case-insensitive alphabetical by name. Every
/// entry passed in is preserved — dotfiles and git-ignored paths included —
/// because the Explorer reflects the filesystem, not git tracking.
pub fn sort_entries(mut entries: Vec<FileTreeEntry>) -> Vec<FileTreeEntry> {
    entries.sort_by(|a, b| {
        // Real directories sort ahead of everything else; symlinks sort with
        // files since they are non-expandable leaves.
        b.is_expandable_dir()
            .cmp(&a.is_expandable_dir())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

/// List a directory's immediate children via `std::fs::read_dir`, classify each
/// without following symlinks, and return them sorted for display. Every entry
/// is included (dotfiles and git-ignored paths alike). A directory that fails to
/// read (permissions, races, or not a directory) yields an empty list rather
/// than an error — the caller caches that empty result so the failed read is
/// never retried in a loop.
pub fn read_dir_sorted(dir: &Path) -> Vec<FileTreeEntry> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        // `read_dir`'s `file_type` does not traverse symlinks, so a symlink is
        // reported as a symlink (never as its target's kind) and the tree is
        // never followed into it.
        let kind = match entry.file_type() {
            Ok(ft) if ft.is_symlink() => FileEntryKind::Symlink,
            Ok(ft) if ft.is_dir() => FileEntryKind::Directory,
            _ => FileEntryKind::File,
        };
        entries.push(FileTreeEntry { name, path, kind });
    }
    sort_entries(entries)
}

/// Truncate a display name to fit `max_width` columns, appending an ellipsis
/// when it is clipped. Widths below 1 yield an empty string; a width of 1 with
/// an over-long name yields just the ellipsis.
pub fn truncate_with_ellipsis(name: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let count = name.chars().count();
    if count <= max_width {
        return name.to_string();
    }
    let keep = max_width.saturating_sub(1);
    let mut out: String = name.chars().take(keep).collect();
    out.push('…');
    out
}

/// One visible row of the flattened Explorer tree: either an entry node or a
/// dim indicator beneath an expanded directory that has no children (genuinely
/// empty, or unreadable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTreeRow {
    Node(FileTreeNode),
    /// Placeholder shown when an expanded directory loaded no entries. `depth`
    /// is the indentation level of the (would-be) children.
    Empty {
        depth: usize,
    },
}

/// An entry node positioned in the flattened tree, carrying its indentation
/// `depth` and (for directories) whether it is currently expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTreeNode {
    pub name: String,
    pub path: PathBuf,
    pub kind: FileEntryKind,
    pub depth: usize,
    /// True when this is an expandable directory currently in the expanded set.
    pub expanded: bool,
}

/// Flatten the lazily-loaded tree into the ordered rows the Explorer renders and
/// scrolls over. Walks the `root` directory's cached children depth-first,
/// descending into a directory only when it is in `expanded` (and so was loaded
/// into `cache`). Symlinks are never expandable, so the walk never follows one —
/// preventing cycles. An expanded directory whose cached child list is empty
/// contributes a single [`FileTreeRow::Empty`] indicator rather than recursing.
///
/// Pure over its inputs: it reads only `cache` and `expanded`, never the
/// filesystem, so it is unit-testable over in-memory fixtures.
pub fn flatten_tree(
    root: &Path,
    cache: &HashMap<PathBuf, Vec<FileTreeEntry>>,
    expanded: &HashSet<PathBuf>,
) -> Vec<FileTreeRow> {
    let mut rows = Vec::new();
    flatten_into(root, 0, cache, expanded, &mut rows);
    rows
}

fn flatten_into(
    dir: &Path,
    depth: usize,
    cache: &HashMap<PathBuf, Vec<FileTreeEntry>>,
    expanded: &HashSet<PathBuf>,
    rows: &mut Vec<FileTreeRow>,
) {
    let children = cache.get(dir).map(Vec::as_slice).unwrap_or(&[]);
    for entry in children {
        let is_expanded = entry.is_expandable_dir() && expanded.contains(&entry.path);
        rows.push(FileTreeRow::Node(FileTreeNode {
            name: entry.name.clone(),
            path: entry.path.clone(),
            kind: entry.kind,
            depth,
            expanded: is_expanded,
        }));
        if is_expanded {
            let grandchildren = cache.get(&entry.path).map(Vec::as_slice).unwrap_or(&[]);
            if grandchildren.is_empty() {
                rows.push(FileTreeRow::Empty { depth: depth + 1 });
            } else {
                flatten_into(&entry.path, depth + 1, cache, expanded, rows);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{ChangeStatus, ChangedFile};

    fn entry(name: &str, kind: FileEntryKind) -> FileTreeEntry {
        FileTreeEntry {
            name: name.to_string(),
            path: PathBuf::from("/root").join(name),
            kind,
        }
    }

    fn dir(name: &str) -> FileTreeEntry {
        entry(name, FileEntryKind::Directory)
    }
    fn file(name: &str) -> FileTreeEntry {
        entry(name, FileEntryKind::File)
    }

    /// Build an entry whose path nests under `parent` rather than `/root`, so a
    /// multi-level cache can be assembled.
    fn child(parent: &Path, name: &str, kind: FileEntryKind) -> FileTreeEntry {
        FileTreeEntry {
            name: name.to_string(),
            path: parent.join(name),
            kind,
        }
    }

    #[test]
    fn flatten_descends_only_into_expanded_directories() {
        let root = PathBuf::from("/root");
        let src = root.join("src");
        let mut cache = HashMap::new();
        cache.insert(root.clone(), vec![dir("src"), file("README.md")]);
        cache.insert(
            src.clone(),
            vec![child(&src, "main.rs", FileEntryKind::File)],
        );

        // Collapsed: only the root's immediate children show.
        let collapsed = flatten_tree(&root, &cache, &HashSet::new());
        let names: Vec<&str> = collapsed
            .iter()
            .filter_map(|r| match r {
                FileTreeRow::Node(n) => Some(n.name.as_str()),
                FileTreeRow::Empty { .. } => None,
            })
            .collect();
        assert_eq!(names, vec!["src", "README.md"]);

        // Expanding src reveals its child indented one level deeper.
        let mut expanded = HashSet::new();
        expanded.insert(src.clone());
        let rows = flatten_tree(&root, &cache, &expanded);
        let nodes: Vec<(&str, usize, bool)> = rows
            .iter()
            .filter_map(|r| match r {
                FileTreeRow::Node(n) => Some((n.name.as_str(), n.depth, n.expanded)),
                FileTreeRow::Empty { .. } => None,
            })
            .collect();
        assert_eq!(
            nodes,
            vec![
                ("src", 0, true),
                ("main.rs", 1, false),
                ("README.md", 0, false)
            ]
        );
    }

    #[test]
    fn flatten_never_follows_a_symlink_even_when_expanded() {
        let root = PathBuf::from("/root");
        let link = root.join("link");
        let mut cache = HashMap::new();
        cache.insert(root.clone(), vec![entry("link", FileEntryKind::Symlink)]);
        // Pretend the symlink target's children were somehow cached under its
        // path, and the path is in the expanded set.
        cache.insert(
            link.clone(),
            vec![child(&link, "secret.rs", FileEntryKind::File)],
        );
        let mut expanded = HashSet::new();
        expanded.insert(link.clone());

        let rows = flatten_tree(&root, &cache, &expanded);
        let names: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                FileTreeRow::Node(n) => Some(n.name.as_str()),
                FileTreeRow::Empty { .. } => None,
            })
            .collect();
        // The symlink shows, but its target's children are never walked.
        assert_eq!(names, vec!["link"]);
        let link_node = match &rows[0] {
            FileTreeRow::Node(n) => n,
            FileTreeRow::Empty { .. } => panic!("expected a node row"),
        };
        assert!(!link_node.expanded, "a symlink is never marked expanded");
    }

    #[test]
    fn expanded_empty_or_unreadable_directory_yields_an_indicator_row() {
        let root = PathBuf::from("/root");
        let empty = root.join("empty");
        let mut cache = HashMap::new();
        cache.insert(root.clone(), vec![dir("empty")]);
        // An unreadable directory caches as an empty child list (read_dir failed)
        // — indistinguishable from a genuinely empty directory, and crucially it
        // is present in the cache so it is never re-read in a retry loop.
        cache.insert(empty.clone(), Vec::new());
        let mut expanded = HashSet::new();
        expanded.insert(empty.clone());

        let rows = flatten_tree(&root, &cache, &expanded);
        assert_eq!(
            rows,
            vec![
                FileTreeRow::Node(FileTreeNode {
                    name: "empty".to_string(),
                    path: empty,
                    kind: FileEntryKind::Directory,
                    depth: 0,
                    expanded: true,
                }),
                FileTreeRow::Empty { depth: 1 },
            ]
        );
    }

    #[test]
    fn read_dir_sorted_is_empty_for_a_nonexistent_directory() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing = std::env::temp_dir().join(format!(
            "herdr-file-tree-missing-{}-{}",
            std::process::id(),
            nanos,
        ));
        // The directory does not exist, so read_dir fails — but the call must
        // degrade to an empty list rather than panic.
        assert!(read_dir_sorted(&missing).is_empty());
    }

    #[test]
    fn read_dir_sorted_lists_real_children_including_dotfiles_and_dirs_first() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "herdr-file-tree-read-{}-{}",
            std::process::id(),
            nanos,
        ));
        std::fs::create_dir_all(base.join("subdir")).unwrap();
        std::fs::write(base.join("Zfile.txt"), b"x").unwrap();
        std::fs::write(base.join(".dotfile"), b"x").unwrap();

        let entries = read_dir_sorted(&base);
        let _ = std::fs::remove_dir_all(&base);

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // The directory sorts first; the dotfile is included and sorts with the
        // files ahead of "Zfile.txt" (case-insensitive).
        assert_eq!(names, vec!["subdir", ".dotfile", "Zfile.txt"]);
        assert_eq!(entries[0].kind, FileEntryKind::Directory);
    }

    #[test]
    fn truncate_with_ellipsis_clips_long_names_and_keeps_short_ones() {
        assert_eq!(truncate_with_ellipsis("main.rs", 20), "main.rs");
        assert_eq!(
            truncate_with_ellipsis("a_very_long_name.rs", 10),
            "a_very_lo…"
        );
        assert_eq!(truncate_with_ellipsis("name", 0), "");
        // Exactly the available width: no ellipsis.
        assert_eq!(truncate_with_ellipsis("abcd", 4), "abcd");
    }

    #[test]
    fn includes_dotfiles_and_treats_symlinks_as_non_followed_leaves() {
        let sorted = sort_entries(vec![
            entry(".gitignore", FileEntryKind::File),
            entry(".config", FileEntryKind::Directory),
            entry("link-to-dir", FileEntryKind::Symlink),
            entry("zebra", FileEntryKind::Directory),
        ]);

        let names: Vec<&str> = sorted.iter().map(|e| e.name.as_str()).collect();
        // Dotfiles are not filtered out; a symlink (even pointing at a
        // directory) sorts with the files because it is a non-followed leaf.
        assert_eq!(names, vec![".config", "zebra", ".gitignore", "link-to-dir"]);

        let symlink = sorted.iter().find(|e| e.name == "link-to-dir").unwrap();
        assert_eq!(symlink.kind, FileEntryKind::Symlink);
        assert!(
            !symlink.is_expandable_dir(),
            "a symlink must never be expandable, so the tree is never followed into it"
        );
    }

    fn changed(path: &str, status: ChangeStatus) -> ChangedFile {
        ChangedFile {
            path: PathBuf::from(path),
            status,
        }
    }

    #[test]
    fn rollup_decorates_changed_files_and_marks_every_ancestor_folder() {
        let root = PathBuf::from("/root");
        // Two changes nested at different depths, plus an unrelated folder
        // (tests/) that contains no change.
        let changes = vec![
            changed("src/app/state.rs", ChangeStatus::Modified),
            changed("docs/readme.md", ChangeStatus::Added),
        ];

        let decorations = rollup_git_status(&root, &changes);

        // The changed files carry their precise status.
        assert_eq!(
            decorations.get(&root.join("src/app/state.rs")),
            Some(&FileTreeDecoration::Changed(ChangeStatus::Modified)),
        );
        assert_eq!(
            decorations.get(&root.join("docs/readme.md")),
            Some(&FileTreeDecoration::Changed(ChangeStatus::Added)),
        );

        // Every ancestor folder up to the root is marked as containing changes,
        // so a collapsed folder shows a rolled-up dot.
        assert_eq!(
            decorations.get(&root.join("src/app")),
            Some(&FileTreeDecoration::ContainsChanges),
        );
        assert_eq!(
            decorations.get(&root.join("src")),
            Some(&FileTreeDecoration::ContainsChanges),
        );
        assert_eq!(
            decorations.get(&root.join("docs")),
            Some(&FileTreeDecoration::ContainsChanges),
        );

        // Unrelated folders are never marked.
        assert_eq!(decorations.get(&root.join("tests")), None);
        assert_eq!(decorations.get(&root.join("src/ui")), None);
    }

    #[test]
    fn sorts_directories_before_files_each_case_insensitive_alphabetical() {
        let sorted = sort_entries(vec![
            entry("README.md", FileEntryKind::File),
            entry("src", FileEntryKind::Directory),
            entry("Cargo.toml", FileEntryKind::File),
            entry("docs", FileEntryKind::Directory),
        ]);

        let names: Vec<&str> = sorted.iter().map(|e| e.name.as_str()).collect();
        // Directories first (docs, src — case-insensitive), then files
        // (Cargo.toml, README.md — case-insensitive).
        assert_eq!(names, vec!["docs", "src", "Cargo.toml", "README.md"]);
    }

    #[test]
    fn editor_resolution_prefers_configured_then_visual_then_editor_then_vi() {
        // Configured `source_panel.editor` wins over everything.
        assert_eq!(
            resolve_editor_command(Some("hx"), Some("nvim"), Some("nano")),
            "hx"
        );
        // No configured editor → $VISUAL.
        assert_eq!(
            resolve_editor_command(None, Some("nvim"), Some("nano")),
            "nvim"
        );
        // No configured editor and no $VISUAL → $EDITOR.
        assert_eq!(resolve_editor_command(None, None, Some("nano")), "nano");
        // Nothing set → vi.
        assert_eq!(resolve_editor_command(None, None, None), "vi");
        // Blank values are treated as unset and skipped.
        assert_eq!(
            resolve_editor_command(Some("  "), Some(""), Some("nano")),
            "nano"
        );
    }

    #[test]
    fn editor_open_argv_runs_the_editor_on_the_file_without_a_pager() {
        let argv = editor_open_argv("nvim", Path::new("/repo/src/main.rs"));

        // Runs through `sh -c` so an editor command with flags (e.g. `code -w`)
        // works, but — unlike the diff/commit commands — is never piped into a
        // pager: an editor manages its own screen.
        assert_eq!(argv[0], "sh");
        assert_eq!(argv[1], "-c");
        let script = &argv[2];
        assert!(script.contains("nvim"), "script should invoke the editor");
        assert!(
            !script.contains("less") && !script.to_lowercase().contains("pager"),
            "editor command must not be wrapped in a pager: {script}"
        );
        // The file is passed as a positional parameter, not interpolated into the
        // script, so paths with spaces or shell metacharacters are safe.
        assert_eq!(argv.last().map(String::as_str), Some("/repo/src/main.rs"));
    }

    #[test]
    fn editor_open_argv_preserves_editor_flags() {
        let argv = editor_open_argv("code -w", Path::new("/repo/a.txt"));
        assert!(argv[2].contains("code -w"));
    }

    #[test]
    fn watch_targets_always_includes_the_root_plus_every_expanded_folder() {
        let root = PathBuf::from("/root");
        let src = root.join("src");
        let app = src.join("app");
        let mut expanded = HashSet::new();
        expanded.insert(src.clone());
        expanded.insert(app.clone());

        let targets = watch_targets(Some(&root), &expanded);

        // The root is watched even though it is not in the expanded set, and each
        // expanded folder is watched so changes inside it are seen live.
        assert!(targets.contains(&root));
        assert!(targets.contains(&src));
        assert!(targets.contains(&app));
        assert_eq!(targets.len(), 3);

        // A collapsed folder (never expanded) is not a watch target — its changes
        // are picked up by the re-read on next expand.
        assert!(!targets.contains(&root.join("docs")));
    }

    #[test]
    fn watch_targets_is_empty_when_no_tree_is_rooted() {
        assert!(watch_targets(None, &HashSet::new()).is_empty());
    }

    #[test]
    fn reconcile_watches_adds_newly_expanded_and_drops_collapsed_folders() {
        let root = PathBuf::from("/root");
        let src = root.join("src");
        let docs = root.join("docs");

        // Currently watching root + docs; the user has just collapsed docs and
        // expanded src, so the desired set is root + src.
        let mut watched = HashSet::new();
        watched.insert(root.clone());
        watched.insert(docs.clone());
        let mut desired = HashSet::new();
        desired.insert(root.clone());
        desired.insert(src.clone());

        let (to_add, to_drop) = reconcile_watches(&desired, &watched);

        assert_eq!(to_add, vec![src]);
        assert_eq!(to_drop, vec![docs]);
    }

    #[test]
    fn reconcile_watches_is_a_no_op_when_the_sets_already_match() {
        let root = PathBuf::from("/root");
        let mut set = HashSet::new();
        set.insert(root.clone());
        set.insert(root.join("src"));

        let (to_add, to_drop) = reconcile_watches(&set, &set);
        assert!(to_add.is_empty());
        assert!(to_drop.is_empty());
    }
}
