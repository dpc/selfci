mod common;

use common::parse_selfci_env_file;
use duct::cmd;
use std::fs;
use std::path::Path;

/// Helper to get the selfci binary path
fn selfci_bin() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let profile = std::env::var("CARGO_PROFILE").unwrap_or_else(|_| "debug".to_string());
    let dir = if profile == "dev" { "debug" } else { &profile };
    format!("{}/target/{}/selfci", manifest_dir, dir)
}

/// Setup a Git repository with diverging history for test merge verification
fn setup_git_check_repo(merge_style: &str) -> tempfile::TempDir {
    let repo_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let repo_path = repo_dir.path();

    // Initialize git repo
    cmd!("git", "init").dir(repo_path).run().unwrap();
    cmd!("git", "config", "user.name", "Test User")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "config", "user.email", "test@example.com")
        .dir(repo_path)
        .run()
        .unwrap();

    // Create config with merge style
    // CI command dumps all env vars to a file for verification
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    let env_file = repo_path.join(".ci_env");
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            r#"job:
  command: 'env > {env_file}'
mq:
  base-branch: main
  merge-style: {merge_style}
"#,
            env_file = env_file.display(),
        ),
    )
    .unwrap();

    // Create initial commit on main (this will be our base)
    fs::write(repo_path.join("main.txt"), "main content").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "branch", "-M", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Create feature branch with commits
    cmd!("git", "checkout", "-b", "feature")
        .dir(repo_path)
        .run()
        .unwrap();

    fs::write(repo_path.join("feature1.txt"), "feature 1").unwrap();
    cmd!("git", "add", "feature1.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Feature commit 1")
        .dir(repo_path)
        .run()
        .unwrap();

    fs::write(repo_path.join("feature2.txt"), "feature 2").unwrap();
    cmd!("git", "add", "feature2.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Feature commit 2")
        .dir(repo_path)
        .run()
        .unwrap();

    // Get the feature branch HEAD (original candidate commit)
    let feature_commit = cmd!("git", "rev-parse", "HEAD")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    // Switch back to main and create diverging history
    cmd!("git", "checkout", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    fs::write(repo_path.join("main_update.txt"), "main update").unwrap();
    cmd!("git", "add", "main_update.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Main branch update")
        .dir(repo_path)
        .run()
        .unwrap();

    // Store feature commit for later use
    fs::write(repo_path.join(".feature_commit"), feature_commit).unwrap();

    repo_dir
}

/// Setup a Jujutsu repository with diverging history for test merge verification
fn setup_jj_check_repo(merge_style: &str) -> tempfile::TempDir {
    let repo_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let repo_path = repo_dir.path();

    // Create a unique HOME for this test to avoid parallel test interference
    // Use a subdirectory of repo_path to ensure uniqueness per test
    let test_home = repo_path.join(".test_home");
    fs::create_dir_all(&test_home).unwrap();

    // Initialize jj repo with isolated HOME
    cmd!("jj", "git", "init")
        .dir(repo_path)
        .env("HOME", &test_home)
        .env("JJ_USER", "Test User")
        .env("JJ_EMAIL", "test@example.com")
        .run()
        .unwrap();

    // Configure jj user in repo config (required in Nix environment)
    cmd!("jj", "config", "set", "--repo", "user.name", "Test User")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!(
        "jj",
        "config",
        "set",
        "--repo",
        "user.email",
        "test@example.com"
    )
    .dir(repo_path)
    .env("HOME", &test_home)
    .run()
    .unwrap();

    // Create config with merge style
    // CI command dumps all env vars to a file for verification
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    let output_dir = repo_path.join(".ci_output");
    fs::create_dir_all(&output_dir).unwrap();
    let env_file = output_dir.join(".ci_env");
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            r#"job:
  command: 'env > {env_file}'
mq:
  base-branch: main
  merge-style: {merge_style}
"#,
            env_file = env_file.display(),
        ),
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "main content").unwrap();
    cmd!("jj", "file", "track", "main.txt")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "file", "track", ".config/selfci/ci.yaml")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Initial commit")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();

    // Create main bookmark
    cmd!("jj", "bookmark", "create", "main")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();

    // Create feature commits (branching off from main)
    cmd!("jj", "new", "main")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    fs::write(repo_path.join("feature1.txt"), "feature 1").unwrap();
    cmd!("jj", "file", "track", "feature1.txt")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Feature commit 1")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();

    cmd!("jj", "new")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    fs::write(repo_path.join("feature2.txt"), "feature 2").unwrap();
    cmd!("jj", "file", "track", "feature2.txt")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Feature commit 2")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();

    // Get the feature commit ID (original candidate)
    let feature_commit = cmd!("jj", "log", "-r", "@", "--no-graph", "-T", "commit_id")
        .dir(repo_path)
        .env("HOME", &test_home)
        .read()
        .unwrap()
        .trim()
        .to_string();

    // Create diverging history on main
    cmd!("jj", "new", "main")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    fs::write(repo_path.join("main_update.txt"), "main update").unwrap();
    cmd!("jj", "file", "track", "main_update.txt")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Main branch update")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "bookmark", "set", "main")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();

    // Store feature commit for later use
    fs::write(repo_path.join(".feature_commit"), feature_commit).unwrap();

    // Create a new empty @ commit to absorb any file changes during tests
    // This prevents CI output files from modifying existing commits
    cmd!("jj", "new")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();

    repo_dir
}

/// Verify the env vars passed to CI:
/// - SELFCI_CANDIDATE_COMMIT_ID should match the original (user-submitted) candidate
/// - SELFCI_MERGED_COMMIT_ID should differ (the test merge/rebase result)
fn verify_env_vars(repo_path: &Path) {
    // Read original feature commit
    let original_commit = fs::read_to_string(repo_path.join(".feature_commit"))
        .expect("Failed to read .feature_commit")
        .trim()
        .to_string();

    // Find the env file (git: repo_path/.ci_env, jj: repo_path/.ci_output/.ci_env)
    let output_dir = repo_path.join(".ci_output");
    let env_file_path = if repo_path.join(".ci_env").exists() {
        repo_path.join(".ci_env")
    } else {
        output_dir.join(".ci_env")
    };

    // Parse env vars from CI output
    let env_vars = parse_selfci_env_file(&env_file_path);

    eprintln!("\n=== CI env vars ===\n{:#?}", env_vars);

    let candidate_commit = env_vars
        .get("SELFCI_CANDIDATE_COMMIT_ID")
        .expect("SELFCI_CANDIDATE_COMMIT_ID not found - CI command may not have run");

    let merged_commit = env_vars
        .get("SELFCI_MERGED_COMMIT_ID")
        .expect("SELFCI_MERGED_COMMIT_ID not found - CI command may not have run");

    assert!(
        !candidate_commit.is_empty(),
        "SELFCI_CANDIDATE_COMMIT_ID is empty"
    );

    assert!(
        !merged_commit.is_empty(),
        "SELFCI_MERGED_COMMIT_ID is empty"
    );

    // SELFCI_CANDIDATE_COMMIT_ID should be the SAME as original (what user submitted)
    assert_eq!(
        &original_commit, candidate_commit,
        "SELFCI_CANDIDATE_COMMIT_ID should match the original candidate commit!\n\
         Original: {}\n\
         SELFCI_CANDIDATE_COMMIT_ID: {}\n\
         This env var should refer to what the user submitted.",
        original_commit, candidate_commit
    );

    // SELFCI_MERGED_COMMIT_ID should be DIFFERENT from original (test merge/rebase result)
    assert_ne!(
        &original_commit, merged_commit,
        "SELFCI_MERGED_COMMIT_ID should differ from original candidate commit!\n\
         Original: {}\n\
         SELFCI_MERGED_COMMIT_ID: {}\n\
         This means the test merge/rebase didn't happen before running CI.",
        original_commit, merged_commit
    );

    eprintln!("\n=== Commit ID verification ===");
    eprintln!("Original candidate commit: {}", original_commit);
    eprintln!(
        "SELFCI_CANDIDATE_COMMIT_ID: {} (should match original)",
        candidate_commit
    );
    eprintln!(
        "SELFCI_MERGED_COMMIT_ID: {} (should differ - test merge result)",
        merged_commit
    );
}

#[test]
fn test_git_check_rebase() {
    let repo = setup_git_check_repo("rebase");
    let repo_path = repo.path();

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    // Run selfci check
    let output = cmd!(
        selfci_bin(),
        "check",
        "--root",
        repo_path,
        "--base",
        "main",
        "--candidate",
        feature_commit.trim()
    )
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    eprintln!("Output:\n{}", output);

    assert!(
        output.contains("passed") || output.contains("✅"),
        "Check should pass"
    );

    // Verify env vars
    verify_env_vars(repo_path);
}

#[test]
fn test_git_check_merge() {
    let repo = setup_git_check_repo("merge");
    let repo_path = repo.path();

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    // Run selfci check
    let output = cmd!(
        selfci_bin(),
        "check",
        "--root",
        repo_path,
        "--base",
        "main",
        "--candidate",
        feature_commit.trim()
    )
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    eprintln!("Output:\n{}", output);

    assert!(
        output.contains("passed") || output.contains("✅"),
        "Check should pass"
    );

    // Verify env vars
    verify_env_vars(repo_path);
}

#[test]
fn test_jj_check_rebase() {
    let repo = setup_jj_check_repo("rebase");
    let repo_path = repo.path();
    let test_home = repo_path.join(".test_home");

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    // Run selfci check with isolated HOME to avoid parallel test interference
    let output = cmd!(
        selfci_bin(),
        "check",
        "--root",
        repo_path,
        "--base",
        "main",
        "--candidate",
        feature_commit.trim()
    )
    .env("HOME", &test_home)
    .env("JJ_USER", "Test User")
    .env("JJ_EMAIL", "test@example.com")
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    eprintln!("Output:\n{}", output);

    assert!(
        output.contains("passed") || output.contains("✅"),
        "Check should pass"
    );

    // Verify env vars
    verify_env_vars(repo_path);
}

#[test]
fn test_jj_check_merge() {
    let repo = setup_jj_check_repo("merge");
    let repo_path = repo.path();
    let test_home = repo_path.join(".test_home");

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    // Run selfci check with isolated HOME to avoid parallel test interference
    let output = cmd!(
        selfci_bin(),
        "check",
        "--root",
        repo_path,
        "--base",
        "main",
        "--candidate",
        feature_commit.trim()
    )
    .env("HOME", &test_home)
    .env("JJ_USER", "Test User")
    .env("JJ_EMAIL", "test@example.com")
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    eprintln!("Output:\n{}", output);

    assert!(
        output.contains("passed") || output.contains("✅"),
        "Check should pass"
    );

    // Verify env vars
    verify_env_vars(repo_path);
}

/// Test that jj test merge commits are cleaned up after check
#[test]
fn test_jj_check_cleanup() {
    let repo = setup_jj_check_repo("rebase");
    let repo_path = repo.path();
    let test_home = repo_path.join(".test_home");

    // Print jj version for debugging
    let jj_version = cmd!("jj", "--version").read().unwrap();
    eprintln!("jj version: {}", jj_version);

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    // Get commits before check (with descriptions for debugging)
    // Use "all() ~ @" to exclude the working copy commit (which absorbs test file changes)
    let commits_before_details = cmd!(
        "jj",
        "--ignore-working-copy",
        "log",
        "-r",
        "all() ~ @",
        "--no-graph",
        "-T",
        r#"commit_id.short() ++ " " ++ description.first_line() ++ "\n""#
    )
    .dir(repo_path)
    .env("HOME", &test_home)
    .read()
    .unwrap();
    eprintln!("Commits BEFORE check:\n{}", commits_before_details);

    let commits_before = commits_before_details.lines().count();

    // Run selfci check with isolated HOME to avoid parallel test interference
    let output = cmd!(
        selfci_bin(),
        "check",
        "--root",
        repo_path,
        "--base",
        "main",
        "--candidate",
        feature_commit.trim()
    )
    .env("HOME", &test_home)
    .env("JJ_USER", "Test User")
    .env("JJ_EMAIL", "test@example.com")
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    eprintln!("Output:\n{}", output);

    assert!(
        output.contains("passed") || output.contains("✅"),
        "Check should pass"
    );

    // Get commits after check (with descriptions for debugging)
    // Use "all() ~ @" to exclude the working copy commit (which absorbs test file changes)
    let commits_after_details = cmd!(
        "jj",
        "--ignore-working-copy",
        "log",
        "-r",
        "all() ~ @",
        "--no-graph",
        "-T",
        r#"commit_id.short() ++ " " ++ description.first_line() ++ "\n""#
    )
    .dir(repo_path)
    .env("HOME", &test_home)
    .read()
    .unwrap();
    eprintln!("Commits AFTER check:\n{}", commits_after_details);

    let commits_after = commits_after_details.lines().count();

    // Should have same number of commits (test merge was cleaned up)
    assert_eq!(
        commits_before, commits_after,
        "Test merge commits should be cleaned up.\n\
         Commits before: {}\n\
         Commits after: {}\n\
         Before:\n{}\n\
         After:\n{}",
        commits_before, commits_after, commits_before_details, commits_after_details
    );
}

/// Test that when base and candidate are the same, SELFCI_MERGED_* is not set
#[test]
fn test_git_check_same_base_candidate() {
    let repo_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let repo_path = repo_dir.path();

    // Initialize git repo
    cmd!("git", "init").dir(repo_path).run().unwrap();
    cmd!("git", "config", "user.name", "Test User")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "config", "user.email", "test@example.com")
        .dir(repo_path)
        .run()
        .unwrap();

    // Create config that dumps env vars
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    let env_file = repo_path.join(".ci_env");
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            r#"job:
  command: 'env > {env_file}'
"#,
            env_file = env_file.display(),
        ),
    )
    .unwrap();

    // Create a single commit
    fs::write(repo_path.join("main.txt"), "main content").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();

    let commit = cmd!("git", "rev-parse", "HEAD")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    // Run selfci check with same base and candidate
    let output = cmd!(
        selfci_bin(),
        "check",
        "--root",
        repo_path,
        "--base",
        &commit,
        "--candidate",
        &commit
    )
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    eprintln!("Output:\n{}", output);

    assert!(
        output.contains("passed") || output.contains("✅"),
        "Check should pass"
    );

    // Parse env vars from CI output
    let env_vars = parse_selfci_env_file(&env_file);

    eprintln!("\n=== CI env vars ===\n{:#?}", env_vars);

    // SELFCI_CANDIDATE_COMMIT_ID should be set
    let candidate_commit = env_vars
        .get("SELFCI_CANDIDATE_COMMIT_ID")
        .expect("SELFCI_CANDIDATE_COMMIT_ID not found");

    assert_eq!(
        &commit, candidate_commit,
        "SELFCI_CANDIDATE_COMMIT_ID should match the commit"
    );

    // SELFCI_MERGED_COMMIT_ID should be empty (no merge when base == candidate)
    let merged_commit = env_vars
        .get("SELFCI_MERGED_COMMIT_ID")
        .map(|s| s.as_str())
        .unwrap_or("");

    assert!(
        merged_commit.is_empty(),
        "SELFCI_MERGED_COMMIT_ID should be empty when base == candidate.\n\
         Got: {}",
        merged_commit
    );
}
