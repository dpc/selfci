use duct::cmd;
use selfci::{
    CheckError, MainError, WorkDirError, copy_revisions_to_workdirs, detect_vcs, protocol,
    read_config,
};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::debug;

struct RunJobRequest {
    base_dir: PathBuf,
    candidate_dir: PathBuf,
    job_name: String,
    job_full_command: Vec<String>,
    print_output: bool,
    socket_path: PathBuf,
}

struct RunJobOutcome {
    job_name: String,
    output: String,
    exit_code: Option<i32>,
    duration: Duration,
    steps: Vec<protocol::StepLogEntry>,
}

enum JobMessage {
    Started { job_name: String },
    Completed(RunJobOutcome),
}

/// Worker thread function that processes jobs
fn job_worker(
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
            .env("SELFCI_BASE_DIR", &job.base_dir)
            .env("SELFCI_CANDIDATE_DIR", &job.candidate_dir)
            .env("SELFCI_JOB_NAME", &job.job_name)
            .env("SELFCI_JOB_SOCK_PATH", &job.socket_path)
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
            steps: Vec::new(), // Will be populated in check() function
        };
        let _ = messages_sender.send(JobMessage::Completed(outcome));
    }
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

pub fn check(
    root: Option<String>,
    base: Option<String>,
    candidate: Option<String>,
    print_output: bool,
    jobs: Option<usize>,
) -> Result<(), MainError> {
    // Determine root directory
    let root_dir = root
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("Failed to get current directory"));

    // Detect VCS
    let vcs = detect_vcs(&root_dir)?;
    debug!(vcs = ?vcs, root_dir = %root_dir.display(), "Detected VCS");

    // Use defaults for base and candidate if not provided
    let base_rev = base.as_deref().unwrap_or("@-");
    let candidate_rev = candidate.as_deref().unwrap_or("@");

    // Allocate base work directory
    let base_workdir = tempfile::tempdir().map_err(WorkDirError::CreateFailed)?;

    debug!(
        base_workdir = %base_workdir.path().display(),
        "Allocated base work directory"
    );
    debug!(base_rev, candidate_rev, "Check command invoked");

    // Create a single candidate workdir (shared by all jobs)
    let candidate_workdir = tempfile::tempdir().map_err(WorkDirError::CreateFailed)?;

    // Copy revisions to workdirs
    copy_revisions_to_workdirs(
        &vcs,
        &root_dir,
        base_workdir.path(),
        base_rev,
        candidate_workdir.path(),
        candidate_rev,
    )?;

    // Read config from base workdir
    let config = read_config(base_workdir.path())?;
    debug!("Loaded config");

    // Determine parallelism level
    let parallelism = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    debug!(parallelism, "Using parallelism level");

    // Create control socket
    let socket_file = tempfile::NamedTempFile::new().map_err(WorkDirError::CreateFailed)?;
    let socket_path = socket_file.path().to_path_buf();
    // Remove the temp file so we can bind to it as a socket
    drop(socket_file);

    let listener = UnixListener::bind(&socket_path).map_err(|_| CheckError::CheckFailed)?;
    debug!(socket_path = %socket_path.display(), "Created control socket");

    // Create channels for jobs (SPMC) and messages (MPSC)
    let (jobs_sender, jobs_receiver) = mpsc::channel::<RunJobRequest>();
    let (messages_sender, messages_receiver) = mpsc::channel::<JobMessage>();
    let jobs_receiver = Arc::new(Mutex::new(jobs_receiver));

    // Job tracking: (started, completed)
    let job_counts = Arc::new(Mutex::new((0usize, 0usize)));

    // Track used job names
    let used_job_names = Arc::new(Mutex::new(std::collections::HashSet::new()));

    // Track steps for each job
    let job_steps = Arc::new(Mutex::new(HashMap::<String, Vec<protocol::StepLogEntry>>::new()));

    // Spawn worker threads
    let mut workers = Vec::new();
    for _ in 0..parallelism {
        let jobs_rx = Arc::clone(&jobs_receiver);
        let messages_tx = messages_sender.clone();

        let handle = std::thread::spawn(move || {
            job_worker(jobs_rx, messages_tx);
        });
        workers.push(handle);
    }

    // Spawn control socket listener thread
    let jobs_sender_clone = jobs_sender.clone();
    let job_counts_clone = Arc::clone(&job_counts);
    let used_job_names_clone = Arc::clone(&used_job_names);
    let job_steps_clone = Arc::clone(&job_steps);
    let base_workdir_path = base_workdir.path().to_path_buf();
    let candidate_workdir_path = candidate_workdir.path().to_path_buf();
    let job_config = config.job.clone();
    let socket_path_clone = socket_path.clone();

    let _listener_thread = std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    // Read request
                    let request = match protocol::read_request(&stream) {
                        Ok(req) => req,
                        Err(e) => {
                            debug!("Failed to read request: {}", e);
                            let _ = protocol::write_response(
                                &mut stream,
                                protocol::JobControlResponse::Error(e),
                            );
                            continue;
                        }
                    };

                    match request {
                        protocol::JobControlRequest::StartJob { name } => {
                            // Check if job name is unique
                            let mut used_names = used_job_names_clone.lock().unwrap();
                            if used_names.contains(&name) {
                                let error_msg = format!("Job '{}' already started", name);
                                debug!("{}", error_msg);
                                let _ = protocol::write_response(
                                    &mut stream,
                                    protocol::JobControlResponse::Error(error_msg),
                                );
                                continue;
                            }
                            used_names.insert(name.clone());
                            drop(used_names);

                            // Build the full command
                            let mut full_command = job_config.command_prefix.clone();
                            full_command.push(job_config.command.clone());

                            // Create job request
                            let job = RunJobRequest {
                                base_dir: base_workdir_path.clone(),
                                candidate_dir: candidate_workdir_path.clone(),
                                job_name: name.clone(),
                                job_full_command: full_command,
                                print_output,
                                socket_path: socket_path_clone.clone(),
                            };

                            // Send job to workers
                            if let Err(e) = jobs_sender_clone.send(job) {
                                let error_msg = format!("Failed to send job: {}", e);
                                debug!("{}", error_msg);
                                let _ = protocol::write_response(
                                    &mut stream,
                                    protocol::JobControlResponse::Error(error_msg),
                                );
                                continue;
                            }

                            // Increment started counter
                            {
                                let mut counts = job_counts_clone.lock().unwrap();
                                counts.0 += 1;
                            }

                            // Send success response
                            let _ = protocol::write_response(
                                &mut stream,
                                protocol::JobControlResponse::JobStarted,
                            );

                            debug!(job = %name, "Job started via control socket");
                        }
                        protocol::JobControlRequest::LogStep { job_name, step_name } => {
                            // Get current timestamp
                            let ts = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();

                            // Create step entry with Running status
                            let entry = protocol::StepLogEntry {
                                ts,
                                name: step_name.clone(),
                                status: protocol::StepStatus::Running,
                            };

                            // Add to steps for this job
                            {
                                let mut steps = job_steps_clone.lock().unwrap();
                                steps.entry(job_name.clone()).or_insert_with(Vec::new).push(entry);
                            }

                            // Send success response
                            let _ = protocol::write_response(
                                &mut stream,
                                protocol::JobControlResponse::StepLogged,
                            );

                            debug!(job = %job_name, step = %step_name, "Step logged via control socket");
                        }
                        protocol::JobControlRequest::MarkStepFailed { job_name, ignore } => {
                            // Find the last step for this job and mark it as failed
                            {
                                let mut steps = job_steps_clone.lock().unwrap();
                                if let Some(job_steps) = steps.get_mut(&job_name) {
                                    if let Some(last_step) = job_steps.last_mut() {
                                        last_step.status = protocol::StepStatus::Failed { ignored: ignore };
                                        debug!(job = %job_name, step = %last_step.name, ignore, "Step marked as failed");
                                    } else {
                                        let error_msg = format!("No steps found for job '{}'", job_name);
                                        debug!("{}", error_msg);
                                        let _ = protocol::write_response(
                                            &mut stream,
                                            protocol::JobControlResponse::Error(error_msg),
                                        );
                                        continue;
                                    }
                                } else {
                                    let error_msg = format!("Job '{}' not found", job_name);
                                    debug!("{}", error_msg);
                                    let _ = protocol::write_response(
                                        &mut stream,
                                        protocol::JobControlResponse::Error(error_msg),
                                    );
                                    continue;
                                }
                            }

                            // Send success response
                            let _ = protocol::write_response(
                                &mut stream,
                                protocol::JobControlResponse::StepMarkedFailed,
                            );
                        }
                    }
                }
                Err(e) => {
                    debug!("Failed to accept connection: {}", e);
                }
            }
        }
    });

    // Start the "main" job
    {
        let mut used_names = used_job_names.lock().unwrap();
        used_names.insert("main".to_string());
        drop(used_names);

        let mut full_command = config.job.command_prefix.clone();
        full_command.push(config.job.command.clone());

        let job = RunJobRequest {
            base_dir: base_workdir.path().to_path_buf(),
            candidate_dir: candidate_workdir.path().to_path_buf(),
            job_name: "main".to_string(),
            job_full_command: full_command,
            print_output,
            socket_path: socket_path.clone(),
        };

        jobs_sender.send(job).map_err(|_| CheckError::CheckFailed)?;

        // Increment started counter
        {
            let mut counts = job_counts.lock().unwrap();
            counts.0 += 1;
        }
    }

    // Drop the original senders
    drop(jobs_sender);
    drop(messages_sender);

    println!("Running jobs with parallelism {}", parallelism);

    // Collect messages as they arrive
    let mut failed_jobs = Vec::new();
    let mut total_duration = Duration::ZERO;

    for message in messages_receiver {
        match message {
            JobMessage::Started { job_name } => {
                let (started, completed) = {
                    let counts = job_counts.lock().unwrap();
                    *counts
                };
                println!("[{}/{}] Job started: {}", completed, started, job_name);
            }
            JobMessage::Completed(mut outcome) => {
                // Increment completed counter
                let (started, completed) = {
                    let mut counts = job_counts.lock().unwrap();
                    counts.1 += 1;
                    *counts
                };

                // Look up steps for this job and mark Running steps as Success
                {
                    let steps_map = job_steps.lock().unwrap();
                    if let Some(steps) = steps_map.get(&outcome.job_name) {
                        outcome.steps = steps.iter().map(|step| {
                            let mut step = step.clone();
                            if matches!(step.status, protocol::StepStatus::Running) {
                                step.status = protocol::StepStatus::Success;
                            }
                            step
                        }).collect();
                    }
                }

                // Check if any step has a non-ignored failure
                let has_step_failure = outcome.steps.iter().any(|step| {
                    matches!(step.status, protocol::StepStatus::Failed { ignored: false })
                });

                total_duration += outcome.duration;

                let duration_secs = outcome.duration.as_secs_f64();

                // Determine if job passed or failed
                let job_passed = if let Some(exit_code) = outcome.exit_code {
                    exit_code == 0 && !has_step_failure
                } else {
                    false
                };

                if job_passed {
                    println!(
                        "[{}/{}] Job passed: {} ({:.2}s)",
                        completed, started, outcome.job_name, duration_secs
                    );
                } else {
                    let failure_reason = if has_step_failure {
                        "step failure"
                    } else if let Some(_exit_code) = outcome.exit_code {
                        "exit code"
                    } else {
                        "no exit code"
                    };

                    let exit_code_str = outcome.exit_code
                        .map(|c| format!(" (exit code: {})", c))
                        .unwrap_or_default();

                    println!(
                        "[{}/{}] Job failed: {} ({}{}, {:.2}s)",
                        completed, started, outcome.job_name, failure_reason, exit_code_str, duration_secs
                    );
                    // Print output on failure if we weren't already printing it in real-time
                    if !print_output && !outcome.output.is_empty() {
                        println!("{}", outcome.output);
                    }
                    failed_jobs.push((outcome.job_name, outcome.output));
                }

                // Print steps if any
                if !outcome.steps.is_empty() {
                    let end_ts = outcome.steps[0].ts + outcome.duration.as_secs();
                    for (i, step_entry) in outcome.steps.iter().enumerate() {
                        let next_ts = if i + 1 < outcome.steps.len() {
                            outcome.steps[i + 1].ts
                        } else {
                            end_ts
                        };
                        let step_duration = next_ts.saturating_sub(step_entry.ts);

                        // Choose emoji based on step status
                        let status_emoji = match &step_entry.status {
                            protocol::StepStatus::Success => "✅",
                            protocol::StepStatus::Failed { ignored: true } => "⚠️",
                            protocol::StepStatus::Failed { ignored: false } => "❌",
                            protocol::StepStatus::Running => "⏳",
                        };

                        println!("  {} {} ({:.2}s)", status_emoji, step_entry.name, step_duration as f64);
                    }
                }

                // Check if all started jobs are completed
                if started == completed {
                    break;
                }
            }
        }
    }

    // Clean up socket
    let _ = std::fs::remove_file(&socket_path);

    // Note: We don't wait for workers or listener thread to finish.
    // They will be cleaned up when the process exits.
    // Workers are blocked waiting for jobs, and the listener holds a jobs_sender clone,
    // so the jobs channel won't close until the listener exits.

    // Print total time
    println!("\nTotal time: {:.2}s", total_duration.as_secs_f64());

    // Report failures
    if !failed_jobs.is_empty() {
        println!("{} job(s) failed:", failed_jobs.len());
        for (name, _output) in &failed_jobs {
            println!("  - {}", name);
        }
        return Err(CheckError::CheckFailed.into());
    }

    debug!("All jobs succeeded");

    Ok(())
}
