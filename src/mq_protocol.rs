use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::SystemTime;

use crate::protocol::StepLogEntry;
use crate::revision::ResolvedRevision;

/// Unique identifier for an MQ run (a candidate submitted via `selfci mq add`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub u64);

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Reason why a run passed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PassedReason {
    /// Run passed and was merged into base branch
    Merged,
    /// Run passed but merge was skipped (--no-merge flag)
    NoMerge,
}

/// Reason why a run failed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FailedReason {
    /// Pre-clone hook failed
    PreClone,
    /// Post-clone hook failed
    PostClone,
    /// Test rebase (before CI check) failed
    TestRebase,
    /// Test merge (before CI check) failed
    TestMerge,
    /// The check command itself failed
    Check,
    /// Pre-merge hook failed
    PreMerge,
    /// The merge operation failed
    Merge,
    /// Failed to resolve base branch
    BaseResolve,
}

/// Status of an MQ run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MQRunStatus {
    Queued,
    Running,
    Passed(PassedReason),
    Failed(FailedReason),
}

impl MQRunStatus {
    /// Format the status for display
    pub fn display(&self) -> String {
        match self {
            MQRunStatus::Queued => "Queued".to_string(),
            MQRunStatus::Running => "Running".to_string(),
            MQRunStatus::Passed(reason) => {
                let reason_str = match reason {
                    PassedReason::Merged => "merged",
                    PassedReason::NoMerge => "no-merge",
                };
                format!("Passed: {}", reason_str)
            }
            MQRunStatus::Failed(reason) => {
                let reason_str = match reason {
                    FailedReason::PreClone => "pre-clone",
                    FailedReason::PostClone => "post-clone",
                    FailedReason::TestRebase => "rebase",
                    FailedReason::TestMerge => "merge",
                    FailedReason::Check => "check",
                    FailedReason::PreMerge => "pre-merge",
                    FailedReason::Merge => "final-merge",
                    FailedReason::BaseResolve => "base-resolve",
                };
                format!("Failed: {}", reason_str)
            }
        }
    }

    /// Check if status represents a passed state
    pub fn is_passed(&self) -> bool {
        matches!(self, MQRunStatus::Passed(_))
    }

    /// Check if status represents a failed state
    pub fn is_failed(&self) -> bool {
        matches!(self, MQRunStatus::Failed(_))
    }
}

/// Information about an MQ run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MQRunInfo {
    pub id: RunId,
    pub candidate: ResolvedRevision,
    pub status: MQRunStatus,
    pub queued_at: SystemTime,
    pub started_at: Option<SystemTime>,
    pub completed_at: Option<SystemTime>,
    /// The merge style used (rebase or merge)
    pub merge_style: crate::config::MergeStyle,
    /// Output from the test merge/rebase (pre-CI check)
    pub test_merge_output: String,
    /// Output from the check command
    pub output: String,
    /// Active jobs within this run (for display purposes)
    pub active_jobs: Vec<StepLogEntry>,
    pub no_merge: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum MQRequest {
    Hello,
    AddCandidate { candidate: String, no_merge: bool },
    List { limit: Option<usize> },
    GetStatus { run_id: RunId },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum MQResponse {
    HelloAck,
    CandidateAdded { run_id: RunId },
    RunList { runs: Vec<MQRunInfo> },
    RunStatus { run: Option<MQRunInfo> },
    Error(String),
}

pub fn send_mq_request(socket_path: &Path, request: MQRequest) -> Result<MQResponse, String> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("Failed to connect to merge queue daemon: {}", e))?;

    // Send request
    ciborium::into_writer(&request, &mut stream)
        .map_err(|e| format!("Failed to send request: {}", e))?;

    // Shutdown write side to signal end of request
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("Failed to shutdown write: {}", e))?;

    // Read response
    let response: MQResponse = ciborium::from_reader(&mut stream)
        .map_err(|e| format!("Failed to read response: {}", e))?;

    Ok(response)
}

pub fn read_mq_request<R: Read>(reader: R) -> Result<MQRequest, String> {
    ciborium::from_reader(reader).map_err(|e| format!("Failed to decode request: {}", e))
}

pub fn write_mq_response<W: Write>(mut writer: W, response: MQResponse) -> Result<(), String> {
    ciborium::into_writer(&response, &mut writer)
        .map_err(|e| format!("Failed to encode response: {}", e))
}
