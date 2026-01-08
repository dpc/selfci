pub mod config;
pub mod constants;
pub mod envs;
pub mod error;
pub mod exit_codes;
pub mod mq_protocol;
pub mod protocol;
pub mod revision;

use duct::cmd;
use std::path::Path;

pub use config::{CloneMode, SelfCIConfig, init_config, read_config};
pub use error::{
    CheckError, ConfigError, MainError, MergeError, VCSError, VCSOperationError, WorkDirError,
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

            // Create temporary bookmarks for the revisions we need to export
            // (jj git export only exports commits reachable from bookmarks)
            cmd!(
                "jj",
                "bookmark",
                "create",
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
                "-r",
                candidate_revision.as_str(),
                &candidate_bookmark
            )
            .dir(root_dir)
            .run()
            .map_err(VCSOperationError::CommandFailed)?;

            // Export jj changes to the underlying git repo
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

            // Delete the temporary bookmarks (cleanup)
            let _ = cmd!("jj", "bookmark", "delete", &base_bookmark)
                .dir(root_dir)
                .run();

            let _ = cmd!("jj", "bookmark", "delete", &candidate_bookmark)
                .dir(root_dir)
                .run();

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
