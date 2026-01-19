use duct::cmd;
use selfci::{CheckError, envs, protocol};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tracing::debug;

#[derive(Debug)]
pub struct RunJobRequest {
    pub base_dir: PathBuf,
    pub candidate_dir: PathBuf,
    pub job_name: String,
    pub job_full_command: Vec<String>,
    pub print_output: bool,
    pub socket_path: PathBuf,
    /// Candidate commit ID (git/jj commit hash)
    pub candidate_commit_id: String,
    /// Candidate change ID (jj change ID, may be empty for git)
    pub candidate_change_id: String,
    /// Candidate ID (user-provided identifier)
    pub candidate_id: String,
}

pub struct RunJobOutcome {
    pub job_name: String,
    pub exit_code: Option<i32>,
    pub output: String,
    pub duration: Duration,
    pub steps: Vec<protocol::StepLogEntry>,
}

pub enum JobMessage {
    Started {
        job_name: String,
    },
    Completed(RunJobOutcome),
    StepStarted {
        job_name: String,
        step_name: String,
    },
    StepCompleted {
        job_name: String,
        step_name: String,
        status: protocol::StepStatus,
    },
}

/// Capture output from a reader, optionally printing to stdout in real-time
/// Returns the captured output and the reader (to allow checking exit status)
fn capture_and_print_output<R: Read>(
    mut reader: R,
    print_to_stdout: bool,
) -> Result<(String, R), CheckError> {
    let mut captured_output = String::new();
    let mut buffer = [0; 8192];

    loop {
        let n = reader
            .read(&mut buffer)
            .map_err(|_e| CheckError::CheckFailed)?;
        if n == 0 {
            break;
        }
        let chunk = &buffer[..n];

        if print_to_stdout {
            std::io::stdout()
                .write_all(chunk)
                .map_err(|_e| CheckError::CheckFailed)?;
            std::io::stdout()
                .flush()
                .map_err(|_e| CheckError::CheckFailed)?;
        }

        captured_output.push_str(&String::from_utf8_lossy(chunk));
    }

    Ok((captured_output, reader))
}

/// Worker thread function - processes jobs from the queue
pub fn job_worker(
    jobs_receiver: Arc<Mutex<mpsc::Receiver<RunJobRequest>>>,
    messages_sender: mpsc::Sender<JobMessage>,
) {
    loop {
        // Receive a job from the channel
        let job = {
            let receiver = jobs_receiver.lock().unwrap();
            match receiver.recv() {
                Ok(job) => job,
                Err(_) => break, // Channel closed, no more jobs
            }
        };

        debug!(job = %job.job_name, "Worker processing job");

        // Send started message
        let _ = messages_sender.send(JobMessage::Started {
            job_name: job.job_name.clone(),
        });

        // Record start time
        let start_time = Instant::now();

        // Execute the job command
        let handle = match cmd(&job.job_full_command[0], &job.job_full_command[1..])
            .dir(&job.candidate_dir)
            .env(envs::SELFCI_VERSION, env!("CARGO_PKG_VERSION"))
            .env(envs::SELFCI_BASE_DIR, &job.base_dir)
            .env(envs::SELFCI_CANDIDATE_DIR, &job.candidate_dir)
            .env(envs::SELFCI_CANDIDATE_COMMIT_ID, &job.candidate_commit_id)
            .env(envs::SELFCI_CANDIDATE_CHANGE_ID, &job.candidate_change_id)
            .env(envs::SELFCI_CANDIDATE_ID, &job.candidate_id)
            .env(envs::SELFCI_JOB_NAME, &job.job_name)
            .env(envs::SELFCI_JOB_SOCK_PATH, &job.socket_path)
            .stderr_to_stdout()
            .unchecked()
            .reader()
        {
            Ok(h) => h,
            Err(e) => {
                let outcome = RunJobOutcome {
                    job_name: job.job_name,
                    output: format!("Failed to start command: {}", e),
                    exit_code: None,
                    duration: start_time.elapsed(),
                    steps: Vec::new(),
                };
                let _ = messages_sender.send(JobMessage::Completed(outcome));
                continue;
            }
        };

        // Capture output
        let (captured_output, handle) = match capture_and_print_output(handle, job.print_output) {
            Ok(result) => result,
            Err(_) => {
                let outcome = RunJobOutcome {
                    job_name: job.job_name,
                    output: "Failed to capture output".to_string(),
                    exit_code: None,
                    duration: start_time.elapsed(),
                    steps: Vec::new(),
                };
                let _ = messages_sender.send(JobMessage::Completed(outcome));
                continue;
            }
        };

        // Get exit status
        let exit_code = match handle.try_wait() {
            Ok(Some(status)) => status.status.code(),
            Ok(None) => None,
            Err(_) => None,
        };

        // Record end time and calculate duration
        let duration = start_time.elapsed();

        // Send outcome (steps will be looked up separately)
        let outcome = RunJobOutcome {
            job_name: job.job_name,
            output: captured_output,
            exit_code,
            duration,
            steps: Vec::new(), // Will be populated by caller
        };
        let _ = messages_sender.send(JobMessage::Completed(outcome));
    }
}

/// Context needed to spawn new jobs dynamically
#[derive(Clone)]
pub struct JobSpawnContext {
    pub base_dir: PathBuf,
    pub candidate_dir: PathBuf,
    pub command_prefix: Vec<String>,
    pub command: String,
    pub print_output: bool,
    pub socket_path: PathBuf,
    /// Candidate commit ID (git/jj commit hash)
    pub candidate_commit_id: String,
    /// Candidate change ID (jj change ID, may be empty for git)
    pub candidate_change_id: String,
    /// Candidate ID (user-provided identifier)
    pub candidate_id: String,
}

/// Control socket listener - handles step logging and job control
/// The shutdown flag is checked after each accept(). To wake up a blocking accept(),
/// the caller should connect to the socket after setting the shutdown flag.
#[allow(clippy::too_many_arguments)]
pub fn control_socket_listener(
    listener: UnixListener,
    job_steps: Arc<Mutex<HashMap<String, Vec<protocol::StepLogEntry>>>>,
    used_job_names: Arc<Mutex<std::collections::HashSet<String>>>,
    job_completions: Arc<Mutex<HashMap<String, protocol::JobStatus>>>,
    jobs_sender: mpsc::Sender<RunJobRequest>,
    messages_sender: mpsc::Sender<JobMessage>,
    spawn_context: JobSpawnContext,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Check for shutdown before processing
                if shutdown.load(Ordering::SeqCst) {
                    debug!("Control socket listener shutting down");
                    break;
                }
                let job_steps_clone = Arc::clone(&job_steps);
                let used_job_names_clone = Arc::clone(&used_job_names);
                let job_completions_clone = Arc::clone(&job_completions);
                let jobs_sender_clone = jobs_sender.clone();
                let messages_sender_clone = messages_sender.clone();
                let spawn_context_clone = spawn_context.clone();

                std::thread::spawn(move || {
                    if let Ok(request) = protocol::read_request(&mut stream) {
                        match request {
                            protocol::JobControlRequest::WaitForJob { name } => {
                                // Check if job name is known
                                let is_known = {
                                    let used = used_job_names_clone.lock().unwrap();
                                    used.contains(&name)
                                };

                                if !is_known {
                                    let _ = protocol::write_response(
                                        &mut stream,
                                        protocol::JobControlResponse::JobNotFound,
                                    );
                                    return;
                                }

                                // Poll for completion (no timeout)
                                let poll_interval = std::time::Duration::from_millis(100);

                                loop {
                                    {
                                        let completions = job_completions_clone.lock().unwrap();
                                        if let Some(status) = completions.get(&name) {
                                            let _ = protocol::write_response(
                                                &mut stream,
                                                protocol::JobControlResponse::JobCompleted {
                                                    status: status.clone(),
                                                },
                                            );
                                            return;
                                        }
                                    }

                                    std::thread::sleep(poll_interval);
                                }
                            }
                            protocol::JobControlRequest::StartJob { name } => {
                                let mut used = used_job_names_clone.lock().unwrap();
                                if used.contains(&name) {
                                    let error_msg = format!("Job '{}' already started", name);
                                    debug!("{}", error_msg);
                                    let _ = protocol::write_response(
                                        &mut stream,
                                        protocol::JobControlResponse::Error(error_msg),
                                    );
                                } else {
                                    used.insert(name.clone());

                                    // Actually spawn the job by creating a RunJobRequest
                                    let mut full_command =
                                        spawn_context_clone.command_prefix.clone();
                                    full_command.push(spawn_context_clone.command.clone());

                                    let job_request = RunJobRequest {
                                        base_dir: spawn_context_clone.base_dir.clone(),
                                        candidate_dir: spawn_context_clone.candidate_dir.clone(),
                                        job_name: name.clone(),
                                        job_full_command: full_command,
                                        print_output: spawn_context_clone.print_output,
                                        socket_path: spawn_context_clone.socket_path.clone(),
                                        candidate_commit_id: spawn_context_clone
                                            .candidate_commit_id
                                            .clone(),
                                        candidate_change_id: spawn_context_clone
                                            .candidate_change_id
                                            .clone(),
                                        candidate_id: spawn_context_clone.candidate_id.clone(),
                                    };

                                    match jobs_sender_clone.send(job_request) {
                                        Ok(_) => {
                                            let _ = protocol::write_response(
                                                &mut stream,
                                                protocol::JobControlResponse::JobStarted,
                                            );
                                            debug!(job = %name, "Job started via control socket");
                                        }
                                        Err(e) => {
                                            let error_msg = format!("Failed to queue job: {}", e);
                                            debug!("{}", error_msg);
                                            let _ = protocol::write_response(
                                                &mut stream,
                                                protocol::JobControlResponse::Error(error_msg),
                                            );
                                        }
                                    }
                                }
                            }
                            protocol::JobControlRequest::LogStep {
                                job_name,
                                step_name,
                            } => {
                                let ts = std::time::SystemTime::now();

                                // Check if there's a previous Running step and complete it
                                {
                                    let mut steps = job_steps_clone.lock().unwrap();
                                    let job_steps_vec = steps.entry(job_name.clone()).or_default();

                                    if let Some(prev_step) = job_steps_vec.last_mut()
                                        && matches!(prev_step.status, protocol::StepStatus::Running)
                                    {
                                        prev_step.status = protocol::StepStatus::Success;
                                        let _ =
                                            messages_sender_clone.send(JobMessage::StepCompleted {
                                                job_name: job_name.clone(),
                                                step_name: prev_step.name.clone(),
                                                status: protocol::StepStatus::Success,
                                            });
                                    }

                                    let entry = protocol::StepLogEntry {
                                        ts,
                                        name: step_name.clone(),
                                        status: protocol::StepStatus::Running,
                                    };
                                    job_steps_vec.push(entry);
                                }

                                // Send StepStarted message
                                let _ = messages_sender_clone.send(JobMessage::StepStarted {
                                    job_name: job_name.clone(),
                                    step_name: step_name.clone(),
                                });

                                let _ = protocol::write_response(
                                    &mut stream,
                                    protocol::JobControlResponse::StepLogged,
                                );

                                debug!(job = %job_name, step = %step_name, "Step logged via control socket");
                            }
                            protocol::JobControlRequest::MarkStepFailed { job_name, ignore } => {
                                let mut steps = job_steps_clone.lock().unwrap();
                                if let Some(job_steps) = steps.get_mut(&job_name) {
                                    if let Some(last_step) = job_steps.last_mut() {
                                        let status =
                                            protocol::StepStatus::Failed { ignored: ignore };
                                        last_step.status = status.clone();
                                        let step_name = last_step.name.clone();
                                        debug!(job = %job_name, step = %step_name, ignore, "Step marked as failed");

                                        // Send StepCompleted message with failed status
                                        let _ =
                                            messages_sender_clone.send(JobMessage::StepCompleted {
                                                job_name: job_name.clone(),
                                                step_name,
                                                status,
                                            });

                                        let _ = protocol::write_response(
                                            &mut stream,
                                            protocol::JobControlResponse::StepMarkedFailed,
                                        );
                                    } else {
                                        let error_msg =
                                            format!("No steps found for job '{}'", job_name);
                                        debug!("{}", error_msg);
                                        let _ = protocol::write_response(
                                            &mut stream,
                                            protocol::JobControlResponse::Error(error_msg),
                                        );
                                    }
                                } else {
                                    let error_msg = format!("Job '{}' not found", job_name);
                                    debug!("{}", error_msg);
                                    let _ = protocol::write_response(
                                        &mut stream,
                                        protocol::JobControlResponse::Error(error_msg),
                                    );
                                }
                            }
                        }
                    }
                });
            }
            Err(e) => {
                // Check for shutdown on error
                if shutdown.load(Ordering::SeqCst) {
                    debug!("Control socket listener shutting down");
                    break;
                }
                debug!("Control socket accept error: {}", e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}
