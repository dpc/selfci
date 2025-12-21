use duct::cmd;
use selfci::{
    step, CheckError, MainError, WorkDirError, copy_revisions_to_workdirs, detect_vcs,
    read_config,
};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::debug;

struct RunJobRequest {
    base_dir: PathBuf,
    candidate_dir: PathBuf,
    job_name: String,
    job_full_command: Vec<String>,
    print_output: bool,
    step_log_path: PathBuf,
}

struct RunJobOutcome {
    job_name: String,
    output: String,
    exit_code: Option<i32>,
    duration: Duration,
    steps: Vec<step::StepLogEntry>,
}

/// Worker thread function that processes jobs
fn job_worker(
    jobs_receiver: Arc<Mutex<mpsc::Receiver<RunJobRequest>>>,
    outcomes_sender: mpsc::Sender<RunJobOutcome>,
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

        // Record start time
        let start_time = Instant::now();

        // Initialize step log file
        if let Err(e) = std::fs::write(&job.step_log_path, "[]") {
            let outcome = RunJobOutcome {
                job_name: job.job_name,
                output: format!("Failed to initialize step log: {}", e),
                exit_code: None,
                duration: start_time.elapsed(),
                steps: Vec::new(),
            };
            let _ = outcomes_sender.send(outcome);
            continue;
        }

        // Execute the job command
        let handle = match cmd(&job.job_full_command[0], &job.job_full_command[1..])
            .dir(&job.candidate_dir)
            .env("SELFCI_BASE_DIR", &job.base_dir)
            .env("SELFCI_CANDIDATE_DIR", &job.candidate_dir)
            .env("SELFCI_JOB", &job.job_name)
            .env("SELFCI_STEP_LOG_PATH", &job.step_log_path)
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
                let _ = outcomes_sender.send(outcome);
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
                let _ = outcomes_sender.send(outcome);
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

        // Read step log
        let steps = step::read_step_log(&job.step_log_path).unwrap_or_else(|_| Vec::new());

        // Send outcome
        let outcome = RunJobOutcome {
            job_name: job.job_name,
            output: captured_output,
            exit_code,
            duration,
            steps,
        };
        let _ = outcomes_sender.send(outcome);
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

    // Copy base revision to base workdir only
    // We'll create separate candidate workdirs for each check
    let temp_candidate = tempfile::tempdir().map_err(WorkDirError::CreateFailed)?;
    copy_revisions_to_workdirs(
        &vcs,
        &root_dir,
        base_workdir.path(),
        base_rev,
        temp_candidate.path(),
        candidate_rev,
    )?;
    drop(temp_candidate);

    // Read config from base workdir
    let config = read_config(base_workdir.path())?;
    debug!(jobs_count = config.jobs.len(), "Loaded config");

    if config.jobs.is_empty() {
        println!("No jobs defined in config");
        return Ok(());
    }

    // Determine parallelism level
    let parallelism = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    debug!(parallelism, "Using parallelism level");

    println!("Running {} jobs with parallelism {}", config.jobs.len(), parallelism);

    // Create channels for jobs (SPMC) and outcomes (MPSC)
    let (jobs_sender, jobs_receiver) = mpsc::channel::<RunJobRequest>();
    let (outcomes_sender, outcomes_receiver) = mpsc::channel::<RunJobOutcome>();
    let jobs_receiver = Arc::new(Mutex::new(jobs_receiver));

    // Spawn worker threads
    let mut workers = Vec::new();
    for _ in 0..parallelism {
        let jobs_rx = Arc::clone(&jobs_receiver);
        let outcomes_tx = outcomes_sender.clone();

        let handle = std::thread::spawn(move || {
            job_worker(jobs_rx, outcomes_tx);
        });
        workers.push(handle);
    }
    // Drop the original outcomes sender so the channel closes when all workers are done
    drop(outcomes_sender);

    // Prepare and send all jobs
    let total_jobs = config.jobs.len();
    for (job_name, job_config) in config.jobs {
        // Create candidate workdir for this job
        let candidate_workdir = tempfile::tempdir().map_err(WorkDirError::CreateFailed)?;

        // Copy candidate revision
        copy_revisions_to_workdirs(
            &vcs,
            &root_dir,
            candidate_workdir.path(),
            candidate_rev,
            candidate_workdir.path(),
            candidate_rev,
        )?;

        // Build the full command
        let mut full_command = job_config.command_prefix.clone();
        full_command.push(job_config.command.clone());

        // Create step log file path
        let step_log_file = tempfile::NamedTempFile::new()
            .map_err(WorkDirError::CreateFailed)?;
        let step_log_path = step_log_file.path().to_path_buf();

        // Send job to workers
        let job = RunJobRequest {
            base_dir: base_workdir.path().to_path_buf(),
            candidate_dir: candidate_workdir.path().to_path_buf(),
            job_name: job_name.clone(),
            job_full_command: full_command,
            print_output,
            step_log_path,
        };

        jobs_sender.send(job).map_err(|_| CheckError::CheckFailed)?;

        // Keep the tempdir and step log file alive by leaking them
        std::mem::forget(candidate_workdir);
        std::mem::forget(step_log_file);
    }

    // Drop jobs sender to signal no more jobs
    drop(jobs_sender);

    // Collect outcomes as they arrive
    let mut completed = 0;
    let mut failed_jobs = Vec::new();
    let mut total_duration = Duration::ZERO;

    for outcome in outcomes_receiver {
        completed += 1;
        total_duration += outcome.duration;

        let duration_secs = outcome.duration.as_secs_f64();

        if let Some(exit_code) = outcome.exit_code {
            if exit_code == 0 {
                println!("[{}/{}] Job '{}' passed ({:.2}s)", completed, total_jobs, outcome.job_name, duration_secs);
            } else {
                println!("[{}/{}] Job '{}' failed (exit code: {}, {:.2}s)", completed, total_jobs, outcome.job_name, exit_code, duration_secs);
                // Print output on failure if we weren't already printing it in real-time
                if !print_output && !outcome.output.is_empty() {
                    println!("{}", outcome.output);
                }
                failed_jobs.push((outcome.job_name, outcome.output));
            }
        } else {
            println!("[{}/{}] Job '{}' failed (no exit code, {:.2}s)", completed, total_jobs, outcome.job_name, duration_secs);
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
                println!("  - {} ({:.2}s)", step_entry.name, step_duration as f64);
            }
        }
    }

    // Wait for all workers to finish
    for worker in workers {
        let _ = worker.join();
    }

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
