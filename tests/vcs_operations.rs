mod common;

use selfci::{copy_revisions_to_workdirs, VCS, revision};
use std::fs;
use tempfile::TempDir;

#[test]
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
    );
    assert!(result.is_ok(), "copy_revisions_to_workdirs failed: {:?}", result);

    // Verify base workdir has base.txt
    let base_file = base_workdir.path().join("base.txt");
    assert!(base_file.exists(), "base.txt should exist in base workdir");
    let base_content = fs::read_to_string(&base_file).expect("Failed to read base file");
    assert_eq!(base_content, "base content");

    // Verify candidate workdir has both files
    let candidate_base_file = candidate_workdir.path().join("base.txt");
    let candidate_file = candidate_workdir.path().join("candidate.txt");
    assert!(candidate_base_file.exists(), "base.txt should exist in candidate workdir");
    assert!(candidate_file.exists(), "candidate.txt should exist in candidate workdir");
    let candidate_content = fs::read_to_string(&candidate_file).expect("Failed to read candidate file");
    assert_eq!(candidate_content, "candidate content");
}

#[test]
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
    );
    assert!(result.is_ok(), "copy_revisions_to_workdirs failed: {:?}", result);

    // Verify base workdir has base.txt
    let base_file = base_workdir.path().join("base.txt");
    assert!(base_file.exists(), "base.txt should exist in base workdir");
    let base_content = fs::read_to_string(&base_file).expect("Failed to read base file");
    assert_eq!(base_content, "base content");

    // Verify candidate workdir has both files
    let candidate_base_file = candidate_workdir.path().join("base.txt");
    let candidate_file = candidate_workdir.path().join("candidate.txt");
    assert!(candidate_base_file.exists(), "base.txt should exist in candidate workdir");
    assert!(candidate_file.exists(), "candidate.txt should exist in candidate workdir");
    let candidate_content = fs::read_to_string(&candidate_file).expect("Failed to read candidate file");
    assert_eq!(candidate_content, "candidate content");
}
