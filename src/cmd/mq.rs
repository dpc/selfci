use duct::cmd;
use selfci::{MainError, WorkDirError, get_vcs, mq_protocol, protocol};
use std::collections::HashMap;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::SystemTime;
use tracing::debug;

struct MQState {
    base_branch: String,
    next_job_id: u64,
    pending: HashMap<u64, mq_protocol::MQJobInfo>,
    completed: Vec<mq_protocol::MQJobInfo>,
}

pub fn start_daemon(base_branch: String) -> Result<(), MainError> {
    // Determine paths
    let root_dir = std::env::current_dir()
        .map_err(WorkDirError::CreateFailed)?;

    let socket_path = root_dir.join(".config").join("selfci").join(".mq.sock");

    // Check if daemon is already running
    if socket_path.exists() {
        match mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::Hello) {
            Ok(mq_protocol::MQResponse::HelloAck) => {
                println!("Merge queue daemon is already running");
                return Ok(());
            }
            _ => {
                // Socket exists but daemon not responding, remove it
                debug!("Removing stale socket");
                std::fs::remove_file(&socket_path).ok();
            }
        }
    }

    // Create socket directory if needed
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(WorkDirError::CreateFailed)?;
    }

    // Bind socket
    let listener = UnixListener::bind(&socket_path)
        .map_err(WorkDirError::CreateFailed)?;

    println!("Merge queue daemon started for base branch: {}", base_branch);
    println!("Socket: {}", socket_path.display());

    // Initialize state
    let state = Arc::new(Mutex::new(MQState {
        base_branch: base_branch.clone(),
        next_job_id: 1,
        pending: HashMap::new(),
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

        mq_protocol::MQRequest::AddCandidate { candidate, no_merge } => {
            let (job_id, send_result) = {
                let mut state = state.lock().unwrap();
                let job_id = state.next_job_id;
                state.next_job_id += 1;

                let job = mq_protocol::MQJobInfo {
                    id: job_id,
                    candidate: candidate.clone(),
                    status: mq_protocol::MQJobStatus::Queued,
                    queued_at: SystemTime::now(),
                    started_at: None,
                    completed_at: None,
                    output: String::new(),
                    steps: Vec::new(),
                    no_merge,
                };

                state.pending.insert(job_id, job);
                debug!("Added candidate {} to queue with ID {} (no_merge: {})", candidate, job_id, no_merge);

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
            let mut jobs: Vec<_> = state.pending.values()
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
            let job = state.pending.get(&job_id)
                .or_else(|| state.completed.iter().find(|j| j.id == job_id))
                .cloned();

            mq_protocol::MQResponse::JobStatus { job }
        }
    }
}

fn process_queue(
    state: Arc<Mutex<MQState>>,
    root_dir: PathBuf,
    mq_jobs_receiver: mpsc::Receiver<u64>,
) {
    loop {
        // Wait for next job ID from channel
        let job_id = match mq_jobs_receiver.recv() {
            Ok(id) => id,
            Err(_) => {
                debug!("MQ jobs channel closed, exiting process_queue");
                break;
            }
        };

        // Get job info from pending and mark as Running
        let mut job_info = {
            let mut state = state.lock().unwrap();
            match state.pending.remove(&job_id) {
                Some(mut job) => {
                    job.status = mq_protocol::MQJobStatus::Running;
                    job.started_at = Some(SystemTime::now());
                    job
                }
                None => {
                    debug!("Job {} not found in pending map", job_id);
                    continue;
                }
            }
        };

        debug!("Processing MQ candidate check {}: {}", job_info.id, job_info.candidate);

        // Get base branch
        let base_branch = {
            let state = state.lock().unwrap();
            state.base_branch.clone()
        };

        // Determine parallelism (default to 1 for merge queue)
        let parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        // Run the candidate check using the shared implementation
        match super::check::run_candidate_check(
            &root_dir,
            &base_branch,
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
                        debug!("MQ candidate check {} passed (no-merge mode, skipping merge)", job_info.id);
                    } else {
                        debug!("MQ candidate check {} passed, merging into {}", job_info.id, base_branch);
                        if let Err(e) = merge_candidate(&root_dir, &base_branch, &job_info.candidate) {
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

        // Move job to completed
        {
            let mut state = state.lock().unwrap();
            state.completed.push(job_info);
        }
    }
}

fn merge_candidate(root_dir: &Path, base_branch: &str, candidate: &str) -> Result<(), String> {
    // Detect VCS
    let vcs = get_vcs(root_dir, None)
        .map_err(|e| format!("VCS error: {}", e))?;

    match vcs {
        selfci::VCS::Git => {
            // Git merge
            cmd!("git", "checkout", base_branch)
                .dir(root_dir)
                .run()
                .map_err(|e| format!("Failed to checkout {}: {}", base_branch, e))?;

            cmd!("git", "merge", "--ff-only", candidate)
                .dir(root_dir)
                .run()
                .map_err(|e| format!("Failed to merge {}: {}", candidate, e))?;

            Ok(())
        }
        selfci::VCS::Jujutsu => {
            // Jujutsu merge - rebase the candidate onto base branch
            cmd!("jj", "rebase", "-r", candidate, "-d", base_branch)
                .dir(root_dir)
                .run()
                .map_err(|e| format!("Failed to rebase {} onto {}: {}", candidate, base_branch, e))?;

            // Move the base branch bookmark to the rebased commit
            cmd!("jj", "bookmark", "set", base_branch, "-r", candidate)
                .dir(root_dir)
                .run()
                .map_err(|e| format!("Failed to move bookmark {}: {}", base_branch, e))?;

            Ok(())
        }
    }
}

pub fn add_candidate(candidate: String, no_merge: bool) -> Result<(), MainError> {
    let root_dir = std::env::current_dir()
        .map_err(WorkDirError::CreateFailed)?;
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
                println!("Added to merge queue with job ID: {} (no-merge mode)", job_id);
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
    let root_dir = std::env::current_dir()
        .map_err(WorkDirError::CreateFailed)?;
    let socket_path = root_dir.join(".config").join("selfci").join(".mq.sock");

    let response = mq_protocol::send_mq_request(
        &socket_path,
        mq_protocol::MQRequest::List { limit },
    ).map_err(|e| {
        eprintln!("Error: {}", e);
        eprintln!("Is the merge queue daemon running?");
        MainError::CheckFailed
    })?;

    match response {
        mq_protocol::MQResponse::JobList { jobs } => {
            if jobs.is_empty() {
                println!("No jobs in queue");
            } else {
                println!("{:<6} {:<10} {:<40} {:<20}", "ID", "Status", "Candidate", "Queued");
                println!("{}", "-".repeat(80));
                for job in jobs {
                    let status = match job.status {
                        mq_protocol::MQJobStatus::Queued => "Queued",
                        mq_protocol::MQJobStatus::Running => "Running",
                        mq_protocol::MQJobStatus::Passed => "Passed",
                        mq_protocol::MQJobStatus::Failed => "Failed",
                    };

                    let queued = humantime::format_rfc3339_seconds(job.queued_at);
                    println!("{:<6} {:<10} {:<40} {:<20}", job.id, status, job.candidate, queued);
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
    let root_dir = std::env::current_dir()
        .map_err(WorkDirError::CreateFailed)?;
    let socket_path = root_dir.join(".config").join("selfci").join(".mq.sock");

    let response = mq_protocol::send_mq_request(
        &socket_path,
        mq_protocol::MQRequest::GetStatus { job_id },
    ).map_err(|e| {
        eprintln!("Error: {}", e);
        eprintln!("Is the merge queue daemon running?");
        MainError::CheckFailed
    })?;

    match response {
        mq_protocol::MQResponse::JobStatus { job: Some(job) } => {
            println!("Job ID: {}", job.id);
            println!("Candidate: {}", job.candidate);
            println!("Status: {:?}", job.status);
            println!("Queued at: {}", humantime::format_rfc3339_seconds(job.queued_at));

            if let Some(started_at) = job.started_at {
                println!("Started at: {}", humantime::format_rfc3339_seconds(started_at));
            }

            if let Some(completed_at) = job.completed_at {
                println!("Completed at: {}", humantime::format_rfc3339_seconds(completed_at));
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
