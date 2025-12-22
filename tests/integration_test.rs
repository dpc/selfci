mod common;

use duct::cmd;
use std::fs;

/// Integration test for a Git repository with steps and jobs
#[test]
fn test_git_repo_with_steps_and_jobs() {
    let repo = common::setup_git_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    // Create a more complex config that uses steps and jobs
    let config_content = format!(r#"# Test config
job:
  command: |
    set -e

    # Start some steps
    {} step start "build"
    echo "Building..."
    sleep 0.1

    {} step start "test"
    echo "Testing..."
    sleep 0.1

    # Start a sub-job
    {} job start "lint" &

    {} step start "integration"
    echo "Integration tests..."
    sleep 0.1

    wait
    echo "All done!"
"#, selfci_bin, selfci_bin, selfci_bin, selfci_bin);

    fs::write(
        repo_path.join(".config").join("selfci").join("config.yml"),
        &config_content,
    )
    .expect("Failed to write config");

    // Commit the updated config
    cmd!("git", "add", ".config/selfci/config.yml")
        .dir(repo_path)
        .run()
        .expect("Failed to git add config");

    cmd!("git", "commit", "--amend", "--no-edit")
        .dir(repo_path)
        .run()
        .expect("Failed to amend commit");

    // Run selfci check
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");
    let output = cmd!(selfci_bin, "check", "--root", repo_path, "--base", "HEAD", "--candidate", "HEAD", "--print-output")
        .env("SELFCI_VCS_FORCE", "git")
        .env("RUST_LOG", "debug")
        .stderr_to_stdout()
        .unchecked()
        .read()
        .expect("Failed to run selfci check");

    println!("Output:\n{}", output);

    // Verify output contains expected messages
    assert!(output.contains("Job started: main"), "Should show job started");
    assert!(output.contains("Job passed: main") || output.contains("Job failed: main"),
            "Should show job completion");
}

/// Test with failing steps (non-ignored)
#[test]
fn test_failing_step() {
    let repo = common::setup_git_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    let config_content = format!(r#"
job:
  command: |
    set -e

    {} step start "passing step"
    echo "This step passes"

    {} step start "failing step"
    echo "This step will fail"
    {} step fail

    # This should cause the job to fail
    echo "Done"
"#, selfci_bin, selfci_bin, selfci_bin);

    fs::write(
        repo_path.join(".config").join("selfci").join("config.yml"),
        config_content,
    )
    .expect("Failed to write config");

    cmd!("git", "add", ".config/selfci/config.yml")
        .dir(repo_path)
        .run()
        .expect("Failed to git add config");

    cmd!("git", "commit", "--amend", "--no-edit")
        .dir(repo_path)
        .run()
        .expect("Failed to amend commit");

    let selfci_bin = env!("CARGO_BIN_EXE_selfci");
    let output = cmd!(selfci_bin, "check", "--root", repo_path, "--base", "HEAD", "--candidate", "HEAD", "--print-output")
        .env("SELFCI_VCS_FORCE", "git")
        .stderr_to_stdout()
        .unchecked()
        .read()
        .expect("Failed to run selfci check");

    println!("Output:\n{}", output);

    // Should fail due to step failure
    assert!(output.contains("Job failed: main"), "Job should fail due to step failure");
    assert!(output.contains("step failure"), "Should mention step failure");
    assert!(output.contains("❌"), "Should show failed step with ❌");
}

/// Test with failing steps (ignored)
#[test]
fn test_ignored_failing_step() {
    let repo = common::setup_git_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    let config_content = format!(r#"
job:
  command: |
    set -e

    {} step start "passing step"
    echo "This step passes"

    {} step start "optional failing step"
    echo "This step will fail but is ignored"
    {} step fail --ignore

    {} step start "final step"
    echo "This runs because previous failure was ignored"

    echo "Done"
"#, selfci_bin, selfci_bin, selfci_bin, selfci_bin);

    fs::write(
        repo_path.join(".config").join("selfci").join("config.yml"),
        config_content,
    )
    .expect("Failed to write config");

    cmd!("git", "add", ".config/selfci/config.yml")
        .dir(repo_path)
        .run()
        .expect("Failed to git add config");

    cmd!("git", "commit", "--amend", "--no-edit")
        .dir(repo_path)
        .run()
        .expect("Failed to amend commit");

    let selfci_bin = env!("CARGO_BIN_EXE_selfci");
    let output = cmd!(selfci_bin, "check", "--root", repo_path, "--base", "HEAD", "--candidate", "HEAD", "--print-output")
        .env("SELFCI_VCS_FORCE", "git")
        .stderr_to_stdout()
        .unchecked()
        .read()
        .expect("Failed to run selfci check");

    println!("Output:\n{}", output);

    // Should pass because failure was ignored
    assert!(output.contains("Job passed: main"), "Job should pass with ignored failure");
    assert!(output.contains("⚠️"), "Should show ignored failed step with ⚠️");
    assert!(output.contains("✅"), "Should show successful steps with ✅");
}

/// Test Jujutsu repository
#[test]
fn test_jj_repo_basic() {
    let repo = common::setup_jj_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    let config_content = format!(r#"
job:
  command: |
    {} step start "build"
    echo "Building with Jujutsu..."

    {} step start "test"
    echo "Testing..."

    echo "Complete!"
"#, selfci_bin, selfci_bin);

    fs::write(
        repo_path.join(".config").join("selfci").join("config.yml"),
        config_content,
    )
    .expect("Failed to write config");

    cmd!("jj", "file", "track", ".config/selfci/config.yml")
        .dir(repo_path)
        .run()
        .expect("Failed to track config");

    cmd!("jj", "describe", "-m", "Updated config")
        .dir(repo_path)
        .run()
        .expect("Failed to describe");

    let selfci_bin = env!("CARGO_BIN_EXE_selfci");
    let output = cmd!(selfci_bin, "check", "--root", repo_path, "--base", "@", "--candidate", "@")
        .stderr_to_stdout()
        .unchecked()
        .read()
        .expect("Failed to run selfci check");

    println!("Output:\n{}", output);

    assert!(output.contains("Job started: main"), "Should show job started");
    assert!(output.contains("Job passed: main") || output.contains("Job failed: main"),
            "Should show job completion");
}

/// SECURITY TEST: Verify config is read from base workdir, not candidate
///
/// This is a critical security feature. If the config were read from the candidate
/// workdir, an attacker could bypass CI checks by modifying the config in their
/// commit to do nothing or always pass. By reading from the base workdir, we ensure
/// that:
/// 1. Config changes themselves must pass CI before being used
/// 2. You cannot bypass checks by modifying the config in your commit
/// 3. The CI behavior is determined by the already-reviewed base, not the untrusted candidate
///
/// This test verifies this by:
/// 1. Creating a base commit with a strict config that fails
/// 2. Creating a candidate commit that changes the config to always pass
/// 3. Running check with base=HEAD^ and candidate=HEAD
/// 4. Verifying the strict base config is used (job fails), not the lax candidate config
#[test]
fn test_config_read_from_base_security() {
    let repo = common::setup_git_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    // Update the Candidate commit to have a STRICT config that will fail
    let strict_config = format!(r#"
job:
  command: |
    {} step start "security check"
    echo "Running strict security check..."
    {} step fail
    echo "This should not be reached"
"#, selfci_bin, selfci_bin);

    fs::write(
        repo_path.join(".config").join("selfci").join("config.yml"),
        strict_config,
    )
    .expect("Failed to write strict config");

    cmd!("git", "add", ".config/selfci/config.yml")
        .dir(repo_path)
        .run()
        .expect("Failed to git add strict config");

    cmd!("git", "commit", "--amend", "--no-edit")
        .dir(repo_path)
        .run()
        .expect("Failed to amend with strict config");

    // Now create a new commit that changes the config to be LAX (always passes)
    let lax_config = r#"
job:
  command: |
    echo "Lax config - always passes!"
    exit 0
"#;

    fs::write(
        repo_path.join(".config").join("selfci").join("config.yml"),
        lax_config,
    )
    .expect("Failed to write lax config");

    cmd!("git", "add", ".config/selfci/config.yml")
        .dir(repo_path)
        .run()
        .expect("Failed to git add lax config");

    cmd!("git", "commit", "-m", "Attempt to bypass CI with lax config")
        .dir(repo_path)
        .run()
        .expect("Failed to commit lax config");

    // Now check HEAD^ (strict config) vs HEAD (lax config)
    // The base workdir (HEAD^) has the strict config
    // The candidate workdir (HEAD) has the lax config
    // If config is correctly read from base, it should FAIL (strict config)
    // If config is incorrectly read from candidate, it would PASS (lax config)
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");
    let output = cmd!(selfci_bin, "check", "--root", repo_path, "--print-output")
        .env("SELFCI_VCS_FORCE", "git")
        .stderr_to_stdout()
        .unchecked()
        .read()
        .expect("Failed to run selfci check");

    println!("Output:\n{}", output);

    // CRITICAL: The job should FAIL because the base config (strict) is used
    assert!(output.contains("Job failed: main"),
            "Job should fail with strict base config. If it passes, config is being read from candidate (SECURITY ISSUE!)");
    assert!(output.contains("security check"),
            "Should show the strict security check step from base config");
    assert!(output.contains("❌"),
            "Should show failed step from strict base config");

    // Also verify the lax config message is NOT present
    assert!(!output.contains("Lax config - always passes!"),
            "Should NOT see lax config output (would indicate config read from candidate - SECURITY ISSUE!)");
}
