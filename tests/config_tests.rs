use selfci::{constants, init_config, read_config};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_init_config() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let root_path = temp_dir.path();

    // Initialize config
    let result = init_config(root_path);
    assert!(result.is_ok(), "init_config failed: {:?}", result);

    // Build config directory path
    let mut config_dir = root_path.to_path_buf();
    for segment in constants::CONFIG_DIR_PATH {
        config_dir.push(segment);
    }

    // Verify ci.yaml was created
    let config_path = config_dir.join(constants::CONFIG_FILENAME);
    assert!(
        config_path.exists(),
        "{} should exist",
        constants::CONFIG_FILENAME
    );

    // Verify ci.yaml contains the template
    let content = fs::read_to_string(&config_path).expect("Failed to read config");
    assert!(
        content.contains("command:"),
        "Config should contain 'command:' field"
    );
    assert!(
        content.contains("SelfCI Configuration"),
        "Config should contain header comment"
    );

    // Verify local.yaml was created
    let local_config_path = config_dir.join(constants::LOCAL_CONFIG_FILENAME);
    assert!(
        local_config_path.exists(),
        "{} should exist",
        constants::LOCAL_CONFIG_FILENAME
    );

    // Verify local.yaml contains the template
    let local_content =
        fs::read_to_string(&local_config_path).expect("Failed to read local config");
    assert!(
        local_content.contains("SelfCI Local Configuration"),
        "Local config should contain header comment"
    );
    assert!(
        local_content.contains("pre-start:"),
        "Local config should contain pre-start hook example"
    );
    assert!(
        local_content.contains("post-start:"),
        "Local config should contain post-start hook example"
    );
    assert!(
        local_content.contains("pre-clone:"),
        "Local config should contain pre-clone hook example"
    );
    assert!(
        local_content.contains("post-clone:"),
        "Local config should contain post-clone hook example"
    );
    assert!(
        local_content.contains("pre-merge:"),
        "Local config should contain pre-merge hook example"
    );
    assert!(
        local_content.contains("post-merge:"),
        "Local config should contain post-merge hook example"
    );

    // Verify .gitignore was created
    let gitignore_path = config_dir.join(constants::GITIGNORE_FILENAME);
    assert!(gitignore_path.exists(), ".gitignore should exist");

    // Verify .gitignore ignores local.yaml
    let gitignore_content = fs::read_to_string(&gitignore_path).expect("Failed to read .gitignore");
    assert!(
        gitignore_content.contains("local.yaml"),
        ".gitignore should ignore local.yaml"
    );

    // Verify we can parse the config
    let config = read_config(root_path);
    assert!(config.is_ok(), "Should be able to read initialized config");
}

#[test]
fn test_init_config_preserves_existing() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let root_path = temp_dir.path();

    // Create config directory
    let mut config_dir = root_path.to_path_buf();
    for segment in constants::CONFIG_DIR_PATH {
        config_dir.push(segment);
    }
    fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    // Write custom content to ci.yaml
    let config_path = config_dir.join(constants::CONFIG_FILENAME);
    let custom_content = "# Custom config\njob:\n  command: my custom command\n";
    fs::write(&config_path, custom_content).expect("Failed to write custom config");

    // Write custom content to local.yaml
    let local_config_path = config_dir.join(constants::LOCAL_CONFIG_FILENAME);
    let custom_local_content = "# Custom local config\nmq:\n  pre-start:\n    command: my hook\n";
    fs::write(&local_config_path, custom_local_content)
        .expect("Failed to write custom local config");

    // Call init_config again
    let result = init_config(root_path);
    assert!(result.is_ok(), "Second init_config failed: {:?}", result);

    // Verify ci.yaml custom content is preserved (not overwritten)
    let preserved_content =
        fs::read_to_string(&config_path).expect("Failed to read config after second init");
    assert_eq!(
        preserved_content, custom_content,
        "ci.yaml should not be overwritten"
    );
    assert!(
        preserved_content.contains("my custom command"),
        "Custom command should be preserved"
    );

    // Verify local.yaml custom content is preserved (not overwritten)
    let preserved_local_content = fs::read_to_string(&local_config_path)
        .expect("Failed to read local config after second init");
    assert_eq!(
        preserved_local_content, custom_local_content,
        "local.yaml should not be overwritten"
    );
    assert!(
        preserved_local_content.contains("my hook"),
        "Custom hook should be preserved"
    );
}

#[test]
fn test_read_config_missing() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let result = read_config(temp_dir.path());
    assert!(result.is_err(), "Should fail when config doesn't exist");
}

#[test]
fn test_unknown_top_level_field() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let root_path = temp_dir.path();

    // Create config directory
    let mut config_dir = root_path.to_path_buf();
    for segment in constants::CONFIG_DIR_PATH {
        config_dir.push(segment);
    }
    fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    // Write config with unknown top-level field
    let config_path = config_dir.join(constants::CONFIG_FILENAME);
    let invalid_config = r#"
job:
  command: echo test
unknown_field: this should cause an error
"#;
    fs::write(&config_path, invalid_config).expect("Failed to write config");

    // Try to read config - should fail
    let result = read_config(root_path);
    assert!(
        result.is_err(),
        "Should fail when config has unknown top-level field"
    );

    // Verify error mentions unknown field
    let err = result.unwrap_err();
    let err_msg = format!("{}", err);
    // Print for debugging
    eprintln!("Error message: {}", err_msg);

    // The error should be a ParseFailed error
    match err {
        selfci::ConfigError::ParseFailed(_) => {
            // Success - it failed to parse
        }
        _ => panic!("Expected ParseFailed error, got: {:?}", err),
    }
}

#[test]
fn test_unknown_job_field() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let root_path = temp_dir.path();

    // Create config directory
    let mut config_dir = root_path.to_path_buf();
    for segment in constants::CONFIG_DIR_PATH {
        config_dir.push(segment);
    }
    fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    // Write config with unknown field in job section
    let config_path = config_dir.join(constants::CONFIG_FILENAME);
    let invalid_config = r#"
job:
  command: echo test
  typo_field: this is a typo
"#;
    fs::write(&config_path, invalid_config).expect("Failed to write config");

    // Try to read config - should fail
    let result = read_config(root_path);
    assert!(
        result.is_err(),
        "Should fail when config has unknown field in job section"
    );

    // Verify error mentions unknown field
    let err = result.unwrap_err();
    let err_msg = format!("{}", err);
    // Print for debugging
    eprintln!("Error message: {}", err_msg);

    // The error should be a ParseFailed error
    match err {
        selfci::ConfigError::ParseFailed(_) => {
            // Success - it failed to parse
        }
        _ => panic!("Expected ParseFailed error, got: {:?}", err),
    }
}

#[test]
fn test_valid_config_with_all_fields() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let root_path = temp_dir.path();

    // Create config directory
    let mut config_dir = root_path.to_path_buf();
    for segment in constants::CONFIG_DIR_PATH {
        config_dir.push(segment);
    }
    fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    // Write config with all valid fields
    let config_path = config_dir.join(constants::CONFIG_FILENAME);
    let valid_config = r#"
job:
  command: echo test
  command-prefix: ["sh", "-c"]
"#;
    fs::write(&config_path, valid_config).expect("Failed to write config");

    // Should successfully read config
    let result = read_config(root_path);
    assert!(result.is_ok(), "Should succeed with all valid fields");

    let config = result.unwrap();
    assert_eq!(config.job.command, "echo test");
    assert_eq!(config.job.command_prefix, vec!["sh", "-c"]);
}
