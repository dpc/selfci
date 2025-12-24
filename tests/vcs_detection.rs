mod common;

use selfci::{VCS, detect_vcs, get_vcs, parse_vcs};

#[test]
fn test_detect_jujutsu() {
    let repo = common::setup_jj_repo();
    let vcs = detect_vcs(repo.path()).expect("Should detect Jujutsu");
    assert!(matches!(vcs, VCS::Jujutsu));
}

#[test]
fn test_detect_git() {
    let repo = common::setup_git_repo();
    let vcs = detect_vcs(repo.path()).expect("Should detect Git");
    assert!(matches!(vcs, VCS::Git));
}

#[test]
fn test_detect_no_vcs() {
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let result = detect_vcs(temp_dir.path());
    assert!(result.is_err(), "Should fail when no VCS found");
}

#[test]
fn test_parse_vcs_jj() {
    assert!(matches!(parse_vcs("jj").unwrap(), VCS::Jujutsu));
    assert!(matches!(parse_vcs("JJ").unwrap(), VCS::Jujutsu));
    assert!(matches!(parse_vcs("jujutsu").unwrap(), VCS::Jujutsu));
    assert!(matches!(parse_vcs("Jujutsu").unwrap(), VCS::Jujutsu));
}

#[test]
fn test_parse_vcs_git() {
    assert!(matches!(parse_vcs("git").unwrap(), VCS::Git));
    assert!(matches!(parse_vcs("GIT").unwrap(), VCS::Git));
    assert!(matches!(parse_vcs("Git").unwrap(), VCS::Git));
}

#[test]
fn test_parse_vcs_invalid() {
    assert!(parse_vcs("svn").is_err());
    assert!(parse_vcs("mercurial").is_err());
    assert!(parse_vcs("").is_err());
}

#[test]
fn test_get_vcs_forced_jj() {
    let repo = common::setup_git_repo();
    // Force Jujutsu even though it's a Git repo
    let vcs = get_vcs(repo.path(), Some("jj")).expect("Should use forced VCS");
    assert!(matches!(vcs, VCS::Jujutsu));
}

#[test]
fn test_get_vcs_forced_git() {
    let repo = common::setup_jj_repo();
    // Force Git even though it's a Jujutsu repo
    let vcs = get_vcs(repo.path(), Some("git")).expect("Should use forced VCS");
    assert!(matches!(vcs, VCS::Git));
}

#[test]
fn test_get_vcs_auto_detect() {
    let repo = common::setup_git_repo();
    let vcs = get_vcs(repo.path(), None).expect("Should auto-detect VCS");
    assert!(matches!(vcs, VCS::Git));
}

#[test]
fn test_get_vcs_forced_invalid() {
    let repo = common::setup_git_repo();
    let result = get_vcs(repo.path(), Some("svn"));
    assert!(result.is_err(), "Should fail with invalid VCS type");
}
