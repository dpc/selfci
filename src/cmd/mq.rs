use daemonize::Daemonize;
use duct::cmd;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use selfci::{MainError, WorkDirError, get_vcs, mq_protocol, protocol};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::SystemTime;
use tracing::debug;

struct MQState {
    root_dir: PathBuf,
    base_branch: String,
    next_job_id: u64,
    queued: HashMap<u64, mq_protocol::MQJobInfo>,
    active: HashMap<u64, mq_protocol::MQJobInfo>,
    completed: Vec<mq_protocol::MQJobInfo>,
}

#[derive(Debug)]
enum DaemonStatus {
    Running(u32), // PID
    NotRunning,
    Stale, // PID file exists but process dead
}

fn is_daemon_running(pid_path: &Path, socket_path: &Path) -> DaemonStatus {
    // Check if PID file exists
    if !pid_path.exists() {
        return DaemonStatus::NotRunning;
    }

    // Read PID from file
    let pid = match std::fs::read_to_string(pid_path) {
        Ok(content) => match content.trim().parse::<u32>() {
            Ok(p) => p,
            Err(_) => return DaemonStatus::Stale,
        },
        Err(_) => return DaemonStatus::Stale,
    };

    // Check if process with that PID exists by sending signal 0
    let process_exists = signal::kill(Pid::from_raw(pid as i32), None).is_ok();

    if !process_exists {
        return DaemonStatus::Stale;
    }

    // Double-check with socket
    if socket_path.exists() {
        match mq_protocol::send_mq_request(socket_path, mq_protocol::MQRequest::Hello) {
            Ok(mq_protocol::MQResponse::HelloAck) => DaemonStatus::Running(pid),
            _ => DaemonStatus::Stale,
        }
    } else {
        DaemonStatus::Stale
    }
}

pub fn start_daemon(
    base_branch: String,
    foreground: bool,
    log_file: Option<PathBuf>,
) -> Result<(), MainError> {
    // Determine paths
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let config_dir = root_dir.join(".config").join("selfci");
    let socket_path = config_dir.join(".mq.sock");
    let pid_path = config_dir.join(".mq.pid");
    let default_log_path = config_dir.join("mq.log");
    let log_path = log_file.unwrap_or(default_log_path);

    // Create config directory if needed
    std::fs::create_dir_all(&config_dir).map_err(WorkDirError::CreateFailed)?;

    // Check if daemon is already running
    match is_daemon_running(&pid_path, &socket_path) {
        DaemonStatus::Running(pid) => {
            println!("Merge queue daemon is already running (PID: {})", pid);
            println!("Use 'selfci mq stop' to stop it");
            return Ok(());
        }
        DaemonStatus::Stale => {
            // Clean up stale files
            debug!("Removing stale PID and socket files");
            std::fs::remove_file(&pid_path).ok();
            std::fs::remove_file(&socket_path).ok();
        }
        DaemonStatus::NotRunning => {
            // Clean up any stale socket
            if socket_path.exists() {
                std::fs::remove_file(&socket_path).ok();
            }
        }
    }

    // Daemonize if not in foreground mode
    if !foreground {
        // Print startup message before forking (so parent process shows it to user)
        println!("Merge queue daemon starting in background...");
        println!("Base branch: {}", base_branch);
        println!("Socket: {}", socket_path.display());
        println!("Log: {}", log_path.display());

        // Create log file
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(WorkDirError::CreateFailed)?;

        let daemonize = Daemonize::new()
            .pid_file(&pid_path)
            .working_directory(&root_dir)
            .stdout(log_file.try_clone().map_err(WorkDirError::CreateFailed)?)
            .stderr(log_file)
            .umask(0o027);

        match daemonize.start() {
            Ok(_) => {
                // We're now in the daemon process (child)
                // Parent has exited and shown message to user
                debug!("Daemon process started");
            }
            Err(e) => {
                eprintln!("Failed to daemonize: {}", e);
                return Err(MainError::CheckFailed);
            }
        }
    } else {
        // In foreground mode, write our PID manually for consistency
        let pid = std::process::id();
        std::fs::write(&pid_path, pid.to_string()).map_err(WorkDirError::CreateFailed)?;

        println!(
            "Merge queue daemon started for base branch: {}",
            base_branch
        );
        println!("Socket: {}", socket_path.display());
    }

    // Set up cleanup on exit
    let socket_path_cleanup = socket_path.clone();
    let pid_path_cleanup = pid_path.clone();
    let _guard = scopeguard::guard((), move |_| {
        std::fs::remove_file(&socket_path_cleanup).ok();
        std::fs::remove_file(&pid_path_cleanup).ok();
    });

    // Bind socket
    let listener = UnixListener::bind(&socket_path).map_err(WorkDirError::CreateFailed)?;

    // Initialize state
    let state = Arc::new(Mutex::new(MQState {
        root_dir: root_dir.clone(),
        base_branch: base_branch.clone(),
        next_job_id: 1,
        queued: HashMap::new(),
        active: HashMap::new(),
        completed: Vec::new(),
    }));

    // Create channel for queueing jobs
    let (mq_jobs_sender, mq_jobs_receiver) = mpsc::channel::<u64>();

    // Spawn worker thread to process queue
    let state_clone = Arc::clone(&state);
    let root_dir_clone = root_dir.clone();
    std::thread::spawn(move || {
        // process_queue creates worker pools for each candidate check via run_candidate_check
        process_queue(state_clone, root_dir_clone, mq_jobs_receiver);
    });

    // Main loop: accept connections and handle requests
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let state_clone = Arc::clone(&state);
                let mq_jobs_sender_clone = mq_jobs_sender.clone();
                std::thread::spawn(move || {
                    if let Ok(request) = mq_protocol::read_mq_request(&mut stream) {
                        let response = handle_request(state_clone, request, mq_jobs_sender_clone);
                        let _ = mq_protocol::write_mq_response(&mut stream, response);
                    }
                });
            }
            Err(e) => {
                debug!("Connection error: {}", e);
            }
        }
    }

    Ok(())
}

fn handle_request(
    state: Arc<Mutex<MQState>>,
    request: mq_protocol::MQRequest,
    mq_jobs_sender: mpsc::Sender<u64>,
) -> mq_protocol::MQResponse {
    match request {
        mq_protocol::MQRequest::Hello => mq_protocol::MQResponse::HelloAck,

        mq_protocol::MQRequest::AddCandidate {
            candidate,
            no_merge,
        } => {
            // Get root_dir and VCS for resolution
            let (root_dir, vcs) = {
                let state = state.lock().unwrap();
                let root_dir = state.root_dir.clone();
                let vcs = match get_vcs(&root_dir, None) {
                    Ok(v) => v,
                    Err(e) => return mq_protocol::MQResponse::Error(format!("VCS error: {}", e)),
                };
                (root_dir, vcs)
            };

            // Resolve candidate to immutable IDs
            let resolved_candidate =
                match selfci::revision::resolve_revision(&vcs, &root_dir, &candidate) {
                    Ok(r) => r,
                    Err(e) => {
                        return mq_protocol::MQResponse::Error(format!(
                            "Failed to resolve revision '{}': {}",
                            candidate, e
                        ));
                    }
                };

            let (job_id, send_result) = {
                let mut state = state.lock().unwrap();
                let job_id = state.next_job_id;
                state.next_job_id += 1;

                let job = mq_protocol::MQJobInfo {
                    id: job_id,
                    candidate: resolved_candidate.clone(),
                    status: mq_protocol::MQJobStatus::Queued,
                    queued_at: SystemTime::now(),
                    started_at: None,
                    completed_at: None,
                    output: String::new(),
                    steps: Vec::new(),
                    no_merge,
                };

                state.queued.insert(job_id, job);
                debug!(
                    candidate_user = %resolved_candidate.user,
                    candidate_commit = %resolved_candidate.commit_id,
                    job_id,
                    no_merge,
                    "Added candidate to queue"
                );

                // Send job ID to process_queue
                let send_result = mq_jobs_sender.send(job_id);
                (job_id, send_result)
            };

            match send_result {
                Ok(_) => mq_protocol::MQResponse::CandidateAdded { job_id },
                Err(e) => mq_protocol::MQResponse::Error(format!("Failed to queue job: {}", e)),
            }
        }

        mq_protocol::MQRequest::List { limit } => {
            let state = state.lock().unwrap();
            let mut jobs: Vec<_> = state
                .queued
                .values()
                .chain(state.active.values())
                .chain(state.completed.iter())
                .cloned()
                .collect();

            // Sort by ID descending (most recent first)
            jobs.sort_by(|a, b| b.id.cmp(&a.id));

            if let Some(limit) = limit {
                jobs.truncate(limit);
            }

            mq_protocol::MQResponse::JobList { jobs }
        }

        mq_protocol::MQRequest::GetStatus { job_id } => {
            let state = state.lock().unwrap();
            let job = state
                .queued
                .get(&job_id)
                .or_else(|| state.active.get(&job_id))
                .cloned()
                .or_else(|| state.completed.iter().find(|j| j.id == job_id).cloned());

            mq_protocol::MQResponse::JobStatus { job }
        }
    }
}

fn process_queue(
    state: Arc<Mutex<MQState>>,
    root_dir: PathBuf,
    mq_jobs_receiver: mpsc::Receiver<u64>,
) {
    // Get VCS once at the start
    let vcs = match get_vcs(&root_dir, None) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to detect VCS: {}", e);
            return;
        }
    };

    loop {
        // Wait for next job ID from channel
        let job_id = match mq_jobs_receiver.recv() {
            Ok(id) => id,
            Err(_) => {
                debug!("MQ jobs channel closed, exiting process_queue");
                break;
            }
        };

        // Move job from queued to active
        let mut job_info = {
            let mut state = state.lock().unwrap();
            match state.queued.remove(&job_id) {
                Some(mut job) => {
                    job.status = mq_protocol::MQJobStatus::Running;
                    job.started_at = Some(SystemTime::now());
                    state.active.insert(job_id, job.clone());
                    job
                }
                None => {
                    debug!("Job {} not found in queued map", job_id);
                    continue;
                }
            }
        };

        debug!(
            job_id = job_info.id,
            candidate_user = %job_info.candidate.user,
            candidate_commit = %job_info.candidate.commit_id,
            "Processing MQ candidate check"
        );

        // Get base branch
        let base_branch = {
            let state = state.lock().unwrap();
            state.base_branch.clone()
        };

        // Resolve base branch to immutable ID
        let resolved_base = match selfci::revision::resolve_revision(&vcs, &root_dir, &base_branch)
        {
            Ok(r) => r,
            Err(e) => {
                job_info.status = mq_protocol::MQJobStatus::Failed;
                job_info.output = format!("Failed to resolve base branch '{}': {}", base_branch, e);
                job_info.completed_at = Some(SystemTime::now());

                let mut state = state.lock().unwrap();
                state.active.remove(&job_id);
                state.completed.push(job_info);
                continue;
            }
        };

        // Determine parallelism (default to 1 for merge queue)
        let parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        // Run the candidate check using the shared implementation
        match super::check::run_candidate_check(
            &root_dir,
            &resolved_base,
            &job_info.candidate,
            parallelism,
            None,
            super::check::CheckMode::MergeQueue,
        ) {
            Ok(result) => {
                // Check if any step failed (non-ignored)
                let has_step_failure = result.steps.iter().any(|step| {
                    matches!(step.status, protocol::StepStatus::Failed { ignored: false })
                });

                // Determine if job passed
                let job_passed = if let Some(exit_code) = result.exit_code {
                    exit_code == 0 && !has_step_failure
                } else {
                    false
                };

                job_info.output = result.output;
                job_info.steps = result.steps;
                job_info.completed_at = Some(SystemTime::now());

                if job_passed {
                    job_info.status = mq_protocol::MQJobStatus::Passed;

                    // Merge into base branch if no_merge is false
                    if job_info.no_merge {
                        debug!(
                            "MQ candidate check {} passed (no-merge mode, skipping merge)",
                            job_info.id
                        );
                    } else {
                        debug!(
                            "MQ candidate check {} passed, merging into {}",
                            job_info.id, base_branch
                        );
                        if let Err(e) =
                            merge_candidate(&root_dir, &base_branch, &job_info.candidate)
                        {
                            job_info.output.push_str(&format!("\nMerge failed: {}", e));
                            job_info.status = mq_protocol::MQJobStatus::Failed;
                        }
                    }
                } else {
                    job_info.status = mq_protocol::MQJobStatus::Failed;
                    debug!("MQ candidate check {} failed", job_info.id);
                }
            }
            Err(e) => {
                job_info.status = mq_protocol::MQJobStatus::Failed;
                job_info.output = format!("Check failed: {}", e);
                job_info.completed_at = Some(SystemTime::now());
                debug!("MQ candidate check {} failed: {}", job_info.id, e);
            }
        }

        // Move job from active to completed
        {
            let mut state = state.lock().unwrap();
            state.active.remove(&job_id);
            state.completed.push(job_info);
        }
    }
}

fn merge_candidate(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<(), String> {
    // Detect VCS
    let vcs = get_vcs(root_dir, None).map_err(|e| format!("VCS error: {}", e))?;

    match vcs {
        selfci::VCS::Git => {
            // Git merge using immutable commit ID
            cmd!("git", "checkout", base_branch)
                .dir(root_dir)
                .run()
                .map_err(|e| format!("Failed to checkout {}: {}", base_branch, e))?;

            cmd!("git", "merge", "--ff-only", candidate.commit_id.as_str())
                .dir(root_dir)
                .run()
                .map_err(|e| {
                    format!(
                        "Failed to merge {} ({}): {}",
                        candidate.user, candidate.commit_id, e
                    )
                })?;

            Ok(())
        }
        selfci::VCS::Jujutsu => {
            // Jujutsu merge - rebase the candidate onto base branch using immutable commit ID
            cmd!(
                "jj",
                "rebase",
                "-r",
                candidate.commit_id.as_str(),
                "-d",
                base_branch
            )
            .dir(root_dir)
            .run()
            .map_err(|e| {
                format!(
                    "Failed to rebase {} ({}) onto {}: {}",
                    candidate.user, candidate.commit_id, base_branch, e
                )
            })?;

            // Move the base branch bookmark to the rebased commit
            cmd!(
                "jj",
                "bookmark",
                "set",
                base_branch,
                "-r",
                candidate.commit_id.as_str()
            )
            .dir(root_dir)
            .run()
            .map_err(|e| format!("Failed to move bookmark {}: {}", base_branch, e))?;

            Ok(())
        }
    }
}

pub fn add_candidate(candidate: String, no_merge: bool) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;
    let socket_path = root_dir.join(".config").join("selfci").join(".mq.sock");

    let response = mq_protocol::send_mq_request(
        &socket_path,
        mq_protocol::MQRequest::AddCandidate { candidate, no_merge },
    ).map_err(|e| {
        eprintln!("Error: {}", e);
        eprintln!("Is the merge queue daemon running? Start it with: selfci mq start --base-branch <branch>");
        MainError::CheckFailed
    })?;

    match response {
        mq_protocol::MQResponse::CandidateAdded { job_id } => {
            if no_merge {
                println!(
                    "Added to merge queue with job ID: {} (no-merge mode)",
                    job_id
                );
            } else {
                println!("Added to merge queue with job ID: {}", job_id);
            }
            Ok(())
        }
        mq_protocol::MQResponse::Error(e) => {
            eprintln!("Error: {}", e);
            Err(MainError::CheckFailed)
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            Err(MainError::CheckFailed)
        }
    }
}

pub fn list_jobs(limit: Option<usize>) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;
    let socket_path = root_dir.join(".config").join("selfci").join(".mq.sock");

    let response =
        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::List { limit })
            .map_err(|e| {
                eprintln!("Error: {}", e);
                eprintln!("Is the merge queue daemon running?");
                MainError::CheckFailed
            })?;

    match response {
        mq_protocol::MQResponse::JobList { jobs } => {
            if jobs.is_empty() {
                println!("No jobs in queue");
            } else {
                println!(
                    "{:<6} {:<10} {:<40} {:<20}",
                    "ID", "Status", "Candidate", "Queued"
                );
                println!("{}", "-".repeat(80));
                for job in jobs {
                    let status = match job.status {
                        mq_protocol::MQJobStatus::Queued => "Queued",
                        mq_protocol::MQJobStatus::Running => "Running",
                        mq_protocol::MQJobStatus::Passed => "Passed",
                        mq_protocol::MQJobStatus::Failed => "Failed",
                    };

                    let queued = humantime::format_rfc3339_seconds(job.queued_at);
                    println!(
                        "{:<6} {:<10} {:<40} {:<20}",
                        job.id,
                        status,
                        job.candidate.user.as_str(),
                        queued
                    );
                }
            }
            Ok(())
        }
        mq_protocol::MQResponse::Error(e) => {
            eprintln!("Error: {}", e);
            Err(MainError::CheckFailed)
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            Err(MainError::CheckFailed)
        }
    }
}

pub fn get_status(job_id: u64) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;
    let socket_path = root_dir.join(".config").join("selfci").join(".mq.sock");

    let response =
        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::GetStatus { job_id })
            .map_err(|e| {
                eprintln!("Error: {}", e);
                eprintln!("Is the merge queue daemon running?");
                MainError::CheckFailed
            })?;

    match response {
        mq_protocol::MQResponse::JobStatus { job: Some(job) } => {
            println!("Run ID: {}", job.id);
            println!(
                "Candidate: {} (commit: {})",
                job.candidate.user, job.candidate.commit_id
            );
            println!("Status: {:?}", job.status);
            println!(
                "Queued at: {}",
                humantime::format_rfc3339_seconds(job.queued_at)
            );

            if let Some(started_at) = job.started_at {
                println!(
                    "Started at: {}",
                    humantime::format_rfc3339_seconds(started_at)
                );
            }

            if let Some(completed_at) = job.completed_at {
                println!(
                    "Completed at: {}",
                    humantime::format_rfc3339_seconds(completed_at)
                );
            }

            if !job.output.is_empty() {
                println!("\nOutput:");
                println!("{}", job.output);
            }

            Ok(())
        }
        mq_protocol::MQResponse::JobStatus { job: None } => {
            eprintln!("Job {} not found", job_id);
            Err(MainError::CheckFailed)
        }
        mq_protocol::MQResponse::Error(e) => {
            eprintln!("Error: {}", e);
            Err(MainError::CheckFailed)
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            Err(MainError::CheckFailed)
        }
    }
}

pub fn stop_daemon() -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;
    let config_dir = root_dir.join(".config").join("selfci");
    let pid_path = config_dir.join(".mq.pid");
    let socket_path = config_dir.join(".mq.sock");

    // Check daemon status
    match is_daemon_running(&pid_path, &socket_path) {
        DaemonStatus::NotRunning => {
            eprintln!("Merge queue daemon is not running");
            Err(MainError::CheckFailed)
        }
        DaemonStatus::Stale => {
            println!("Cleaning up stale daemon files");
            std::fs::remove_file(&pid_path).ok();
            std::fs::remove_file(&socket_path).ok();
            Ok(())
        }
        DaemonStatus::Running(pid) => {
            println!("Stopping merge queue daemon (PID: {})...", pid);

            // Send SIGTERM for graceful shutdown
            if signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM).is_err() {
                eprintln!("Failed to send SIGTERM to process {}", pid);
                return Err(MainError::CheckFailed);
            }

            // Wait for process to exit (with timeout)
            let timeout = std::time::Duration::from_secs(30);
            let start = std::time::Instant::now();

            loop {
                // Check if process still exists
                let exists = signal::kill(Pid::from_raw(pid as i32), None).is_ok();

                if !exists {
                    println!("Daemon stopped successfully");
                    // Clean up files
                    std::fs::remove_file(&pid_path).ok();
                    std::fs::remove_file(&socket_path).ok();
                    return Ok(());
                }

                if start.elapsed() > timeout {
                    eprintln!("Timeout waiting for daemon to stop, sending SIGKILL...");
                    // Send SIGKILL as last resort
                    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
                    std::thread::sleep(std::time::Duration::from_millis(500));

                    // Clean up files
                    std::fs::remove_file(&pid_path).ok();
                    std::fs::remove_file(&socket_path).ok();

                    println!("Daemon forcefully terminated");
                    return Ok(());
                }

                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}
