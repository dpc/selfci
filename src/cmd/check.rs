use selfci::{
    CheckError, MainError, WorkDirError, copy_revisions_to_workdirs, get_vcs, protocol,
    read_config, revision::ResolvedRevision,
};
use std::collections::HashMap;
use std::fmt::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tracing::{debug, info};

/// Create a temporary directory with "selfci-" prefix for easier identification
fn create_selfci_tempdir() -> Result<tempfile::TempDir, WorkDirError> {
    tempfile::Builder::new()
        .prefix("selfci-")
        .tempdir()
        .map_err(WorkDirError::CreateFailed)
}

/// Mode for running a check - determines output behavior and result handling
pub enum CheckMode {
    /// Interactive check - prints output in real-time to stdout
    Inline { print_output: bool },
    /// Merge queue check - captures all output and returns it
    MergeQueue,
}

/// Result of running a check job
pub struct CheckResult {
    pub output: String,
    pub steps: Vec<protocol::StepLogEntry>,
    pub exit_code: Option<i32>,
    pub duration: Duration,
}

impl CheckMode {
    fn print_output(&self) -> bool {
        match self {
            CheckMode::Inline { print_output } => *print_output,
            CheckMode::MergeQueue => false,
        }
    }
}

/// Run a candidate check - shared implementation for both inline checks and merge queue
///
/// A "candidate check" runs the configured CI command against a candidate revision,
/// starting with the "main" job and potentially spawning additional jobs for parallelism.
pub fn run_candidate_check(
    root_dir: &Path,
    base_rev: &ResolvedRevision,
    candidate_rev: &ResolvedRevision,
    parallelism: usize,
    forced_vcs: Option<&str>,
    mode: CheckMode,
) -> Result<CheckResult, MainError> {
    // Get VCS (forced or auto-detected)
    let vcs = get_vcs(root_dir, forced_vcs)?;
    debug!(vcs = ?vcs, root_dir = %root_dir.display(), forced = forced_vcs.is_some(), "Using VCS");

    // Read config from root directory to get clone mode setting
    let root_config = read_config(root_dir)?;
    debug!(clone_mode = ?root_config.job.clone_mode, "Using clone mode from config");

    // Allocate base work directory
    let base_workdir = create_selfci_tempdir()?;

    debug!(
        base_workdir = %base_workdir.path().display(),
        "Allocated base work directory"
    );

    debug!(
        base_user = %base_rev.user,
        base_commit = %base_rev.commit_id,
        candidate_user = %candidate_rev.user,
        candidate_commit = %candidate_rev.commit_id,
        "Running candidate check"
    );

    // Create a single candidate workdir (shared by all jobs)
    let candidate_workdir = create_selfci_tempdir()?;

    // Copy revisions to workdirs using configured clone mode
    copy_revisions_to_workdirs(
        &vcs,
        root_dir,
        base_workdir.path(),
        &base_rev.commit_id,
        candidate_workdir.path(),
        &candidate_rev.commit_id,
        root_config.job.clone_mode,
    )?;

    // Read config from base workdir to get the actual CI command
    let config = read_config(base_workdir.path())?;
    debug!("Loaded job config from base revision");

    // Create control socket
    let socket_file = tempfile::NamedTempFile::new().map_err(WorkDirError::CreateFailed)?;
    let socket_path = socket_file.path().to_path_buf();
    drop(socket_file);

    let listener = UnixListener::bind(&socket_path).map_err(|_| CheckError::CheckFailed)?;
    debug!(socket_path = %socket_path.display(), "Created control socket");

    // Create channels for jobs (SPMC) and messages (MPSC)
    let (jobs_sender, jobs_receiver) = mpsc::channel::<super::worker::RunJobRequest>();
    let (messages_sender, messages_receiver) = mpsc::channel::<super::worker::JobMessage>();
    let jobs_receiver = Arc::new(Mutex::new(jobs_receiver));

    // Track steps for each job
    let job_steps = Arc::new(Mutex::new(
        HashMap::<String, Vec<protocol::StepLogEntry>>::new(),
    ));

    // Track used job names
    let used_job_names = Arc::new(Mutex::new(std::collections::HashSet::new()));

    // Track job completions (for job wait)
    let job_completions = Arc::new(Mutex::new(HashMap::<String, protocol::JobStatus>::new()));

    // Spawn worker threads
    for _ in 0..parallelism {
        let jobs_rx = Arc::clone(&jobs_receiver);
        let messages_tx = messages_sender.clone();

        std::thread::spawn(move || {
            super::worker::job_worker(jobs_rx, messages_tx);
        });
    }

    // Create shutdown flag for control socket listener
    let listener_shutdown = Arc::new(AtomicBool::new(false));

    // Spawn control socket listener thread
    let job_steps_clone = Arc::clone(&job_steps);
    let used_job_names_clone = Arc::clone(&used_job_names);
    let job_completions_clone = Arc::clone(&job_completions);
    let jobs_sender_clone = jobs_sender.clone();
    let messages_sender_clone = messages_sender.clone();
    let listener_shutdown_clone = Arc::clone(&listener_shutdown);
    let spawn_context = super::worker::JobSpawnContext {
        base_dir: base_workdir.path().to_path_buf(),
        candidate_dir: candidate_workdir.path().to_path_buf(),
        command_prefix: config.job.command_prefix.clone(),
        command: config.job.command.clone(),
        print_output: mode.print_output(),
        socket_path: socket_path.clone(),
    };
    std::thread::spawn(move || {
        super::worker::control_socket_listener(
            listener,
            job_steps_clone,
            used_job_names_clone,
            job_completions_clone,
            jobs_sender_clone,
            messages_sender_clone,
            spawn_context,
            listener_shutdown_clone,
        );
    });

    // Start the "main" job
    {
        let mut used_names = used_job_names.lock().unwrap();
        used_names.insert("main".to_string());
        drop(used_names);

        let mut full_command = config.job.command_prefix.clone();
        full_command.push(config.job.command.clone());

        let job = super::worker::RunJobRequest {
            base_dir: base_workdir.path().to_path_buf(),
            candidate_dir: candidate_workdir.path().to_path_buf(),
            job_name: "main".to_string(),
            job_full_command: full_command,
            print_output: mode.print_output(),
            socket_path: socket_path.clone(),
        };

        jobs_sender.send(job).map_err(|_| CheckError::CheckFailed)?;
    }

    // Drop the original messages sender (workers have their own clones)
    drop(messages_sender);

    // Track running jobs and collect results
    let mut active_jobs = 0;
    let mut total_jobs = 0;
    let mut all_outputs = String::new();
    let mut all_steps = Vec::new();
    let mut any_job_failed = false;
    let check_start = std::time::Instant::now();

    // Track step start times for duration calculation
    let mut step_start_times: HashMap<(String, String), std::time::Instant> = HashMap::new();

    // Helper macro to output based on mode: println for Inline, writeln to buffer for MergeQueue
    macro_rules! output {
        ($($arg:tt)*) => {
            match mode {
                CheckMode::Inline { .. } => println!($($arg)*),
                CheckMode::MergeQueue => { let _ = writeln!(all_outputs, $($arg)*); }
            }
        };
    }

    for message in messages_receiver {
        match message {
            super::worker::JobMessage::Started { job_name } => {
                debug!(job = %job_name, "Started");
                active_jobs += 1;
                total_jobs += 1;
                output!(
                    "[{}/{}] 🚀 started: {}",
                    total_jobs - active_jobs,
                    total_jobs,
                    job_name
                );
            }
            super::worker::JobMessage::StepStarted {
                job_name,
                step_name,
            } => {
                debug!(job = %job_name, step = %step_name, "Step started");
                step_start_times.insert(
                    (job_name.clone(), step_name.clone()),
                    std::time::Instant::now(),
                );
            }
            super::worker::JobMessage::StepCompleted {
                job_name,
                step_name,
                status,
            } => {
                debug!(job = %job_name, step = %step_name, ?status, "Step completed");
                let jobs_completed = total_jobs - active_jobs;
                let duration = step_start_times
                    .remove(&(job_name.clone(), step_name.clone()))
                    .map(|start| start.elapsed())
                    .unwrap_or(Duration::ZERO);
                let emoji = match &status {
                    protocol::StepStatus::Success => "✅",
                    protocol::StepStatus::Failed { ignored: true } => "⚠️",
                    protocol::StepStatus::Failed { ignored: false } => "❌",
                    protocol::StepStatus::Running => "⏳",
                };
                output!(
                    "[{}/{}] {} {}: {} / {} ({:.3}s)",
                    jobs_completed,
                    total_jobs,
                    emoji,
                    if matches!(status, protocol::StepStatus::Success) {
                        "passed"
                    } else {
                        "failed"
                    },
                    job_name,
                    step_name,
                    duration.as_secs_f64()
                );
            }
            super::worker::JobMessage::Completed(mut outcome) => {
                debug!(job = %outcome.job_name, exit_code = ?outcome.exit_code, "completed");

                // Look up steps for this job and mark Running steps as Success
                {
                    let steps_map = job_steps.lock().unwrap();
                    if let Some(steps) = steps_map.get(&outcome.job_name) {
                        outcome.steps = steps
                            .iter()
                            .map(|step| {
                                let mut step = step.clone();
                                if matches!(step.status, protocol::StepStatus::Running) {
                                    step.status = protocol::StepStatus::Success;
                                }
                                step
                            })
                            .collect();
                    }
                }

                // Output completion for the last running step (if any)
                if let Some(last_step) = outcome.steps.last() {
                    // Check if we have a start time for this step (meaning it wasn't already completed)
                    let key = (outcome.job_name.clone(), last_step.name.clone());
                    if let Some(start) = step_start_times.remove(&key) {
                        let duration = start.elapsed();
                        let jobs_completed = total_jobs - active_jobs;
                        let emoji = match &last_step.status {
                            protocol::StepStatus::Success => "✅",
                            protocol::StepStatus::Failed { ignored: true } => "⚠️",
                            protocol::StepStatus::Failed { ignored: false } => "❌",
                            protocol::StepStatus::Running => "⏳",
                        };
                        output!(
                            "[{}/{}] {} {}: {} / {} ({:.3}s)",
                            jobs_completed,
                            total_jobs,
                            emoji,
                            if matches!(last_step.status, protocol::StepStatus::Success) {
                                "passed"
                            } else {
                                "failed"
                            },
                            outcome.job_name,
                            last_step.name,
                            duration.as_secs_f64()
                        );
                    }
                }

                all_steps.extend(outcome.steps.clone());

                let has_failed_step = outcome.steps.iter().any(|step| {
                    matches!(step.status, protocol::StepStatus::Failed { ignored: false })
                });

                let job_failed = match outcome.exit_code {
                    Some(code) => code != 0 || has_failed_step,
                    None => true,
                };

                if job_failed {
                    any_job_failed = true;
                }

                // Record job completion status for wait command
                {
                    let mut completions = job_completions.lock().unwrap();
                    completions.insert(
                        outcome.job_name.clone(),
                        if job_failed {
                            protocol::JobStatus::Failed
                        } else {
                            protocol::JobStatus::Succeeded
                        },
                    );
                }

                // Output job completion status
                let jobs_completed = total_jobs - active_jobs + 1;
                let duration_secs = outcome.duration.as_secs_f64();
                let emoji = if job_failed { "❌" } else { "✅" };

                if job_failed {
                    let reason = if has_failed_step {
                        "step failure"
                    } else if let Some(code) = outcome.exit_code {
                        &format!("exit code: {}", code)
                    } else {
                        "no exit code"
                    };
                    output!(
                        "[{}/{}] {} failed: {} ({}, {:.3}s)",
                        jobs_completed,
                        total_jobs,
                        emoji,
                        outcome.job_name,
                        reason,
                        duration_secs
                    );
                } else {
                    output!(
                        "[{}/{}] {} passed: {} ({:.3}s)",
                        jobs_completed,
                        total_jobs,
                        emoji,
                        outcome.job_name,
                        duration_secs
                    );
                }

                // Output command output with header/trailer
                let should_output = match mode {
                    CheckMode::MergeQueue => true,
                    CheckMode::Inline { print_output } => print_output || job_failed,
                };
                if should_output && !outcome.output.is_empty() {
                    output!("--- output: {} ---", outcome.job_name);
                    // For MergeQueue, append to buffer; for Inline, print directly
                    match mode {
                        CheckMode::MergeQueue => all_outputs.push_str(&outcome.output),
                        CheckMode::Inline { .. } => print!("{}", outcome.output),
                    }
                    output!("--- end output ---");
                }

                active_jobs -= 1;
                if active_jobs == 0 {
                    break;
                }
            }
        }
    }

    // Signal control socket listener to shut down and wake it up
    listener_shutdown.store(true, Ordering::SeqCst);
    let _ = UnixStream::connect(&socket_path); // Wake up blocking accept()

    // Drop the jobs sender to close the channel and let workers exit
    drop(jobs_sender);

    // Clean up socket
    let _ = std::fs::remove_file(&socket_path);

    // Return aggregated result from all jobs
    let total_duration = check_start.elapsed();

    Ok(CheckResult {
        output: all_outputs,
        steps: all_steps,
        exit_code: if any_job_failed { Some(1) } else { Some(0) },
        duration: total_duration,
    })
}

pub fn check(
    root: Option<String>,
    base: Option<String>,
    candidate: Option<String>,
    print_output: bool,
    jobs: Option<usize>,
    forced_vcs: Option<&str>,
) -> Result<(), MainError> {
    // Determine root directory
    let root_dir = root
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("Failed to get current directory"));

    // Get VCS (forced or auto-detected)
    let vcs = get_vcs(&root_dir, forced_vcs)?;
    debug!(vcs = ?vcs, root_dir = %root_dir.display(), forced = forced_vcs.is_some(), "Using VCS");

    // Use VCS-specific defaults for candidate if not provided
    let candidate_rev_str = candidate.as_deref().unwrap_or(match vcs {
        selfci::VCS::Jujutsu => "@",
        selfci::VCS::Git => "HEAD",
    });
    // Base defaults to the same as candidate if not provided
    let base_rev_str = base.as_deref().unwrap_or(candidate_rev_str);

    // Resolve revisions to immutable IDs
    let resolved_base = selfci::revision::resolve_revision(&vcs, &root_dir, base_rev_str)?;
    let resolved_candidate =
        selfci::revision::resolve_revision(&vcs, &root_dir, candidate_rev_str)?;

    debug!(
        base_user = %resolved_base.user,
        base_commit = %resolved_base.commit_id,
        candidate_user = %resolved_candidate.user,
        candidate_commit = %resolved_candidate.commit_id,
        "Resolved revisions"
    );

    // Log the start of the check with candidate info
    info!(
        candidate = %resolved_candidate.user,
        commit = &resolved_candidate.commit_id.as_str()[..8],
        "Starting check"
    );
    // Determine parallelism level
    let parallelism = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    debug!(parallelism, "Using parallelism level");

    debug!("Running jobs with parallelism {}", parallelism);

    // Run the candidate check using shared implementation
    let result = run_candidate_check(
        &root_dir,
        &resolved_base,
        &resolved_candidate,
        parallelism,
        forced_vcs,
        CheckMode::Inline { print_output },
    )?;

    // Print total time
    debug!(
        secs = format!("{:.3}s", result.duration.as_secs_f64()),
        "Total time",
    );

    // Check if any step failed (non-ignored)
    let has_step_failure = result
        .steps
        .iter()
        .any(|step| matches!(step.status, protocol::StepStatus::Failed { ignored: false }));

    // Determine if check passed
    let check_passed = if let Some(exit_code) = result.exit_code {
        exit_code == 0 && !has_step_failure
    } else {
        false
    };

    // Report failure
    if !check_passed {
        return Err(CheckError::CheckFailed.into());
    }

    debug!("All jobs succeeded");

    Ok(())
}
