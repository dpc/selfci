mod common;

use duct::cmd;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// Helper to get the selfci binary path
fn selfci_bin() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let profile = std::env::var("CARGO_PROFILE").unwrap_or_else(|_| "debug".to_string());
    // Cargo's "dev" profile outputs to "debug" directory
    let dir = if profile == "dev" { "debug" } else { &profile };
    format!("{}/target/{}/selfci", manifest_dir, dir)
}

/// Extract runtime directory from `mq start` output
fn extract_runtime_dir(output: &str) -> PathBuf {
    output
        .lines()
        .find(|l| l.starts_with("Runtime directory:"))
        .map(|l| PathBuf::from(l.trim_start_matches("Runtime directory:").trim()))
        .expect("Failed to extract runtime directory from mq start output")
}

/// Wait for daemon to be ready by polling for mq.sock
fn wait_for_daemon_ready(runtime_dir: &Path, timeout_secs: u64) {
    let socket_path = runtime_dir.join("mq.sock");
    let start = std::time::Instant::now();
    while !socket_path.exists() {
        if start.elapsed().as_secs() > timeout_secs {
            panic!(
                "Daemon did not become ready within {} seconds (socket {} not found)",
                timeout_secs,
                socket_path.display()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Helper to wait for job completion
fn wait_for_job_completion(repo_path: &Path, job_id: u64, timeout_secs: u64) -> bool {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed().as_secs() > timeout_secs {
            return false;
        }

        let output = cmd!(selfci_bin(), "mq", "status", job_id.to_string())
            .dir(repo_path)
            .read()
            .ok();

        if let Some(output) = output
            && (output.contains("Status: Passed") || output.contains("Status: Failed"))
        {
            return output.contains("Status: Passed");
        }

        thread::sleep(Duration::from_millis(100));
    }
}

/// Extract job ID from mq add output
fn extract_job_id(output: &str) -> u64 {
    output
        .lines()
        .find(|l| l.contains("job ID"))
        .and_then(|l| l.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to extract job ID")
}

/// Setup a base Git repository with just main branch and initial commit
fn setup_git_base_repo(merge_style: &str) -> tempfile::TempDir {
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
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            r#"job:
  command: 'true'
mq:
  base-branch: main
  merge-style: {merge_style}
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
fn setup_jj_base_repo(merge_style: &str) -> (tempfile::TempDir, std::path::PathBuf) {
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

    // Create config with merge style
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            r#"job:
  command: 'true'
mq:
  base-branch: main
  merge-style: {merge_style}
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
fn verify_all_merged_git(repo_path: &Path, num_candidates: usize, merge_style: &str) {
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
        if merge_style == "rebase" {
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
        merge_style, num_candidates
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
fn verify_all_merged_jj(repo_path: &Path, num_candidates: usize, merge_style: &str) {
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
        if merge_style == "rebase" {
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
        merge_style, num_candidates
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
fn run_git_merge_test(merge_style: &str, num_candidates: usize) {
    let repo = setup_git_base_repo(merge_style);
    let repo_path = repo.path();

    // Create N candidates
    let mut candidates = Vec::new();
    for i in 1..=num_candidates {
        let commit = create_git_candidate(repo_path, i);
        candidates.push(commit);
    }

    // Start MQ daemon in background
    let start_output = cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .env("SELFCI_LOG", "debug")
        .read()
        .unwrap();
    let runtime_dir = extract_runtime_dir(&start_output);
    wait_for_daemon_ready(&runtime_dir, 10);

    // Add all candidates and collect job IDs
    let mut job_ids = Vec::new();
    for commit in &candidates {
        let output = cmd!(selfci_bin(), "mq", "add", commit)
            .dir(repo_path)
            .read()
            .unwrap();
        let job_id = extract_job_id(&output);
        job_ids.push(job_id);
        eprintln!("Added candidate {} as job {}", &commit[..8], job_id);
    }

    // Wait for all jobs to complete in order
    for (i, job_id) in job_ids.iter().enumerate() {
        assert!(
            wait_for_job_completion(repo_path, *job_id, 30),
            "Job {} (candidate {}) did not complete successfully",
            job_id,
            i + 1
        );
        eprintln!("Job {} (candidate {}) completed", job_id, i + 1);
    }

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    // Verify working directory unchanged
    verify_working_dir_unchanged_git(repo_path);

    // Verify all candidates were merged
    verify_all_merged_git(repo_path, num_candidates, merge_style);
}

/// Run the jj merge test with N candidates
fn run_jj_merge_test(merge_style: &str, num_candidates: usize) {
    let (repo, test_home) = setup_jj_base_repo(merge_style);
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
    let start_output = cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .env("HOME", &test_home)
        .env("JJ_USER", "Test User")
        .env("JJ_EMAIL", "test@example.com")
        .env("SELFCI_LOG", "debug")
        .read()
        .unwrap();
    let runtime_dir = extract_runtime_dir(&start_output);
    wait_for_daemon_ready(&runtime_dir, 10);

    // Add all candidates and collect job IDs
    let mut job_ids = Vec::new();
    for commit in &candidates {
        let output = cmd!(selfci_bin(), "mq", "add", commit)
            .dir(repo_path)
            .env("HOME", &test_home)
            .env("JJ_USER", "Test User")
            .env("JJ_EMAIL", "test@example.com")
            .read()
            .unwrap();
        let job_id = extract_job_id(&output);
        job_ids.push(job_id);
        eprintln!("Added candidate {} as job {}", &commit[..8], job_id);
    }

    // Wait for all jobs to complete in order
    for (i, job_id) in job_ids.iter().enumerate() {
        assert!(
            wait_for_job_completion(repo_path, *job_id, 30),
            "Job {} (candidate {}) did not complete successfully",
            job_id,
            i + 1
        );
        eprintln!("Job {} (candidate {}) completed", job_id, i + 1);
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
    verify_all_merged_jj(repo_path, num_candidates, merge_style);
}

// ============================================================================
// Git rebase tests
// ============================================================================

#[test]
fn test_git_rebase_merge_single() {
    run_git_merge_test("rebase", 1);
}

#[test]
fn test_git_rebase_merge_multi() {
    run_git_merge_test("rebase", 5);
}

// ============================================================================
// Git merge tests
// ============================================================================

#[test]
fn test_git_merge_merge_single() {
    run_git_merge_test("merge", 1);
}

#[test]
fn test_git_merge_merge_multi() {
    run_git_merge_test("merge", 5);
}

// ============================================================================
// Jujutsu rebase tests
// ============================================================================

#[test]
fn test_jj_rebase_merge_single() {
    run_jj_merge_test("rebase", 1);
}

#[test]
fn test_jj_rebase_merge_multi() {
    run_jj_merge_test("rebase", 5);
}

// ============================================================================
// Jujutsu merge tests
// ============================================================================

#[test]
fn test_jj_merge_merge_single() {
    run_jj_merge_test("merge", 1);
}

#[test]
fn test_jj_merge_merge_multi() {
    run_jj_merge_test("merge", 5);
}

// ============================================================================
// Daemon stop tests
// ============================================================================

/// Test that stopping the MQ daemon via command works correctly
#[test]
fn test_mq_stop_via_command() {
    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();

    // Start daemon in background
    let start_output = cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .read()
        .unwrap();
    let runtime_dir = extract_runtime_dir(&start_output);
    wait_for_daemon_ready(&runtime_dir, 10);

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
fn test_mq_stop_via_signal() {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();

    // Start daemon in background
    let start_output = cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .read()
        .unwrap();
    let runtime_dir = extract_runtime_dir(&start_output);
    wait_for_daemon_ready(&runtime_dir, 10);

    // Read PID from runtime directory
    let pid_str =
        std::fs::read_to_string(runtime_dir.join("mq.pid")).expect("Failed to read mq.pid");
    let pid: i32 = pid_str.trim().parse().expect("Invalid PID in mq.pid");

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
fn test_mq_stop_foreground_via_command() {
    let repo = setup_git_base_repo("rebase");
    let repo_path = repo.path();

    // Use explicit runtime directory so we know where to poll for socket
    let runtime_dir = repo_path.join(".selfci-mq-runtime");

    // Start daemon in foreground mode (runs as direct child process)
    // Use stdin/stdout/stderr_null to avoid blocking on IO
    let _handle = cmd!(selfci_bin(), "mq", "start", "-f")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .stdin_null()
        .stdout_null()
        .stderr_null()
        .start()
        .unwrap();
    wait_for_daemon_ready(&runtime_dir, 10);

    // Stop via command (must use same runtime dir)
    let start = std::time::Instant::now();
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .env("SELFCI_MQ_RUNTIME_DIR", &runtime_dir)
        .run()
        .unwrap();
    let elapsed = start.elapsed();

    // Should stop quickly (under 5 seconds)
    assert!(
        elapsed.as_secs() < 5,
        "Foreground daemon stop via command took too long: {:?}",
        elapsed
    );
}

/// Test that stopping the MQ daemon in foreground mode via SIGTERM signal works correctly
#[test]
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
    wait_for_daemon_ready(&runtime_dir, 10);

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
