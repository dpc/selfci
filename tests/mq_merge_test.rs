mod common;

use duct::cmd;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// Helper to get the selfci binary path
fn selfci_bin() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{}/target/debug/selfci", manifest_dir)
}

/// Helper to wait for daemon to start
fn wait_for_daemon_start() {
    thread::sleep(Duration::from_millis(500));
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

/// Setup a Git repository with multi-commit candidate on a feature branch
fn setup_git_mq_repo(merge_style: &str) -> tempfile::TempDir {
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
            "job:\n  command: 'true'\nmq:\n  base-branch: main\n  merge-style: {}\n",
            merge_style
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

    // Create feature branch with multiple commits
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

    fs::write(repo_path.join("feature3.txt"), "feature 3").unwrap();
    cmd!("git", "add", "feature3.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Feature commit 3")
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

    // Switch to a completely different state (back to main, modify a file)
    cmd!("git", "checkout", "main")
        .dir(repo_path)
        .run()
        .unwrap();
    fs::write(repo_path.join("working.txt"), "working on something else").unwrap();

    // Store feature commit for later use
    fs::write(repo_path.join(".feature_commit"), feature_commit).unwrap();

    repo_dir
}

/// Setup a Jujutsu repository with multi-commit candidate
fn setup_jj_mq_repo(merge_style: &str) -> tempfile::TempDir {
    let repo_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let repo_path = repo_dir.path();

    // Initialize jj repo
    cmd!("jj", "git", "init").dir(repo_path).run().unwrap();

    // Create config with merge style
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            "job:\n  command: 'true'\nmq:\n  base-branch: main\n  merge-style: {}\n",
            merge_style
        ),
    )
    .unwrap();

    // Create initial commit
    fs::write(repo_path.join("main.txt"), "main content").unwrap();
    cmd!("jj", "file", "track", "main.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("jj", "file", "track", ".config/selfci/ci.yaml")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Initial commit")
        .dir(repo_path)
        .run()
        .unwrap();

    // Create main bookmark
    cmd!("jj", "bookmark", "create", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Create feature commits
    cmd!("jj", "new", "main").dir(repo_path).run().unwrap();
    fs::write(repo_path.join("feature1.txt"), "feature 1").unwrap();
    cmd!("jj", "file", "track", "feature1.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Feature commit 1")
        .dir(repo_path)
        .run()
        .unwrap();

    cmd!("jj", "new").dir(repo_path).run().unwrap();
    fs::write(repo_path.join("feature2.txt"), "feature 2").unwrap();
    cmd!("jj", "file", "track", "feature2.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Feature commit 2")
        .dir(repo_path)
        .run()
        .unwrap();

    cmd!("jj", "new").dir(repo_path).run().unwrap();
    fs::write(repo_path.join("feature3.txt"), "feature 3").unwrap();
    cmd!("jj", "file", "track", "feature3.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Feature commit 3")
        .dir(repo_path)
        .run()
        .unwrap();

    // Get the feature commit ID
    let feature_commit = cmd!("jj", "log", "-r", "@", "--no-graph", "-T", "commit_id")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    // Switch to a completely different state
    cmd!("jj", "new", "main").dir(repo_path).run().unwrap();
    fs::write(repo_path.join("working.txt"), "working on something else").unwrap();

    // Store feature commit for later use
    fs::write(repo_path.join(".feature_commit"), feature_commit).unwrap();

    repo_dir
}

/// Verify that working directory state hasn't changed
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

    // feature files should not be in working directory
    assert!(
        !repo_path.join("feature1.txt").exists(),
        "Feature files appeared in working directory!"
    );
}

/// Verify that working directory state hasn't changed
fn verify_working_dir_unchanged_jj(repo_path: &Path) {
    // working.txt should still exist with original content
    let content = fs::read_to_string(repo_path.join("working.txt")).unwrap();
    assert_eq!(
        content, "working on something else",
        "Working directory file changed!"
    );

    // feature files should not be in working directory
    assert!(
        !repo_path.join("feature1.txt").exists(),
        "Feature files appeared in working directory!"
    );
}

/// Verify that main branch has the feature commits
fn verify_merge_succeeded_git(repo_path: &Path, merge_style: &str) {
    // Force update working directory to match main branch state
    // (main branch was updated by update-ref while checked out, so working dir is stale)
    cmd!("git", "reset", "--hard", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    assert!(
        repo_path.join("feature1.txt").exists(),
        "Feature file 1 not in main"
    );
    assert!(
        repo_path.join("feature2.txt").exists(),
        "Feature file 2 not in main"
    );
    assert!(
        repo_path.join("feature3.txt").exists(),
        "Feature file 3 not in main"
    );

    // Verify merge style was applied correctly
    let log = cmd!("git", "log", "--oneline", "main")
        .dir(repo_path)
        .read()
        .unwrap();

    if merge_style == "merge" {
        // Should have a merge commit
        assert!(
            log.contains("Merge") || log.lines().count() > 4,
            "Expected merge commit not found"
        );
    } else {
        // Rebase mode - should have linear history with all 3 feature commits
        assert!(
            log.contains("Feature commit 1"),
            "Feature commit 1 not in history"
        );
        assert!(
            log.contains("Feature commit 2"),
            "Feature commit 2 not in history"
        );
        assert!(
            log.contains("Feature commit 3"),
            "Feature commit 3 not in history"
        );
    }
}

/// Verify that main bookmark has the feature commits
fn verify_merge_succeeded_jj(repo_path: &Path, _merge_style: &str) {
    // Check that feature files are in main bookmark
    let files = cmd!("jj", "file", "list", "-r", "main")
        .dir(repo_path)
        .read()
        .unwrap();

    assert!(files.contains("feature1.txt"), "Feature file 1 not in main");
    assert!(files.contains("feature2.txt"), "Feature file 2 not in main");
    assert!(files.contains("feature3.txt"), "Feature file 3 not in main");

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

    assert!(
        log.contains("Feature commit 1"),
        "Feature commit 1 not in history"
    );
    assert!(
        log.contains("Feature commit 2"),
        "Feature commit 2 not in history"
    );
    assert!(
        log.contains("Feature commit 3"),
        "Feature commit 3 not in history"
    );
}

#[test]
fn test_git_rebase_merge() {
    let repo = setup_git_mq_repo("rebase");
    let repo_path = repo.path();

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    // Start MQ daemon in background
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .env("SELFCI_LOG", "debug")
        .run()
        .unwrap();
    wait_for_daemon_start();

    // Add candidate
    let output = cmd!(selfci_bin(), "mq", "add", feature_commit.trim())
        .dir(repo_path)
        .read()
        .unwrap();

    // Extract job ID
    let job_id: u64 = output
        .lines()
        .find(|l| l.contains("job ID"))
        .and_then(|l| l.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to extract job ID");

    // Wait for completion
    assert!(
        wait_for_job_completion(repo_path, job_id, 30),
        "Job did not complete successfully"
    );

    // Stop daemon
    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    // Verify working directory unchanged
    verify_working_dir_unchanged_git(repo_path);

    // Verify merge succeeded
    verify_merge_succeeded_git(repo_path, "rebase");
}

#[test]
fn test_git_merge_merge() {
    let repo = setup_git_mq_repo("merge");
    let repo_path = repo.path();

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    // Start MQ daemon in background
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .env("SELFCI_LOG", "debug")
        .run()
        .unwrap();
    wait_for_daemon_start();

    // Add candidate
    let output = cmd!(selfci_bin(), "mq", "add", feature_commit.trim())
        .dir(repo_path)
        .read()
        .unwrap();

    let job_id: u64 = output
        .lines()
        .find(|l| l.contains("job ID"))
        .and_then(|l| l.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to extract job ID");

    assert!(
        wait_for_job_completion(repo_path, job_id, 30),
        "Job did not complete successfully"
    );

    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    verify_working_dir_unchanged_git(repo_path);
    verify_merge_succeeded_git(repo_path, "merge");
}

#[test]
fn test_jj_rebase_merge() {
    let repo = setup_jj_mq_repo("rebase");
    let repo_path = repo.path();

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    cmd!(selfci_bin(), "mq", "start", "-f")
        .dir(repo_path)
        .env("SELFCI_LOG", "debug")
        .start()
        .unwrap();
    wait_for_daemon_start();

    let output = cmd!(selfci_bin(), "mq", "add", feature_commit.trim())
        .dir(repo_path)
        .read()
        .unwrap();

    let job_id: u64 = output
        .lines()
        .find(|l| l.contains("job ID"))
        .and_then(|l| l.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to extract job ID");

    assert!(
        wait_for_job_completion(repo_path, job_id, 30),
        "Job did not complete successfully"
    );

    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    verify_working_dir_unchanged_jj(repo_path);
    verify_merge_succeeded_jj(repo_path, "rebase");
}

#[test]
fn test_jj_merge_merge() {
    let repo = setup_jj_mq_repo("merge");
    let repo_path = repo.path();

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    cmd!(selfci_bin(), "mq", "start", "-f")
        .dir(repo_path)
        .env("SELFCI_LOG", "debug")
        .start()
        .unwrap();
    wait_for_daemon_start();

    let output = cmd!(selfci_bin(), "mq", "add", feature_commit.trim())
        .dir(repo_path)
        .read()
        .unwrap();

    let job_id: u64 = output
        .lines()
        .find(|l| l.contains("job ID"))
        .and_then(|l| l.split(':').next_back())
        .and_then(|s| s.trim().parse().ok())
        .expect("Failed to extract job ID");

    assert!(
        wait_for_job_completion(repo_path, job_id, 30),
        "Job did not complete successfully"
    );

    cmd!(selfci_bin(), "mq", "stop")
        .dir(repo_path)
        .run()
        .unwrap();

    verify_working_dir_unchanged_jj(repo_path);
    verify_merge_succeeded_jj(repo_path, "merge");
}
