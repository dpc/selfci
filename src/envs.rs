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

/// Force a specific VCS (git or jujutsu)
pub const SELFCI_VCS_FORCE: &str = "SELFCI_VCS_FORCE";

/// Root working directory for CI checks
pub const SELFCI_ROOT_DIR: &str = "SELFCI_ROOT_DIR";

/// Explicit runtime directory for MQ daemon (optional)
pub const SELFCI_MQ_RUNTIME_DIR: &str = "SELFCI_MQ_RUNTIME_DIR";
