/// Configuration file name
pub const CONFIG_FILENAME: &str = "ci.yaml";

/// Local configuration file name (not checked into source control)
pub const LOCAL_CONFIG_FILENAME: &str = "local.yaml";

/// Gitignore file name for the config directory
pub const GITIGNORE_FILENAME: &str = ".gitignore";

/// Configuration directory path relative to project root
pub const CONFIG_DIR_PATH: &[&str] = &[".config", "selfci"];

/// MQ daemon socket file name
pub const MQ_SOCK_FILENAME: &str = "mq.sock";

/// MQ daemon PID file name
pub const MQ_PID_FILENAME: &str = "mq.pid";

/// MQ daemon directory marker file name (stores project root path)
pub const MQ_DIR_FILENAME: &str = "mq.dir";
