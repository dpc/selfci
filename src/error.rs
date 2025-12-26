use error_set::error_set;

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
        #[display("Not initialized: .config/selfci/{} not found", crate::constants::CONFIG_FILENAME)]
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
        ResolutionFailed(crate::revision::RevisionError),
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
            MainError::NoVCSFound => crate::exit_codes::EXIT_NO_VCS_FOUND,
            MainError::InvalidVCSType => crate::exit_codes::EXIT_INVALID_VCS_TYPE,
            MainError::CreateFailed(_) => crate::exit_codes::EXIT_WORKDIR_CREATE_FAILED,
            MainError::CommandFailed(_) => crate::exit_codes::EXIT_VCS_COMMAND_FAILED,
            MainError::NotInitialized => crate::exit_codes::EXIT_NOT_INITIALIZED,
            MainError::ReadFailed(_) => crate::exit_codes::EXIT_CONFIG_READ_FAILED,
            MainError::ParseFailed(_) => crate::exit_codes::EXIT_CONFIG_PARSE_FAILED,
            MainError::CheckFailed => crate::exit_codes::EXIT_CHECK_FAILED,
            MainError::ResolutionFailed(e) => match e {
                crate::revision::RevisionError::ResolutionFailed { .. } => {
                    crate::exit_codes::EXIT_REVISION_RESOLUTION_FAILED
                }
                crate::revision::RevisionError::InvalidCommitId(_) => {
                    crate::exit_codes::EXIT_REVISION_INVALID_COMMIT_ID
                }
                crate::revision::RevisionError::InvalidOutput { .. } => {
                    crate::exit_codes::EXIT_REVISION_INVALID_OUTPUT
                }
            },
        }
    }
}
