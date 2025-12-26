pub mod config;
pub mod constants;
pub mod envs;
pub mod exit_codes;
pub mod mq_protocol;
pub mod protocol;
pub mod revision;

use duct::cmd;
use error_set::error_set;
use std::path::Path;

pub use config::{SelfCIConfig, init_config, read_config};

error_set! {
    VCSError := {
        #[display("No supported VCS found (looking for .jj or .git directory)")]
        NoVCSFound,
        #[display("Invalid VCS type (must be 'jj' or 'git')")]
        InvalidVCSType,
    }

    WorkDirError := {
        #[display("Failed to create work directory")]
        CreateFailed(std::io::Error),
    }

    VCSOperationError := {
        #[display("Failed to execute VCS command")]
        CommandFailed(std::io::Error),
    }

    ConfigError := {
        #[display("Not initialized: .config/selfci/{} not found", constants::CONFIG_FILENAME)]
        NotInitialized,
        #[display("Failed to read config file")]
        ReadFailed(std::io::Error),
        #[display("Failed to parse config file")]
        ParseFailed(serde_yaml::Error),
    }

    CheckError := {
        #[display("Check command failed")]
        CheckFailed,
    }

    RevisionError := {
        #[display("Failed to resolve revision")]
        ResolutionFailed(revision::RevisionError),
    }

    MainError := VCSError || WorkDirError || VCSOperationError || ConfigError || CheckError || RevisionError
}

error_set! {
    MergeError := {
        #[display("Failed to read config from base branch")]
        ConfigReadFailed(std::io::Error),
        #[display("Failed to parse config from base branch")]
        ConfigParseFailed(serde_yaml::Error),
        #[display("Failed to create temporary worktree")]
        WorktreeCreateFailed(std::io::Error),
        #[display("Failed to rebase")]
        RebaseFailed(std::io::Error),
        #[display("Failed to merge")]
        MergeFailed(std::io::Error),
        #[display("Failed to update branch")]
        BranchUpdateFailed(std::io::Error),
        #[display("Failed to get change ID")]
        ChangeIdFailed(std::io::Error),
    }
}

impl MainError {
    pub fn exit_code(&self) -> i32 {
        match self {
            MainError::NoVCSFound => exit_codes::EXIT_NO_VCS_FOUND,
            MainError::InvalidVCSType => exit_codes::EXIT_INVALID_VCS_TYPE,
            MainError::CreateFailed(_) => exit_codes::EXIT_WORKDIR_CREATE_FAILED,
            MainError::CommandFailed(_) => exit_codes::EXIT_VCS_COMMAND_FAILED,
            MainError::NotInitialized => exit_codes::EXIT_NOT_INITIALIZED,
            MainError::ReadFailed(_) => exit_codes::EXIT_CONFIG_READ_FAILED,
            MainError::ParseFailed(_) => exit_codes::EXIT_CONFIG_PARSE_FAILED,
            MainError::CheckFailed => exit_codes::EXIT_CHECK_FAILED,
            MainError::ResolutionFailed(e) => match e {
                revision::RevisionError::ResolutionFailed { .. } => {
                    exit_codes::EXIT_REVISION_RESOLUTION_FAILED
                }
                revision::RevisionError::InvalidCommitId(_) => {
                    exit_codes::EXIT_REVISION_INVALID_COMMIT_ID
                }
                revision::RevisionError::InvalidOutput { .. } => {
                    exit_codes::EXIT_REVISION_INVALID_OUTPUT
                }
            },
        }
    }
}

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
) -> Result<(), VCSOperationError> {
    match vcs {
        VCS::Jujutsu => {
            // Export jj changes to the underlying git repo once
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
            copy_revision_to_workdir_jj(root_dir, base_workdir, base_revision.as_str(), git_dir)?;

            // Copy candidate revision
            copy_revision_to_workdir_jj(
                root_dir,
                candidate_workdir,
                candidate_revision.as_str(),
                git_dir,
            )?;

            Ok(())
        }
        VCS::Git => {
            // Copy base revision
            copy_revision_to_workdir_git(root_dir, base_workdir, base_revision.as_str())?;

            // Copy candidate revision
            copy_revision_to_workdir_git(root_dir, candidate_workdir, candidate_revision.as_str())?;

            Ok(())
        }
    }
}

fn copy_revision_to_workdir_jj(
    _root_dir: &Path,
    workdir: &Path,
    revision: &str,
    git_dir: &str,
) -> Result<(), VCSOperationError> {
    // Clone the git repository into the workdir
    cmd!("git", "clone", "--quiet", git_dir, ".")
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

    Ok(())
}

fn copy_revision_to_workdir_git(
    root_dir: &Path,
    workdir: &Path,
    revision: &str,
) -> Result<(), VCSOperationError> {
    // Clone the git repository into the workdir
    cmd!("git", "clone", "--quiet", root_dir, ".")
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

    Ok(())
}
