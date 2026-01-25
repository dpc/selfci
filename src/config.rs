use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::ConfigError;
use crate::constants::{CONFIG_DIR_PATH, CONFIG_FILENAME, LOCAL_CONFIG_FILENAME};

const CONFIG_TEMPLATE: &str = include_str!("../share/ci-template.yaml");
const LOCAL_CONFIG_TEMPLATE: &str = include_str!("../share/local-template.yaml");

fn default_command_prefix() -> Vec<String> {
    vec!["bash".to_string(), "-c".to_string()]
}

/// Shared command configuration used by job and mq hooks
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CommandConfig {
    pub command: Option<String>,
    #[serde(default = "default_command_prefix", rename = "command-prefix")]
    pub command_prefix: Vec<String>,
}

impl CommandConfig {
    /// Check if this command config is set (has a command)
    pub fn is_set(&self) -> bool {
        self.command.is_some()
    }

    /// Get the full command including prefix
    pub fn full_command(&self) -> Vec<String> {
        let mut full = self.command_prefix.clone();
        if let Some(ref cmd) = self.command {
            full.push(cmd.clone());
        }
        full
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CloneMode {
    Full,
    Partial,
    Shallow,
}

fn default_clone_mode() -> CloneMode {
    CloneMode::Partial
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobConfig {
    pub command: String,
    #[serde(default = "default_command_prefix", rename = "command-prefix")]
    pub command_prefix: Vec<String>,
    #[serde(default = "default_clone_mode", rename = "clone-mode")]
    pub clone_mode: CloneMode,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MergeStyle {
    #[default]
    Rebase,
    Merge,
}

fn default_merge_style() -> MergeStyle {
    MergeStyle::default()
}

/// MQ hooks configuration (pre_start, post_start, pre_clone, post_clone, pre_merge, post_merge)
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MQHooksConfig {
    #[serde(default, rename = "pre-start")]
    pub pre_start: Option<CommandConfig>,
    #[serde(default, rename = "post-start")]
    pub post_start: Option<CommandConfig>,
    #[serde(default, rename = "pre-clone")]
    pub pre_clone: Option<CommandConfig>,
    #[serde(default, rename = "post-clone")]
    pub post_clone: Option<CommandConfig>,
    #[serde(default, rename = "pre-merge")]
    pub pre_merge: Option<CommandConfig>,
    #[serde(default, rename = "post-merge")]
    pub post_merge: Option<CommandConfig>,
}

impl MQHooksConfig {
    /// Override with another config - other takes precedence for set values
    pub fn override_with(&mut self, other: &MQHooksConfig) {
        if other.pre_start.is_some() {
            self.pre_start = other.pre_start.clone();
        }
        if other.post_start.is_some() {
            self.post_start = other.post_start.clone();
        }
        if other.pre_clone.is_some() {
            self.pre_clone = other.pre_clone.clone();
        }
        if other.post_clone.is_some() {
            self.post_clone = other.post_clone.clone();
        }
        if other.pre_merge.is_some() {
            self.pre_merge = other.pre_merge.clone();
        }
        if other.post_merge.is_some() {
            self.post_merge = other.post_merge.clone();
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MQConfig {
    #[serde(rename = "base-branch")]
    pub base_branch: Option<String>,
    #[serde(rename = "merge-style", default = "default_merge_style")]
    pub merge_style: MergeStyle,
    #[serde(flatten)]
    pub hooks: MQHooksConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfCIConfig {
    pub job: JobConfig,
    pub mq: Option<MQConfig>,
}

/// Local configuration file structure (for user-local overrides)
/// This file is typically not checked into source control
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {
    pub mq: Option<MQHooksConfig>,
}

/// Combined MQ configuration after merging ci.yaml and local.yaml
#[derive(Debug, Clone, Default)]
pub struct MergedMQConfig {
    pub base_branch: Option<String>,
    pub merge_style: MergeStyle,
    pub hooks: MQHooksConfig,
}

impl MergedMQConfig {
    /// Create from main config, then merge with local config
    pub fn from_configs(main_mq: Option<&MQConfig>, local: Option<&LocalConfig>) -> Self {
        let mut result = if let Some(mq) = main_mq {
            MergedMQConfig {
                base_branch: mq.base_branch.clone(),
                merge_style: mq.merge_style.clone(),
                hooks: mq.hooks.clone(),
            }
        } else {
            MergedMQConfig::default()
        };

        // Merge local config hooks if present
        if let Some(local) = local
            && let Some(local_hooks) = &local.mq
        {
            result.hooks.override_with(local_hooks);
        }

        result
    }
}

fn get_config_dir(base_workdir: &Path) -> std::path::PathBuf {
    let mut config_path = base_workdir.to_path_buf();
    for segment in CONFIG_DIR_PATH {
        config_path.push(segment);
    }
    config_path
}

pub fn read_config(base_workdir: &Path) -> Result<SelfCIConfig, ConfigError> {
    let config_dir = get_config_dir(base_workdir);
    let config_path = config_dir.join(CONFIG_FILENAME);

    if !config_path.exists() {
        return Err(ConfigError::NotInitialized);
    }

    let config_content = std::fs::read_to_string(&config_path).map_err(ConfigError::ReadFailed)?;

    let config: SelfCIConfig = serde_yaml::from_str(&config_content)?;

    Ok(config)
}

/// Read local config file if it exists
pub fn read_local_config(base_workdir: &Path) -> Result<Option<LocalConfig>, ConfigError> {
    let config_dir = get_config_dir(base_workdir);
    let config_path = config_dir.join(LOCAL_CONFIG_FILENAME);

    if !config_path.exists() {
        return Ok(None);
    }

    let config_content = std::fs::read_to_string(&config_path).map_err(ConfigError::ReadFailed)?;

    let config: LocalConfig = serde_yaml::from_str(&config_content)?;

    Ok(Some(config))
}

/// Read both main and local configs, returning merged MQ config
pub fn read_merged_mq_config(base_workdir: &Path) -> Result<MergedMQConfig, ConfigError> {
    let main_config = read_config(base_workdir)?;
    let local_config = read_local_config(base_workdir)?;

    Ok(MergedMQConfig::from_configs(
        main_config.mq.as_ref(),
        local_config.as_ref(),
    ))
}

pub fn init_config(root_dir: &Path) -> Result<(), ConfigError> {
    let mut config_dir = root_dir.to_path_buf();
    for segment in CONFIG_DIR_PATH {
        config_dir.push(segment);
    }
    let config_path = config_dir.join(CONFIG_FILENAME);
    let local_config_path = config_dir.join(LOCAL_CONFIG_FILENAME);

    // Create directory if it doesn't exist
    std::fs::create_dir_all(&config_dir).map_err(ConfigError::ReadFailed)?;

    // Only write templates if config files don't exist
    if !config_path.exists() {
        std::fs::write(&config_path, CONFIG_TEMPLATE).map_err(ConfigError::ReadFailed)?;
    }
    if !local_config_path.exists() {
        std::fs::write(&local_config_path, LOCAL_CONFIG_TEMPLATE)
            .map_err(ConfigError::ReadFailed)?;
    }

    Ok(())
}
