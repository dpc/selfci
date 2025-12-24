mod common;

use duct::cmd;
use selfci::constants;
use std::fs;
use std::path::{Path, PathBuf};

/// Helper to build config path
fn config_path(repo_path: &Path) -> PathBuf {
    let mut path = repo_path.to_path_buf();
    for segment in constants::CONFIG_DIR_PATH {
        path.push(segment);
    }
    path.push(constants::CONFIG_FILENAME);
    path
}

/// Helper to build config path string for git commands
fn config_path_str() -> String {
    format!(".config/selfci/{}", constants::CONFIG_FILENAME)
}

/// Test basic candidate check execution with simple command
#[test]
fn test_basic_candidate_check_execution() {
    let repo = common::setup_git_repo();
    let repo_path = repo.path();

    // Reset to base commit
    cmd!("git", "reset", "--hard", "HEAD^")
        .dir(repo_path)
        .run()
        .expect("Failed to reset to base");

    // Create a simple config that just echoes some text
    let config_content = r#"
job:
  command: |
    echo "Running candidate check"
    echo "Base: $SELFCI_BASE_DIR"
    echo "Candidate: $SELFCI_CANDIDATE_DIR"
    echo "Job: $SELFCI_JOB_NAME"
"#;

    fs::write(config_path(repo_path), config_content).expect("Failed to write config");

    // Commit the new config as the base
    cmd!("git", "add", &config_path_str())
        .dir(repo_path)
        .run()
        .expect("Failed to git add config");

    cmd!("git", "commit", "--amend", "--no-edit")
        .dir(repo_path)
        .run()
        .expect("Failed to amend base commit");

    // Create candidate commit
    fs::write(repo_path.join("candidate.txt"), "candidate content")
        .expect("Failed to write candidate file");

    cmd!("git", "add", "candidate.txt")
        .dir(repo_path)
        .run()
        .expect("Failed to git add candidate");

    cmd!("git", "commit", "-m", "Candidate commit")
        .dir(repo_path)
        .run()
        .expect("Failed to commit candidate");

    // Run selfci check
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");
    let output = cmd!(
        selfci_bin,
        "check",
        "--root",
        repo_path,
        "--base",
        "HEAD^",
        "--candidate",
        "HEAD",
        "--print-output"
    )
    .env("SELFCI_VCS_FORCE", "git")
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    println!("Output:\n{}", output);

    // Verify the command ran and output appeared
    assert!(
        output.contains("Running candidate check"),
        "Should show our echo message"
    );
    assert!(
        output.contains("Job: main"),
        "Should show job name is 'main'"
    );
    assert!(
        output.contains("Job passed: main") || output.contains("Job started: main"),
        "Should show job status"
    );
}

/// Integration test for a Git repository with steps and jobs
#[test]
fn test_git_repo_with_steps_and_jobs() {
    let repo = common::setup_git_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    // Create a more complex config that uses steps and jobs
    let config_content = format!(
        r#"# Test config
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
"#,
        selfci_bin, selfci_bin, selfci_bin, selfci_bin
    );

    fs::write(config_path(repo_path), &config_content).expect("Failed to write config");

    // Commit the updated config
    cmd!("git", "add", &config_path_str())
        .dir(repo_path)
        .run()
        .expect("Failed to git add config");

    cmd!("git", "commit", "--amend", "--no-edit")
        .dir(repo_path)
        .run()
        .expect("Failed to amend commit");

    // Run selfci check
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");
    let output = cmd!(
        selfci_bin,
        "check",
        "--root",
        repo_path,
        "--base",
        "HEAD",
        "--candidate",
        "HEAD",
        "--print-output"
    )
    .env("SELFCI_VCS_FORCE", "git")
    .env("RUST_LOG", "debug")
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    println!("Output:\n{}", output);

    // Verify output contains expected messages
    assert!(
        output.contains("Job started: main"),
        "Should show job started"
    );
    assert!(
        output.contains("Job passed: main") || output.contains("Job failed: main"),
        "Should show job completion"
    );
}

/// Test with failing steps (non-ignored)
#[test]
fn test_failing_step() {
    let repo = common::setup_git_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    let config_content = format!(
        r#"
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
"#,
        selfci_bin, selfci_bin, selfci_bin
    );

    fs::write(config_path(repo_path), config_content).expect("Failed to write config");

    cmd!("git", "add", &config_path_str())
        .dir(repo_path)
        .run()
        .expect("Failed to git add config");

    cmd!("git", "commit", "--amend", "--no-edit")
        .dir(repo_path)
        .run()
        .expect("Failed to amend commit");

    let selfci_bin = env!("CARGO_BIN_EXE_selfci");
    let output = cmd!(
        selfci_bin,
        "check",
        "--root",
        repo_path,
        "--base",
        "HEAD",
        "--candidate",
        "HEAD",
        "--print-output"
    )
    .env("SELFCI_VCS_FORCE", "git")
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    println!("Output:\n{}", output);

    // Should fail due to step failure
    assert!(
        output.contains("Job failed: main"),
        "Job should fail due to step failure"
    );
    assert!(
        output.contains("step failure"),
        "Should mention step failure"
    );
    assert!(output.contains("❌"), "Should show failed step with ❌");
}

/// Test with failing steps (ignored)
#[test]
fn test_ignored_failing_step() {
    let repo = common::setup_git_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    let config_content = format!(
        r#"
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
"#,
        selfci_bin, selfci_bin, selfci_bin, selfci_bin
    );

    fs::write(config_path(repo_path), config_content).expect("Failed to write config");

    cmd!("git", "add", &config_path_str())
        .dir(repo_path)
        .run()
        .expect("Failed to git add config");

    cmd!("git", "commit", "--amend", "--no-edit")
        .dir(repo_path)
        .run()
        .expect("Failed to amend commit");

    let selfci_bin = env!("CARGO_BIN_EXE_selfci");
    let output = cmd!(
        selfci_bin,
        "check",
        "--root",
        repo_path,
        "--base",
        "HEAD",
        "--candidate",
        "HEAD",
        "--print-output"
    )
    .env("SELFCI_VCS_FORCE", "git")
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    println!("Output:\n{}", output);

    // Should pass because failure was ignored
    assert!(
        output.contains("Job passed: main"),
        "Job should pass with ignored failure"
    );
    assert!(
        output.contains("⚠️"),
        "Should show ignored failed step with ⚠️"
    );
    assert!(
        output.contains("✅"),
        "Should show successful steps with ✅"
    );
}

/// Test Jujutsu repository
#[test]
fn test_jj_repo_basic() {
    let repo = common::setup_jj_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    let config_content = format!(
        r#"
job:
  command: |
    {} step start "build"
    echo "Building with Jujutsu..."

    {} step start "test"
    echo "Testing..."

    echo "Complete!"
"#,
        selfci_bin, selfci_bin
    );

    fs::write(config_path(repo_path), config_content).expect("Failed to write config");

    cmd!("jj", "file", "track", &config_path_str())
        .dir(repo_path)
        .run()
        .expect("Failed to track config");

    cmd!("jj", "describe", "-m", "Updated config")
        .dir(repo_path)
        .run()
        .expect("Failed to describe");

    let selfci_bin = env!("CARGO_BIN_EXE_selfci");
    let output = cmd!(
        selfci_bin,
        "check",
        "--root",
        repo_path,
        "--base",
        "@",
        "--candidate",
        "@"
    )
    .stderr_to_stdout()
    .unchecked()
    .read()
    .expect("Failed to run selfci check");

    println!("Output:\n{}", output);

    assert!(
        output.contains("Job started: main"),
        "Should show job started"
    );
    assert!(
        output.contains("Job passed: main") || output.contains("Job failed: main"),
        "Should show job completion"
    );
}

/// Test merge queue functionality end-to-end
#[test]
fn test_merge_queue_flow() {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    let repo = common::setup_git_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    // Create a config that fails if README contains "FORBIDDEN"
    let config_content = r#"
job:
  command: |
    if grep -q "FORBIDDEN" README.md 2>/dev/null; then
      echo "README contains forbidden string!"
      exit 1
    fi
    echo "Check passed!"
"#;

    fs::write(config_path(repo_path), config_content).expect("Failed to write config");

    // Create initial README (passing)
    fs::write(
        repo_path.join("README.md"),
        "# Test Project\nThis is fine.\n",
    )
    .expect("Failed to write README");

    // Commit config and README to master
    cmd!("git", "add", ".")
        .dir(repo_path)
        .run()
        .expect("Failed to git add");

    cmd!("git", "commit", "-m", "Initial commit with config")
        .dir(repo_path)
        .run()
        .expect("Failed to commit");

    // Create a commit that fails the check
    fs::write(
        repo_path.join("README.md"),
        "# Test Project\nFORBIDDEN content here!\n",
    )
    .expect("Failed to write failing README");

    cmd!("git", "add", "README.md")
        .dir(repo_path)
        .run()
        .expect("Failed to git add");

    cmd!("git", "commit", "-m", "Add forbidden content")
        .dir(repo_path)
        .run()
        .expect("Failed to commit");

    let failing_commit = cmd!("git", "rev-parse", "HEAD")
        .dir(repo_path)
        .read()
        .expect("Failed to get commit hash")
        .trim()
        .to_string();

    // Create a commit that passes the check
    fs::write(
        repo_path.join("README.md"),
        "# Test Project\nSafe content.\n",
    )
    .expect("Failed to write passing README");

    cmd!("git", "add", "README.md")
        .dir(repo_path)
        .run()
        .expect("Failed to git add");

    cmd!("git", "commit", "-m", "Fix forbidden content")
        .dir(repo_path)
        .run()
        .expect("Failed to commit");

    let passing_commit = cmd!("git", "rev-parse", "HEAD")
        .dir(repo_path)
        .read()
        .expect("Failed to get commit hash")
        .trim()
        .to_string();

    // Reset master to the initial commit (before the two test commits)
    cmd!("git", "reset", "--hard", "HEAD~2")
        .dir(repo_path)
        .run()
        .expect("Failed to reset");

    // Start MQ daemon in background
    let mut mq_daemon = Command::new(selfci_bin)
        .args(["mq", "start", "--base-branch", "master"])
        .current_dir(repo_path)
        .env("SELFCI_VCS_FORCE", "git")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to start MQ daemon");

    // Give daemon time to start
    thread::sleep(Duration::from_millis(500));

    // Add failing commit to queue
    let add_output = cmd!(selfci_bin, "mq", "add", "--candidate", &failing_commit)
        .dir(repo_path)
        .env("SELFCI_VCS_FORCE", "git")
        .read()
        .expect("Failed to add candidate");

    println!("Add failing candidate output:\n{}", add_output);

    // Extract job ID from output (format: "Added to merge queue with job ID: 1")
    let failing_job_id: u64 = add_output
        .lines()
        .find(|line| line.contains("job ID"))
        .and_then(|line| line.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to parse job ID");

    println!("Failing job ID: {}", failing_job_id);

    // Wait for job to complete (poll status)
    let mut attempts = 0;
    let max_attempts = 50;
    let job_status;

    loop {
        thread::sleep(Duration::from_millis(200));
        attempts += 1;

        let status_output = cmd!(selfci_bin, "mq", "status", failing_job_id.to_string())
            .dir(repo_path)
            .env("SELFCI_VCS_FORCE", "git")
            .read()
            .expect("Failed to get status");

        if status_output.contains("Status: Failed") || status_output.contains("Status: Passed") {
            job_status = status_output;
            break;
        }

        if attempts >= max_attempts {
            panic!("Job did not complete within expected time");
        }
    }

    println!("Failing job status:\n{}", job_status);

    // Verify job failed
    assert!(
        job_status.contains("Status: Failed"),
        "Job should have failed"
    );
    assert!(
        job_status.contains("FORBIDDEN") || job_status.contains("forbidden"),
        "Output should mention the forbidden string"
    );

    // Verify master branch didn't change
    let master_hash = cmd!("git", "rev-parse", "master")
        .dir(repo_path)
        .read()
        .expect("Failed to get master hash")
        .trim()
        .to_string();

    assert_ne!(
        master_hash, failing_commit,
        "Master should not have merged failing commit"
    );

    // Now add passing commit to queue
    let add_output = cmd!(selfci_bin, "mq", "add", "--candidate", &passing_commit)
        .dir(repo_path)
        .env("SELFCI_VCS_FORCE", "git")
        .read()
        .expect("Failed to add passing candidate");

    println!("Add passing candidate output:\n{}", add_output);

    let passing_job_id: u64 = add_output
        .lines()
        .find(|line| line.contains("job ID"))
        .and_then(|line| line.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to parse job ID");

    println!("Passing job ID: {}", passing_job_id);

    // Wait for passing job to complete
    attempts = 0;
    let passing_job_status;

    loop {
        thread::sleep(Duration::from_millis(200));
        attempts += 1;

        let status_output = cmd!(selfci_bin, "mq", "status", passing_job_id.to_string())
            .dir(repo_path)
            .env("SELFCI_VCS_FORCE", "git")
            .read()
            .expect("Failed to get status");

        if status_output.contains("Status: Failed") || status_output.contains("Status: Passed") {
            passing_job_status = status_output;
            break;
        }

        if attempts >= max_attempts {
            panic!("Passing job did not complete within expected time");
        }
    }

    println!("Passing job status:\n{}", passing_job_status);

    // Verify job passed
    assert!(
        passing_job_status.contains("Status: Passed"),
        "Job should have passed"
    );
    assert!(
        passing_job_status.contains("Check passed!"),
        "Output should show success message"
    );

    // Verify master branch was updated to passing commit
    let new_master_hash = cmd!("git", "rev-parse", "master")
        .dir(repo_path)
        .read()
        .expect("Failed to get new master hash")
        .trim()
        .to_string();

    assert_eq!(
        new_master_hash, passing_commit,
        "Master should have merged passing commit"
    );

    // Clean up: kill daemon
    let _ = mq_daemon.kill();
    let _ = mq_daemon.wait();

    // Clean up socket file
    let socket_path = repo_path.join(".config").join("selfci").join(".mq.sock");
    let _ = std::fs::remove_file(socket_path);
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
    let strict_config = format!(
        r#"
job:
  command: |
    {} step start "security check"
    echo "Running strict security check..."
    {} step fail
    echo "This should not be reached"
"#,
        selfci_bin, selfci_bin
    );

    fs::write(config_path(repo_path), strict_config).expect("Failed to write strict config");

    cmd!("git", "add", &config_path_str())
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

    fs::write(config_path(repo_path), lax_config).expect("Failed to write lax config");

    cmd!("git", "add", &config_path_str())
        .dir(repo_path)
        .run()
        .expect("Failed to git add lax config");

    cmd!(
        "git",
        "commit",
        "-m",
        "Attempt to bypass CI with lax config"
    )
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
    assert!(
        output.contains("Job failed: main"),
        "Job should fail with strict base config. If it passes, config is being read from candidate (SECURITY ISSUE!)"
    );
    assert!(
        output.contains("security check"),
        "Should show the strict security check step from base config"
    );
    assert!(
        output.contains("❌"),
        "Should show failed step from strict base config"
    );

    // Also verify the lax config message is NOT present
    assert!(
        !output.contains("Lax config - always passes!"),
        "Should NOT see lax config output (would indicate config read from candidate - SECURITY ISSUE!)"
    );
}
