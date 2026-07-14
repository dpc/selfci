use super::ensure_private_runtime_directory;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};

#[test]
fn creates_private_runtime_directory() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("runtime");
    ensure_private_runtime_directory(&path, nix::unistd::getuid().as_raw()).unwrap();

    assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o700);
}

#[test]
fn rejects_symlink_runtime_directory() {
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("target");
    let path = parent.path().join("runtime");
    std::fs::create_dir(&target).unwrap();
    symlink(target, &path).unwrap();

    assert!(ensure_private_runtime_directory(&path, nix::unistd::getuid().as_raw()).is_err());
}

#[test]
fn rejects_insecure_runtime_directory_permissions() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("runtime");
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(ensure_private_runtime_directory(&path, nix::unistd::getuid().as_raw()).is_err());
}

#[test]
fn rejects_foreign_runtime_directory_owner() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("runtime");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    let foreign_uid = std::fs::metadata(&path).unwrap().uid().wrapping_add(1);

    assert!(ensure_private_runtime_directory(&path, foreign_uid).is_err());
}
