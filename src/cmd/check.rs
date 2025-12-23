use selfci::{
    CheckError, MainError, WorkDirError, copy_revisions_to_workdirs, get_vcs, protocol, read_config,
};
use std::collections::HashMap;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tracing::debug;

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
    base_rev: &str,
    candidate_rev: &str,
    parallelism: usize,
    forced_vcs: Option<&str>,
    mode: CheckMode,
) -> Result<CheckResult, MainError> {
    // Get VCS (forced or auto-detected)
    let vcs = get_vcs(root_dir, forced_vcs)?;
    debug!(vcs = ?vcs, root_dir = %root_dir.display(), forced = forced_vcs.is_some(), "Using VCS");

    // Allocate base work directory
    let base_workdir = tempfile::tempdir().map_err(WorkDirError::CreateFailed)?;

    debug!(
        base_workdir = %base_workdir.path().display(),
        "Allocated base work directory"
    );
    debug!(base_rev, candidate_rev, "Running candidate check");

    // Create a single candidate workdir (shared by all jobs)
    let candidate_workdir = tempfile::tempdir().map_err(WorkDirError::CreateFailed)?;

    // Copy revisions to workdirs
    copy_revisions_to_workdirs(
        &vcs,
        root_dir,
        base_workdir.path(),
        base_rev,
        candidate_workdir.path(),
        candidate_rev,
    )?;

    // Read config from base workdir
    let config = read_config(base_workdir.path())?;
    debug!("Loaded config");

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

    // Spawn worker threads
    for _ in 0..parallelism {
        let jobs_rx = Arc::clone(&jobs_receiver);
        let messages_tx = messages_sender.clone();

        std::thread::spawn(move || {
            super::worker::job_worker(jobs_rx, messages_tx);
        });
    }

    // Spawn control socket listener thread
    let job_steps_clone = Arc::clone(&job_steps);
    let used_job_names_clone = Arc::clone(&used_job_names);
    let jobs_sender_clone = jobs_sender.clone();
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
            jobs_sender_clone,
            spawn_context,
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

    for message in messages_receiver {
        match message {
            super::worker::JobMessage::Started { job_name } => {
                debug!(job = %job_name, "Job started");
                active_jobs += 1;
                total_jobs += 1;

                // Always print job status messages in Inline mode
                if matches!(mode, CheckMode::Inline { .. }) {
                    println!(
                        "[{}/{}] Job started: {}",
                        total_jobs - active_jobs,
                        total_jobs,
                        job_name
                    );
                }
            }
            super::worker::JobMessage::Completed(mut outcome) => {
                debug!(job = %outcome.job_name, exit_code = ?outcome.exit_code, "Job completed");

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

                // Collect output from all jobs
                if !outcome.output.is_empty() {
                    all_outputs.push_str(&outcome.output);
                }

                // Collect steps from all jobs
                all_steps.extend(outcome.steps.clone());

                // Check if job failed (either by exit code or by step failure)
                let has_failed_step = outcome.steps.iter().any(|step| {
                    matches!(step.status, protocol::StepStatus::Failed { ignored: false })
                });

                let job_failed = if let Some(exit_code) = outcome.exit_code {
                    exit_code != 0 || has_failed_step
                } else {
                    true
                };

                if job_failed {
                    any_job_failed = true;
                }

                // Print job completion status and steps in Inline mode
                if matches!(mode, CheckMode::Inline { .. }) {
                    let duration_secs = outcome.duration.as_secs_f64();
                    let jobs_completed = total_jobs - active_jobs + 1;

                    if job_failed {
                        let failure_reason = if has_failed_step {
                            "step failure".to_string()
                        } else if let Some(exit_code) = outcome.exit_code {
                            format!("exit code: {}", exit_code)
                        } else {
                            "no exit code".to_string()
                        };
                        println!(
                            "[{}/{}] Job failed: {} ({}, {:.3}s)",
                            jobs_completed,
                            total_jobs,
                            outcome.job_name,
                            failure_reason,
                            duration_secs
                        );
                    } else {
                        println!(
                            "[{}/{}] Job passed: {} ({:.3}s)",
                            jobs_completed, total_jobs, outcome.job_name, duration_secs
                        );
                    }

                    // Print steps for this job
                    if !outcome.steps.is_empty() {
                        for (i, step_entry) in outcome.steps.iter().enumerate() {
                            let next_ts = if i + 1 < outcome.steps.len() {
                                outcome.steps[i + 1].ts
                            } else {
                                step_entry.ts + outcome.duration
                            };

                            let step_duration = next_ts
                                .duration_since(step_entry.ts)
                                .unwrap_or(Duration::ZERO);

                            let status_emoji = match &step_entry.status {
                                protocol::StepStatus::Success => "✅",
                                protocol::StepStatus::Failed { ignored: true } => "⚠️",
                                protocol::StepStatus::Failed { ignored: false } => "❌",
                                protocol::StepStatus::Running => "⏳",
                            };

                            println!(
                                "  {} {} ({:.3}s)",
                                status_emoji,
                                step_entry.name,
                                step_duration.as_secs_f64()
                            );
                        }
                    }
                }

                // Decrement active jobs and check if we're done
                active_jobs -= 1;
                if active_jobs == 0 {
                    break;
                }
            }
        }
    }

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

    // Use VCS-specific defaults for base and candidate if not provided
    let base_rev = base.as_deref().unwrap_or_else(|| match vcs {
        selfci::VCS::Jujutsu => "@-",
        selfci::VCS::Git => "HEAD^",
    });
    let candidate_rev = candidate.as_deref().unwrap_or_else(|| match vcs {
        selfci::VCS::Jujutsu => "@",
        selfci::VCS::Git => "HEAD",
    });

    // Determine parallelism level
    let parallelism = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    debug!(parallelism, "Using parallelism level");

    println!("Running jobs with parallelism {}", parallelism);

    // Run the candidate check using shared implementation
    let result = run_candidate_check(
        &root_dir,
        base_rev,
        candidate_rev,
        parallelism,
        forced_vcs,
        CheckMode::Inline { print_output },
    )?;

    // Print output on failure if we weren't already printing it in real-time
    if !print_output && !result.output.is_empty() {
        println!("{}", result.output);
    }

    // Print total time
    println!("\nTotal time: {:.3}s", result.duration.as_secs_f64());

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
