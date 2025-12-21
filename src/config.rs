use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::ConfigError;

const CONFIG_TEMPLATE: &str = include_str!("../share/config-template.yml");

fn default_command_prefix() -> Vec<String> {
    vec!["bash".to_string(), "-c".to_string()]
}

#[derive(Debug, Deserialize)]
pub struct JobConfig {
    pub command: String,
    #[serde(default = "default_command_prefix", rename = "command-prefix")]
    pub command_prefix: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SelfCIConfig {
    pub jobs: HashMap<String, JobConfig>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_init_config() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root_path = temp_dir.path();

        // Initialize config
        let result = init_config(root_path);
        assert!(result.is_ok(), "init_config failed: {:?}", result);

        // Verify config file was created
        let config_path = root_path.join(".config").join("selfci").join("config.yml");
        assert!(config_path.exists(), "config.yml should exist");

        // Verify config file contains the template
        let content = std::fs::read_to_string(&config_path).expect("Failed to read config");
        assert!(content.contains("command:"), "Config should contain 'command:' field");
        assert!(content.contains("SelfCI Configuration"), "Config should contain header comment");

        // Verify we can parse the config
        let config = read_config(root_path);
        assert!(config.is_ok(), "Should be able to read initialized config");

        // Write custom content to the config
        let custom_content = "# Custom config\njobs:\n  test:\n    command: my custom command\n";
        std::fs::write(&config_path, custom_content).expect("Failed to write custom config");

        // Call init_config again
        let result = init_config(root_path);
        assert!(result.is_ok(), "Second init_config failed: {:?}", result);

        // Verify the custom content is preserved (not overwritten)
        let preserved_content = std::fs::read_to_string(&config_path).expect("Failed to read config after second init");
        assert_eq!(preserved_content, custom_content, "Config should not be overwritten");
        assert!(preserved_content.contains("my custom command"), "Custom command should be preserved");
    }
}
