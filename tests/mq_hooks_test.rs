mod common;

use common::parse_selfci_env_file;
use duct::cmd;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// Helper to get the selfci binary path
fn selfci_bin() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let profile = std::env::var("CARGO_PROFILE").unwrap_or_else(|_| "debug".to_string());
    let dir = if profile == "dev" { "debug" } else { &profile };
    format!("{}/target/{}/selfci", manifest_dir, dir)
}

/// Wait for daemon to be ready by sending a request
/// This ensures the daemon has completed initialization (including post-start hook)
/// since the accept loop only starts after post-start hook completes
fn wait_for_daemon_ready(repo_path: &Path, timeout_secs: u64) {
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < timeout_secs {
        // mq list will find daemon via mq.dir (written before fork) and connect to socket
        // This blocks until daemon's accept loop is running (after post-start hook)
        let output = cmd!(selfci_bin(), "mq", "list")
            .dir(repo_path)
            .unchecked()
            .stderr_to_stdout()
            .read();
        if output.is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "Daemon did not become ready within {} seconds",
        timeout_secs
    );
}

/// Helper to wait for job completion and return the status output
fn wait_for_job_completion_with_output(
    repo_path: &Path,
    job_id: u64,
    timeout_secs: u64,
) -> Option<String> {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed().as_secs() > timeout_secs {
            return None;
        }

        let output = cmd!(selfci_bin(), "mq", "status", job_id.to_string())
            .dir(repo_path)
            .read()
            .ok();

        if let Some(ref out) = output
            && (out.contains("Status: Passed") || out.contains("Status: Failed"))
        {
            return output;
        }

        thread::sleep(Duration::from_millis(100));
    }
}

/// Setup a Git repository with hooks configured in both ci.yaml and local.yaml
fn setup_git_repo_with_hooks() -> tempfile::TempDir {
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

    // Create config directory
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();

    // Create ci.yaml with pre-clone hook (from ci.yaml)
    // and post-merge hook (will be overridden by local.yaml)
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        r#"job:
  command: 'true'

mq:
  base-branch: main
  merge-mode: merge
  pre-clone:
    command: 'echo "pre-clone from ci.yaml" && touch pre_clone_executed.txt'
  post-merge:
    command: 'echo "post-merge from ci.yaml - should be overridden" && touch post_merge_ci.txt'
"#,
    )
    .unwrap();

    // Create local.yaml with pre-merge hook and override post-merge
    fs::write(
        repo_path.join(".config/selfci/local.yaml"),
        r#"mq:
  pre-merge:
    command: 'echo "pre-merge from local.yaml" && touch pre_merge_executed.txt'
  post-merge:
    command: 'echo "post-merge from local.yaml" && touch post_merge_local.txt'
"#,
    )
    .unwrap();

    // Create initial commit on main
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

    // Create feature branch with a commit
    cmd!("git", "checkout", "-b", "feature")
        .dir(repo_path)
        .run()
        .unwrap();

    fs::write(repo_path.join("feature.txt"), "feature content").unwrap();
    cmd!("git", "add", "feature.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Feature commit")
        .dir(repo_path)
        .run()
        .unwrap();

    // Get the feature branch HEAD
    let feature_commit = cmd!("git", "rev-parse", "HEAD")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    // Switch back to main
    cmd!("git", "checkout", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Store feature commit for later use
    fs::write(repo_path.join(".feature_commit"), feature_commit).unwrap();

    repo_dir
}

#[test]
fn test_mq_hooks_execution() {
    let repo = setup_git_repo_with_hooks();
    let repo_path = repo.path();

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    // Start MQ daemon
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .env("SELFCI_LOG", "debug")
        .run()
        .unwrap();
    wait_for_daemon_ready(repo_path, 10);

    // Add candidate
    let output = cmd!(selfci_bin(), "mq", "add", feature_commit.trim())
        .dir(repo_path)
        .read()
        .unwrap();

    // Extract job ID
    let job_id: u64 = output
        .lines()
        .find(|l| l.contains("run ID"))
        .and_then(|l| l.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to extract job ID");

    // Wait for completion
    let status_output =
        wait_for_job_completion_with_output(repo_path, job_id, 30).expect("Job did not complete");

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    // Verify job passed and was merged
    assert!(
        status_output.contains("Status: Passed: merged"),
        "Job should have passed with merged status, got: {}",
        status_output
    );

    // === Verify hooks were executed by checking marker files ===

    // pre-clone hook from ci.yaml should have run
    assert!(
        repo_path.join("pre_clone_executed.txt").exists(),
        "pre-clone hook should have created pre_clone_executed.txt"
    );

    // pre-merge hook from local.yaml should have run
    assert!(
        repo_path.join("pre_merge_executed.txt").exists(),
        "pre-merge hook should have created pre_merge_executed.txt"
    );

    // post-merge from local.yaml should have run (overriding ci.yaml)
    assert!(
        repo_path.join("post_merge_local.txt").exists(),
        "post-merge hook from local.yaml should have created post_merge_local.txt"
    );

    // post-merge from ci.yaml should NOT have run (was overridden)
    assert!(
        !repo_path.join("post_merge_ci.txt").exists(),
        "post-merge hook from ci.yaml should NOT have run (was overridden by local.yaml)"
    );

    // === Verify hook output is recorded in status output ===
    eprintln!("\n=== Status output ===\n{}", status_output);

    assert!(
        status_output.contains("pre-clone from ci.yaml"),
        "Status should contain pre-clone output"
    );
    assert!(
        status_output.contains("pre-merge from local.yaml"),
        "Status should contain pre-merge output"
    );
    assert!(
        status_output.contains("post-merge from local.yaml"),
        "Status should contain post-merge output"
    );

    // === Verify hook output sections are present ===
    assert!(
        status_output.contains("### Pre-Clone Hook"),
        "Status should contain Pre-Clone Hook section"
    );
    assert!(
        status_output.contains("### Pre-Merge Hook"),
        "Status should contain Pre-Merge Hook section"
    );
    assert!(
        status_output.contains("### Post-Merge Hook"),
        "Status should contain Post-Merge Hook section"
    );

    // === Verify hook output is NOT in the merge commit message ===
    // Reset to main to see the merge commit
    cmd!("git", "reset", "--hard", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Get the merge commit message
    let commit_message = cmd!("git", "--no-pager", "log", "-1", "--format=%B", "main")
        .dir(repo_path)
        .read()
        .unwrap();

    eprintln!("\n=== Merge commit message ===\n{}", commit_message);

    // Merge commit should have the standard message but NOT hook output
    assert!(
        commit_message.contains("Merge commit"),
        "Merge commit message should contain 'Merge commit'"
    );
    assert!(
        commit_message.contains("by SelfCI"),
        "Merge commit message should contain 'by SelfCI'"
    );

    // Hook outputs should NOT be in the commit message
    assert!(
        !commit_message.contains("pre-clone from ci.yaml"),
        "Merge commit should NOT contain pre-clone hook output"
    );
    assert!(
        !commit_message.contains("pre-merge from local.yaml"),
        "Merge commit should NOT contain pre-merge hook output"
    );
    assert!(
        !commit_message.contains("post-merge from local.yaml"),
        "Merge commit should NOT contain post-merge hook output"
    );
    assert!(
        !commit_message.contains("### Pre-Clone Hook"),
        "Merge commit should NOT contain hook section headers"
    );
}

#[test]
fn test_pre_clone_hook_failure() {
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

    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();

    // Create ci.yaml with a failing pre-clone hook
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        r#"job:
  command: 'true'

mq:
  base-branch: main
  pre-clone:
    command: 'echo "pre-clone failing" && exit 1'
"#,
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "main").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "branch", "-M", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Create feature branch
    cmd!("git", "checkout", "-b", "feature")
        .dir(repo_path)
        .run()
        .unwrap();
    fs::write(repo_path.join("feature.txt"), "feature").unwrap();
    cmd!("git", "add", "feature.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Feature")
        .dir(repo_path)
        .run()
        .unwrap();

    let feature_commit = cmd!("git", "rev-parse", "HEAD")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    cmd!("git", "checkout", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Start daemon
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .run()
        .unwrap();
    wait_for_daemon_ready(repo_path, 10);

    // Add candidate
    let output = cmd!(selfci_bin(), "mq", "add", &feature_commit)
        .dir(repo_path)
        .read()
        .unwrap();

    let job_id: u64 = output
        .lines()
        .find(|l| l.contains("run ID"))
        .and_then(|l| l.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to extract job ID");

    // Wait for completion
    let status_output =
        wait_for_job_completion_with_output(repo_path, job_id, 30).expect("Job did not complete");

    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    eprintln!(
        "\n=== Status output (pre-clone failure) ===\n{}",
        status_output
    );

    // Job should have failed due to pre-clone hook failure
    assert!(
        status_output.contains("Status: Failed: pre-clone"),
        "Job should have failed with pre-clone reason, got: {}",
        status_output
    );
    assert!(
        status_output.contains("pre-clone failing"),
        "Status should show pre-clone hook output"
    );
    assert!(
        status_output.contains("Pre-clone hook failed"),
        "Status should indicate pre-clone hook failed"
    );
}

#[test]
fn test_pre_merge_hook_failure() {
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

    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();

    // Create ci.yaml with a failing pre-merge hook
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        r#"job:
  command: 'true'

mq:
  base-branch: main
  merge-mode: merge
  pre-merge:
    command: 'echo "pre-merge failing" && exit 1'
"#,
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "main").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "branch", "-M", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Store main commit hash to verify no merge happened
    let main_before = cmd!("git", "rev-parse", "main")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    // Create feature branch
    cmd!("git", "checkout", "-b", "feature")
        .dir(repo_path)
        .run()
        .unwrap();
    fs::write(repo_path.join("feature.txt"), "feature").unwrap();
    cmd!("git", "add", "feature.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Feature")
        .dir(repo_path)
        .run()
        .unwrap();

    let feature_commit = cmd!("git", "rev-parse", "HEAD")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    cmd!("git", "checkout", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Start daemon
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .run()
        .unwrap();
    wait_for_daemon_ready(repo_path, 10);

    // Add candidate
    let output = cmd!(selfci_bin(), "mq", "add", &feature_commit)
        .dir(repo_path)
        .read()
        .unwrap();

    let job_id: u64 = output
        .lines()
        .find(|l| l.contains("run ID"))
        .and_then(|l| l.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to extract job ID");

    // Wait for completion
    let status_output =
        wait_for_job_completion_with_output(repo_path, job_id, 30).expect("Job did not complete");

    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    eprintln!(
        "\n=== Status output (pre-merge failure) ===\n{}",
        status_output
    );

    // Job should have failed due to pre-merge hook failure
    assert!(
        status_output.contains("Status: Failed: pre-merge"),
        "Job should have failed with pre-merge reason, got: {}",
        status_output
    );
    assert!(
        status_output.contains("pre-merge failing"),
        "Status should show pre-merge hook output"
    );
    assert!(
        status_output.contains("Pre-merge hook failed"),
        "Status should indicate pre-merge hook failed"
    );

    // Verify that merge did NOT happen
    let main_after = cmd!("git", "rev-parse", "main")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    assert_eq!(
        main_before, main_after,
        "Main branch should not have changed when pre-merge hook fails"
    );
}

#[test]
fn test_pre_start_hook_execution() {
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

    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();

    // Create ci.yaml with a pre-start hook
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        r#"job:
  command: 'true'

mq:
  base-branch: main
  pre-start:
    command: 'echo "pre-start hook running" && touch pre_start_executed.txt'
"#,
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "main").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "branch", "-M", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Verify marker file does NOT exist before daemon start
    assert!(
        !repo_path.join("pre_start_executed.txt").exists(),
        "Marker file should not exist before daemon starts"
    );

    // Start daemon - this should run the pre-start hook
    let output = cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .stderr_to_stdout()
        .read()
        .unwrap();

    eprintln!("\n=== Daemon start output ===\n{}", output);

    wait_for_daemon_ready(repo_path, 10);

    // Verify marker file was created by pre-start hook
    assert!(
        repo_path.join("pre_start_executed.txt").exists(),
        "pre-start hook should have created pre_start_executed.txt"
    );

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();
}

#[test]
fn test_pre_start_hook_failure() {
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

    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();

    // Create ci.yaml with a failing pre-start hook
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        r#"job:
  command: 'true'

mq:
  base-branch: main
  pre-start:
    command: 'echo "pre-start failing" && exit 1'
"#,
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "main").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "branch", "-M", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Try to start daemon - should fail due to pre-start hook failure
    let result = cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .stderr_to_stdout()
        .unchecked()
        .read();

    eprintln!(
        "\n=== Daemon start output (pre-start failure) ===\n{}",
        result.as_ref().unwrap_or(&"<error>".to_string())
    );

    // The daemon should have failed to start
    // Give it a moment to ensure it didn't actually start
    thread::sleep(Duration::from_millis(500));

    // Check that no daemon is running by trying to stop (should fail or report no daemon)
    let stop_result = cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .stderr_to_stdout()
        .unchecked()
        .read();

    eprintln!(
        "\n=== Stop attempt output ===\n{}",
        stop_result.as_ref().unwrap_or(&"<error>".to_string())
    );

    // The stop should indicate no daemon is running
    if let Ok(ref output) = stop_result {
        assert!(
            output.contains("No MQ daemon") || output.contains("not running"),
            "Stop should indicate no daemon is running when pre-start hook fails"
        );
    }
}

#[test]
fn test_post_start_hook_execution() {
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

    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();

    // Create ci.yaml with a post-start hook
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        r#"job:
  command: 'true'

mq:
  base-branch: main
  post-start:
    command: 'echo "post-start hook running" && touch post_start_executed.txt'
"#,
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "main").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "branch", "-M", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Verify marker file does NOT exist before daemon start
    assert!(
        !repo_path.join("post_start_executed.txt").exists(),
        "Marker file should not exist before daemon starts"
    );

    // Start daemon - this should run the post-start hook after daemonization
    let output = cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .stderr_to_stdout()
        .read()
        .unwrap();

    eprintln!("\n=== Daemon start output ===\n{}", output);

    wait_for_daemon_ready(repo_path, 10);

    // Verify marker file was created by post-start hook
    assert!(
        repo_path.join("post_start_executed.txt").exists(),
        "post-start hook should have created post_start_executed.txt"
    );

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();
}

#[test]
fn test_post_start_hook_failure() {
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

    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();

    // Create ci.yaml with a failing post-start hook
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        r#"job:
  command: 'true'

mq:
  base-branch: main
  post-start:
    command: 'echo "post-start failing" && exit 1'
"#,
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "main").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "branch", "-M", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Try to start daemon - post-start hook will fail after daemonization
    let result = cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .stderr_to_stdout()
        .unchecked()
        .read();

    eprintln!(
        "\n=== Daemon start output (post-start failure) ===\n{}",
        result.as_ref().unwrap_or(&"<error>".to_string())
    );

    // The daemon should have exited after post-start hook failure
    // Give it a moment to ensure it cleaned up
    thread::sleep(Duration::from_millis(500));

    // Check that no daemon is running by trying to stop (should fail or report no daemon)
    let stop_result = cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .stderr_to_stdout()
        .unchecked()
        .read();

    eprintln!(
        "\n=== Stop attempt output ===\n{}",
        stop_result.as_ref().unwrap_or(&"<error>".to_string())
    );

    // The stop should indicate no daemon is running
    if let Ok(ref output) = stop_result {
        assert!(
            output.contains("No MQ daemon") || output.contains("not running"),
            "Stop should indicate no daemon is running when post-start hook fails"
        );
    }
}

#[test]
fn test_hook_env_vars() {
    let repo_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let repo_path = repo_dir.path();

    // Create output directory for env dumps
    let env_output_dir = repo_path.join(".env_output");
    fs::create_dir_all(&env_output_dir).unwrap();
    let pre_clone_env = env_output_dir.join("pre_clone.env");
    let post_merge_env = env_output_dir.join("post_merge.env");

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

    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();

    // Create ci.yaml with hooks that dump env vars to files
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            r#"job:
  command: 'true'

mq:
  base-branch: main
  merge-mode: merge
  pre-clone:
    command: 'env > {pre_clone}'
  post-merge:
    command: 'env > {post_merge}'
"#,
            pre_clone = pre_clone_env.display(),
            post_merge = post_merge_env.display(),
        ),
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "main").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "branch", "-M", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Create feature branch
    cmd!("git", "checkout", "-b", "feature")
        .dir(repo_path)
        .run()
        .unwrap();
    fs::write(repo_path.join("feature.txt"), "feature").unwrap();
    cmd!("git", "add", "feature.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Feature")
        .dir(repo_path)
        .run()
        .unwrap();

    let feature_commit = cmd!("git", "rev-parse", "HEAD")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    cmd!("git", "checkout", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Start daemon
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .run()
        .unwrap();
    wait_for_daemon_ready(repo_path, 10);

    // Add candidate
    let output = cmd!(selfci_bin(), "mq", "add", &feature_commit)
        .dir(repo_path)
        .read()
        .unwrap();

    let job_id: u64 = output
        .lines()
        .find(|l| l.contains("run ID"))
        .and_then(|l| l.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to extract job ID");

    // Wait for completion
    let status_output =
        wait_for_job_completion_with_output(repo_path, job_id, 30).expect("Job did not complete");

    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    // Verify job passed
    assert!(
        status_output.contains("Status: Passed"),
        "Job should have passed, got: {}",
        status_output
    );

    // Parse env vars from hook output files
    let pre_clone_vars = parse_selfci_env_file(&pre_clone_env).unwrap();
    let post_merge_vars = parse_selfci_env_file(&post_merge_env).unwrap();

    eprintln!("\n=== pre-clone env vars ===\n{:#?}", pre_clone_vars);
    eprintln!("\n=== post-merge env vars ===\n{:#?}", post_merge_vars);

    // Verify pre-clone hook env vars
    assert!(
        pre_clone_vars.contains_key("SELFCI_CANDIDATE_COMMIT_ID"),
        "pre-clone should have SELFCI_CANDIDATE_COMMIT_ID"
    );
    assert!(
        !pre_clone_vars["SELFCI_CANDIDATE_COMMIT_ID"].is_empty(),
        "SELFCI_CANDIDATE_COMMIT_ID should not be empty"
    );

    assert!(
        pre_clone_vars.contains_key("SELFCI_CANDIDATE_ID"),
        "pre-clone should have SELFCI_CANDIDATE_ID"
    );
    assert!(
        !pre_clone_vars["SELFCI_CANDIDATE_ID"].is_empty(),
        "SELFCI_CANDIDATE_ID should not be empty"
    );

    assert_eq!(
        pre_clone_vars.get("SELFCI_MQ_BASE_BRANCH"),
        Some(&"main".to_string()),
        "pre-clone SELFCI_MQ_BASE_BRANCH should be 'main'"
    );

    // Verify post-merge hook env vars
    assert!(
        post_merge_vars.contains_key("SELFCI_CANDIDATE_COMMIT_ID"),
        "post-merge should have SELFCI_CANDIDATE_COMMIT_ID"
    );
    assert!(
        !post_merge_vars["SELFCI_CANDIDATE_COMMIT_ID"].is_empty(),
        "post-merge SELFCI_CANDIDATE_COMMIT_ID should not be empty"
    );

    assert_eq!(
        post_merge_vars.get("SELFCI_MQ_BASE_BRANCH"),
        Some(&"main".to_string()),
        "post-merge SELFCI_MQ_BASE_BRANCH should be 'main'"
    );

    // Both hooks should see the same candidate commit ID
    assert_eq!(
        pre_clone_vars.get("SELFCI_CANDIDATE_COMMIT_ID"),
        post_merge_vars.get("SELFCI_CANDIDATE_COMMIT_ID"),
        "pre-clone and post-merge should see the same SELFCI_CANDIDATE_COMMIT_ID"
    );
}

#[test]
fn test_invalid_local_config_fails_daemon_start() {
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

    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();

    // Create valid ci.yaml
    let ci_config = r#"
job:
  command: echo test

mq:
  base-branch: main
"#;
    fs::write(repo_path.join(".config/selfci/ci.yaml"), ci_config).unwrap();

    // Create INVALID local.yaml with syntax error
    let invalid_local_config = r#"
mq:
  pre-start:
    command: echo hello
  invalid_field_that_doesnt_exist: true
"#;
    fs::write(
        repo_path.join(".config/selfci/local.yaml"),
        invalid_local_config,
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "content").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();

    // Try to start daemon - should fail due to invalid config
    let start_result = cmd!(selfci_bin(), "mq", "start", "--base-branch", "main")
        .dir(repo_path)
        .stderr_to_stdout()
        .unchecked()
        .read();

    // Daemon should fail to start
    assert!(
        start_result.is_err() || {
            let output = start_result.as_ref().unwrap();
            output.contains("unknown field") || output.contains("Error")
        },
        "Daemon should fail to start with invalid local.yaml: {:?}",
        start_result
    );
}

#[test]
fn test_invalid_ci_config_fails_daemon_start() {
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

    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();

    // Create INVALID ci.yaml with syntax error (missing required field)
    let invalid_ci_config = r#"
mq:
  base-branch: main
  unknown_field: value
"#;
    fs::write(repo_path.join(".config/selfci/ci.yaml"), invalid_ci_config).unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "content").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();

    // Try to start daemon - should fail due to invalid config
    let start_result = cmd!(selfci_bin(), "mq", "start", "--base-branch", "main")
        .dir(repo_path)
        .stderr_to_stdout()
        .unchecked()
        .read();

    // Daemon should fail to start
    assert!(
        start_result.is_err() || {
            let output = start_result.as_ref().unwrap();
            output.contains("unknown field")
                || output.contains("missing field")
                || output.contains("Error")
        },
        "Daemon should fail to start with invalid ci.yaml: {:?}",
        start_result
    );
}
