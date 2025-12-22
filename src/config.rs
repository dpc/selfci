use serde::Deserialize;
use std::path::Path;

use crate::ConfigError;

const CONFIG_TEMPLATE: &str = include_str!("../share/config-template.yml");

fn default_command_prefix() -> Vec<String> {
    vec!["bash".to_string(), "-c".to_string()]
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobConfig {
    pub command: String,
    #[serde(default = "default_command_prefix", rename = "command-prefix")]
    pub command_prefix: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SelfCIConfig {
    pub job: JobConfig,
}

pub fn read_config(base_workdir: &Path) -> Result<SelfCIConfig, ConfigError> {
    let config_path = base_workdir.join(".config").join("selfci").join("config.yml");

    if !config_path.exists() {
        return Err(ConfigError::NotInitialized);
    }

    let config_content = std::fs::read_to_string(&config_path)
        .map_err(ConfigError::ReadFailed)?;

    let config: SelfCIConfig = serde_yaml::from_str(&config_content)
        .map_err(ConfigError::ParseFailed)?;

    Ok(config)
}

pub fn init_config(root_dir: &Path) -> Result<(), ConfigError> {
    let config_dir = root_dir.join(".config").join("selfci");
    let config_path = config_dir.join("config.yml");

    // Create directory if it doesn't exist
    std::fs::create_dir_all(&config_dir)
        .map_err(ConfigError::ReadFailed)?;

    // Only write template if config file doesn't exist
    if !config_path.exists() {
        std::fs::write(&config_path, CONFIG_TEMPLATE)
            .map_err(ConfigError::ReadFailed)?;
    }

    Ok(())
}

