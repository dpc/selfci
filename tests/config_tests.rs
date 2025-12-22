use selfci::{init_config, read_config};
use std::fs;
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
    let content = fs::read_to_string(&config_path).expect("Failed to read config");
    assert!(content.contains("command:"), "Config should contain 'command:' field");
    assert!(content.contains("SelfCI Configuration"), "Config should contain header comment");

    // Verify we can parse the config
    let config = read_config(root_path);
    assert!(config.is_ok(), "Should be able to read initialized config");
}

#[test]
fn test_init_config_preserves_existing() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let root_path = temp_dir.path();

    // Create config directory
    let config_dir = root_path.join(".config").join("selfci");
    fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    // Write custom content to the config
    let config_path = config_dir.join("config.yml");
    let custom_content = "# Custom config\njob:\n  command: my custom command\n";
    fs::write(&config_path, custom_content).expect("Failed to write custom config");

    // Call init_config again
    let result = init_config(root_path);
    assert!(result.is_ok(), "Second init_config failed: {:?}", result);

    // Verify the custom content is preserved (not overwritten)
    let preserved_content = fs::read_to_string(&config_path).expect("Failed to read config after second init");
    assert_eq!(preserved_content, custom_content, "Config should not be overwritten");
    assert!(preserved_content.contains("my custom command"), "Custom command should be preserved");
}

#[test]
fn test_read_config_missing() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let result = read_config(temp_dir.path());
    assert!(result.is_err(), "Should fail when config doesn't exist");
}
