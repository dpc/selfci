mod common;

use duct::cmd;
use selfci::constants;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use tracing::info;
use tracing_test::traced_test;

/// Helper to get the selfci binary path
fn selfci_bin() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let profile = std::env::var("CARGO_PROFILE").unwrap_or_else(|_| "debug".to_string());
    // Cargo's "dev" profile outputs to "debug" directory
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
        // This blocks until daemon's accept loop is running (after initialization)
        // Note: Don't use .unchecked() - we need .read() to return Err on non-zero exit
        let result = cmd!(selfci_bin(), "mq", "list")
            .dir(repo_path)
            .stderr_null()
            .stdout_null()
            .run();
        if result.is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "Daemon did not become ready within {} seconds",
        timeout_secs
    );
}

/// Wait for daemon with explicit runtime dir (for foreground mode tests)
fn wait_for_daemon_ready_with_env(repo_path: &Path, runtime_dir: &Path, timeout_secs: u64) {
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < timeout_secs {
        // Note: Don't use .unchecked() - we need .run() to return Err on non-zero exit
        let result = cmd!(selfci_bin(), "mq", "list")
            .dir(repo_path)
            .env("SELFCI_MQ_RUNTIME_DIR", runtime_dir)
            .stderr_null()
            .stdout_null()
            .run();
        if result.is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "Daemon did not become ready within {} seconds",
        timeout_secs
    );
}

/// Helper to wait for run completion
fn wait_for_run_completion(repo_path: &Path, run_id: u64, timeout_secs: u64) -> bool {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed().as_secs() > timeout_secs {
            return false;
        }

        let output = cmd!(selfci_bin(), "mq", "status", run_id.to_string())
            .dir(repo_path)
            .read()
            .ok();

        if let Some(output) = output
            && (output.contains("Status: Success") || output.contains("Status: Failed"))
        {
            return output.contains("Status: Success");
        }

        thread::sleep(Duration::from_millis(100));
    }
}

/// Extract run ID from mq add output
fn extract_run_id(output: &str) -> u64 {
    output
        .lines()
        .find(|l| l.contains("run ID"))
        .and_then(|l| l.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to extract run ID")
}

/// Setup a base Git repository with just main branch and initial commit
fn setup_git_base_repo(merge_mode: &str) -> tempfile::TempDir {
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
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            r#"job:
  command: 'true'
mq:
  base-branch: main
  merge-mode: {merge_mode}
"#
        ),
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

    // Create working file (simulating user's uncommitted work)
    fs::write(repo_path.join("working.txt"), "working on something else").unwrap();

    repo_dir
}

/// Create a feature branch candidate in a git repo
/// Returns the commit ID of the candidate tip
fn create_git_candidate(repo_path: &Path, candidate_num: usize) -> String {
    let branch_name = format!("feature-{}", candidate_num);

    // Create feature branch from main
    cmd!("git", "checkout", "-b", &branch_name, "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Add commits for this candidate
    for commit_num in 1..=3 {
        let filename = format!("feature{}_{}.txt", candidate_num, commit_num);
        fs::write(
            repo_path.join(&filename),
            format!("feature {} commit {}", candidate_num, commit_num),
        )
        .unwrap();
        cmd!("git", "add", &filename).dir(repo_path).run().unwrap();
        cmd!(
            "git",
            "commit",
            "-m",
            format!("Feature {} commit {}", candidate_num, commit_num)
        )
        .dir(repo_path)
        .run()
        .unwrap();
    }

    // Get the commit ID
    let commit = cmd!("git", "rev-parse", "HEAD")
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

    commit
}

/// Setup a base Jujutsu repository with just main bookmark and initial commit
/// Returns (repo_dir, test_home_path)
fn setup_jj_base_repo(merge_mode: &str) -> (tempfile::TempDir, PathBuf) {
    let repo_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let repo_path = repo_dir.path();

    // Create isolated HOME for this test (required in Nix environment)
    let test_home = repo_path.join(".test_home");
    fs::create_dir_all(&test_home).unwrap();

    // Initialize jj repo
    cmd!("jj", "git", "init")
        .dir(repo_path)
        .env("HOME", &test_home)
        .env("JJ_USER", "Test User")
        .env("JJ_EMAIL", "test@example.com")
        .run()
        .unwrap();

    // Configure jj user in repo config
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
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            r#"job:
  command: 'true'
mq:
  base-branch: main
  merge-mode: {merge_mode}
"#
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

    (repo_dir, test_home)
}

/// Create a feature branch candidate in a jj repo
/// Returns the commit ID of the candidate tip
fn create_jj_candidate(repo_path: &Path, test_home: &Path, candidate_num: usize) -> String {
    // Create feature commits branching off from main
    cmd!("jj", "new", "main")
        .dir(repo_path)
        .env("HOME", test_home)
        .run()
        .unwrap();

    // Add commits for this candidate
    for commit_num in 1..=3 {
        let filename = format!("feature{}_{}.txt", candidate_num, commit_num);
        fs::write(
            repo_path.join(&filename),
            format!("feature {} commit {}", candidate_num, commit_num),
        )
        .unwrap();
        cmd!("jj", "file", "track", &filename)
            .dir(repo_path)
            .env("HOME", test_home)
            .run()
            .unwrap();
        cmd!(
            "jj",
            "describe",
            "-m",
            format!("Feature {} commit {}", candidate_num, commit_num)
        )
        .dir(repo_path)
        .env("HOME", test_home)
        .run()
        .unwrap();

        if commit_num < 3 {
            cmd!("jj", "new")
                .dir(repo_path)
                .env("HOME", test_home)
                .run()
                .unwrap();
        }
    }

    // Get the commit ID
    let commit = cmd!("jj", "log", "-r", "@", "--no-graph", "-T", "commit_id")
        .dir(repo_path)
        .env("HOME", test_home)
        .read()
        .unwrap()
        .trim()
        .to_string();

    // Return to working state (new commit on main, not affecting the candidate)
    cmd!("jj", "new", "main")
        .dir(repo_path)
        .env("HOME", test_home)
        .run()
        .unwrap();

    commit
}

/// Verify that working directory state hasn't changed (git)
fn verify_working_dir_unchanged_git(repo_path: &Path) {
    // Should still be on main branch
    let branch = cmd!("git", "branch", "--show-current")
        .dir(repo_path)
        .read()
        .unwrap();
    assert_eq!(branch.trim(), "main", "Working directory branch changed!");

    // working.txt should still exist with original content
    let content = fs::read_to_string(repo_path.join("working.txt")).unwrap();
    assert_eq!(
        content, "working on something else",
        "Working directory file changed!"
    );
}

/// Verify that working directory state hasn't changed (jj)
fn verify_working_dir_unchanged_jj(repo_path: &Path) {
    // working.txt should still exist with original content
    let content = fs::read_to_string(repo_path.join("working.txt")).unwrap();
    assert_eq!(
        content, "working on something else",
        "Working directory file changed!"
    );
}

/// Verify that all candidates were merged into main (git)
fn verify_all_merged_git(repo_path: &Path, num_candidates: usize, merge_mode: &str) {
    // Force update working directory to match main branch state
    cmd!("git", "reset", "--hard", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Check all feature files exist
    for candidate in 1..=num_candidates {
        for commit in 1..=3 {
            let filename = format!("feature{}_{}.txt", candidate, commit);
            assert!(
                repo_path.join(&filename).exists(),
                "Feature file {} not in main",
                filename
            );
        }
    }

    // Verify commit history contains all feature commits
    let log = cmd!("git", "log", "--oneline", "main")
        .dir(repo_path)
        .read()
        .unwrap();

    for candidate in 1..=num_candidates {
        if merge_mode == "rebase" {
            // Rebase mode should have linear history with all commits
            for commit in 1..=3 {
                assert!(
                    log.contains(&format!("Feature {} commit {}", candidate, commit)),
                    "Feature {} commit {} not in history",
                    candidate,
                    commit
                );
            }
        }
    }

    eprintln!(
        "\n=== git {} merge result ({} candidates) ===",
        merge_mode, num_candidates
    );
    cmd!(
        "git",
        "--no-pager",
        "log",
        "--oneline",
        "--graph",
        "main",
        "-20"
    )
    .dir(repo_path)
    .run()
    .unwrap();
}

/// Verify that all candidates were merged into main (jj)
fn verify_all_merged_jj(repo_path: &Path, num_candidates: usize, merge_mode: &str) {
    // Check all feature files exist in main bookmark
    let files = cmd!("jj", "file", "list", "-r", "main")
        .dir(repo_path)
        .read()
        .unwrap();

    for candidate in 1..=num_candidates {
        for commit in 1..=3 {
            let filename = format!("feature{}_{}.txt", candidate, commit);
            assert!(
                files.contains(&filename),
                "Feature file {} not in main",
                filename
            );
        }
    }

    // Verify commit history
    let log = cmd!(
        "jj",
        "log",
        "-r",
        "::main",
        "--no-graph",
        "-T",
        "description"
    )
    .dir(repo_path)
    .read()
    .unwrap();

    for candidate in 1..=num_candidates {
        if merge_mode == "rebase" {
            for commit in 1..=3 {
                assert!(
                    log.contains(&format!("Feature {} commit {}", candidate, commit)),
                    "Feature {} commit {} not in history",
                    candidate,
                    commit
                );
            }
        }
    }

    eprintln!(
        "\n=== jj {} merge result ({} candidates) ===",
        merge_mode, num_candidates
    );
    cmd!(
        "jj",
        "--no-pager",
        "log",
        "-r",
        &format!("ancestors(main, {})", 5 + num_candidates * 3)
    )
    .dir(repo_path)
    .run()
    .unwrap();
}

/// Run the git merge test with N candidates
fn run_git_merge_test(merge_mode: &str, num_candidates: usize) {
    let repo = setup_git_base_repo(merge_mode);
    let repo_path = repo.path();

    // Create N candidates
    let mut candidates = Vec::new();
    for i in 1..=num_candidates {
        let commit = create_git_candidate(repo_path, i);
        candidates.push(commit);
    }

    // Start MQ daemon in background
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .env("SELFCI_LOG", "debug")
        .run()
        .unwrap();
    wait_for_daemon_ready(repo_path, 10);

    // Add all candidates and collect run IDs
    let mut run_ids = Vec::new();
    for commit in &candidates {
        let output = cmd!(selfci_bin(), "mq", "add", commit)
            .dir(repo_path)
            .read()
            .unwrap();
        let run_id = extract_run_id(&output);
        run_ids.push(run_id);
        eprintln!("Added candidate {} as run {}", &commit[..8], run_id);
    }

    // Wait for all runs to complete in order
    for (i, run_id) in run_ids.iter().enumerate() {
        assert!(
            wait_for_run_completion(repo_path, *run_id, 30),
            "Job {} (candidate {}) did not complete successfully",
            run_id,
            i + 1
        );
        eprintln!("Job {} (candidate {}) completed", run_id, i + 1);
    }

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    // Verify working directory unchanged
    verify_working_dir_unchanged_git(repo_path);

    // Verify all candidates were merged
    verify_all_merged_git(repo_path, num_candidates, merge_mode);
}

/// Run the jj merge test with N candidates
fn run_jj_merge_test(merge_mode: &str, num_candidates: usize) {
    let (repo, test_home) = setup_jj_base_repo(merge_mode);
    let repo_path = repo.path();

    // Create N candidates
    let mut candidates = Vec::new();
    for i in 1..=num_candidates {
        let commit = create_jj_candidate(repo_path, &test_home, i);
        candidates.push(commit);
    }

    // Create working file AFTER all candidates are created
    // (jj new main loses untracked files, so we create it last)
    fs::write(repo_path.join("working.txt"), "working on something else").unwrap();

    // Start MQ daemon in background
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .env("HOME", &test_home)
        .env("JJ_USER", "Test User")
        .env("JJ_EMAIL", "test@example.com")
        .env("SELFCI_LOG", "debug")
        .run()
        .unwrap();
    wait_for_daemon_ready(repo_path, 10);

    // Add all candidates and collect run IDs
    let mut run_ids = Vec::new();
    for commit in &candidates {
        let output = cmd!(selfci_bin(), "mq", "add", commit)
            .dir(repo_path)
            .env("HOME", &test_home)
            .env("JJ_USER", "Test User")
            .env("JJ_EMAIL", "test@example.com")
            .read()
            .unwrap();
        let run_id = extract_run_id(&output);
        run_ids.push(run_id);
        eprintln!("Added candidate {} as run {}", &commit[..8], run_id);
    }

    // Wait for all runs to complete in order
    for (i, run_id) in run_ids.iter().enumerate() {
        assert!(
            wait_for_run_completion(repo_path, *run_id, 30),
            "Job {} (candidate {}) did not complete successfully",
            run_id,
            i + 1
        );
        eprintln!("Job {} (candidate {}) completed", run_id, i + 1);
    }

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .env("HOME", &test_home)
        .env("JJ_USER", "Test User")
        .env("JJ_EMAIL", "test@example.com")
        .run()
        .unwrap();

    // Verify working directory unchanged
    verify_working_dir_unchanged_jj(repo_path);

    // Verify all candidates were merged
    verify_all_merged_jj(repo_path, num_candidates, merge_mode);
}

// ============================================================================
// Git rebase tests
// ============================================================================

#[test]
#[traced_test]
fn test_git_rebase_merge_single() {
    run_git_merge_test("rebase", 1);
}

#[test]
#[traced_test]
fn test_git_rebase_merge_multi() {
    run_git_merge_test("rebase", 5);
}

// ============================================================================
// Git merge tests
// ============================================================================

#[test]
#[traced_test]
fn test_git_merge_merge_single() {
    run_git_merge_test("merge", 1);
}

#[test]
#[traced_test]
fn test_git_merge_merge_multi() {
    run_git_merge_test("merge", 5);
}

// ============================================================================
// Jujutsu rebase tests
// ============================================================================

#[test]
#[traced_test]
fn test_jj_rebase_merge_single() {
    run_jj_merge_test("rebase", 1);
}

#[test]
#[traced_test]
fn test_jj_rebase_merge_multi() {
    run_jj_merge_test("rebase", 5);
}

// ============================================================================
// Jujutsu merge tests
// ============================================================================

#[test]
#[traced_test]
fn test_jj_merge_merge_single() {
    run_jj_merge_test("merge", 1);
}

#[test]
#[traced_test]
fn test_jj_merge_merge_multi() {
    run_jj_merge_test("merge", 5);
}

// ============================================================================
// Daemon stop tests
// ============================================================================

/// Test that stopping the MQ daemon via command works correctly
#[test]
#[traced_test]
fn test_mq_stop_via_command() {
    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();

    // Start daemon in background
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .run()
        .unwrap();
    wait_for_daemon_ready(repo_path, 10);

    // Stop via command
    let start = std::time::Instant::now();
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();
    let elapsed = start.elapsed();

    // Should stop quickly (under 5 seconds)
    assert!(
        elapsed.as_secs() < 5,
        "Daemon stop via command took too long: {:?}",
        elapsed
    );
}

/// Test that stopping the MQ daemon via SIGTERM signal works correctly
#[test]
#[traced_test]
fn test_mq_stop_via_signal() {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();

    // Start daemon in background
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .run()
        .unwrap();
    wait_for_daemon_ready(repo_path, 10);

    // Get daemon PID
    let pid_str = cmd!(selfci_bin(), "mq", "pid")
        .dir(repo_path)
        .read()
        .unwrap();
    let pid: i32 = pid_str.trim().parse().expect("Invalid PID");

    // Send SIGTERM directly
    let start = std::time::Instant::now();
    signal::kill(Pid::from_raw(pid), Signal::SIGTERM).unwrap();

    // Wait for process to exit
    for _ in 0..50 {
        if signal::kill(Pid::from_raw(pid), None).is_err() {
            break; // Process exited
        }
        thread::sleep(Duration::from_millis(100));
    }
    let elapsed = start.elapsed();

    // Should stop quickly (under 5 seconds)
    assert!(
        elapsed.as_secs() < 5,
        "Daemon stop via SIGTERM took too long: {:?}",
        elapsed
    );

    // Verify process is gone
    assert!(
        signal::kill(Pid::from_raw(pid), None).is_err(),
        "Daemon process should have exited"
    );
}

/// Test that stopping the MQ daemon in foreground mode via command works correctly
#[test]
#[traced_test]
fn test_mq_stop_foreground_via_command() {
    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();
    info!(repo_path = %repo_path.display(), "Test started");

    // Use explicit runtime directory so we know where to poll for socket
    let runtime_dir = repo_path.join(".selfci-mq-runtime");

    // Start daemon in foreground mode (runs as direct child process)
    // Use stdin/stdout/stderr_null to avoid blocking on IO
    info!(runtime_dir = %runtime_dir.display(), "Starting daemon in foreground mode");
    let _handle = cmd!(selfci_bin(), "mq", "start", "-f")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .stdin_null()
        .stdout_null()
        .stderr_null()
        .start()
        .unwrap();
    info!("Waiting for daemon to be ready");
    wait_for_daemon_ready_with_env(repo_path, &runtime_dir, 10);
    info!("Daemon is ready");

    // Stop via command (must use same runtime dir)
    info!("Stopping daemon via command");
    let start = std::time::Instant::now();
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .run()
        .unwrap();
    let elapsed = start.elapsed();
    info!(?elapsed, "Daemon stopped");

    // Should stop quickly (under 5 seconds)
    assert!(
        elapsed.as_secs() < 5,
        "Foreground daemon stop via command took too long: {:?}",
        elapsed
    );
}

/// Test that stopping the MQ daemon in foreground mode via SIGTERM signal works correctly
#[test]
#[traced_test]
fn test_mq_stop_foreground_via_signal() {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;
    use std::process::{Command, Stdio};

    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();

    // Use explicit runtime directory so we know where to poll for socket
    let runtime_dir = repo_path.join(".selfci-mq-runtime");

    // Start daemon in foreground mode using std::process::Command
    // Use null IO to avoid any blocking on pipe buffers
    let child = Command::new(selfci_bin())
        .args(["mq", "start", "-f"])
        .current_dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn daemon");
    let pid = child.id() as i32;
    wait_for_daemon_ready_with_env(repo_path, &runtime_dir, 10);

    // Send SIGTERM directly
    let start = std::time::Instant::now();
    signal::kill(Pid::from_raw(pid), Signal::SIGTERM).unwrap();

    // Wait for process to exit using try_wait() to properly reap zombies
    // Note: signal::kill(pid, None) returns success for zombie processes,
    // so we must use try_wait() which will reap the zombie
    let mut child = child;
    let mut exited = false;
    for _ in 0..50 {
        match child.try_wait() {
            Ok(Some(_status)) => {
                exited = true;
                break;
            }
            Ok(None) => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }
    let elapsed = start.elapsed();

    // Should stop quickly (under 5 seconds)
    assert!(
        exited && elapsed.as_secs() < 5,
        "Foreground daemon stop via SIGTERM took too long: {:?} (exited: {})",
        elapsed,
        exited
    );
}

// ============================================================================
// Daemon start and lifecycle tests
// ============================================================================

/// Test basic daemon start and helper commands (runtime-dir, pid)
#[test]
#[traced_test]
fn test_mq_start_and_helper_commands() {
    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();

    // Start daemon
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .run()
        .unwrap();
    wait_for_daemon_ready(repo_path, 10);

    // Test runtime-dir command
    let runtime_dir = cmd!(selfci_bin(), "mq", "runtime-dir")
        .dir(repo_path)
        .read()
        .unwrap();
    let runtime_dir = runtime_dir.trim();
    assert!(!runtime_dir.is_empty(), "runtime-dir should return a path");
    assert!(
        std::path::Path::new(runtime_dir).exists(),
        "runtime-dir path should exist"
    );

    // Test pid command
    let pid_str = cmd!(selfci_bin(), "mq", "pid")
        .dir(repo_path)
        .read()
        .unwrap();
    let pid: u32 = pid_str.trim().parse().expect("pid should be a number");
    assert!(pid > 0, "pid should be positive");

    // Verify the daemon process exists
    assert!(
        std::path::Path::new(&format!("/proc/{}", pid)).exists(),
        "daemon process should exist"
    );

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    // After stop, runtime-dir and pid should fail
    let result = cmd!(selfci_bin(), "mq", "runtime-dir")
        .dir(repo_path)
        .unchecked()
        .run()
        .unwrap();
    assert!(
        !result.status.success(),
        "runtime-dir should fail after stop"
    );

    let result = cmd!(selfci_bin(), "mq", "pid")
        .dir(repo_path)
        .unchecked()
        .run()
        .unwrap();
    assert!(!result.status.success(), "pid should fail after stop");
}

/// Test that starting daemon when already running is idempotent
#[test]
#[traced_test]
fn test_mq_start_already_running() {
    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();

    // Start daemon first time
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .run()
        .unwrap();
    wait_for_daemon_ready(repo_path, 10);

    // Get initial PID
    let pid1 = cmd!(selfci_bin(), "mq", "pid")
        .dir(repo_path)
        .read()
        .unwrap();

    // Start daemon again - should be idempotent
    let output = cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .stderr_to_stdout()
        .read()
        .unwrap();
    assert!(
        output.contains("already running"),
        "should report daemon already running"
    );

    // PID should be the same (same daemon)
    let pid2 = cmd!(selfci_bin(), "mq", "pid")
        .dir(repo_path)
        .read()
        .unwrap();
    assert_eq!(pid1, pid2, "daemon PID should not change");

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();
}

/// Test auto-start: daemon starts automatically when running `mq add` without daemon
#[test]
#[traced_test]
fn test_mq_auto_start() {
    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();

    // Create a feature branch
    cmd!("git", "checkout", "-b", "feature")
        .dir(repo_path)
        .run()
        .unwrap();
    fs::write(repo_path.join("feature.txt"), "feature").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Feature")
        .dir(repo_path)
        .run()
        .unwrap();

    // Verify daemon is not running
    let result = cmd!(selfci_bin(), "mq", "pid")
        .dir(repo_path)
        .unchecked()
        .run()
        .unwrap();
    assert!(
        !result.status.success(),
        "daemon should not be running initially"
    );

    // Run mq add - should auto-start daemon
    let output = cmd!(selfci_bin(), "mq", "add", "feature")
        .dir(repo_path)
        .stderr_to_stdout()
        .read()
        .unwrap();
    assert!(
        output.contains("Auto-starting"),
        "should auto-start daemon: {}",
        output
    );

    // Daemon should now be running
    let pid = cmd!(selfci_bin(), "mq", "pid")
        .dir(repo_path)
        .read()
        .unwrap();
    assert!(
        !pid.trim().is_empty(),
        "daemon should be running after auto-start"
    );

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();
}

/// Test daemon lifecycle with explicit SELFCI_MQ_RUNTIME_DIR
#[test]
#[traced_test]
fn test_mq_explicit_runtime_dir() {
    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();

    // Use explicit runtime directory
    let runtime_dir = repo_path.join(".selfci-mq-explicit");

    // Start daemon with explicit runtime dir
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .run()
        .unwrap();
    wait_for_daemon_ready_with_env(repo_path, &runtime_dir, 10);

    // Verify runtime directory was created
    assert!(
        runtime_dir.exists(),
        "explicit runtime dir should be created"
    );
    assert!(
        runtime_dir.join(constants::MQ_SOCK_FILENAME).exists(),
        "socket should exist in explicit runtime dir"
    );
    assert!(
        runtime_dir.join(constants::MQ_PID_FILENAME).exists(),
        "pid file should exist in explicit runtime dir"
    );
    assert!(
        runtime_dir.join(constants::MQ_DIR_FILENAME).exists(),
        "dir file should exist in explicit runtime dir"
    );

    // Test runtime-dir command returns the explicit path
    let returned_dir = cmd!(selfci_bin(), "mq", "runtime-dir")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .read()
        .unwrap();
    assert_eq!(
        returned_dir.trim(),
        runtime_dir.to_str().unwrap(),
        "runtime-dir should return explicit path"
    );

    // Test pid command
    let pid_str = cmd!(selfci_bin(), "mq", "pid")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .read()
        .unwrap();
    let pid: u32 = pid_str.trim().parse().expect("pid should be a number");
    assert!(pid > 0, "pid should be positive");

    // Test list command (should show no runs initially)
    let list_output = cmd!(selfci_bin(), "mq", "list")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .read()
        .unwrap();
    assert!(
        list_output.contains("No runs"),
        "should show no runs initially"
    );

    // Create a feature branch and add it to the queue
    cmd!("git", "checkout", "-b", "feature")
        .dir(repo_path)
        .run()
        .unwrap();
    fs::write(repo_path.join("feature.txt"), "feature content").unwrap();
    cmd!("git", "add", ".").dir(repo_path).run().unwrap();
    cmd!("git", "commit", "-m", "Add feature")
        .dir(repo_path)
        .run()
        .unwrap();

    // Add candidate to queue
    let add_output = cmd!(selfci_bin(), "mq", "add", "--no-merge", "feature")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .read()
        .unwrap();
    assert!(
        add_output.contains("run ID: 1"),
        "should add candidate with run ID 1: {}",
        add_output
    );

    // Wait a moment for the run to be processed
    thread::sleep(Duration::from_millis(500));

    // Test status command
    let status_output = cmd!(selfci_bin(), "mq", "status", "1")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .read()
        .unwrap();
    assert!(
        status_output.contains("Run ID: 1"),
        "status should show run 1: {}",
        status_output
    );

    // List should now show the run
    let list_output = cmd!(selfci_bin(), "mq", "list")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .read()
        .unwrap();
    assert!(
        !list_output.contains("No runs"),
        "should show runs after adding: {}",
        list_output
    );

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .run()
        .unwrap();

    // Verify daemon stopped - runtime dir should be cleaned up
    assert!(
        !runtime_dir.join(constants::MQ_SOCK_FILENAME).exists(),
        "socket should be removed after stop"
    );

    // Commands should fail after stop
    let result = cmd!(selfci_bin(), "mq", "pid")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .unchecked()
        .run()
        .unwrap();
    assert!(!result.status.success(), "pid should fail after stop");
}
