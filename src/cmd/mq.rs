use duct::cmd;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use selfci::{MainError, WorkDirError, envs, get_vcs, mq_protocol, protocol};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::SystemTime;
use tracing::debug;

/// Get the selfci runtime directory for auto-discovery mode
/// Returns $XDG_RUNTIME_DIR/selfci, falling back to /tmp/selfci-{uid}
fn get_selfci_runtime_dir() -> Result<PathBuf, MainError> {
    let base = dirs::runtime_dir().unwrap_or_else(|| {
        // Fallback to /tmp/selfci-{uid} if XDG_RUNTIME_DIR not available
        let uid = nix::unistd::getuid();
        PathBuf::from(format!("/tmp/selfci-{}", uid))
    });

    Ok(base.join("selfci"))
}

/// Get the daemon runtime directory for this project
/// Returns explicit dir if SELFCI_MQ_RUNTIME_DIR is set, otherwise searches for daemon
fn get_daemon_runtime_dir(project_root: &Path) -> Result<Option<PathBuf>, MainError> {
    // Mode 1: Explicit runtime directory
    if let Ok(explicit_dir) = std::env::var(envs::SELFCI_MQ_RUNTIME_DIR) {
        let dir = PathBuf::from(explicit_dir);

        // Verify it's for our project (if initialized)
        let dir_file = dir.join("mq.dir");
        if dir_file.exists() {
            let stored_root =
                std::fs::read_to_string(&dir_file).map_err(WorkDirError::CreateFailed)?;
            if Path::new(stored_root.trim()) == project_root {
                return Ok(Some(dir));
            } else {
                return Ok(None); // Wrong project
            }
        } else {
            // Not initialized yet, return the directory
            return Ok(Some(dir));
        }
    }

    // Mode 2: Auto-discovery - scan PID directories
    let runtime_dir = get_selfci_runtime_dir()?;
    if !runtime_dir.exists() {
        return Ok(None);
    }

    for entry in std::fs::read_dir(&runtime_dir).map_err(WorkDirError::CreateFailed)? {
        let entry = entry.map_err(WorkDirError::CreateFailed)?;
        let pid_dir = entry.path();

        // Read mq.dir to check project match
        let dir_file = pid_dir.join("mq.dir");
        let stored_root = match std::fs::read_to_string(&dir_file) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if Path::new(stored_root.trim()) == project_root {
            // Found matching project, verify daemon is running
            if verify_daemon_running(&pid_dir)? {
                return Ok(Some(pid_dir));
            } else {
                // Stale daemon, clean up
                std::fs::remove_dir_all(&pid_dir).ok();
            }
        }
    }

    Ok(None)
}

/// Verify a daemon is actually running in the given directory
fn verify_daemon_running(daemon_dir: &Path) -> Result<bool, MainError> {
    let pid_file = daemon_dir.join("mq.pid");
    let socket_path = daemon_dir.join("mq.sock");

    // Read PID
    let pid = match std::fs::read_to_string(&pid_file) {
        Ok(content) => match content.trim().parse::<u32>() {
            Ok(p) => p,
            Err(_) => return Ok(false),
        },
        Err(_) => return Ok(false),
    };

    // Check process exists
    if signal::kill(Pid::from_raw(pid as i32), None).is_err() {
        return Ok(false);
    }

    // Verify socket responds
    if socket_path.exists() {
        match mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::Hello) {
            Ok(mq_protocol::MQResponse::HelloAck) => Ok(true),
            _ => Ok(false),
        }
    } else {
        Ok(false)
    }
}

struct MQState {
    root_dir: PathBuf,
    base_branch: String,
    next_job_id: u64,
    queued: HashMap<u64, mq_protocol::MQJobInfo>,
    active: HashMap<u64, mq_protocol::MQJobInfo>,
    completed: Vec<mq_protocol::MQJobInfo>,
}

pub fn start_daemon(
    base_branch: Option<String>,
    foreground: bool,
    log_file: Option<PathBuf>,
) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    // Read config to get default base branch if not specified
    let base_branch = if let Some(branch) = base_branch {
        branch
    } else {
        // Try to read config from current directory
        match selfci::config::read_config(&root_dir) {
            Ok(config) => {
                if let Some(mq_config) = config.mq {
                    if let Some(branch) = mq_config.base_branch {
                        branch
                    } else {
                        eprintln!(
                            "Error: --base-branch not specified and mq.base-branch not set in config"
                        );
                        eprintln!(
                            "Either provide --base-branch or set mq.base-branch in .config/selfci/ci.yaml"
                        );
                        return Err(MainError::CheckFailed);
                    }
                } else {
                    eprintln!(
                        "Error: --base-branch not specified and mq.base-branch not set in config"
                    );
                    eprintln!(
                        "Either provide --base-branch or set mq.base-branch in .config/selfci/ci.yaml"
                    );
                    return Err(MainError::CheckFailed);
                }
            }
            Err(e) => {
                eprintln!(
                    "Error: --base-branch not specified and failed to read config: {}",
                    e
                );
                eprintln!(
                    "Either provide --base-branch or set mq.base-branch in .config/selfci/ci.yaml"
                );
                return Err(MainError::CheckFailed);
            }
        }
    };

    // Check if daemon is already running for this project
    if let Some(existing_dir) = get_daemon_runtime_dir(&root_dir)?
        && let Ok(pid_str) = std::fs::read_to_string(existing_dir.join("mq.pid"))
        && let Ok(pid) = pid_str.trim().parse::<u32>()
    {
        println!("Merge queue daemon is already running (PID: {})", pid);
        println!("Runtime directory: {}", existing_dir.display());
        println!("Use 'selfci mq stop' to stop it");
        return Ok(());
    }

    // Determine initial daemon directory (may be modified after fork in background mode)
    let daemon_dir_initial = if let Ok(explicit_dir) = std::env::var(envs::SELFCI_MQ_RUNTIME_DIR) {
        // Mode 1: Use explicit directory
        PathBuf::from(explicit_dir)
    } else {
        // Mode 2: Will determine after getting PID
        PathBuf::new()
    };

    // Daemonize and set up runtime directory
    let daemon_dir = if !foreground {
        // Background mode - manual fork so we can report PID back to parent
        println!("Base branch: {}", base_branch);

        // Create a pipe for the child to send its info back to the parent
        let (reader, mut writer) =
            std::os::unix::net::UnixStream::pair().map_err(WorkDirError::CreateFailed)?;

        // Manual fork (instead of using daemonize crate, so parent can read from pipe)
        use nix::unistd::{ForkResult, fork, setsid};
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child: _ }) => {
                // Parent process - read daemon dir from pipe and print it
                drop(writer);

                use std::io::BufRead;
                let reader = std::io::BufReader::new(reader);
                if let Ok(Some(daemon_dir)) = reader.lines().next().transpose() {
                    println!("Runtime directory: {}", daemon_dir);
                }
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("Failed to fork: {}", e);
                return Err(MainError::CheckFailed);
            }
            Ok(ForkResult::Child) => {
                // Child process - become session leader and continue as daemon
                drop(reader);

                // Become session leader
                setsid().map_err(|_| {
                    WorkDirError::CreateFailed(std::io::Error::other(
                        "Failed to become session leader",
                    ))
                })?;

                // Close stdin
                use nix::unistd::close;
                close(0).ok();

                // Change to working directory
                std::env::set_current_dir(&root_dir).map_err(WorkDirError::CreateFailed)?;

                // Set umask
                use nix::sys::stat::{Mode, umask};
                umask(Mode::from_bits_truncate(0o027));

                let pid = std::process::id();
                let daemon_dir = if daemon_dir_initial.as_os_str().is_empty() {
                    // Auto mode: create PID-based directory
                    get_selfci_runtime_dir()?.join(pid.to_string())
                } else {
                    // Explicit mode: use provided directory
                    daemon_dir_initial
                };

                // Send daemon directory path back to parent
                use std::io::Write;
                let _ = writeln!(writer, "{}", daemon_dir.display());
                drop(writer); // Close pipe so parent can exit

                // Create directory and initialize (BEFORE redirecting stderr so errors are visible)
                if let Err(e) = std::fs::create_dir_all(&daemon_dir) {
                    eprintln!(
                        "ERROR: Failed to create daemon directory {}: {}",
                        daemon_dir.display(),
                        e
                    );
                    eprintln!("Daemon startup failed - check permissions");
                    return Err(WorkDirError::CreateFailed(e).into());
                }

                std::fs::write(
                    daemon_dir.join("mq.dir"),
                    root_dir.to_string_lossy().as_bytes(),
                )
                .map_err(WorkDirError::CreateFailed)?;
                std::fs::write(daemon_dir.join("mq.pid"), pid.to_string())
                    .map_err(WorkDirError::CreateFailed)?;

                // Set up log file redirection
                let log_path = log_file.unwrap_or_else(|| daemon_dir.join("mq.log"));
                let log_file_handle =
                    match OpenOptions::new().create(true).append(true).open(&log_path) {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!(
                                "ERROR: Failed to open log file {}: {}",
                                log_path.display(),
                                e
                            );
                            return Err(WorkDirError::CreateFailed(e).into());
                        }
                    };

                // Redirect stdout/stderr using nix
                use nix::unistd::dup2;
                use std::os::unix::io::IntoRawFd;
                // Transfer ownership of the fd so we can close it without double-close
                let log_fd = log_file_handle.into_raw_fd();
                dup2(log_fd, 1).map_err(|_| {
                    WorkDirError::CreateFailed(std::io::Error::other("Failed to redirect stdout"))
                })?;
                dup2(log_fd, 2).map_err(|_| {
                    WorkDirError::CreateFailed(std::io::Error::other("Failed to redirect stderr"))
                })?;
                close(log_fd).ok();

                // Now stderr/stdout go to log file
                eprintln!("Daemon process started successfully");
                eprintln!("PID: {}", pid);
                eprintln!("Runtime directory: {}", daemon_dir.display());
                debug!("Daemon initialization complete");
                daemon_dir
            }
        }
    } else {
        // Foreground mode - simpler
        let pid = std::process::id();
        let daemon_dir = if daemon_dir_initial.as_os_str().is_empty() {
            get_selfci_runtime_dir()?.join(pid.to_string())
        } else {
            daemon_dir_initial
        };

        std::fs::create_dir_all(&daemon_dir).map_err(WorkDirError::CreateFailed)?;
        std::fs::write(
            daemon_dir.join("mq.dir"),
            root_dir.to_string_lossy().as_bytes(),
        )
        .map_err(WorkDirError::CreateFailed)?;
        std::fs::write(daemon_dir.join("mq.pid"), pid.to_string())
            .map_err(WorkDirError::CreateFailed)?;

        println!(
            "Merge queue daemon started for base branch: {}",
            base_branch
        );
        println!("Runtime directory: {}", daemon_dir.display());
        daemon_dir
    };

    // Set up cleanup on exit - remove entire daemon directory
    let daemon_dir_cleanup = daemon_dir.clone();
    let _guard = scopeguard::guard((), move |_| {
        std::fs::remove_dir_all(&daemon_dir_cleanup).ok();
    });

    // Bind socket
    let socket_path = daemon_dir.join("mq.sock");
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
                        match merge_candidate(&root_dir, &base_branch, &job_info.candidate) {
                            Ok(merge_log) => {
                                // Append merge output with separator
                                job_info.output.push_str("\n\n=== Merge Output ===\n");
                                job_info.output.push_str(&merge_log);
                            }
                            Err(e) => {
                                job_info
                                    .output
                                    .push_str(&format!("\n\n=== Merge Failed ===\n{}", e));
                                job_info.status = mq_protocol::MQJobStatus::Failed;
                            }
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
) -> Result<String, String> {
    let mut merge_log = String::new();
    // Detect VCS first (needed to read config from base branch)
    let vcs = get_vcs(root_dir, None).map_err(|e| format!("VCS error: {}", e))?;

    // Read config from base branch (not from local working directory which could have uncommitted changes)
    let config_path = {
        let mut path = selfci::constants::CONFIG_DIR_PATH.join("/");
        path.push('/');
        path.push_str(selfci::constants::CONFIG_FILENAME);
        path
    };

    let config_content = match vcs {
        selfci::VCS::Git => {
            // Read config from base branch using git show
            cmd!("git", "show", format!("{}:{}", base_branch, config_path))
                .dir(root_dir)
                .read()
                .map_err(|e| format!("Failed to read config from {}: {}", base_branch, e))?
        }
        selfci::VCS::Jujutsu => {
            // Read config from base branch using jj cat
            cmd!("jj", "cat", "-r", base_branch, &config_path)
                .dir(root_dir)
                .read()
                .map_err(|e| format!("Failed to read config from {}: {}", base_branch, e))?
        }
    };

    let config: selfci::config::SelfCIConfig = serde_yaml::from_str(&config_content)
        .map_err(|e| format!("Failed to parse config: {}", e))?;

    let merge_style = config
        .mq
        .as_ref()
        .map(|mq| &mq.merge_style)
        .unwrap_or(&selfci::config::MergeStyle::Rebase);

    match (vcs, merge_style) {
        (selfci::VCS::Git, selfci::config::MergeStyle::Rebase) => {
            // Git rebase mode - use a temporary worktree to avoid touching user's working directory
            let temp_worktree =
                root_dir.join(format!(".git/selfci-worktree-{}", candidate.commit_id));

            merge_log.push_str(&format!(
                "Git rebase mode: rebasing {} onto {}\n",
                candidate.commit_id, base_branch
            ));

            // Create temporary worktree in detached HEAD state at candidate commit
            merge_log.push_str(&format!(
                "Creating temporary worktree at {}\n",
                temp_worktree.display()
            ));
            let output = cmd!(
                "git",
                "worktree",
                "add",
                "--detach",
                &temp_worktree,
                candidate.commit_id.as_str()
            )
            .dir(root_dir)
            .stderr_to_stdout()
            .read()
            .map_err(|e| format!("Failed to create temporary worktree: {}", e))?;
            merge_log.push_str(&output);
            merge_log.push('\n');

            // Ensure cleanup on any exit path
            let cleanup = scopeguard::guard((), |_| {
                let _ = cmd!("git", "worktree", "remove", "--force", &temp_worktree)
                    .dir(root_dir)
                    .run();
            });

            // In the worktree, rebase onto base_branch
            merge_log.push_str(&format!("Rebasing onto {}\n", base_branch));
            let output = cmd!("git", "rebase", base_branch)
                .dir(&temp_worktree)
                .stderr_to_stdout()
                .read()
                .map_err(|e| {
                    format!(
                        "Failed to rebase {} ({}) onto {}: {}",
                        candidate.user, candidate.commit_id, base_branch, e
                    )
                })?;
            merge_log.push_str(&output);
            merge_log.push('\n');

            // Update base_branch to point to the rebased commits (HEAD in worktree)
            merge_log.push_str(&format!("Updating {} to rebased commits\n", base_branch));
            let output = cmd!(
                "git",
                "update-ref",
                format!("refs/heads/{}", base_branch),
                "HEAD"
            )
            .dir(&temp_worktree)
            .stderr_to_stdout()
            .read()
            .map_err(|e| format!("Failed to update {}: {}", base_branch, e))?;
            merge_log.push_str(&output);
            merge_log.push('\n');

            // Cleanup is handled by scopeguard
            drop(cleanup);

            merge_log.push_str("Rebase completed successfully\n");
            Ok(merge_log)
        }
        (selfci::VCS::Git, selfci::config::MergeStyle::Merge) => {
            // Git merge mode - use a temporary worktree to avoid touching user's working directory
            let temp_worktree =
                root_dir.join(format!(".git/selfci-worktree-{}", candidate.commit_id));

            merge_log.push_str(&format!(
                "Git merge mode: merging {} into {}\n",
                candidate.commit_id, base_branch
            ));

            // Create temporary worktree in detached HEAD state at base branch
            merge_log.push_str(&format!(
                "Creating temporary worktree at {}\n",
                temp_worktree.display()
            ));
            let output = cmd!(
                "git",
                "worktree",
                "add",
                "--detach",
                &temp_worktree,
                base_branch
            )
            .dir(root_dir)
            .stderr_to_stdout()
            .read()
            .map_err(|e| format!("Failed to create temporary worktree: {}", e))?;
            merge_log.push_str(&output);
            merge_log.push('\n');

            // Ensure cleanup on any exit path
            let cleanup = scopeguard::guard((), |_| {
                let _ = cmd!("git", "worktree", "remove", "--force", &temp_worktree)
                    .dir(root_dir)
                    .run();
            });

            // In the worktree, merge candidate
            merge_log.push_str(&format!("Merging {} with --no-ff\n", candidate.commit_id));
            let output = cmd!("git", "merge", "--no-ff", candidate.commit_id.as_str())
                .dir(&temp_worktree)
                .stderr_to_stdout()
                .read()
                .map_err(|e| {
                    format!(
                        "Failed to merge {} ({}): {}",
                        candidate.user, candidate.commit_id, e
                    )
                })?;
            merge_log.push_str(&output);
            merge_log.push('\n');

            // Update base_branch to point to the merge commit (HEAD in worktree)
            merge_log.push_str(&format!("Updating {} to merge commit\n", base_branch));
            let output = cmd!(
                "git",
                "update-ref",
                format!("refs/heads/{}", base_branch),
                "HEAD"
            )
            .dir(&temp_worktree)
            .stderr_to_stdout()
            .read()
            .map_err(|e| format!("Failed to update {}: {}", base_branch, e))?;
            merge_log.push_str(&output);
            merge_log.push('\n');

            // Cleanup is handled by scopeguard
            drop(cleanup);

            merge_log.push_str("Merge completed successfully\n");
            Ok(merge_log)
        }
        (selfci::VCS::Jujutsu, selfci::config::MergeStyle::Rebase) => {
            // Jujutsu rebase mode - rebase candidate branch onto base branch
            merge_log.push_str(&format!(
                "Jujutsu rebase mode: rebasing {} onto {}\n",
                candidate.commit_id, base_branch
            ));

            // Get the change ID of the candidate (stable across rebases)
            merge_log.push_str("Getting change ID of candidate\n");
            let change_id = cmd!(
                "jj",
                "log",
                "-r",
                candidate.commit_id.as_str(),
                "-T",
                "change_id",
                "--no-graph",
                "--color=never"
            )
            .dir(root_dir)
            .read()
            .map_err(|e| format!("Failed to get change ID: {}", e))?
            .trim()
            .to_string();
            merge_log.push_str(&format!("Change ID: {}\n", change_id));

            // Use -b (branch) to rebase the candidate and its ancestors (that aren't in base) onto base branch
            merge_log.push_str("Rebasing branch\n");
            let output = cmd!(
                "jj",
                "rebase",
                "-b",
                candidate.commit_id.as_str(),
                "-d",
                base_branch
            )
            .dir(root_dir)
            .stderr_to_stdout()
            .read()
            .map_err(|e| {
                format!(
                    "Failed to rebase {} ({}) onto {}: {}",
                    candidate.user, candidate.commit_id, base_branch, e
                )
            })?;
            merge_log.push_str(&output);
            merge_log.push('\n');

            // Move the base branch bookmark to the rebased commit using change ID
            merge_log.push_str(&format!(
                "Moving {} bookmark to rebased commit\n",
                base_branch
            ));
            let output = cmd!("jj", "bookmark", "set", base_branch, "-r", &change_id)
                .dir(root_dir)
                .stderr_to_stdout()
                .read()
                .map_err(|e| format!("Failed to move bookmark {}: {}", base_branch, e))?;
            merge_log.push_str(&output);
            merge_log.push('\n');

            merge_log.push_str("Rebase completed successfully\n");
            Ok(merge_log)
        }
        (selfci::VCS::Jujutsu, selfci::config::MergeStyle::Merge) => {
            // Jujutsu merge mode - create a merge commit
            merge_log.push_str(&format!(
                "Jujutsu merge mode: creating merge commit of {} into {}\n",
                candidate.commit_id, base_branch
            ));

            // Create a new merge commit with both base and candidate as parents
            merge_log.push_str("Creating merge commit\n");
            let output = cmd!("jj", "new", base_branch, candidate.commit_id.as_str())
                .dir(root_dir)
                .stderr_to_stdout()
                .read()
                .map_err(|e| {
                    format!(
                        "Failed to create merge commit of {} ({}) into {}: {}",
                        candidate.user, candidate.commit_id, base_branch, e
                    )
                })?;
            merge_log.push_str(&output);
            merge_log.push('\n');

            // Move the base branch bookmark to this new merge commit
            merge_log.push_str(&format!(
                "Moving {} bookmark to merge commit\n",
                base_branch
            ));
            let output = cmd!("jj", "bookmark", "set", base_branch, "-r", "@")
                .dir(root_dir)
                .stderr_to_stdout()
                .read()
                .map_err(|e| format!("Failed to move bookmark {}: {}", base_branch, e))?;
            merge_log.push_str(&output);
            merge_log.push('\n');

            merge_log.push_str("Merge completed successfully\n");
            Ok(merge_log)
        }
    }
}

pub fn add_candidate(candidate: String, no_merge: bool) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let daemon_dir = get_daemon_runtime_dir(&root_dir)?.ok_or_else(|| {
        eprintln!("Merge queue daemon is not running for this project");
        eprintln!("Start it with: selfci mq start --base-branch <branch>");
        MainError::CheckFailed
    })?;

    let socket_path = daemon_dir.join("mq.sock");

    let response = mq_protocol::send_mq_request(
        &socket_path,
        mq_protocol::MQRequest::AddCandidate {
            candidate,
            no_merge,
        },
    )
    .map_err(|e| {
        eprintln!("Error: {}", e);
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

    let daemon_dir = get_daemon_runtime_dir(&root_dir)?.ok_or_else(|| {
        eprintln!("Merge queue daemon is not running for this project");
        MainError::CheckFailed
    })?;

    let socket_path = daemon_dir.join("mq.sock");

    let response =
        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::List { limit })
            .map_err(|e| {
                eprintln!("Error: {}", e);
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

    let daemon_dir = get_daemon_runtime_dir(&root_dir)?.ok_or_else(|| {
        eprintln!("Merge queue daemon is not running for this project");
        MainError::CheckFailed
    })?;

    let socket_path = daemon_dir.join("mq.sock");

    let response =
        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::GetStatus { job_id })
            .map_err(|e| {
                eprintln!("Error: {}", e);
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

    // Find daemon directory
    let daemon_dir = get_daemon_runtime_dir(&root_dir)?.ok_or_else(|| {
        eprintln!("Merge queue daemon is not running for this project");
        MainError::CheckFailed
    })?;

    let pid_file = daemon_dir.join("mq.pid");

    // Read PID
    let pid = match std::fs::read_to_string(&pid_file) {
        Ok(content) => match content.trim().parse::<u32>() {
            Ok(p) => p,
            Err(_) => {
                println!("Invalid PID file, cleaning up");
                std::fs::remove_dir_all(&daemon_dir).ok();
                return Ok(());
            }
        },
        Err(_) => {
            println!("PID file not found, cleaning up stale directory");
            std::fs::remove_dir_all(&daemon_dir).ok();
            return Ok(());
        }
    };

    // Check if process exists
    if signal::kill(Pid::from_raw(pid as i32), None).is_err() {
        println!("Process not running, cleaning up");
        std::fs::remove_dir_all(&daemon_dir).ok();
        return Ok(());
    }

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
            std::fs::remove_dir_all(&daemon_dir).ok();
            return Ok(());
        }

        if start.elapsed() > timeout {
            eprintln!("Timeout waiting for daemon to stop, sending SIGKILL...");
            // Send SIGKILL as last resort
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            std::thread::sleep(std::time::Duration::from_millis(500));

            std::fs::remove_dir_all(&daemon_dir).ok();
            println!("Daemon forcefully terminated");
            return Ok(());
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
