use crate::VCS;
use duct::cmd;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

/// User-provided revision string (e.g., "@", "main", "HEAD", "abc123")
/// This is what the user types on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserRevision(String);

impl UserRevision {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UserRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Jujutsu change ID or Git commit SHA.
/// For Git, this equals the CommitId.
/// For Jujutsu, this is the change ID that can be rebased.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChangeId(String);

impl ChangeId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Immutable Git commit SHA (40 hex characters).
/// This is what we use for all VCS operations to ensure immutability.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommitId(String);

impl CommitId {
    pub fn new(s: impl Into<String>) -> Result<Self, RevisionError> {
        let s = s.into();
        if s.len() != 40 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(RevisionError::InvalidCommitId(s));
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Resolved revision triple containing all three forms.
/// This is passed throughout the system after early resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRevision {
    /// What the user typed (for display purposes)
    pub user: UserRevision,
    /// Change ID (for Jujutsu operations, equals commit_id for Git)
    pub change_id: ChangeId,
    /// Immutable commit ID (for VCS operations)
    pub commit_id: CommitId,
}

#[derive(Debug)]
pub enum RevisionError {
    ResolutionFailed {
        vcs: String,
        revision: String,
        source: std::io::Error,
    },
    InvalidCommitId(String),
    InvalidOutput {
        vcs: String,
        output: String,
    },
}

impl fmt::Display for RevisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RevisionError::ResolutionFailed {
                vcs,
                revision,
                source,
            } => {
                write!(
                    f,
                    "Failed to resolve revision '{}' using {}: {}",
                    revision, vcs, source
                )
            }
            RevisionError::InvalidCommitId(s) => {
                write!(f, "Invalid commit ID: {} (expected 40-char hex string)", s)
            }
            RevisionError::InvalidOutput { vcs, output } => {
                write!(f, "Unexpected VCS output for {}: {}", vcs, output)
            }
        }
    }
}

impl std::error::Error for RevisionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RevisionError::ResolutionFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Resolves a user-provided revision string to a ResolvedRevision triple.
///
/// This function:
/// 1. Takes the user's input string (e.g., "@", "main", "abc123")
/// 2. Calls the appropriate VCS to resolve it to immutable IDs
/// 3. Returns a triple of (UserRevision, ChangeId, CommitId)
///
/// For Git:
///   - ChangeId = CommitId (Git doesn't have separate change IDs)
///   - Uses `git rev-parse` to resolve ref to commit SHA
///
/// For Jujutsu:
///   - Resolves both change ID and commit ID
///   - Uses `jj log` with template to extract both IDs
pub fn resolve_revision(
    vcs: &VCS,
    root_dir: &Path,
    user_revision: &str,
) -> Result<ResolvedRevision, RevisionError> {
    let user = UserRevision::new(user_revision);

    match vcs {
        VCS::Git => resolve_git_revision(root_dir, user),
        VCS::Jujutsu => resolve_jujutsu_revision(root_dir, user),
    }
}

fn resolve_git_revision(
    root_dir: &Path,
    user: UserRevision,
) -> Result<ResolvedRevision, RevisionError> {
    // Use `git rev-parse` to resolve the ref to a full commit SHA
    let output = cmd!("git", "rev-parse", user.as_str())
        .dir(root_dir)
        .read()
        .map_err(|e| RevisionError::ResolutionFailed {
            vcs: "git".to_string(),
            revision: user.as_str().to_string(),
            source: e,
        })?;

    let commit_sha = output.trim().to_string();

    // Validate it's a full 40-char SHA
    let commit_id = CommitId::new(commit_sha.clone())
        .map_err(|_| RevisionError::InvalidCommitId(commit_sha.clone()))?;

    // For Git, change_id = commit_id
    let change_id = ChangeId::new(commit_sha);

    Ok(ResolvedRevision {
        user,
        change_id,
        commit_id,
    })
}

fn resolve_jujutsu_revision(
    root_dir: &Path,
    user: UserRevision,
) -> Result<ResolvedRevision, RevisionError> {
    // First export to git to ensure commit IDs are available
    cmd!("jj", "git", "export", "--quiet")
        .dir(root_dir)
        .run()
        .map_err(|e| RevisionError::ResolutionFailed {
            vcs: "jujutsu".to_string(),
            revision: user.as_str().to_string(),
            source: e,
        })?;

    // Use jj log with template to get both change ID and commit ID
    // Template: change_id + newline + commit_id
    let output = cmd!(
        "jj",
        "log",
        "--no-graph",
        "-r",
        user.as_str(),
        "-T",
        r#"change_id ++ "\n" ++ commit_id"#
    )
    .dir(root_dir)
    .read()
    .map_err(|e| RevisionError::ResolutionFailed {
        vcs: "jujutsu".to_string(),
        revision: user.as_str().to_string(),
        source: e,
    })?;

    let mut lines = output.lines();
    let change_id_str = lines
        .next()
        .ok_or_else(|| RevisionError::InvalidOutput {
            vcs: "jujutsu".to_string(),
            output: output.clone(),
        })?
        .trim()
        .to_string();

    let commit_id_str = lines
        .next()
        .ok_or_else(|| RevisionError::InvalidOutput {
            vcs: "jujutsu".to_string(),
            output: output.clone(),
        })?
        .trim()
        .to_string();

    let change_id = ChangeId::new(change_id_str);
    let commit_id = CommitId::new(commit_id_str)?;

    Ok(ResolvedRevision {
        user,
        change_id,
        commit_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commit_id_validation() {
        // Valid
        let valid = "a".repeat(40);
        assert!(CommitId::new(valid).is_ok());

        // Invalid length
        assert!(CommitId::new("abc123").is_err());

        // Invalid chars
        assert!(CommitId::new("x".repeat(40)).is_err());
    }

    #[test]
    fn test_user_revision_display() {
        let user = UserRevision::new("main");
        assert_eq!(format!("{}", user), "main");
    }

    #[test]
    fn test_change_id_display() {
        let change_id = ChangeId::new("abc123def");
        assert_eq!(format!("{}", change_id), "abc123def");
    }

    #[test]
    fn test_commit_id_display() {
        let commit_id = CommitId::new("a".repeat(40)).unwrap();
        assert_eq!(format!("{}", commit_id), "a".repeat(40));
    }
}
