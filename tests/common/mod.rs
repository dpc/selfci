use duct::cmd;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Create a Jujutsu repository with initial commits
pub fn setup_jj_repo() -> TempDir {
    let repo_dir = TempDir::new().expect("Failed to create temp dir");
    let repo_path = repo_dir.path();

    // Initialize a jj repo
    cmd!("jj", "git", "init")
        .dir(repo_path)
        .run()
        .expect("Failed to run jj git init");

    // Create config file
    fs::create_dir_all(repo_path.join(".config").join("selfci"))
        .expect("Failed to create config dir");
    fs::write(
        repo_path.join(".config").join("selfci").join("config.yml"),
        "job:\n  command: echo test\n",
    )
    .expect("Failed to write config");

    // Create base revision
    fs::write(repo_path.join("base.txt"), "base content")
        .expect("Failed to write base file");

    cmd!("jj", "file", "track", "base.txt")
        .dir(repo_path)
        .run()
        .expect("Failed to track base file");

    cmd!("jj", "file", "track", ".config/selfci/config.yml")
        .dir(repo_path)
        .run()
        .expect("Failed to track config");

    cmd!("jj", "describe", "-m", "Base commit")
        .dir(repo_path)
        .run()
        .expect("Failed to describe base");

    // Create a new commit for candidate
    cmd!("jj", "new")
        .dir(repo_path)
        .run()
        .expect("Failed to create new commit");

    // Create candidate revision
    fs::write(repo_path.join("candidate.txt"), "candidate content")
        .expect("Failed to write candidate file");

    cmd!("jj", "file", "track", "candidate.txt")
        .dir(repo_path)
        .run()
        .expect("Failed to track candidate file");

    cmd!("jj", "describe", "-m", "Candidate commit")
        .dir(repo_path)
        .run()
        .expect("Failed to describe candidate");

    repo_dir
}

/// Create a Git repository with initial commits
pub fn setup_git_repo() -> TempDir {
    let repo_dir = TempDir::new().expect("Failed to create temp dir");
    let repo_path = repo_dir.path();

    // Initialize a git repo
    cmd!("git", "init")
        .dir(repo_path)
        .run()
        .expect("Failed to run git init");

    // Configure git for commits
    cmd!("git", "config", "user.name", "Test User")
        .dir(repo_path)
        .run()
        .expect("Failed to set git user.name");

    cmd!("git", "config", "user.email", "test@example.com")
        .dir(repo_path)
        .run()
        .expect("Failed to set git user.email");

    // Create config file
    fs::create_dir_all(repo_path.join(".config").join("selfci"))
        .expect("Failed to create config dir");
    fs::write(
        repo_path.join(".config").join("selfci").join("config.yml"),
        "job:\n  command: echo test\n",
    )
    .expect("Failed to write config");

    // Create base revision
    fs::write(repo_path.join("base.txt"), "base content")
        .expect("Failed to write base file");

    cmd!("git", "add", ".")
        .dir(repo_path)
        .run()
        .expect("Failed to git add");

    cmd!("git", "commit", "-m", "Base commit")
        .dir(repo_path)
        .run()
        .expect("Failed to git commit");

    // Create candidate revision
    fs::write(repo_path.join("candidate.txt"), "candidate content")
        .expect("Failed to write candidate file");

    cmd!("git", "add", "candidate.txt")
        .dir(repo_path)
        .run()
        .expect("Failed to git add candidate");

    cmd!("git", "commit", "-m", "Candidate commit")
        .dir(repo_path)
        .run()
        .expect("Failed to git commit candidate");

    repo_dir
}

/// Get the base revision ID for a Jujutsu repo (the parent of @)
#[allow(dead_code)]
pub fn get_jj_base_rev(repo_path: &Path) -> String {
    cmd!("jj", "log", "-r", "@-", "--no-graph", "-T", "change_id")
        .dir(repo_path)
        .read()
        .expect("Failed to get base revision")
        .trim()
        .to_string()
}
