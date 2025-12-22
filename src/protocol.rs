use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepStatus {
    Running,
    Success,
    Failed { ignored: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepLogEntry {
    pub ts: SystemTime,
    pub name: String,
    pub status: StepStatus,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum JobControlRequest {
    StartJob { name: String },
    LogStep { job_name: String, step_name: String },
    MarkStepFailed { job_name: String, ignore: bool },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum JobControlResponse {
    JobStarted,
    StepLogged,
    StepMarkedFailed,
    Error(String),
}

pub fn send_request(
    socket_path: &Path,
    request: JobControlRequest,
) -> Result<JobControlResponse, String> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("Failed to connect to control socket: {}", e))?;

    // Send request
    ciborium::into_writer(&request, &mut stream)
        .map_err(|e| format!("Failed to send request: {}", e))?;

    // Shutdown write side to signal end of request
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("Failed to shutdown write: {}", e))?;

    // Read response
    let response: JobControlResponse = ciborium::from_reader(&mut stream)
        .map_err(|e| format!("Failed to read response: {}", e))?;

    Ok(response)
}

pub fn read_request<R: Read>(reader: R) -> Result<JobControlRequest, String> {
    ciborium::from_reader(reader).map_err(|e| format!("Failed to decode request: {}", e))
}

pub fn write_response<W: Write>(
    mut writer: W,
    response: JobControlResponse,
) -> Result<(), String> {
    ciborium::into_writer(&response, &mut writer)
        .map_err(|e| format!("Failed to encode response: {}", e))
}
