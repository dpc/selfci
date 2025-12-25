use serde::Deserialize;
use std::path::Path;

use crate::ConfigError;
use crate::constants::{CONFIG_DIR_PATH, CONFIG_FILENAME};

const CONFIG_TEMPLATE: &str = include_str!("../share/ci-template.yaml");

fn default_command_prefix() -> Vec<String> {
    vec!["bash".to_string(), "-c".to_string()]
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobConfig {
    pub command: String,
    #[serde(default = "default_command_prefix", rename = "command-prefix")]
    pub command_prefix: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MQConfig {
    #[serde(rename = "base-branch")]
    pub base_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfCIConfig {
    pub job: JobConfig,
    pub mq: Option<MQConfig>,
}

pub fn read_config(base_workdir: &Path) -> Result<SelfCIConfig, ConfigError> {
    let mut config_path = base_workdir.to_path_buf();
    for segment in CONFIG_DIR_PATH {
        config_path.push(segment);
    }
    config_path.push(CONFIG_FILENAME);

    if !config_path.exists() {
        return Err(ConfigError::NotInitialized);
    }

    let config_content = std::fs::read_to_string(&config_path).map_err(ConfigError::ReadFailed)?;

    let config: SelfCIConfig =
        serde_yaml::from_str(&config_content).map_err(ConfigError::ParseFailed)?;

    Ok(config)
}

pub fn init_config(root_dir: &Path) -> Result<(), ConfigError> {
    let mut config_dir = root_dir.to_path_buf();
    for segment in CONFIG_DIR_PATH {
        config_dir.push(segment);
    }
    let config_path = config_dir.join(CONFIG_FILENAME);

    // Create directory if it doesn't exist
    std::fs::create_dir_all(&config_dir).map_err(ConfigError::ReadFailed)?;

    // Only write template if config file doesn't exist
    if !config_path.exists() {
        std::fs::write(&config_path, CONFIG_TEMPLATE).map_err(ConfigError::ReadFailed)?;
    }

    Ok(())
}
