mod common;

use duct::cmd;
use std::fs;
use std::path::Path;
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
/// Creates diverging history so test merge/rebase produces different commit IDs
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
    // CI command writes env vars to files so we can verify:
    // - SELFCI_CANDIDATE_COMMIT_ID should be the same as original (what user submitted)
    // - SELFCI_MERGED_COMMIT_ID should be different (what CI tests)
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    let candidate_id_file = repo_path.join(".tested_candidate_id");
    let merged_id_file = repo_path.join(".tested_merged_id");
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            "job:\n  command: 'echo $SELFCI_CANDIDATE_COMMIT_ID > {} && echo $SELFCI_MERGED_COMMIT_ID > {}'\nmq:\n  base-branch: main\n  merge-style: {}\n",
            candidate_id_file.display(),
            merged_id_file.display(),
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

    // Get the feature branch HEAD (original candidate commit)
    let feature_commit = cmd!("git", "rev-parse", "HEAD")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    // Switch back to main and create diverging history
    // This ensures the test merge/rebase will produce a different commit ID
    cmd!("git", "checkout", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Add multiple commits on main AFTER feature branch was created (diverging history)
    // This tests multi-commit merge/rebase on both sides
    fs::write(repo_path.join("main_update1.txt"), "main update 1").unwrap();
    cmd!("git", "add", "main_update1.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Main branch update 1")
        .dir(repo_path)
        .run()
        .unwrap();

    fs::write(repo_path.join("main_update2.txt"), "main update 2").unwrap();
    cmd!("git", "add", "main_update2.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Main branch update 2")
        .dir(repo_path)
        .run()
        .unwrap();

    fs::write(repo_path.join("main_update3.txt"), "main update 3").unwrap();
    cmd!("git", "add", "main_update3.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("git", "commit", "-m", "Main branch update 3")
        .dir(repo_path)
        .run()
        .unwrap();

    // Create working file (simulating user's uncommitted work)
    fs::write(repo_path.join("working.txt"), "working on something else").unwrap();

    // Store feature commit for later use
    fs::write(repo_path.join(".feature_commit"), feature_commit).unwrap();

    repo_dir
}

/// Setup a Jujutsu repository with multi-commit candidate
/// Creates diverging history so test merge/rebase produces different commit IDs
fn setup_jj_mq_repo(merge_style: &str) -> tempfile::TempDir {
    let repo_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let repo_path = repo_dir.path();

    // Initialize jj repo
    cmd!("jj", "git", "init").dir(repo_path).run().unwrap();

    // Create config with merge style
    // CI command writes env vars to files so we can verify:
    // - SELFCI_CANDIDATE_COMMIT_ID should be the same as original (what user submitted)
    // - SELFCI_MERGED_COMMIT_ID should be different (what CI tests)
    fs::create_dir_all(repo_path.join(".config/selfci")).unwrap();
    let candidate_id_file = repo_path.join(".tested_candidate_id");
    let merged_id_file = repo_path.join(".tested_merged_id");
    fs::write(
        repo_path.join(".config/selfci/ci.yaml"),
        format!(
            "job:\n  command: 'echo $SELFCI_CANDIDATE_COMMIT_ID > {} && echo $SELFCI_MERGED_COMMIT_ID > {}'\nmq:\n  base-branch: main\n  merge-style: {}\n",
            candidate_id_file.display(),
            merged_id_file.display(),
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

    // Create feature commits (branching off from main)
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

    // Get the feature commit ID (original candidate)
    let feature_commit = cmd!("jj", "log", "-r", "@", "--no-graph", "-T", "commit_id")
        .dir(repo_path)
        .read()
        .unwrap()
        .trim()
        .to_string();

    // Create diverging history on main (after feature branch was created)
    // This tests multi-commit merge/rebase on both sides
    cmd!("jj", "new", "main").dir(repo_path).run().unwrap();
    fs::write(repo_path.join("main_update1.txt"), "main update 1").unwrap();
    cmd!("jj", "file", "track", "main_update1.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Main branch update 1")
        .dir(repo_path)
        .run()
        .unwrap();

    cmd!("jj", "new").dir(repo_path).run().unwrap();
    fs::write(repo_path.join("main_update2.txt"), "main update 2").unwrap();
    cmd!("jj", "file", "track", "main_update2.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Main branch update 2")
        .dir(repo_path)
        .run()
        .unwrap();

    cmd!("jj", "new").dir(repo_path).run().unwrap();
    fs::write(repo_path.join("main_update3.txt"), "main update 3").unwrap();
    cmd!("jj", "file", "track", "main_update3.txt")
        .dir(repo_path)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "Main branch update 3")
        .dir(repo_path)
        .run()
        .unwrap();

    cmd!("jj", "bookmark", "set", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Switch to working state
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

/// Verify that temporary git worktrees are cleaned up
fn verify_git_worktrees_cleaned_up(repo_path: &Path) {
    // List all worktrees
    let output = cmd!("git", "worktree", "list")
        .dir(repo_path)
        .read()
        .unwrap();

    // Should not contain any selfci-test-worktree entries
    assert!(
        !output.contains("selfci-test-worktree"),
        "Temporary test worktree not cleaned up! Worktrees:\n{}",
        output
    );

    // Also verify the worktree directory doesn't exist in .git
    let git_dir = repo_path.join(".git");
    if git_dir.is_dir() {
        for entry in std::fs::read_dir(&git_dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            assert!(
                !name.to_string_lossy().starts_with("selfci-test-worktree"),
                "Temporary worktree directory not cleaned up: {:?}",
                entry.path()
            );
        }
    }
}

/// Verify that temporary jj commits don't leave orphaned bookmarks
fn verify_jj_no_orphaned_bookmarks(repo_path: &Path) {
    // List all bookmarks
    let output = cmd!("jj", "bookmark", "list", "--all")
        .dir(repo_path)
        .read()
        .unwrap();

    // Should only have expected bookmarks:
    // - main: the base branch
    // - main@git: git remote tracking
    // - @git: special jj bookmark tracking git HEAD
    // No selfci-test-* or other temporary bookmarks
    for line in output.lines() {
        let bookmark = line.split(':').next().unwrap_or("").trim();
        let is_expected = bookmark.is_empty()
            || bookmark == "main"
            || bookmark.starts_with("main@")
            || bookmark == "@git"
            || bookmark.starts_with("@git@");
        assert!(
            is_expected,
            "Unexpected bookmark found (possible orphaned test bookmark): {}",
            bookmark
        );
    }
}

/// Verify that test merge commits have been cleaned up (abandoned)
fn verify_jj_test_commits_cleaned_up(repo_path: &Path) {
    // Get all visible commits (excluding hidden/abandoned ones)
    // The test merge commits should have been abandoned and not appear here
    let output = cmd!(
        "jj",
        "log",
        "--no-graph",
        "-T",
        r#"change_id ++ " " ++ description.first_line() ++ "\n""#
    )
    .dir(repo_path)
    .read()
    .unwrap();

    // Count how many "Feature commit 3" entries we have
    // There should be exactly one (the final merged one)
    // If test merge commits weren't cleaned up, we'd see duplicates
    let feature3_count = output
        .lines()
        .filter(|line| line.contains("Feature commit 3"))
        .count();

    assert_eq!(
        feature3_count, 1,
        "Expected exactly 1 'Feature commit 3' in visible history, found {}.\n\
         This may indicate test merge commits weren't properly abandoned.\n\
         Visible commits:\n{}",
        feature3_count, output
    );

    // Also check for any commits that look like duplicates (same description, different change ID)
    // by verifying we have exactly 3 feature commits total
    let feature_count = output
        .lines()
        .filter(|line| line.contains("Feature commit"))
        .count();

    assert_eq!(
        feature_count, 3,
        "Expected exactly 3 feature commits in visible history, found {}.\n\
         This may indicate test merge commits weren't properly abandoned.\n\
         Visible commits:\n{}",
        feature_count, output
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

/// Verify the env vars passed to CI:
/// - SELFCI_CANDIDATE_COMMIT_ID should match the original (user-submitted) candidate
/// - SELFCI_MERGED_COMMIT_ID should differ (the test merge/rebase result)
fn verify_tested_commit_differs_from_original(repo_path: &Path) {
    // Read original feature commit
    let original_commit = fs::read_to_string(repo_path.join(".feature_commit"))
        .expect("Failed to read .feature_commit")
        .trim()
        .to_string();

    // Read the SELFCI_CANDIDATE_COMMIT_ID that was passed to CI
    let candidate_commit = fs::read_to_string(repo_path.join(".tested_candidate_id"))
        .expect("Failed to read .tested_candidate_id - CI command may not have run")
        .trim()
        .to_string();

    // Read the SELFCI_MERGED_COMMIT_ID that was passed to CI
    let merged_commit = fs::read_to_string(repo_path.join(".tested_merged_id"))
        .expect("Failed to read .tested_merged_id - CI command may not have run")
        .trim()
        .to_string();

    assert!(
        !candidate_commit.is_empty(),
        "SELFCI_CANDIDATE_COMMIT_ID is empty - CI command may not have run correctly"
    );

    assert!(
        !merged_commit.is_empty(),
        "SELFCI_MERGED_COMMIT_ID is empty - CI command may not have run correctly"
    );

    // SELFCI_CANDIDATE_COMMIT_ID should be the SAME as original (what user submitted)
    assert_eq!(
        original_commit, candidate_commit,
        "SELFCI_CANDIDATE_COMMIT_ID should match the original candidate commit!\n\
         Original: {}\n\
         SELFCI_CANDIDATE_COMMIT_ID: {}\n\
         This env var should refer to what the user submitted.",
        original_commit, candidate_commit
    );

    // SELFCI_MERGED_COMMIT_ID should be DIFFERENT from original (test merge/rebase result)
    assert_ne!(
        original_commit, merged_commit,
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

    // Verify the tested commit differs from original (test merge/rebase happened)
    verify_tested_commit_differs_from_original(repo_path);

    // Verify merge succeeded
    verify_merge_succeeded_git(repo_path, "rebase");

    // Verify temporary worktrees are cleaned up
    verify_git_worktrees_cleaned_up(repo_path);

    // Display commit log for visual inspection
    eprintln!("\n=== git rebase merge result ===");
    cmd!("git", "--no-pager", "log", "--oneline", "--graph", "main")
        .dir(repo_path)
        .run()
        .unwrap();
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
    verify_tested_commit_differs_from_original(repo_path);
    verify_merge_succeeded_git(repo_path, "merge");

    // Verify temporary worktrees are cleaned up
    verify_git_worktrees_cleaned_up(repo_path);

    // Display commit log for visual inspection
    eprintln!("\n=== git merge merge result ===");
    cmd!("git", "--no-pager", "log", "--oneline", "--graph", "main")
        .dir(repo_path)
        .run()
        .unwrap();

    // Display full merge commit
    eprintln!("\n=== git merge commit details ===");
    cmd!("git", "--no-pager", "show", "main")
        .dir(repo_path)
        .run()
        .unwrap();
}

#[test]
fn test_jj_rebase_merge() {
    let repo = setup_jj_mq_repo("rebase");
    let repo_path = repo.path();

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    // Start MQ daemon in background (like git tests)
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .env("SELFCI_LOG", "debug")
        .run()
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
    verify_tested_commit_differs_from_original(repo_path);
    verify_merge_succeeded_jj(repo_path, "rebase");

    // Verify no orphaned bookmarks from test merge
    verify_jj_no_orphaned_bookmarks(repo_path);

    // Verify test merge commits were cleaned up (abandoned)
    verify_jj_test_commits_cleaned_up(repo_path);

    // Display commit log for visual inspection
    eprintln!("\n=== jj rebase merge result ===");
    cmd!("jj", "--no-pager", "log", "-r", "ancestors(main, 5)")
        .dir(repo_path)
        .run()
        .unwrap();
}

#[test]
fn test_jj_merge_merge() {
    let repo = setup_jj_mq_repo("merge");
    let repo_path = repo.path();

    let feature_commit = fs::read_to_string(repo_path.join(".feature_commit")).unwrap();

    // Start MQ daemon in background (like git tests)
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .env("SELFCI_LOG", "debug")
        .run()
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
    verify_tested_commit_differs_from_original(repo_path);
    verify_merge_succeeded_jj(repo_path, "merge");

    // Verify no orphaned bookmarks from test merge
    verify_jj_no_orphaned_bookmarks(repo_path);

    // Verify test merge commits were cleaned up (abandoned)
    verify_jj_test_commits_cleaned_up(repo_path);

    // Display commit log for visual inspection
    eprintln!("\n=== jj merge merge result ===");
    cmd!("jj", "--no-pager", "log", "-r", "ancestors(main, 5)")
        .dir(repo_path)
        .run()
        .unwrap();

    // Display full merge commit
    eprintln!("\n=== jj merge commit details ===");
    cmd!("jj", "--no-pager", "show", "main")
        .dir(repo_path)
        .run()
        .unwrap();
}

/// Test that stopping the MQ daemon via command works correctly
#[test]
fn test_mq_stop_via_command() {
    let repo = setup_git_mq_repo("rebase");
    let repo_path = repo.path();

    // Start daemon in background
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .run()
        .unwrap();
    wait_for_daemon_start();

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

    let repo = setup_git_mq_repo("rebase");
    let repo_path = repo.path();

    // Start daemon in background
    cmd!(selfci_bin(), "mq", "start")
        .dir(repo_path)
        .run()
        .unwrap();
    wait_for_daemon_start();

    // Find the daemon PID from the runtime directory
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map(|d| std::path::PathBuf::from(d).join("selfci"))
        .unwrap_or_else(|_| {
            let uid = nix::unistd::getuid();
            std::path::PathBuf::from(format!("/tmp/selfci-{}/selfci", uid))
        });

    // Find the daemon dir for this repo by checking mq.dir contents
    let canonical_repo = repo_path.canonicalize().unwrap();
    let mut daemon_pid: Option<i32> = None;

    for entry in std::fs::read_dir(&runtime_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        let dir_path = entry.path();
        let mq_dir_file = dir_path.join("mq.dir");
        if let Ok(contents) = std::fs::read_to_string(&mq_dir_file) {
            if contents.trim() == canonical_repo.to_string_lossy() {
                let pid_file = dir_path.join("mq.pid");
                if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
                    daemon_pid = pid_str.trim().parse().ok();
                    break;
                }
            }
        }
    }

    let pid = daemon_pid.expect("Could not find daemon PID for this repo");

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
    let repo = setup_git_mq_repo("rebase");
    let repo_path = repo.path();

    // Start daemon in foreground mode (runs as direct child process)
    // Use stdin/stdout/stderr_null to avoid blocking on IO
    let _handle = cmd!(selfci_bin(), "mq", "start", "-f")
        .dir(repo_path)
        .stdin_null()
        .stdout_null()
        .stderr_null()
        .start()
        .unwrap();
    wait_for_daemon_start();

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

    let repo = setup_git_mq_repo("rebase");
    let repo_path = repo.path();

    // Start daemon in foreground mode using std::process::Command
    // Use null IO to avoid any blocking on pipe buffers
    let child = Command::new(selfci_bin())
        .args(["mq", "start", "-f"])
        .current_dir(repo_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn daemon");
    let pid = child.id() as i32;
    wait_for_daemon_start();

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
