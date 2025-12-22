use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::SystemTime;

use crate::protocol::StepLogEntry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MQJobStatus {
    Queued,
    Running,
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MQJobInfo {
    pub id: u64,
    pub candidate: String,
    pub status: MQJobStatus,
    pub queued_at: SystemTime,
    pub started_at: Option<SystemTime>,
    pub completed_at: Option<SystemTime>,
    pub output: String,
    pub steps: Vec<StepLogEntry>,
    pub no_merge: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum MQRequest {
    Hello,
    AddCandidate { candidate: String, no_merge: bool },
    List { limit: Option<usize> },
    GetStatus { job_id: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum MQResponse {
    HelloAck,
    CandidateAdded { job_id: u64 },
    JobList { jobs: Vec<MQJobInfo> },
    JobStatus { job: Option<MQJobInfo> },
    Error(String),
}

pub fn send_mq_request(
    socket_path: &Path,
    request: MQRequest,
) -> Result<MQResponse, String> {
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

pub fn write_mq_response<W: Write>(
    mut writer: W,
    response: MQResponse,
) -> Result<(), String> {
    ciborium::into_writer(&response, &mut writer)
        .map_err(|e| format!("Failed to encode response: {}", e))
}
