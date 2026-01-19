//! Environment variable names used by selfci

/// Logging level configuration (e.g., "debug", "info", "warn", "error")
pub const SELFCI_LOG: &str = "SELFCI_LOG";

/// Enable full logging format with timestamps and targets
pub const SELFCI_LOG_FULL: &str = "SELFCI_LOG_FULL";

/// Job name (set by selfci when running job commands)
pub const SELFCI_JOB_NAME: &str = "SELFCI_JOB_NAME";

/// Path to the job control socket (set by selfci when running job commands)
pub const SELFCI_JOB_SOCK_PATH: &str = "SELFCI_JOB_SOCK_PATH";

/// Base directory path (set by selfci when running checks)
pub const SELFCI_BASE_DIR: &str = "SELFCI_BASE_DIR";

/// Candidate directory path (set by selfci when running checks)
pub const SELFCI_CANDIDATE_DIR: &str = "SELFCI_CANDIDATE_DIR";

/// Candidate commit ID (the git/jj commit hash)
pub const SELFCI_CANDIDATE_COMMIT_ID: &str = "SELFCI_CANDIDATE_COMMIT_ID";

/// Candidate change ID (jj change ID, may be empty for git)
pub const SELFCI_CANDIDATE_CHANGE_ID: &str = "SELFCI_CANDIDATE_CHANGE_ID";

/// Candidate ID (the user-provided identifier, e.g., branch name or commit reference)
pub const SELFCI_CANDIDATE_ID: &str = "SELFCI_CANDIDATE_ID";

/// MQ base branch name
pub const SELFCI_MQ_BASE_BRANCH: &str = "SELFCI_MQ_BASE_BRANCH";

/// Force a specific VCS (git or jujutsu)
pub const SELFCI_VCS_FORCE: &str = "SELFCI_VCS_FORCE";

/// Root working directory for CI checks
pub const SELFCI_ROOT_DIR: &str = "SELFCI_ROOT_DIR";

/// Explicit runtime directory for MQ daemon (optional)
pub const SELFCI_MQ_RUNTIME_DIR: &str = "SELFCI_MQ_RUNTIME_DIR";

/// Version of selfci (set by selfci when running hooks and jobs)
pub const SELFCI_VERSION: &str = "SELFCI_VERSION";
