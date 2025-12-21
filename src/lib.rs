pub mod config;
pub mod step;

use duct::cmd;
use error_set::error_set;
use std::path::Path;

pub use config::{init_config, read_config, SelfCIConfig};

error_set! {
    VCSError := {
        #[display("No supported VCS found (looking for .jj directory)")]
        NoVCSFound,
    }

    WorkDirError := {
        #[display("Failed to create work directory")]
        CreateFailed(std::io::Error),
    }

    VCSOperationError := {
        #[display("Failed to execute VCS command")]
        CommandFailed(std::io::Error),
    }

    ConfigError := {
        #[display("Not initialized: .config/selfci/config.yml not found")]
        NotInitialized,
        #[display("Failed to read config file")]
        ReadFailed(std::io::Error),
        #[display("Failed to parse config file")]
        ParseFailed(serde_yaml::Error),
    }

    CheckError := {
        #[display("Check command failed")]
        CheckFailed,
    }

    MainError := VCSError || WorkDirError || VCSOperationError || ConfigError || CheckError
}

impl MainError {
    pub fn exit_code(&self) -> i32 {
        match self {
            MainError::NoVCSFound => 1,
            MainError::CreateFailed(_) => 2,
            MainError::CommandFailed(_) => 3,
            MainError::NotInitialized => 4,
            MainError::ReadFailed(_) => 5,
            MainError::ParseFailed(_) => 6,
            MainError::CheckFailed => 7,
        }
    }
}

#[derive(Debug)]
pub enum VCS {
    Jujutsu,
}

pub fn detect_vcs(root: &Path) -> Result<VCS, VCSError> {
    let jj_path = root.join(".jj");
    if jj_path.is_dir() {
        return Ok(VCS::Jujutsu);
    }

    Err(VCSError::NoVCSFound)
}

pub fn copy_revisions_to_workdirs(
    vcs: &VCS,
    root_dir: &Path,
    base_workdir: &Path,
    base_revision: &str,
    candidate_workdir: &Path,
    candidate_revision: &str,
) -> Result<(), VCSOperationError> {
    match vcs {
        VCS::Jujutsu => {
            // Export jj changes to the underlying git repo once
            cmd!("jj", "git", "export", "--quiet")
                .dir(root_dir)
                .run()
                .map_err(VCSOperationError::CommandFailed)?;

            // Get the git directory path from jj
            let git_dir = cmd!("jj", "git", "root")
                .dir(root_dir)
                .read()
                .map_err(VCSOperationError::CommandFailed)?;
            let git_dir = git_dir.trim();

            // Copy base revision
            copy_revision_to_workdir(root_dir, base_workdir, base_revision, git_dir)?;

            // Copy candidate revision
            copy_revision_to_workdir(root_dir, candidate_workdir, candidate_revision, git_dir)?;

            Ok(())
        }
    }
}

fn copy_revision_to_workdir(
    root_dir: &Path,
    workdir: &Path,
    revision: &str,
    git_dir: &str,
) -> Result<(), VCSOperationError> {
    // Generate a random temporary bookmark name
    let random_suffix: u64 = rand::random();
    let bookmark_name = format!("selfci-{:x}", random_suffix);

    // Set temporary bookmark at the revision
    cmd!("jj", "bookmark", "set", "--quiet", "-r", revision, &bookmark_name)
        .dir(root_dir)
        .run()
        .map_err(VCSOperationError::CommandFailed)?;

    // Ensure cleanup happens even on panic or early return
    let root_dir_clone = root_dir.to_path_buf();
    let bookmark_name_clone = bookmark_name.clone();
    let _guard = scopeguard::guard((), move |_| {
        let _ = cmd!("jj", "bookmark", "forget", "--quiet", &bookmark_name_clone)
            .dir(&root_dir_clone)
            .run();
    });

    // Use git archive to export the revision and pipe to tar for extraction
    cmd!("git", "archive", "--format=tar", &bookmark_name)
        .env("GIT_DIR", git_dir)
        .pipe(cmd!("tar", "x").dir(workdir))
        .run()
        .map_err(VCSOperationError::CommandFailed)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_copy_revisions_to_workdirs_jujutsu() {
        // Create a temporary directory for the jj repo
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
            "jobs:\n  test:\n    command: echo test\n",
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

        // Store the base revision
        let base_rev = cmd!("jj", "log", "-r", "@", "--no-graph", "-T", "change_id")
            .dir(repo_path)
            .read()
            .expect("Failed to get base revision");
        let base_rev = base_rev.trim();

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

        // Create work directories
        let base_workdir = TempDir::new().expect("Failed to create base workdir");
        let candidate_workdir = TempDir::new().expect("Failed to create candidate workdir");

        // Copy both revisions
        let result = copy_revisions_to_workdirs(
            &VCS::Jujutsu,
            repo_path,
            base_workdir.path(),
            base_rev,
            candidate_workdir.path(),
            "@",
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
}
