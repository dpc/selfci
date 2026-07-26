use super::{
    FORCE_JJ_POST_MOVE_VERIFY_FAILURE, PublicationOutcome, create_test_merge, daemonize_foreground,
    ensure_private_runtime_directory, publish_prepared_candidate,
};
use duct::cmd;
use selfci::config::MergeMode;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};

static JJ_CONFIG_LOCK: Mutex<()> = Mutex::new(());

/// Isolate Jujutsu's secure configuration in test environments without a HOME.
struct JjTestConfig {
    _lock: MutexGuard<'static, ()>,
    old_config: Option<std::ffi::OsString>,
    directory: tempfile::TempDir,
}

impl JjTestConfig {
    fn new() -> Self {
        let lock = JJ_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        std::fs::write(
            &config,
            "user.name = \"SelfCI Test\"\nuser.email = \"selfci@example.invalid\"\n",
        )
        .unwrap();
        let old_config = std::env::var_os("JJ_CONFIG");
        // SAFETY: Jujutsu-using tests in this binary hold JJ_CONFIG_LOCK for
        // their full lifetime. Other unit tests in this binary do not read it.
        unsafe { std::env::set_var("JJ_CONFIG", config) };
        Self {
            _lock: lock,
            old_config,
            directory,
        }
    }

    fn set_quiet(&self) {
        std::fs::write(
            self.directory.path().join("config.toml"),
            "user.name = \"SelfCI Test\"\n\
             user.email = \"selfci@example.invalid\"\n\
             ui.quiet = true\n",
        )
        .unwrap();
    }
}

impl Drop for JjTestConfig {
    fn drop(&mut self) {
        // SAFETY: This guard still owns JJ_CONFIG_LOCK, so no Jujutsu-using
        // unit test in this binary can observe the restoration concurrently.
        unsafe {
            if let Some(old_config) = self.old_config.take() {
                std::env::set_var("JJ_CONFIG", old_config);
            } else {
                std::env::remove_var("JJ_CONFIG");
            }
        }
    }
}

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

    let error = ensure_private_runtime_directory(&path, nix::unistd::getuid().as_raw())
        .expect_err("insecure runtime directory should be rejected");
    let message = error.to_string();
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(message.contains("mode 0700"), "{message}");
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

#[test]
fn foreground_daemon_reports_failed_runtime_path_and_reason() {
    let parent = tempfile::tempdir().unwrap();
    let blocking_file = parent.path().join("file");
    std::fs::write(&blocking_file, b"not a directory").unwrap();
    let runtime_dir = blocking_file.join("runtime");

    let error = match daemonize_foreground(parent.path(), Some(runtime_dir.clone()), "main") {
        Ok(_) => panic!("runtime below a file should fail"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.contains(&runtime_dir.display().to_string()),
        "{message}"
    );
    assert!(
        message.contains(&std::io::Error::from_raw_os_error(nix::libc::ENOTDIR).to_string()),
        "{message}"
    );
}

fn git_landing_race(merge_mode: MergeMode) {
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    cmd!("git", "init", "--quiet").dir(root).run().unwrap();
    cmd!("git", "config", "user.name", "SelfCI Test")
        .dir(root)
        .run()
        .unwrap();
    cmd!("git", "config", "user.email", "selfci@example.invalid")
        .dir(root)
        .run()
        .unwrap();
    std::fs::write(root.join("base"), "base").unwrap();
    cmd!("git", "add", ".").dir(root).run().unwrap();
    cmd!("git", "commit", "--quiet", "-m", "base")
        .dir(root)
        .run()
        .unwrap();
    cmd!("git", "branch", "-M", "main").dir(root).run().unwrap();
    let base = selfci::revision::resolve_revision(&selfci::VCS::Git, root, "main").unwrap();

    cmd!("git", "switch", "--quiet", "-c", "candidate")
        .dir(root)
        .run()
        .unwrap();
    std::fs::write(root.join("candidate"), "candidate").unwrap();
    cmd!("git", "add", ".").dir(root).run().unwrap();
    cmd!("git", "commit", "--quiet", "-m", "candidate")
        .dir(root)
        .run()
        .unwrap();
    let candidate =
        selfci::revision::resolve_revision(&selfci::VCS::Git, root, "candidate").unwrap();
    let prepared = create_test_merge(root, base.commit_id.as_str(), &candidate, &merge_mode)
        .expect("prepare exact Git integration");
    let prepared = selfci::revision::ResolvedRevision {
        user: candidate.user.clone(),
        change_id: prepared.change_id,
        commit_id: prepared.commit_id,
    };

    cmd!("git", "switch", "--quiet", "main")
        .dir(root)
        .run()
        .unwrap();
    std::fs::write(root.join("external"), "external").unwrap();
    cmd!("git", "add", ".").dir(root).run().unwrap();
    cmd!("git", "commit", "--quiet", "-m", "external")
        .dir(root)
        .run()
        .unwrap();
    let external = cmd!("git", "rev-parse", "main").dir(root).read().unwrap();

    assert!(
        publish_prepared_candidate(root, "main", &base.commit_id, &prepared).is_err(),
        "publication must fail when the checked Git base moved"
    );
    assert_eq!(
        cmd!("git", "rev-parse", "main").dir(root).read().unwrap(),
        external,
        "Git CAS failure must preserve the external update"
    );
}

#[test]
fn git_merge_and_rebase_publication_reject_moved_base() {
    git_landing_race(MergeMode::Merge);
    git_landing_race(MergeMode::Rebase);
}

fn jj_landing_race(merge_mode: MergeMode) {
    let config = JjTestConfig::new();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    cmd!("jj", "git", "init", "--quiet")
        .dir(root)
        .run()
        .unwrap();
    std::fs::write(root.join("base"), "base").unwrap();
    cmd!("jj", "file", "track", "base").dir(root).run().unwrap();
    cmd!("jj", "describe", "-m", "base")
        .dir(root)
        .run()
        .unwrap();
    cmd!("jj", "bookmark", "create", "main")
        .dir(root)
        .run()
        .unwrap();
    let base = selfci::revision::resolve_revision(&selfci::VCS::Jujutsu, root, "main").unwrap();

    cmd!("jj", "new", "main").dir(root).run().unwrap();
    std::fs::write(root.join("candidate"), "candidate").unwrap();
    cmd!("jj", "file", "track", "candidate")
        .dir(root)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "candidate")
        .dir(root)
        .run()
        .unwrap();
    let candidate = selfci::revision::resolve_revision(&selfci::VCS::Jujutsu, root, "@").unwrap();
    let mut prepared = create_test_merge(root, base.commit_id.as_str(), &candidate, &merge_mode)
        .expect("prepare exact Jujutsu integration");
    let mut prepared_revision = selfci::revision::ResolvedRevision {
        user: candidate.user.clone(),
        change_id: prepared.change_id.clone(),
        commit_id: prepared.commit_id.clone(),
    };

    if merge_mode == MergeMode::Merge {
        FORCE_JJ_POST_MOVE_VERIFY_FAILURE.store(true, Ordering::SeqCst);
        assert!(matches!(
            publish_prepared_candidate(root, "main", &base.commit_id, &prepared_revision,).unwrap(),
            PublicationOutcome::AppliedUnverified { .. }
        ));
        let published =
            selfci::revision::resolve_revision(&selfci::VCS::Jujutsu, root, "main").unwrap();
        assert_eq!(
            published.commit_id, prepared_revision.commit_id,
            "post-move failure must not abandon or unpublish the prepared commit"
        );

        prepared = create_test_merge(root, base.commit_id.as_str(), &candidate, &merge_mode)
            .expect("prepare exact integration for failed-CAS cleanup");
        prepared_revision = selfci::revision::ResolvedRevision {
            user: candidate.user.clone(),
            change_id: prepared.change_id.clone(),
            commit_id: prepared.commit_id.clone(),
        };
        cmd!(
            "jj",
            "bookmark",
            "set",
            "--allow-backwards",
            "main",
            "-r",
            prepared_revision.commit_id.as_str()
        )
        .dir(root)
        .run()
        .unwrap();
    }

    cmd!("jj", "new", "main").dir(root).run().unwrap();
    std::fs::write(root.join("external"), "external").unwrap();
    cmd!("jj", "file", "track", "external")
        .dir(root)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "external")
        .dir(root)
        .run()
        .unwrap();
    cmd!("jj", "bookmark", "set", "main", "-r", "@")
        .dir(root)
        .run()
        .unwrap();
    let external = selfci::revision::resolve_revision(&selfci::VCS::Jujutsu, root, "main").unwrap();

    config.set_quiet();
    assert!(
        publish_prepared_candidate(root, "main", &base.commit_id, &prepared_revision,).is_err(),
        "publication must fail when the checked Jujutsu base moved"
    );
    let actual = selfci::revision::resolve_revision(&selfci::VCS::Jujutsu, root, "main").unwrap();
    assert_eq!(
        actual.commit_id, external.commit_id,
        "Jujutsu CAS failure must preserve the external update"
    );
}

#[test]
fn jj_merge_and_rebase_publication_reject_moved_base() {
    jj_landing_race(MergeMode::Merge);
    jj_landing_race(MergeMode::Rebase);
}

#[test]
fn jj_rebase_preparation_uses_exact_submitted_commit() {
    let _config = JjTestConfig::new();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    cmd!("jj", "git", "init", "--quiet")
        .dir(root)
        .run()
        .unwrap();
    std::fs::write(root.join("base"), "base").unwrap();
    cmd!("jj", "file", "track", "base").dir(root).run().unwrap();
    cmd!("jj", "describe", "-m", "base")
        .dir(root)
        .run()
        .unwrap();
    cmd!("jj", "bookmark", "create", "main")
        .dir(root)
        .run()
        .unwrap();
    let base = selfci::revision::resolve_revision(&selfci::VCS::Jujutsu, root, "main").unwrap();
    cmd!("jj", "new", "main").dir(root).run().unwrap();
    std::fs::write(root.join("candidate"), "submitted").unwrap();
    cmd!("jj", "file", "track", "candidate")
        .dir(root)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "candidate")
        .dir(root)
        .run()
        .unwrap();
    let submitted = selfci::revision::resolve_revision(&selfci::VCS::Jujutsu, root, "@").unwrap();

    cmd!(
        "jj",
        "bookmark",
        "set",
        "--allow-backwards",
        "main",
        "-r",
        submitted.commit_id.as_str()
    )
    .dir(root)
    .run()
    .unwrap();
    cmd!("jj", "new", "main").dir(root).run().unwrap();
    std::fs::write(root.join("new-base"), "new base").unwrap();
    cmd!("jj", "file", "track", "new-base")
        .dir(root)
        .run()
        .unwrap();
    cmd!("jj", "describe", "-m", "new base")
        .dir(root)
        .run()
        .unwrap();
    cmd!("jj", "bookmark", "set", "main", "-r", "@")
        .dir(root)
        .run()
        .unwrap();
    let new_base = selfci::revision::resolve_revision(&selfci::VCS::Jujutsu, root, "main").unwrap();
    let noop = create_test_merge(
        root,
        new_base.commit_id.as_str(),
        &submitted,
        &MergeMode::Rebase,
    )
    .expect("strict ancestor should prepare the exact checked base");
    assert_eq!(noop.commit_id, new_base.commit_id);
    let noop_revision = selfci::revision::ResolvedRevision {
        user: submitted.user.clone(),
        change_id: noop.change_id,
        commit_id: noop.commit_id,
    };
    cmd!("jj", "new", "main").dir(root).run().unwrap();
    cmd!("jj", "describe", "-m", "external base movement")
        .dir(root)
        .run()
        .unwrap();
    cmd!("jj", "bookmark", "set", "main", "-r", "@")
        .dir(root)
        .run()
        .unwrap();
    assert!(
        publish_prepared_candidate(root, "main", &new_base.commit_id, &noop_revision,).is_err(),
        "a moved strict-ancestor base is an ordinary not-applied CAS failure"
    );

    cmd!("jj", "edit", submitted.commit_id.as_str())
        .dir(root)
        .run()
        .unwrap();
    std::fs::write(root.join("candidate"), "superseding rewrite").unwrap();
    cmd!("jj", "describe", "-m", "rewritten candidate")
        .dir(root)
        .run()
        .unwrap();
    let prepared = create_test_merge(
        root,
        base.commit_id.as_str(),
        &submitted,
        &MergeMode::Rebase,
    )
    .expect("old exact commit remains a valid submitted candidate");

    assert_eq!(
        cmd!(
            "jj",
            "file",
            "show",
            "-r",
            prepared.commit_id.as_str(),
            "candidate"
        )
        .dir(root)
        .read()
        .unwrap(),
        "submitted",
        "a mutable change-ID rewrite must not supersede the queued commit"
    );
}
