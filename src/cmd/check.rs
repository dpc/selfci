use duct::cmd;
use selfci::{
    CheckError, MainError, WorkDirError, config::CommandConfig, copy_revisions_to_workdirs,
    get_vcs, protocol, read_config, revision::ResolvedRevision,
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
    /// Post-clone hook output (if hook was run)
    pub post_clone_output: Option<String>,
    /// Whether post-clone hook succeeded (None if no hook was run)
    pub post_clone_success: Option<bool>,
}

/// Configuration for running a post-clone hook
pub struct PostCloneHookConfig<'a> {
    /// The hook command configuration
    pub hook: &'a CommandConfig,
    /// Candidate commit ID (original user-submitted commit)
    pub candidate_commit_id: &'a str,
    /// Candidate change ID (original jj change ID)
    pub candidate_change_id: &'a str,
    /// Candidate ID (user-provided revision string)
    pub candidate_id: &'a str,
    /// Base branch name
    pub base_branch: &'a str,
    /// Merged commit ID (MQ only: commit after test merge/rebase)
    pub merged_commit_id: Option<&'a str>,
    /// Merged change ID (MQ only: jj change ID after test merge/rebase)
    pub merged_change_id: Option<&'a str>,
}

impl CheckMode {
    fn print_output(&self) -> bool {
        match self {
            CheckMode::Inline { print_output } => *print_output,
            CheckMode::MergeQueue => false,
        }
    }
}

/// Run the post-clone hook with environment variables set
/// Returns (output, success)
fn run_post_clone_hook(
    hook_config: &PostCloneHookConfig<'_>,
    root_dir: &Path,
    base_dir: &Path,
    candidate_dir: &Path,
) -> (String, bool) {
    use selfci::envs;

    if !hook_config.hook.is_set() {
        return (String::new(), true);
    }

    let full_command = hook_config.hook.full_command();
    debug!(command = ?full_command, "Running post-clone hook");

    let mut command = cmd(&full_command[0], &full_command[1..])
        .dir(root_dir)
        .env(envs::SELFCI_VERSION, env!("CARGO_PKG_VERSION"))
        .env(envs::SELFCI_BASE_DIR, base_dir)
        .env(envs::SELFCI_CANDIDATE_DIR, candidate_dir)
        .env(
            envs::SELFCI_CANDIDATE_COMMIT_ID,
            hook_config.candidate_commit_id,
        )
        .env(
            envs::SELFCI_CANDIDATE_CHANGE_ID,
            hook_config.candidate_change_id,
        )
        .env(envs::SELFCI_CANDIDATE_ID, hook_config.candidate_id)
        .env(envs::SELFCI_MQ_BASE_BRANCH, hook_config.base_branch);

    // Add merged env vars if present (MQ mode only)
    if let Some(merged_commit_id) = hook_config.merged_commit_id {
        command = command.env(envs::SELFCI_MERGED_COMMIT_ID, merged_commit_id);
    }
    if let Some(merged_change_id) = hook_config.merged_change_id {
        command = command.env(envs::SELFCI_MERGED_CHANGE_ID, merged_change_id);
    }

    let result = command
        .stderr_to_stdout()
        .stdout_capture()
        .unchecked()
        .run();

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let success = output.status.success();
            (stdout, success)
        }
        Err(e) => (format!("Hook execution error: {}", e), false),
    }
}

/// Run a candidate check - shared implementation for both inline checks and merge queue
///
/// A "candidate check" runs the configured CI command against a candidate revision,
/// starting with the "main" job and potentially spawning additional jobs for parallelism.
///
/// If `post_clone_hook` is provided, it will be run after worktrees are created but before
/// the job starts. The hook receives environment variables for the worktree paths.
///
/// The `original_candidate` parameter is used in MQ mode to distinguish between:
/// - The original candidate (what the user submitted) -> SELFCI_CANDIDATE_* env vars
/// - The working candidate (after test merge/rebase) -> SELFCI_MERGED_* env vars
///
/// In regular check mode, `original_candidate` is None and SELFCI_MERGED_* is not set.

/// State for tracking jobs during check execution.
#[derive(Default)]
pub struct JobStates {
    /// Steps for each job - key is job name
    pub steps: HashMap<String, Vec<protocol::StepLogEntry>>,
    /// Completion status for each job
    pub completions: HashMap<String, protocol::JobStatus>,
}

/// Shared state for tracking jobs during check execution.
/// Can be passed in to allow external monitoring of job progress.
#[derive(Clone)]
pub struct SharedJobStates(Arc<Mutex<JobStates>>);

impl SharedJobStates {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(JobStates::default())))
    }

    /// Access the inner state with a closure
    pub fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&JobStates) -> R,
    {
        let guard = self.0.lock().unwrap();
        f(&guard)
    }

    /// Mutably access the inner state with a closure
    pub fn with_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut JobStates) -> R,
    {
        let mut guard = self.0.lock().unwrap();
        f(&mut guard)
    }
}

pub fn run_candidate_check(
    root_dir: &Path,
    base_rev: &ResolvedRevision,
    candidate_rev: &ResolvedRevision,
    parallelism: usize,
    forced_vcs: Option<&str>,
    mode: CheckMode,
    post_clone_hook: Option<PostCloneHookConfig<'_>>,
    original_candidate: Option<&ResolvedRevision>,
    shared_job_states: Option<&SharedJobStates>,
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

    // Run post-clone hook if configured
    let (post_clone_output, post_clone_success) = if let Some(hook_config) = &post_clone_hook {
        debug!("Running post-clone hook");
        let result = run_post_clone_hook(
            hook_config,
            root_dir,
            base_workdir.path(),
            candidate_workdir.path(),
        );
        (Some(result.0), Some(result.1))
    } else {
        (None, None)
    };

    // If post-clone hook failed, return early with the hook output
    if post_clone_success == Some(false) {
        return Ok(CheckResult {
            output: String::new(),
            steps: Vec::new(),
            exit_code: Some(1),
            duration: Duration::ZERO,
            post_clone_output,
            post_clone_success,
        });
    }

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

    // Use provided jobs state or create local one
    let shared_job_states = shared_job_states
        .cloned()
        .unwrap_or_else(SharedJobStates::new);

    // Track used job names
    let used_job_names = Arc::new(Mutex::new(std::collections::HashSet::new()));

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
    let shared_job_states_clone = shared_job_states.clone();
    let used_job_names_clone = Arc::clone(&used_job_names);
    let jobs_sender_clone = jobs_sender.clone();
    let messages_sender_clone = messages_sender.clone();
    let listener_shutdown_clone = Arc::clone(&listener_shutdown);
    // Determine which revision info to use for SELFCI_CANDIDATE_* env vars
    // In MQ mode, original_candidate is the user's submitted revision
    // In regular check mode, it's the same as candidate_rev
    let candidate_info = original_candidate.unwrap_or(candidate_rev);

    // SELFCI_MERGED_* is only set in MQ mode (when original_candidate is Some)
    let (merged_commit_id, merged_change_id) = if original_candidate.is_some() {
        (
            Some(candidate_rev.commit_id.to_string()),
            Some(candidate_rev.change_id.to_string()),
        )
    } else {
        (None, None)
    };

    let spawn_context = super::worker::JobSpawnContext {
        base_dir: base_workdir.path().to_path_buf(),
        candidate_dir: candidate_workdir.path().to_path_buf(),
        command_prefix: config.job.command_prefix.clone(),
        command: config.job.command.clone(),
        print_output: mode.print_output(),
        socket_path: socket_path.clone(),
        candidate_commit_id: candidate_info.commit_id.to_string(),
        candidate_change_id: candidate_info.change_id.to_string(),
        candidate_id: candidate_info.user.to_string(),
        merged_commit_id: merged_commit_id.clone(),
        merged_change_id: merged_change_id.clone(),
    };
    std::thread::spawn(move || {
        super::worker::control_socket_listener(
            listener,
            shared_job_states_clone,
            used_job_names_clone,
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
            candidate_commit_id: candidate_info.commit_id.to_string(),
            candidate_change_id: candidate_info.change_id.to_string(),
            candidate_id: candidate_info.user.to_string(),
            merged_commit_id,
            merged_change_id,
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
                let now = std::time::Instant::now();
                step_start_times.insert((job_name.clone(), step_name.clone()), now);
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
                    "[{}/{}] {} {}: {}/{} ({:.3}s)",
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
                outcome.steps = shared_job_states.with(|s| {
                    s.steps
                        .get(&outcome.job_name)
                        .map(|steps| {
                            steps
                                .iter()
                                .map(|step| {
                                    let mut step = step.clone();
                                    if matches!(step.status, protocol::StepStatus::Running) {
                                        step.status = protocol::StepStatus::Success;
                                    }
                                    step
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                });

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
                            "[{}/{}] {} {}: {}/{} ({:.3}s)",
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
                shared_job_states.with_mut(|s| {
                    s.completions.insert(
                        outcome.job_name.clone(),
                        if job_failed {
                            protocol::JobStatus::Failed
                        } else {
                            protocol::JobStatus::Succeeded
                        },
                    );
                });

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

    // Output final run summary
    let total_duration = check_start.elapsed();
    let run_emoji = if any_job_failed { "❌" } else { "✅" };
    let run_status = if any_job_failed { "failed" } else { "passed" };
    output!(
        "[{}/{}] {} {} ({:.3}s)",
        total_jobs,
        total_jobs,
        run_emoji,
        run_status,
        total_duration.as_secs_f64()
    );

    // Signal control socket listener to shut down and wake it up
    listener_shutdown.store(true, Ordering::SeqCst);
    let _ = UnixStream::connect(&socket_path); // Wake up blocking accept()

    // Drop the jobs sender to close the channel and let workers exit
    drop(jobs_sender);

    // Clean up socket
    let _ = std::fs::remove_file(&socket_path);

    // Return aggregated result from all jobs
    Ok(CheckResult {
        output: all_outputs,
        steps: all_steps,
        exit_code: if any_job_failed { Some(1) } else { Some(0) },
        duration: total_duration,
        post_clone_output,
        post_clone_success,
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
        "candidate-commit" = &resolved_candidate.commit_id.as_str()[..8],
        base = %resolved_base.user,
        "Starting check"
    );

    // Read config to get merge style (default to Rebase)
    let merge_style = read_config(&root_dir)
        .ok()
        .and_then(|c| c.mq)
        .map(|mq| mq.merge_style)
        .unwrap_or_default();

    debug!(merge_style = ?merge_style, "Using merge style for test merge");

    // Create test merge/rebase of candidate onto base for CI testing
    // This ensures we test what would actually be merged, just like MQ mode
    // Skip test merge if base and candidate are the same commit (nothing to merge)
    let (merged_candidate, _jj_cleanup_guard): (_, Option<scopeguard::ScopeGuard<(), _>>) =
        if resolved_base.commit_id == resolved_candidate.commit_id {
            debug!("Base and candidate are the same commit, skipping test merge");
            (resolved_candidate.clone(), None)
        } else {
            // Use the resolved base commit ID (works with both branch names and commit hashes)
            let test_merge_result = super::mq::create_test_merge(
                &root_dir,
                resolved_base.commit_id.as_str(),
                &resolved_candidate,
                &merge_style,
            )
            .map_err(|e| {
                eprintln!("Failed to create test merge: {}", e);
                CheckError::CheckFailed
            })?;

            let merged_commit_id = test_merge_result.commit_id.to_string();
            let merged_change_id = test_merge_result.change_id.to_string();

            debug!(
                merged_commit = %merged_commit_id,
                merged_change = %merged_change_id,
                "Created test merge"
            );

            // Set up cleanup guard for jj test merge commits
            // This ensures cleanup happens regardless of check success/failure
            let cleanup_guard = if matches!(vcs, selfci::VCS::Jujutsu) {
                let cleanup_root_dir = root_dir.clone();
                let cleanup_change_id = merged_change_id.clone();
                let cleanup_base_commit = resolved_base.commit_id.to_string();
                Some(scopeguard::guard((), move |_| {
                    super::mq::cleanup_jj_test_merge(
                        &cleanup_root_dir,
                        &cleanup_change_id,
                        &cleanup_base_commit,
                    );
                }))
            } else {
                None
            };

            // Create a ResolvedRevision for the merged commit
            let merged = selfci::revision::ResolvedRevision {
                user: resolved_candidate.user.clone(),
                commit_id: test_merge_result.commit_id,
                change_id: test_merge_result.change_id,
            };

            (merged, cleanup_guard)
        };

    // Determine parallelism level
    let parallelism = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    debug!(parallelism, "Using parallelism level");

    debug!("Running jobs with parallelism {}", parallelism);

    // Determine if a test merge was actually performed
    // If base == candidate, no merge happened, so don't set SELFCI_MERGED_*
    let original_candidate = if resolved_base.commit_id == resolved_candidate.commit_id {
        None // No merge performed, SELFCI_MERGED_* won't be set
    } else {
        Some(&resolved_candidate) // Merge performed, set SELFCI_MERGED_*
    };

    // Run the candidate check using shared implementation
    // Pass merged commit as the working candidate (what CI tests)
    // Pass original candidate so SELFCI_CANDIDATE_* refers to what user specified
    let result = run_candidate_check(
        &root_dir,
        &resolved_base,
        &merged_candidate,
        parallelism,
        forced_vcs,
        CheckMode::Inline { print_output },
        None, // No post-clone hook for inline checks
        original_candidate,
        None, // No step callback for inline checks
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
