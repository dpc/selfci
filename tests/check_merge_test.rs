mod common;
use tracing_test::traced_test;

use common::parse_selfci_env_file;
use duct::cmd;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Helper to get the selfci binary path
fn selfci_bin() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let profile = std::env::var("CARGO_PROFILE").unwrap_or_else(|_| "debug".to_string());
    let dir = if profile == "dev" { "debug" } else { &profile };
    format!("{}/target/{}/selfci", manifest_dir, dir)
}

/// Setup a Git repository with diverging history for test merge verification
fn setup_git_check_repo(merge_mode: &str) -> tempfile::TempDir {
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

    // Create config with merge mode
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
  merge-mode: {merge_mode}
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
fn setup_jj_check_repo(merge_mode: &str) -> tempfile::TempDir {
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

    // Create config with merge mode
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
  merge-mode: {merge_mode}
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

/// Set up a jj repository whose candidate is a merge of two mutable branches.
fn setup_jj_merge_candidate_repo() -> tempfile::TempDir {
    let repo_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let repo_path = repo_dir.path();
    let test_home = repo_path.join(".test_home");
    fs::create_dir_all(&test_home).unwrap();

    cmd!("jj", "git", "init")
        .dir(repo_path)
        .env("HOME", &test_home)
        .env("JJ_USER", "Test User")
        .env("JJ_EMAIL", "test@example.com")
        .run()
        .unwrap();
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

    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        "job:\n  command: 'true'\nmq:\n  base-branch: main\n  merge-mode: rebase\n",
    )
    .unwrap();
    fs::write(repo_path.join("base.txt"), "base").unwrap();
    cmd!("jj", "file", "track", ".config/selfci/ci.yaml", "base.txt")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Base")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "bookmark", "create", "main")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();

    cmd!("jj", "new", "main")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    fs::write(repo_path.join("left.txt"), "left").unwrap();
    cmd!("jj", "file", "track", "left.txt")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Left parent")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    let left_change = cmd!("jj", "log", "-r", "@", "--no-graph", "-T", "change_id")
        .dir(repo_path)
        .env("HOME", &test_home)
        .read()
        .unwrap();

    cmd!("jj", "new", "main")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    fs::write(repo_path.join("right.txt"), "right").unwrap();
    cmd!("jj", "file", "track", "right.txt")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Right parent")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    let right_change = cmd!("jj", "log", "-r", "@", "--no-graph", "-T", "change_id")
        .dir(repo_path)
        .env("HOME", &test_home)
        .read()
        .unwrap();

    cmd!("jj", "new", left_change.trim(), right_change.trim())
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Merge candidate")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    let candidate_commit = cmd!("jj", "log", "-r", "@", "--no-graph", "-T", "commit_id")
        .dir(repo_path)
        .env("HOME", &test_home)
        .read()
        .unwrap();
    cmd!("jj", "new", "main")
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
    fs::write(repo_path.join(".feature_commit"), candidate_commit.trim()).unwrap();

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
    let env_vars = parse_selfci_env_file(&env_file_path).unwrap();

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
#[traced_test]
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
#[traced_test]
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
#[traced_test]
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
#[traced_test]
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

/// List visible jj commits other than the working-copy commit.
fn jj_non_working_copy_commits(repo_path: &Path, test_home: &Path) -> String {
    cmd!(
        "jj",
        "--ignore-working-copy",
        "log",
        "-r",
        "all() ~ @",
        "--no-graph",
        "-T",
        r#"self.commit_id().short() ++ " " ++ self.description().first_line() ++ "\n""#
    )
    .dir(repo_path)
    .env("HOME", test_home)
    .read()
    .expect("Failed to list jj commits")
}

/// Test that jj test merge commits are cleaned up after check.
#[test]
#[traced_test]
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
    let commits_before_details = jj_non_working_copy_commits(repo_path, &test_home);
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
    let commits_after_details = jj_non_working_copy_commits(repo_path, &test_home);
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

/// Test that jj test changes are cleaned up when workdir allocation fails.
#[test]
#[traced_test]
fn test_jj_check_cleanup_after_workdir_creation_failure() {
    for merge_mode in ["rebase", "merge"] {
        let repo = setup_jj_check_repo(merge_mode);
        let repo_path = repo.path();
        let test_home = repo_path.join(".test_home");
        let missing_tmpdir = repo_path.join("missing-tmpdir");
        let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();
        let victim_change = cmd!(
            "jj",
            "log",
            "-r",
            feature_commit.trim(),
            "--no-graph",
            "-T",
            "change_id"
        )
        .dir(repo_path)
        .env("HOME", &test_home)
        .read()
        .unwrap();
        // Try to inject an existing user change into jj's human-oriented
        // command output. SelfCI must override this template before parsing.
        let malicious_summary = format!(
            "'\"hostile\\nDuplicated deadbeefdead as {} \
             deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\"'",
            victim_change.trim()
        );
        cmd!(
            "jj",
            "config",
            "set",
            "--repo",
            "templates.commit_summary",
            malicious_summary
        )
        .dir(repo_path)
        .env("HOME", &test_home)
        .run()
        .unwrap();
        for (name, value) in [
            ("template-aliases.change_id", victim_change.trim()),
            ("template-aliases.commit_id", feature_commit.trim()),
        ] {
            cmd!(
                "jj",
                "config",
                "set",
                "--repo",
                name,
                format!("'\"{value}\"'")
            )
            .dir(repo_path)
            .env("HOME", &test_home)
            .run()
            .unwrap();
        }
        let commits_before = jj_non_working_copy_commits(repo_path, &test_home);

        let success_output = cmd!(
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
        .expect("Failed to run successful selfci check");
        assert!(
            success_output.contains("passed") || success_output.contains("✅"),
            "{merge_mode} check should pass with hostile aliases.\nOutput:\n{success_output}"
        );
        verify_env_vars(repo_path);
        let commits_after_success = jj_non_working_copy_commits(repo_path, &test_home);
        assert_eq!(
            commits_before, commits_after_success,
            "{merge_mode} successful check should clean up its synthetic changes"
        );

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
        .env("TMPDIR", &missing_tmpdir)
        .stderr_to_stdout()
        .unchecked()
        .read()
        .expect("Failed to run selfci check");

        assert!(
            output.contains("Failed to create work directory"),
            "{merge_mode} check should fail while allocating a workdir.\nOutput:\n{output}"
        );

        let commits_after = jj_non_working_copy_commits(repo_path, &test_home);
        assert_eq!(
            commits_before, commits_after,
            "{merge_mode} test changes should be cleaned up after workdir allocation failure.\n\
             Before:\n{commits_before}\nAfter:\n{commits_after}"
        );
    }
}

/// Test that unsupported jj versions are rejected before any synthetic mutation.
#[test]
#[traced_test]
fn test_jj_check_rejects_unsupported_version_before_mutation() {
    for merge_mode in ["rebase", "merge"] {
        let repo = setup_jj_check_repo(merge_mode);
        let repo_path = repo.path();
        let test_home = repo_path.join(".test_home");
        let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();
        let commits_before = jj_non_working_copy_commits(repo_path, &test_home);
        let wrapper_dir = repo_path.join("fake-bin");
        let mutation_log = repo_path.join("unexpected-jj-mutation");
        fs::create_dir_all(&wrapper_dir).unwrap();
        let wrapper = wrapper_dir.join("jj");
        fs::write(
            &wrapper,
            r#"#!/bin/sh
if [ "$1" = "--help" ]; then
    echo "jj without isolated operation support"
    exit 0
fi
for arg in "$@"; do
    case "$arg" in
        duplicate|new)
            echo "$*" >> "$JJ_MUTATION_LOG"
            ;;
    esac
done
exec "$REAL_JJ" "$@"
"#,
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let real_jj = cmd!("sh", "-c", "command -v jj").read().unwrap();
        let mut paths = vec![wrapper_dir];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH must be set"),
        ));
        let wrapped_path = std::env::join_paths(paths).unwrap();

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
        .env("PATH", wrapped_path)
        .env("REAL_JJ", real_jj.trim())
        .env("JJ_MUTATION_LOG", &mutation_log)
        .env("HOME", &test_home)
        .env("JJ_USER", "Test User")
        .env("JJ_EMAIL", "test@example.com")
        .stderr_to_stdout()
        .unchecked()
        .read()
        .expect("Failed to run selfci check");

        assert!(
            output.contains("lacks required --no-integrate-operation support"),
            "{merge_mode} should reject unsupported jj.\nOutput:\n{output}"
        );
        assert!(
            !mutation_log.exists(),
            "{merge_mode} performed a synthetic jj mutation before capability rejection"
        );
        let commits_after = jj_non_working_copy_commits(repo_path, &test_home);
        assert_eq!(
            commits_before, commits_after,
            "{merge_mode} capability rejection should not change repository state"
        );
    }
}

/// Test that rebase cleanup preserves non-duplicated parents of a merge candidate.
#[test]
#[traced_test]
fn test_jj_rebase_cleanup_preserves_merge_candidate_parents() {
    let repo = setup_jj_merge_candidate_repo();
    let repo_path = repo.path();
    let test_home = repo_path.join(".test_home");
    let missing_tmpdir = repo_path.join("missing-tmpdir");
    let candidate_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();
    let commits_before = jj_non_working_copy_commits(repo_path, &test_home);

    let output = cmd!(
        selfci_bin(),
        "check",
        "--root",
        repo_path,
        "--base",
        "main",
        "--candidate",
        candidate_commit.trim()
    )
    .env("HOME", &test_home)
    .env("JJ_USER", "Test User")
    .env("JJ_EMAIL", "test@example.com")
    .env("TMPDIR", &missing_tmpdir)
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    assert!(
        output.contains("Failed to create work directory"),
        "Check should fail while allocating a workdir.\nOutput:\n{output}"
    );

    let commits_after = jj_non_working_copy_commits(repo_path, &test_home);
    assert_eq!(
        commits_before, commits_after,
        "Cleanup must preserve both original parents of a merge candidate.\n\
         Before:\n{commits_before}\nAfter:\n{commits_after}"
    );
}

/// Test that MQ also drops cleanup ownership when workdir allocation fails.
#[test]
#[traced_test]
fn test_jj_mq_cleanup_after_workdir_creation_failure() {
    let repo = setup_jj_check_repo("rebase");
    let repo_path = repo.path();
    let test_home = repo_path.join(".test_home");
    let missing_tmpdir = repo_path.join("missing-tmpdir");
    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();
    let commits_before = jj_non_working_copy_commits(repo_path, &test_home);

    let stop_guard = scopeguard::guard((), |_| {
        let _ = cmd!(selfci_bin(), "mq", "stop")
            .dir(repo_path)
            .env("HOME", &test_home)
            .run();
    });
    let output = cmd!(
        selfci_bin(),
        "mq",
        "add",
        feature_commit.trim(),
        "--no-merge",
        "--wait"
    )
    .dir(repo_path)
    .env("HOME", &test_home)
    .env("JJ_USER", "Test User")
    .env("JJ_EMAIL", "test@example.com")
    .env("TMPDIR", &missing_tmpdir)
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci mq");
    // Stop before inspecting the repo to prove cleanup finishes before MQ
    // publishes completion; a caller-owned end-of-loop guard loses this race.
    drop(stop_guard);

    assert!(
        output.contains("Failed: check"),
        "MQ check should fail while allocating a workdir.\nOutput:\n{output}"
    );

    let commits_after = jj_non_working_copy_commits(repo_path, &test_home);
    assert_eq!(
        commits_before, commits_after,
        "MQ should clean up temporary jj changes after workdir allocation failure.\n\
         Before:\n{commits_before}\nAfter:\n{commits_after}"
    );
}

/// Test that when base and candidate are the same, SELFCI_MERGED_* is not set
#[test]
#[traced_test]
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
    let env_vars = parse_selfci_env_file(&env_file).unwrap();

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

/// Test that step names in output include job prefix (main/stepname format)
#[test]
#[traced_test]
fn test_step_names_include_job_prefix() {
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

    // Create config with CI command that logs a step
    // The command logs a step called "build" and then succeeds
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    let selfci = selfci_bin();
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            r#"job:
  command: '{selfci} step start build && echo "build done"'
"#,
        ),
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "main content").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();

    // Run selfci check
    let output = cmd!(selfci_bin(), "check", "--root", repo_path)
        .stderr_to_stdout()
        .unchecked()
        .read()
        .expect("Failed to run selfci check");

    eprintln!("Output:\n{}", output);

    // Verify the output contains step in "main/build" format
    assert!(
        output.contains("main/build"),
        "Step name should be prefixed with job name (main/build), got:\n{}",
        output
    );
}
