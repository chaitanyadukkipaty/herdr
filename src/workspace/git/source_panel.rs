use std::path::{Path, PathBuf};

/// One entry in the source-control "changes" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub status: ChangeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeStatus {
    Modified,
    Added,
    Deleted,
    Renamed { from: PathBuf },
    Unmerged,
    Untracked,
}

/// One entry in the source-control "graph" (commit history) list. A single
/// `git log --graph` line is either a commit (`sha` is `Some`) or a pure
/// connector line drawing the graph between commits (`sha` is `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// The graph drawing column for this line, preserved verbatim from
    /// `git log --graph` (e.g. `"* "`, `"|\\ "`, `"| | "`).
    pub graph_cell: String,
    /// Abbreviated commit hash, or `None` for a pure graph-connector line.
    pub sha: Option<String>,
    pub subject: String,
    pub author: String,
    /// Ref decorations, e.g. `["HEAD -> main", "origin/main"]`.
    pub decorations: Vec<String>,
}

/// Upper bound on the number of changed files we track per workspace. A repo
/// with a colossal working tree (e.g. a fresh checkout with everything
/// untracked) should not be allowed to grow the cache without limit.
pub(crate) const MAX_CHANGED_FILES: usize = 5000;

/// Default number of commits loaded into the graph section. Phase 5's
/// "load more" affordance bumps this in increments of the same size.
pub(crate) const DEFAULT_LOADED_COMMIT_COUNT: usize = 50;

/// Field separator embedded in the `git log --pretty=format` so subjects,
/// authors, and refs containing spaces parse unambiguously. ASCII unit
/// separator (0x1f) never appears in normal commit metadata.
const LOG_FIELD_SEP: char = '\u{1f}';

/// Run `git status --porcelain=v2 --untracked-files=all` for `cwd` and parse the
/// result. Returns an empty list if git is unavailable or the command fails.
pub(crate) fn changed_files_for_cwd(cwd: &Path) -> Vec<ChangedFile> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--untracked-files=all",
        ])
        .output()
        .ok();

    match output {
        Some(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_git_status_porcelain_v2(&stdout)
        }
        _ => Vec::new(),
    }
}

/// Wrap a git command in a `sh -c` that pipes its colored output into an
/// interactive pager, so the diff pane stays open and scrollable.
///
/// A bare `git show`/`git diff` is one-shot: with no pager configured (or a
/// pager that quits on short output) it exits the instant it finishes printing,
/// which tears the pane down before it can be read. Piping into `less` keeps the
/// pane alive regardless of the user's git pager config — `q` quits the pager,
/// which then closes the pane. `LESS=-R` renders the ANSI colors and overrides
/// any inherited `-F`/`--quit-if-one-screen` so short diffs do not auto-close.
///
/// `git_cmd` must reference the repo via `"$1"` and its targets (file/sha) via
/// `"$2"`, `"$3"`, …; all are passed as `sh` positional parameters so paths and
/// refs with spaces or shell metacharacters are never re-parsed by the shell.
fn pager_wrapped_argv(git_cmd: &str, cwd: &Path, targets: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "sh".into(),
        "-c".into(),
        format!("{git_cmd} | LESS=-R less"),
        "sh".into(),
        cwd.to_string_lossy().into_owned(),
    ];
    argv.extend(targets.iter().map(|target| target.to_string()));
    argv
}

/// Build the argv that opens a changed file's diff in a new pane. Tracked files
/// diff against `HEAD`; untracked files diff against `/dev/null` (which renders
/// the whole file as an addition). `cwd` is the workspace's resolved git
/// identity directory, passed via `-C` so the command is independent of the new
/// pane's working directory.
pub fn changed_file_diff_argv(cwd: &Path, change: &ChangedFile) -> Vec<String> {
    let file = change.path.to_string_lossy();
    match change.status {
        ChangeStatus::Untracked => pager_wrapped_argv(
            "git -C \"$1\" diff --no-index --color /dev/null \"$2\"",
            cwd,
            &[file.as_ref()],
        ),
        _ => pager_wrapped_argv(
            "git -C \"$1\" diff HEAD --color -- \"$2\"",
            cwd,
            &[file.as_ref()],
        ),
    }
}

/// Build the argv that opens a commit in a new pane via `git show`. `cwd` is the
/// workspace's resolved git identity directory, passed via `-C` so the command is
/// independent of the new pane's working directory.
pub fn commit_show_argv(cwd: &Path, sha: &str) -> Vec<String> {
    pager_wrapped_argv("git -C \"$1\" show --color \"$2\"", cwd, &[sha])
}

/// Build the argv that opens a single file's diff within a commit via `git show
/// <sha> -- <path>`. Used when a file row inside an expanded commit is clicked.
/// For a renamed file `change.path` is the new path, which `git show` resolves.
pub fn commit_file_diff_argv(cwd: &Path, sha: &str, change: &ChangedFile) -> Vec<String> {
    let file = change.path.to_string_lossy();
    pager_wrapped_argv(
        "git -C \"$1\" show --color \"$2\" -- \"$3\"",
        cwd,
        &[sha, file.as_ref()],
    )
}

/// Run `git show --name-status` for a single commit in `cwd` and parse the list
/// of files it touched. Returns an empty list if git is unavailable or fails.
pub(crate) fn commit_files_for_cwd(cwd: &Path, sha: &str) -> Vec<ChangedFile> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "--no-optional-locks",
            "show",
            "--no-color",
            "--name-status",
            "--format=",
            "-M",
        ])
        .arg(sha)
        .output()
        .ok();

    match output {
        Some(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_commit_name_status(&stdout)
        }
        _ => Vec::new(),
    }
}

/// Parse the `--name-status` body of `git show`/`git diff-tree`. Each line is a
/// tab-separated status code and path(s): `M\tpath`, `A\tpath`, `D\tpath`, or
/// `R100\told\tnew` / `C100\tsrc\tdst` for renames and copies. Blank lines (the
/// empty `--format=` header) are skipped.
pub fn parse_commit_name_status(stdout: &str) -> Vec<ChangedFile> {
    let mut changes = Vec::new();
    for line in stdout.lines() {
        if changes.len() >= MAX_CHANGED_FILES {
            break;
        }
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let code = fields.next().unwrap_or("");
        let entry = match code.chars().next() {
            // Rename: `R<score>\t<from>\t<to>`.
            Some('R') => match (fields.next(), fields.next()) {
                (Some(from), Some(to)) => ChangedFile {
                    path: PathBuf::from(to),
                    status: ChangeStatus::Renamed {
                        from: PathBuf::from(from),
                    },
                },
                _ => continue,
            },
            // Copy: `C<score>\t<src>\t<dst>`; the new file is an addition.
            Some('C') => {
                let _src = fields.next();
                match fields.next() {
                    Some(dst) => ChangedFile {
                        path: PathBuf::from(dst),
                        status: ChangeStatus::Added,
                    },
                    None => continue,
                }
            }
            Some(c) => {
                let Some(path) = fields.next() else {
                    continue;
                };
                let status = match c {
                    'A' => ChangeStatus::Added,
                    'D' => ChangeStatus::Deleted,
                    'U' => ChangeStatus::Unmerged,
                    // 'M', 'T' (type change), and anything else → Modified.
                    _ => ChangeStatus::Modified,
                };
                ChangedFile {
                    path: PathBuf::from(path),
                    status,
                }
            }
            None => continue,
        };
        changes.push(entry);
    }
    changes
}

/// Parse the stdout of `git status --porcelain=v2`. See `man git-status`
/// ("Porcelain Format Version 2") for the field layout. Unrecognized lines
/// (header lines, ignored entries) are skipped.
pub fn parse_git_status_porcelain_v2(stdout: &str) -> Vec<ChangedFile> {
    let mut changes = Vec::new();
    for line in stdout.lines() {
        if changes.len() >= MAX_CHANGED_FILES {
            break;
        }
        if let Some(entry) = parse_porcelain_v2_line(line) {
            changes.push(entry);
        }
    }
    changes
}

fn parse_porcelain_v2_line(line: &str) -> Option<ChangedFile> {
    let kind = line.chars().next()?;
    match kind {
        // Ordinary changed entry:
        //   1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
        '1' => {
            let rest = line.get(2..)?;
            let mut fields = rest.splitn(8, ' ');
            let xy = fields.next()?;
            let path = fields.nth(6)?; // skip sub mH mI mW hH hI, land on path
            let mut xy = xy.chars();
            let x = xy.next()?;
            let y = xy.next()?;
            Some(ChangedFile {
                path: PathBuf::from(path),
                status: ordinary_status(x, y),
            })
        }
        // Renamed or copied entry:
        //   2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <Xscore> <path>\t<origPath>
        '2' => {
            let rest = line.get(2..)?;
            let mut fields = rest.splitn(9, ' ');
            let _xy = fields.next()?;
            let paths = fields.nth(7)?; // skip sub mH mI mW hH hI Xscore, land on paths
            let (new_path, orig_path) = paths.split_once('\t')?;
            Some(ChangedFile {
                path: PathBuf::from(new_path),
                status: ChangeStatus::Renamed {
                    from: PathBuf::from(orig_path),
                },
            })
        }
        // Unmerged entry:
        //   u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
        'u' => {
            let rest = line.get(2..)?;
            let mut fields = rest.splitn(10, ' ');
            let _xy = fields.next()?;
            let path = fields.nth(8)?; // skip sub m1 m2 m3 mW h1 h2 h3, land on path
            Some(ChangedFile {
                path: PathBuf::from(path),
                status: ChangeStatus::Unmerged,
            })
        }
        // Untracked entry: `? <path>`
        '?' => {
            let path = line.get(2..)?;
            Some(ChangedFile {
                path: PathBuf::from(path),
                status: ChangeStatus::Untracked,
            })
        }
        // Ignored entries (`!`) and header lines (`#`) are not surfaced.
        _ => None,
    }
}

/// Map an ordinary entry's `XY` field to a single display status. `X` is the
/// staged (index-vs-HEAD) state, `Y` is the unstaged (worktree-vs-index) state.
/// We prefer the worktree change when present, falling back to the staged one.
fn ordinary_status(x: char, y: char) -> ChangeStatus {
    let pick = if y != '.' { y } else { x };
    match pick {
        'A' | 'C' => ChangeStatus::Added,
        'D' => ChangeStatus::Deleted,
        'U' => ChangeStatus::Unmerged,
        // 'M' (modified), 'T' (type change), and anything else fall back to
        // Modified, which is the most useful generic label.
        _ => ChangeStatus::Modified,
    }
}

/// Run `git log --graph` for `cwd` and parse the most recent `count` commits.
/// Returns `(commits, has_more)` where `has_more` is true when the repository
/// has history beyond the requested window. An empty list is returned if git is
/// unavailable, the command fails, or the repo has no commits.
pub(crate) fn commits_for_cwd(cwd: &Path, count: usize) -> (Vec<CommitInfo>, bool) {
    let limit = count.max(1);
    // Request one extra commit so we can tell whether more history exists.
    let n = limit.saturating_add(1);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "--no-optional-locks",
            "log",
            "--graph",
            "--color=never",
            "--pretty=format:%h\u{1f}%an\u{1f}%d\u{1f}%s",
        ])
        .arg(format!("-n{n}"))
        .output()
        .ok();

    match output {
        Some(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_git_log_graph(&stdout, limit)
        }
        _ => (Vec::new(), false),
    }
}

/// Parse the stdout of `git log --graph --pretty=format:%h<US>%an<US>%d<US>%s`,
/// keeping at most `limit` commits. Connector-only lines (no commit content) are
/// preserved verbatim so the graph renders correctly. Returns `(entries,
/// has_more)`; `has_more` is true when a `limit + 1`-th commit was present,
/// which the caller derives by requesting one extra commit.
pub fn parse_git_log_graph(stdout: &str, limit: usize) -> (Vec<CommitInfo>, bool) {
    let mut entries = Vec::new();
    let mut commit_count = 0usize;
    let mut has_more = false;
    for line in stdout.lines() {
        let entry = parse_log_graph_line(line);
        if entry.sha.is_some() {
            if commit_count >= limit {
                // The extra commit we asked for: history extends past the window.
                has_more = true;
                break;
            }
            commit_count += 1;
        }
        entries.push(entry);
    }
    (entries, has_more)
}

/// Glyphs `git log --graph` uses to draw the commit graph column. The commit
/// payload begins at the first character outside this set.
fn is_graph_glyph(c: char) -> bool {
    matches!(c, '*' | '|' | '/' | '\\' | '_' | '-' | '.' | ' ')
}

fn parse_log_graph_line(line: &str) -> CommitInfo {
    // A line carrying commit content always contains the field separator; a
    // pure connector line never does.
    if !line.contains(LOG_FIELD_SEP) {
        return CommitInfo {
            graph_cell: line.to_string(),
            sha: None,
            subject: String::new(),
            author: String::new(),
            decorations: Vec::new(),
        };
    }

    let boundary = line
        .char_indices()
        .find(|(_, c)| !is_graph_glyph(*c))
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    let graph_cell = line[..boundary].to_string();
    let content = &line[boundary..];

    let mut fields = content.splitn(4, LOG_FIELD_SEP);
    let sha = fields.next().unwrap_or("").trim().to_string();
    let author = fields.next().unwrap_or("").to_string();
    let decorations = parse_log_decorations(fields.next().unwrap_or(""));
    let subject = fields.next().unwrap_or("").to_string();

    CommitInfo {
        graph_cell,
        sha: Some(sha),
        subject,
        author,
        decorations,
    }
}

/// Split git's `%d` ref-decoration field — e.g. ` (HEAD -> main, origin/main)` —
/// into individual decorations. Returns an empty list when there are no refs.
fn parse_log_decorations(field: &str) -> Vec<String> {
    let trimmed = field.trim();
    let trimmed = trimmed.strip_prefix('(').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix(')').unwrap_or(trimmed).trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_worktree_entry() {
        let changes = parse_git_status_porcelain_v2(
            "1 .M N... 100644 100644 100644 1111111 1111111 src/main.rs\n",
        );
        assert_eq!(
            changes,
            vec![ChangedFile {
                path: PathBuf::from("src/main.rs"),
                status: ChangeStatus::Modified,
            }]
        );
    }

    #[test]
    fn parses_staged_added_entry() {
        let changes = parse_git_status_porcelain_v2(
            "1 A. N... 000000 100644 100644 0000000 2222222 new_file.rs\n",
        );
        assert_eq!(changes[0].status, ChangeStatus::Added);
        assert_eq!(changes[0].path, PathBuf::from("new_file.rs"));
    }

    #[test]
    fn parses_deleted_entry() {
        let changes = parse_git_status_porcelain_v2(
            "1 .D N... 100644 100644 000000 3333333 3333333 gone.rs\n",
        );
        assert_eq!(changes[0].status, ChangeStatus::Deleted);
    }

    #[test]
    fn parses_renamed_entry_with_original_path() {
        let changes = parse_git_status_porcelain_v2(
            "2 R. N... 100644 100644 100644 4444444 4444444 R100 new/name.rs\told/name.rs\n",
        );
        assert_eq!(
            changes,
            vec![ChangedFile {
                path: PathBuf::from("new/name.rs"),
                status: ChangeStatus::Renamed {
                    from: PathBuf::from("old/name.rs"),
                },
            }]
        );
    }

    #[test]
    fn parses_unmerged_entry() {
        let changes = parse_git_status_porcelain_v2(
            "u UU N... 100644 100644 100644 100644 5555555 5555555 5555555 conflict.rs\n",
        );
        assert_eq!(changes[0].status, ChangeStatus::Unmerged);
        assert_eq!(changes[0].path, PathBuf::from("conflict.rs"));
    }

    #[test]
    fn parses_untracked_entry() {
        let changes = parse_git_status_porcelain_v2("? scratch.txt\n");
        assert_eq!(
            changes,
            vec![ChangedFile {
                path: PathBuf::from("scratch.txt"),
                status: ChangeStatus::Untracked,
            }]
        );
    }

    #[test]
    fn skips_ignored_and_header_lines() {
        let changes = parse_git_status_porcelain_v2(
            "# branch.oid 1234\n# branch.head main\n! target/debug/herdr\n",
        );
        assert!(changes.is_empty());
    }

    #[test]
    fn preserves_paths_containing_spaces() {
        let changes = parse_git_status_porcelain_v2(
            "1 .M N... 100644 100644 100644 6666666 6666666 my dir/a file.rs\n",
        );
        assert_eq!(changes[0].path, PathBuf::from("my dir/a file.rs"));
    }

    #[test]
    fn handles_a_mix_of_entry_kinds() {
        let stdout = concat!(
            "# branch.head main\n",
            "1 M. N... 100644 100644 100644 aaa aaa staged.rs\n",
            "1 .M N... 100644 100644 100644 bbb bbb worktree.rs\n",
            "2 R. N... 100644 100644 100644 ccc ccc R100 renamed.rs\toriginal.rs\n",
            "? untracked.rs\n",
        );
        let changes = parse_git_status_porcelain_v2(stdout);
        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0].status, ChangeStatus::Modified);
        assert_eq!(changes[1].status, ChangeStatus::Modified);
        assert!(matches!(changes[2].status, ChangeStatus::Renamed { .. }));
        assert_eq!(changes[3].status, ChangeStatus::Untracked);
    }

    #[test]
    fn diff_argv_for_tracked_file_uses_head() {
        let change = ChangedFile {
            path: PathBuf::from("src/main.rs"),
            status: ChangeStatus::Modified,
        };
        let argv = changed_file_diff_argv(Path::new("/repo"), &change);
        assert_eq!(
            argv,
            vec![
                "sh",
                "-c",
                "git -C \"$1\" diff HEAD --color -- \"$2\" | LESS=-R less",
                "sh",
                "/repo",
                "src/main.rs",
            ]
        );
    }

    #[test]
    fn diff_argv_for_untracked_file_uses_no_index() {
        let change = ChangedFile {
            path: PathBuf::from("scratch.txt"),
            status: ChangeStatus::Untracked,
        };
        let argv = changed_file_diff_argv(Path::new("/repo"), &change);
        assert_eq!(
            argv,
            vec![
                "sh",
                "-c",
                "git -C \"$1\" diff --no-index --color /dev/null \"$2\" | LESS=-R less",
                "sh",
                "/repo",
                "scratch.txt",
            ]
        );
    }

    #[test]
    fn diff_argv_for_deleted_and_renamed_are_tracked() {
        for status in [
            ChangeStatus::Deleted,
            ChangeStatus::Renamed {
                from: PathBuf::from("old.rs"),
            },
            ChangeStatus::Added,
            ChangeStatus::Unmerged,
        ] {
            let change = ChangedFile {
                path: PathBuf::from("file.rs"),
                status,
            };
            let argv = changed_file_diff_argv(Path::new("/repo"), &change);
            assert!(
                argv.iter().any(|a| a.contains("diff HEAD")),
                "argv was {argv:?}"
            );
            assert!(!argv.iter().any(|a| a.contains("--no-index")));
        }
    }

    #[test]
    fn show_argv_uses_sha_and_repo_dir() {
        let argv = commit_show_argv(Path::new("/repo"), "abc1234");
        assert_eq!(
            argv,
            vec![
                "sh",
                "-c",
                "git -C \"$1\" show --color \"$2\" | LESS=-R less",
                "sh",
                "/repo",
                "abc1234",
            ]
        );
    }

    #[test]
    fn commit_file_diff_argv_scopes_show_to_one_path() {
        let change = ChangedFile {
            path: PathBuf::from("src/main.rs"),
            status: ChangeStatus::Modified,
        };
        let argv = commit_file_diff_argv(Path::new("/repo"), "abc1234", &change);
        assert_eq!(
            argv,
            vec![
                "sh",
                "-c",
                "git -C \"$1\" show --color \"$2\" -- \"$3\" | LESS=-R less",
                "sh",
                "/repo",
                "abc1234",
                "src/main.rs",
            ]
        );
    }

    #[test]
    fn parses_commit_name_status_entries() {
        let stdout =
            "\nM\tsrc/lib.rs\nA\tnew.rs\nD\told.rs\nR100\tfrom.rs\tto.rs\nC75\tsrc.rs\tcopy.rs\n";
        let changes = parse_commit_name_status(stdout);
        assert_eq!(
            changes,
            vec![
                ChangedFile {
                    path: PathBuf::from("src/lib.rs"),
                    status: ChangeStatus::Modified,
                },
                ChangedFile {
                    path: PathBuf::from("new.rs"),
                    status: ChangeStatus::Added,
                },
                ChangedFile {
                    path: PathBuf::from("old.rs"),
                    status: ChangeStatus::Deleted,
                },
                ChangedFile {
                    path: PathBuf::from("to.rs"),
                    status: ChangeStatus::Renamed {
                        from: PathBuf::from("from.rs"),
                    },
                },
                ChangedFile {
                    path: PathBuf::from("copy.rs"),
                    status: ChangeStatus::Added,
                },
            ]
        );
    }

    #[test]
    fn commit_name_status_skips_blank_format_header() {
        // `git show --format=` emits a leading blank line before the file list.
        let changes = parse_commit_name_status("\n\nM\tonly.rs\n");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, PathBuf::from("only.rs"));
    }

    #[test]
    fn commit_files_for_non_git_dir_is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "herdr-source-panel-commit-files-{}-{}",
            std::process::id(),
            nanos,
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let files = commit_files_for_cwd(&dir, "HEAD");

        let _ = std::fs::remove_dir_all(&dir);
        assert!(files.is_empty());
    }

    #[test]
    fn changed_files_for_non_git_dir_is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "herdr-source-panel-nongit-{}-{}",
            std::process::id(),
            nanos,
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let changes = changed_files_for_cwd(&dir);

        let _ = std::fs::remove_dir_all(&dir);
        assert!(changes.is_empty());
    }

    /// Build a `git log --graph` fixture line from its parts, joining fields
    /// with the unit separator the real command emits.
    fn log_line(graph: &str, sha: &str, author: &str, decoration: &str, subject: &str) -> String {
        format!("{graph}{sha}\u{1f}{author}\u{1f}{decoration}\u{1f}{subject}")
    }

    #[test]
    fn parses_a_linear_log_preserving_graph_cells() {
        let stdout = format!(
            "{}\n{}\n",
            log_line("* ", "aaaaaaa", "Ada", " (HEAD -> main)", "second"),
            log_line("* ", "bbbbbbb", "Babbage", "", "first"),
        );
        let (commits, has_more) = parse_git_log_graph(&stdout, 50);

        assert!(!has_more);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].graph_cell, "* ");
        assert_eq!(commits[0].sha.as_deref(), Some("aaaaaaa"));
        assert_eq!(commits[0].author, "Ada");
        assert_eq!(commits[0].subject, "second");
        assert_eq!(commits[0].decorations, vec!["HEAD -> main".to_string()]);
        assert_eq!(commits[1].sha.as_deref(), Some("bbbbbbb"));
        assert!(commits[1].decorations.is_empty());
    }

    #[test]
    fn preserves_merge_connector_lines_verbatim() {
        // A merge commit is followed by connector-only lines drawing the graph.
        let stdout = format!(
            "{}\n{}\n{}\n{}\n",
            log_line("*   ", "merge11", "Ada", "", "merge branch"),
            "|\\  ",
            log_line("| * ", "feat222", "Babbage", "", "feature work"),
            log_line("* | ", "main333", "Ada", "", "mainline work"),
        );
        let (commits, _) = parse_git_log_graph(&stdout, 50);

        assert_eq!(commits.len(), 4);
        assert_eq!(commits[0].graph_cell, "*   ");
        assert_eq!(commits[0].sha.as_deref(), Some("merge11"));
        // The connector line is preserved verbatim with no commit payload.
        assert_eq!(commits[1].graph_cell, "|\\  ");
        assert_eq!(commits[1].sha, None);
        assert!(commits[1].subject.is_empty());
        assert_eq!(commits[2].graph_cell, "| * ");
        assert_eq!(commits[2].sha.as_deref(), Some("feat222"));
        assert_eq!(commits[3].graph_cell, "* | ");
    }

    #[test]
    fn splits_multiple_decorations() {
        let decorations = parse_log_decorations(" (HEAD -> main, origin/main)");
        assert_eq!(
            decorations,
            vec!["HEAD -> main".to_string(), "origin/main".to_string()]
        );
    }

    #[test]
    fn decorations_empty_when_no_refs() {
        assert!(parse_log_decorations("").is_empty());
    }

    #[test]
    fn has_more_set_when_log_returns_extra_commit() {
        // limit of 2, but three commits present (the +1 we request) → has_more.
        let stdout = format!(
            "{}\n{}\n{}\n",
            log_line("* ", "ccc1111", "A", "", "c"),
            log_line("* ", "bbb2222", "B", "", "b"),
            log_line("* ", "aaa3333", "C", "", "a"),
        );
        let (commits, has_more) = parse_git_log_graph(&stdout, 2);

        assert!(has_more);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha.as_deref(), Some("ccc1111"));
        assert_eq!(commits[1].sha.as_deref(), Some("bbb2222"));
    }

    #[test]
    fn no_more_when_log_fills_window_exactly() {
        let stdout = format!(
            "{}\n{}\n",
            log_line("* ", "ccc1111", "A", "", "c"),
            log_line("* ", "bbb2222", "B", "", "b"),
        );
        let (commits, has_more) = parse_git_log_graph(&stdout, 2);
        assert!(!has_more);
        assert_eq!(commits.len(), 2);
    }

    #[test]
    fn empty_log_parses_to_no_commits() {
        let (commits, has_more) = parse_git_log_graph("", 50);
        assert!(commits.is_empty());
        assert!(!has_more);
    }

    #[test]
    fn subject_with_separators_and_spaces_survives() {
        let stdout = format!(
            "{}\n",
            log_line(
                "* ",
                "ddd4444",
                "Grace Hopper",
                "",
                "fix: handle a, b, and c"
            )
        );
        let (commits, _) = parse_git_log_graph(&stdout, 50);
        assert_eq!(commits[0].author, "Grace Hopper");
        assert_eq!(commits[0].subject, "fix: handle a, b, and c");
    }

    #[test]
    fn commits_for_non_git_dir_is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "herdr-source-panel-log-nongit-{}-{}",
            std::process::id(),
            nanos,
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let (commits, has_more) = commits_for_cwd(&dir, 50);

        let _ = std::fs::remove_dir_all(&dir);
        assert!(commits.is_empty());
        assert!(!has_more);
    }

    #[test]
    fn caps_changes_at_the_maximum() {
        let mut stdout = String::new();
        for i in 0..(MAX_CHANGED_FILES + 50) {
            stdout.push_str(&format!(
                "1 .M N... 100644 100644 100644 hhh hhh file{i}.rs\n"
            ));
        }
        let changes = parse_git_status_porcelain_v2(&stdout);
        assert_eq!(changes.len(), MAX_CHANGED_FILES);
    }
}
