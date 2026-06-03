mod common;
use tracing_test::traced_test;

use duct::cmd;
use selfci::{CloneMode, VCS, copy_revisions_to_workdirs, revision};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[test]
#[traced_test]
fn test_copy_revisions_jujutsu() {
    let repo = common::setup_jj_repo();
    let base_rev = common::get_jj_base_rev(repo.path());

    // Resolve revisions to commit IDs
    let resolved_base = revision::resolve_revision(&VCS::Jujutsu, repo.path(), &base_rev)
        .expect("Failed to resolve base revision");
    let resolved_candidate = revision::resolve_revision(&VCS::Jujutsu, repo.path(), "@")
        .expect("Failed to resolve candidate revision");

    // Create work directories
    let base_workdir = TempDir::new().expect("Failed to create base workdir");
    let candidate_workdir = TempDir::new().expect("Failed to create candidate workdir");

    // Copy both revisions
    let result = copy_revisions_to_workdirs(
        &VCS::Jujutsu,
        repo.path(),
        base_workdir.path(),
        &resolved_base.commit_id,
        candidate_workdir.path(),
        &resolved_candidate.commit_id,
        CloneMode::Full,
    );
    assert!(
        result.is_ok(),
        "copy_revisions_to_workdirs failed: {:?}",
        result
    );

    // Verify base workdir has base.txt
    let base_file = base_workdir.path().join("base.txt");
    assert!(base_file.exists(), "base.txt should exist in base workdir");
    let base_content = fs::read_to_string(&base_file).expect("Failed to read base file");
    assert_eq!(base_content, "base content");

    // Verify candidate workdir has both files
    let candidate_base_file = candidate_workdir.path().join("base.txt");
    let candidate_file = candidate_workdir.path().join("candidate.txt");
    assert!(
        candidate_base_file.exists(),
        "base.txt should exist in candidate workdir"
    );
    assert!(
        candidate_file.exists(),
        "candidate.txt should exist in candidate workdir"
    );
    let candidate_content =
        fs::read_to_string(&candidate_file).expect("Failed to read candidate file");
    assert_eq!(candidate_content, "candidate content");
    assert_no_selfci_export_refs(repo.path());
}

#[test]
#[traced_test]
fn test_copy_revisions_git() {
    let repo = common::setup_git_repo();

    // Resolve revisions to commit IDs
    let resolved_base = revision::resolve_revision(&VCS::Git, repo.path(), "HEAD^")
        .expect("Failed to resolve base revision");
    let resolved_candidate = revision::resolve_revision(&VCS::Git, repo.path(), "HEAD")
        .expect("Failed to resolve candidate revision");

    // Create work directories
    let base_workdir = TempDir::new().expect("Failed to create base workdir");
    let candidate_workdir = TempDir::new().expect("Failed to create candidate workdir");

    // Copy both revisions
    let result = copy_revisions_to_workdirs(
        &VCS::Git,
        repo.path(),
        base_workdir.path(),
        &resolved_base.commit_id,
        candidate_workdir.path(),
        &resolved_candidate.commit_id,
        CloneMode::Full,
    );
    assert!(
        result.is_ok(),
        "copy_revisions_to_workdirs failed: {:?}",
        result
    );

    // Verify base workdir has base.txt
    let base_file = base_workdir.path().join("base.txt");
    assert!(base_file.exists(), "base.txt should exist in base workdir");
    let base_content = fs::read_to_string(&base_file).expect("Failed to read base file");
    assert_eq!(base_content, "base content");

    // Verify candidate workdir has both files
    let candidate_base_file = candidate_workdir.path().join("base.txt");
    let candidate_file = candidate_workdir.path().join("candidate.txt");
    assert!(
        candidate_base_file.exists(),
        "base.txt should exist in candidate workdir"
    );
    assert!(
        candidate_file.exists(),
        "candidate.txt should exist in candidate workdir"
    );
    let candidate_content =
        fs::read_to_string(&candidate_file).expect("Failed to read candidate file");
    assert_eq!(candidate_content, "candidate content");
}

/// Regression test: verify that temporary jj bookmarks are cleaned up even when
/// git clone fails during copy_revisions_to_workdirs.
///
/// Previously, bookmark cleanup was only on the success path. If git clone failed
/// after bookmarks were created, they were left behind as stale refs.
#[test]
#[traced_test]
fn test_jj_bookmark_cleanup_on_clone_failure() {
    let repo = common::setup_jj_repo();
    let repo_path = repo.path();
    let selfci_bin = env!("CARGO_BIN_EXE_selfci");

    // Create a fake git wrapper that fails on clone but passes everything else
    // through. This simulates the error path in copy_revisions_to_workdirs after
    // bookmarks have been created and exported.
    let wrapper_dir = TempDir::new().expect("Failed to create wrapper dir");
    let wrapper_dir_str = wrapper_dir.path().display().to_string();
    // The wrapper removes its own directory from PATH before delegating to the
    // real git, so we don't need to know the absolute path of git ahead of time.
    let wrapper_script = format!(
        "#!/bin/sh\n\
         for arg in \"$@\"; do\n\
         \x20 if [ \"$arg\" = \"clone\" ]; then\n\
         \x20   echo \"fake git: simulating clone failure\" >&2\n\
         \x20   exit 1\n\
         \x20 fi\n\
         done\n\
         # Remove wrapper dir from PATH so we find the real git\n\
         PATH=$(echo \"$PATH\" | tr ':' '\\n' | grep -v '{}' | tr '\\n' ':')\n\
         exec git \"$@\"\n",
        wrapper_dir_str.replace('/', "\\/")
    );
    let wrapper_path = wrapper_dir.path().join("git");
    fs::write(&wrapper_path, &wrapper_script).expect("Failed to write git wrapper");
    fs::set_permissions(&wrapper_path, fs::Permissions::from_mode(0o755))
        .expect("Failed to set wrapper permissions");

    // Build PATH with wrapper dir prepended
    let original_path = std::env::var("PATH").unwrap_or_default();
    let modified_path = format!("{}:{}", wrapper_dir_str, original_path);

    // Run selfci check with the broken git -- this should fail
    let result = cmd!(
        selfci_bin,
        "check",
        "--root",
        repo_path,
        "--base",
        "@-",
        "--candidate",
        "@"
    )
    .env("PATH", &modified_path)
    .stderr_to_stdout()
    .unchecked()
    .run()
    .expect("Failed to run selfci");

    assert!(
        !result.status.success(),
        "selfci check should fail with broken git clone"
    );

    assert_no_selfci_export_refs(repo_path);
}

fn assert_no_selfci_export_refs(repo_path: &std::path::Path) {
    // Verify no selfci-export-* bookmarks remain in jj.
    let bookmarks = cmd!("jj", "bookmark", "list")
        .dir(repo_path)
        .read()
        .expect("Failed to list bookmarks");

    let stale_bookmarks: Vec<&str> = bookmarks
        .lines()
        .filter(|line| line.contains("selfci-export-"))
        .collect();

    assert!(
        stale_bookmarks.is_empty(),
        "No selfci-export-* bookmarks should remain, but found:\n{}",
        stale_bookmarks.join("\n")
    );

    // The temporary jj bookmarks are exported to git refs before cloning. Verify
    // those refs are gone too, so a later jj import cannot resurrect them.
    let git_refs = cmd!("git", "show-ref")
        .dir(repo_path)
        .stderr_null()
        .unchecked()
        .read()
        .expect("Failed to list git refs");

    let stale_git_refs: Vec<&str> = git_refs
        .lines()
        .filter(|line| line.contains("refs/heads/selfci-export-"))
        .collect();

    assert!(
        stale_git_refs.is_empty(),
        "No selfci-export-* git refs should remain, but found:\n{}",
        stale_git_refs.join("\n")
    );
}
