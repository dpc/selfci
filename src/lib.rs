pub mod config;
pub mod constants;
pub mod duct_util;
pub mod envs;
pub mod error;
pub mod exit_codes;
pub mod mq_protocol;
pub mod protocol;
pub mod revision;

use duct::cmd;
use std::path::Path;

pub use config::{CloneMode, InitResult, SelfCIConfig, init_config, read_config};
pub use error::{
    CheckError, CommandOutputError, ConfigError, MQError, MainError, MergeError, VCSError,
    VCSOperationError, WorkDirError,
};

#[derive(Debug, Clone, Copy)]
pub enum VCS {
    Jujutsu,
    Git,
}

pub fn parse_vcs(vcs_str: &str) -> Result<VCS, VCSError> {
    match vcs_str.to_lowercase().as_str() {
        "jj" | "jujutsu" => Ok(VCS::Jujutsu),
        "git" => Ok(VCS::Git),
        _ => Err(VCSError::InvalidVCSType),
    }
}

pub fn detect_vcs(root: &Path) -> Result<VCS, VCSError> {
    let jj_path = root.join(".jj");
    if jj_path.is_dir() {
        return Ok(VCS::Jujutsu);
    }

    let git_path = root.join(".git");
    if git_path.exists() {
        return Ok(VCS::Git);
    }

    Err(VCSError::NoVCSFound)
}

pub fn get_vcs(root: &Path, forced_vcs: Option<&str>) -> Result<VCS, VCSError> {
    if let Some(vcs_str) = forced_vcs {
        parse_vcs(vcs_str)
    } else {
        detect_vcs(root)
    }
}

fn cleanup_jj_export_bookmarks(root_dir: &Path, bookmarks: &[String]) {
    let mut deleted_any = false;
    for bookmark in bookmarks {
        if cmd!("jj", "bookmark", "delete", "--quiet", bookmark)
            .dir(root_dir)
            .run()
            .is_ok()
        {
            deleted_any = true;
        }
    }

    // The temporary bookmarks are exported to git refs before cloning. Delete the
    // git refs too; otherwise the next jj import can resurrect the bookmarks.
    if deleted_any {
        let _ = cmd!("jj", "git", "export", "--quiet").dir(root_dir).run();
    }
}

fn cleanup_stale_jj_export_bookmarks(root_dir: &Path) {
    let Ok(bookmarks) = cmd!("jj", "bookmark", "list").dir(root_dir).read() else {
        return;
    };

    let stale_bookmarks: Vec<String> = bookmarks
        .lines()
        .filter_map(stale_selfci_export_bookmark)
        .collect();

    cleanup_jj_export_bookmarks(root_dir, &stale_bookmarks);
}

fn stale_selfci_export_bookmark(line: &str) -> Option<String> {
    let bookmark = line.split(':').next()?;
    let suffix = bookmark
        .strip_prefix("selfci-export-base-")
        .or_else(|| bookmark.strip_prefix("selfci-export-candidate-"))?;
    let pid = suffix.split('-').next()?;
    let proc_dir = Path::new("/proc");
    if !proc_dir.is_dir() {
        return None;
    }
    let _: u32 = pid.parse().ok()?;
    if proc_dir.join(pid).exists() {
        return None;
    }

    Some(bookmark.to_string())
}

pub fn copy_revisions_to_workdirs(
    vcs: &VCS,
    root_dir: &Path,
    base_workdir: &Path,
    base_revision: &revision::CommitId,
    candidate_workdir: &Path,
    candidate_revision: &revision::CommitId,
    clone_mode: CloneMode,
) -> Result<(), VCSOperationError> {
    match vcs {
        VCS::Jujutsu => {
            cleanup_stale_jj_export_bookmarks(root_dir);
            // Generate unique suffix for temporary bookmark names (pid + timestamp)
            let suffix = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let base_bookmark = format!("selfci-export-base-{}", suffix);
            let candidate_bookmark = format!("selfci-export-candidate-{}", suffix);

            // Create temporary bookmarks for the revisions we need to clone.
            //
            // Why bookmarks are needed:
            // In a colocated jj/git repo, jj stores commits in git's object store. However,
            // `git clone` only fetches objects that are reachable from refs (branches, tags).
            // Test merge commits and other "dangling" commits exist in the object store but
            // aren't reachable from any ref, so `git clone` won't fetch them.
            //
            // By creating temporary bookmarks and exporting them via `jj git export`, we make
            // the commits reachable from git refs, allowing `git clone` to fetch them.
            // The bookmarks are deleted after cloning completes (or on error).
            // Guard bookmark cleanup so it runs even if bookmark creation or
            // subsequent operations fail.
            let root_dir_owned = root_dir.to_path_buf();
            let cleanup_bookmarks = vec![base_bookmark.clone(), candidate_bookmark.clone()];
            let _bookmark_cleanup = scopeguard::guard((), move |_| {
                cleanup_jj_export_bookmarks(&root_dir_owned, &cleanup_bookmarks);
            });

            cmd!(
                "jj",
                "bookmark",
                "create",
                "--quiet",
                "-r",
                base_revision.as_str(),
                &base_bookmark
            )
            .dir(root_dir)
            .run()
            .map_err(VCSOperationError::CommandFailed)?;

            cmd!(
                "jj",
                "bookmark",
                "create",
                "--quiet",
                "-r",
                candidate_revision.as_str(),
                &candidate_bookmark
            )
            .dir(root_dir)
            .run()
            .map_err(VCSOperationError::CommandFailed)?;

            // Export jj bookmarks to git refs. This makes the commits reachable
            // from git's perspective, so `git clone` will include them.
            cmd!("jj", "git", "export", "--quiet")
                .dir(root_dir)
                .run()
                .map_err(VCSOperationError::CommandFailed)?;

            // Get the git directory path from jj
            let git_dir = cmd!("jj", "git", "root")
                .dir(root_dir)
                .read()
                .map_err(VCSOperationError::CommandFailed)?;
            let git_dir = git_dir.trim();

            // Copy base revision
            copy_revision_to_workdir_jj(
                root_dir,
                base_workdir,
                base_revision.as_str(),
                git_dir,
                clone_mode,
            )?;

            // Copy candidate revision
            copy_revision_to_workdir_jj(
                root_dir,
                candidate_workdir,
                candidate_revision.as_str(),
                git_dir,
                clone_mode,
            )?;

            // _bookmark_cleanup guard runs here on drop (success or error)
            Ok(())
        }
        VCS::Git => {
            // Copy base revision
            copy_revision_to_workdir_git(
                root_dir,
                base_workdir,
                base_revision.as_str(),
                clone_mode,
            )?;

            // Copy candidate revision
            copy_revision_to_workdir_git(
                root_dir,
                candidate_workdir,
                candidate_revision.as_str(),
                clone_mode,
            )?;

            Ok(())
        }
    }
}

fn copy_revision_to_workdir_jj(
    _root_dir: &Path,
    workdir: &Path,
    revision: &str,
    git_dir: &str,
    clone_mode: CloneMode,
) -> Result<(), VCSOperationError> {
    // Convert to file:// URL for local clones to make --filter work
    let git_url = format!("file://{}", git_dir);

    match clone_mode {
        CloneMode::Full => {
            // Clone the full git repository into the workdir
            cmd!("git", "clone", "--quiet", &git_url, ".")
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;

            // Checkout the specific revision (commit ID), suppressing all output
            cmd!("git", "checkout", "--quiet", revision)
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;
        }
        CloneMode::Partial => {
            // Clone with blob filter for partial clone (downloads commits/trees, fetches blobs on-demand)
            cmd!(
                "git",
                "clone",
                "--quiet",
                "--filter=blob:none",
                "--no-checkout",
                &git_url,
                "."
            )
            .dir(workdir)
            .stderr_null()
            .run()
            .map_err(VCSOperationError::CommandFailed)?;

            // Checkout the specific revision (commit ID), suppressing all output
            cmd!("git", "checkout", "--quiet", revision)
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;
        }
        CloneMode::Shallow => {
            // Shallow clone: most compact, only fetches the specific commit
            // Initialize empty repository
            cmd!("git", "init", "--quiet")
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;

            // Add remote
            cmd!("git", "remote", "add", "origin", &git_url)
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;

            // Fetch just the specific commit with depth 1
            cmd!("git", "fetch", "--quiet", "--depth=1", "origin", revision)
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;

            // Checkout the fetched commit
            cmd!("git", "checkout", "--quiet", "FETCH_HEAD")
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;
        }
    }

    Ok(())
}

fn copy_revision_to_workdir_git(
    root_dir: &Path,
    workdir: &Path,
    revision: &str,
    clone_mode: CloneMode,
) -> Result<(), VCSOperationError> {
    // Convert to file:// URL for local clones to make --filter work
    let root_url = format!("file://{}", root_dir.display());

    match clone_mode {
        CloneMode::Full => {
            // Clone the full git repository into the workdir
            cmd!("git", "clone", "--quiet", &root_url, ".")
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;

            // Checkout the specific revision (commit ID), suppressing all output
            cmd!("git", "checkout", "--quiet", revision)
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;
        }
        CloneMode::Partial => {
            // Clone with blob filter for partial clone (downloads commits/trees, fetches blobs on-demand)
            cmd!(
                "git",
                "clone",
                "--quiet",
                "--filter=blob:none",
                "--no-checkout",
                &root_url,
                "."
            )
            .dir(workdir)
            .stderr_null()
            .run()
            .map_err(VCSOperationError::CommandFailed)?;

            // Checkout the specific revision (commit ID), suppressing all output
            cmd!("git", "checkout", "--quiet", revision)
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;
        }
        CloneMode::Shallow => {
            // Shallow clone: most compact, only fetches the specific commit
            // Initialize empty repository
            cmd!("git", "init", "--quiet")
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;

            // Add remote
            cmd!("git", "remote", "add", "origin", &root_url)
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;

            // Fetch just the specific commit with depth 1
            cmd!("git", "fetch", "--quiet", "--depth=1", "origin", revision)
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;

            // Checkout the fetched commit
            cmd!("git", "checkout", "--quiet", "FETCH_HEAD")
                .dir(workdir)
                .stderr_null()
                .run()
                .map_err(VCSOperationError::CommandFailed)?;
        }
    }

    Ok(())
}
