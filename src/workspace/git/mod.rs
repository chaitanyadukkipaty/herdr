mod config;
#[cfg(test)]
mod config_tests;
mod discovery;
mod source_panel;
mod status;
#[cfg(test)]
mod test_support;

pub use self::{
    discovery::{derive_label_from_cwd, git_branch, git_space_metadata, GitSpaceMetadata},
    source_panel::{
        changed_file_diff_argv, commit_file_diff_argv, commit_show_argv, ChangeStatus, ChangedFile,
        CommitInfo,
    },
    status::{git_status_cache_key, git_status_snapshot_for_cwd, GitStatusCacheEntry},
};

pub(crate) use self::source_panel::{commit_files_for_cwd, DEFAULT_LOADED_COMMIT_COUNT};

#[cfg(test)]
pub(super) use self::status::git_ahead_behind;
