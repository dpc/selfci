use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StepLogEntry {
    pub ts: u64, // Unix timestamp in seconds
    pub name: String,
}

impl StepLogEntry {
    pub fn new(name: String) -> Self {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self { ts, name }
    }
}

pub fn log_step(name: String) -> Result<(), String> {
    // Get the step log path from environment variable
    let log_path = std::env::var("SELFCI_STEP_LOG_PATH")
        .map_err(|_| "SELFCI_STEP_LOG_PATH environment variable not set".to_string())?;

    // Read existing entries
    let mut entries: Vec<StepLogEntry> = if Path::new(&log_path).exists() {
        let content = fs::read_to_string(&log_path)
            .map_err(|e| format!("Failed to read step log: {}", e))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse step log: {}", e))?
    } else {
        Vec::new()
    };

    // Append new entry
    entries.push(StepLogEntry::new(name));

    // Write back
    let content = serde_json::to_string(&entries)
        .map_err(|e| format!("Failed to serialize step log: {}", e))?;
    fs::write(&log_path, content)
        .map_err(|e| format!("Failed to write step log: {}", e))?;

    Ok(())
}

pub fn read_step_log(path: &Path) -> Result<Vec<StepLogEntry>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read step log: {}", e))?;
    serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse step log: {}", e))
}
