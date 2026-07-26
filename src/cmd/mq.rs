#[cfg(test)]
#[path = "mq_tests.rs"]
mod tests;

use crate::cmd::process_exit::{ProcessExitWatcher, WaitOutcome, WatchState};
use comfy_table::{ContentArrangement, Table, presets};
use duct::cmd;
use nix::sys::signal::{self, Signal};
use nix::sys::stat::{Mode, umask};
use nix::unistd::{ForkResult, Pid, close, dup2, fork, setsid};
use selfci::duct_util::Cmd;
use selfci::{
    MainError, ProcessControlError, WorkDirError, constants, envs, get_vcs, mq_protocol, protocol,
};
use signal_hook::consts::SIGTERM;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::io::IntoRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::SystemTime;
use tracing::debug;

/// Command-local template for stable jj output. Method calls bypass user
/// aliases for the bare `change_id` and `commit_id` template keywords.
const JJ_MACHINE_COMMIT_SUMMARY_CONFIG: &str =
    r#"templates.commit_summary="self.change_id() ++ \" \" ++ self.commit_id()""#;
const MQ_STOP_GRACE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MQ_STOP_KILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
static FORCE_JJ_POST_MOVE_VERIFY_FAILURE: AtomicBool = AtomicBool::new(false);

/// Get the selfci root runtime directory, where individual runtime for each
/// started instance are located.
fn get_selfci_root_runtime_dir() -> Result<PathBuf, MainError> {
    let uid = nix::unistd::getuid().as_raw();
    let root = if let Some(base) = dirs::runtime_dir() {
        base.join("selfci")
    } else {
        // macOS has no XDG runtime directory. Establish a private directory
        // directly below sticky /tmp before creating any daemon state.
        let base = PathBuf::from(format!("/tmp/selfci-{uid}"));
        ensure_private_runtime_directory(&base, uid)?;
        base.join("selfci")
    };
    ensure_private_runtime_directory(&root, uid)?;
    Ok(root)
}

/// Create or validate a same-user, non-symlink, mode-0700 runtime directory.
fn ensure_private_runtime_directory(path: &Path, uid: u32) -> Result<(), MainError> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(WorkDirError::CreateWorkDirectoryFailed(
                selfci::WorkDirectoryCreateError::new(path.to_path_buf(), error),
            )
            .into());
        }
    }

    let metadata = std::fs::symlink_metadata(path).map_err(WorkDirError::CreateFailed)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(WorkDirError::CreateFailed(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "runtime directory {} must be a same-user, non-symlink directory with mode 0700",
                path.display()
            ),
        ))
        .into());
    }
    // Normalize an overly restrictive same-user directory to the documented
    // mode while never widening a directory that failed validation above.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(WorkDirError::CreateFailed)?;
    Ok(())
}

/// Get the daemon runtime directory for a specific PID
fn get_daemon_dir_for_pid(pid: u32) -> Result<PathBuf, MainError> {
    Ok(get_selfci_root_runtime_dir()?.join(pid.to_string()))
}

/// Compare two paths for equality after canonicalization
/// Falls back to direct comparison if canonicalization fails
fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a_canon), Ok(b_canon)) => a_canon == b_canon,
        _ => a == b, // Fallback to direct comparison
    }
}

/// Get the daemon runtime directory for this project
/// Returns explicit dir if SELFCI_MQ_RUNTIME_DIR is set, otherwise searches for daemon
fn get_project_daemon_runtime_dir(project_root: &Path) -> Result<Option<PathBuf>, MainError> {
    // Mode 1: Explicit runtime directory
    if let Ok(explicit_dir) = std::env::var(envs::SELFCI_MQ_RUNTIME_DIR) {
        let dir = PathBuf::from(&explicit_dir);

        // Verify it's for our project (if initialized)
        let dir_file = dir.join(constants::MQ_DIR_FILENAME);
        if dir_file.exists() {
            let stored_root =
                std::fs::read_to_string(&dir_file).map_err(WorkDirError::CreateFailed)?;
            let stored_root_trimmed = stored_root.trim();
            if paths_equal(Path::new(stored_root_trimmed), project_root) {
                return Ok(Some(dir));
            } else {
                debug!(
                    explicit_dir = %explicit_dir,
                    stored_root = %stored_root_trimmed,
                    project_root = %project_root.display(),
                    stored_canonical = ?Path::new(stored_root_trimmed).canonicalize(),
                    project_canonical = ?project_root.canonicalize(),
                    "Explicit runtime dir project mismatch"
                );
                return Ok(None); // Wrong project
            }
        } else {
            // Not initialized yet, return the directory
            return Ok(Some(dir));
        }
    }

    // Mode 2: Auto-discovery - scan PID directories
    let runtime_dir = get_selfci_root_runtime_dir()?;
    if !runtime_dir.exists() {
        return Ok(None);
    }

    let mut unverifiable_match = None;
    for entry in std::fs::read_dir(&runtime_dir).map_err(WorkDirError::CreateFailed)? {
        let entry = entry.map_err(WorkDirError::CreateFailed)?;
        let pid_dir = entry.path();

        // Read mq.dir to check project match
        let dir_file = pid_dir.join(constants::MQ_DIR_FILENAME);
        let stored_root = match std::fs::read_to_string(&dir_file) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if paths_equal(Path::new(stored_root.trim()), project_root) {
            // Skip unverifiable state without deleting it. A crashed daemon's
            // directory must not mask a later live daemon or prevent restart.
            if verified_daemon_pid(&pid_dir).is_some() {
                return Ok(Some(pid_dir));
            }
            unverifiable_match.get_or_insert(pid_dir);
        }
    }

    Ok(unverifiable_match)
}

/// Return the socket responder PID only when it matches the runtime PID file.
fn verified_daemon_pid(daemon_dir: &Path) -> Option<libc::pid_t> {
    let recorded_pid = std::fs::read_to_string(daemon_dir.join(constants::MQ_PID_FILENAME))
        .ok()?
        .trim()
        .parse::<libc::pid_t>()
        .ok()
        .filter(|pid| *pid > 0)?;
    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);
    match mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::Hello) {
        Ok(mq_protocol::MQResponse::HelloAck { pid }) if pid == recorded_pid => Some(pid),
        _ => None,
    }
}

/// Internal run entry that holds both the protocol info and runtime state
struct RunEntry {
    info: mq_protocol::MQRunInfo,
    /// Job states for active runs - used for real-time job status tracking
    /// Only present while the run is active (started but not completed)
    job_states: Option<super::check::SharedJobStates>,
}

struct MQState {
    root_dir: PathBuf,
    base_branch: String,
    merge_mode: selfci::config::MergeMode,
    hooks: selfci::config::MQHooksConfig,
    next_run_id: mq_protocol::RunId,
    /// All runs - status is derived from started_at/completed_at fields
    runs: HashMap<mq_protocol::RunId, RunEntry>,
}

impl MQState {
    /// Create and queue a new run, returning the run info
    fn queue_run(
        &mut self,
        candidate: selfci::revision::ResolvedRevision,
        no_merge: bool,
    ) -> mq_protocol::MQRunInfo {
        let run_id = self.next_run_id;
        self.next_run_id = mq_protocol::RunId(self.next_run_id.0 + 1);

        let info = mq_protocol::MQRunInfo {
            id: run_id,
            candidate,
            status: mq_protocol::MQRunStatus::Queued,
            queued_at: SystemTime::now(),
            started_at: None,
            completed_at: None,
            merge_mode: self.merge_mode,
            test_merge_output: String::new(),
            output: String::new(),
            active_jobs: Vec::new(),
            completed_steps: Vec::new(),
            completed_jobs: Vec::new(),
            no_merge,
        };

        let entry = RunEntry {
            info: info.clone(),
            job_states: None,
        };
        self.runs.insert(run_id, entry);
        info
    }

    /// Start processing a queued run
    /// Returns the run info, hooks, merge_mode, and shared job states for processing
    fn start_run(
        &mut self,
        run_id: mq_protocol::RunId,
    ) -> Option<(
        mq_protocol::MQRunInfo,
        selfci::config::MQHooksConfig,
        selfci::config::MergeMode,
        super::check::SharedJobStates,
    )> {
        let entry = self.runs.get_mut(&run_id)?;

        // Only start if queued (started_at is None)
        if entry.info.started_at.is_some() {
            return None;
        }

        entry.info.status = mq_protocol::MQRunStatus::Running;
        entry.info.started_at = Some(SystemTime::now());

        // Create shared job states for real-time job status
        let job_states = super::check::SharedJobStates::new();
        entry.job_states = Some(job_states.clone());

        Some((
            entry.info.clone(),
            self.hooks.clone(),
            self.merge_mode,
            job_states,
        ))
    }

    /// Complete an active run by updating its state in place
    fn complete_run(&mut self, run_id: mq_protocol::RunId, run_info: mq_protocol::MQRunInfo) {
        if let Some(entry) = self.runs.get_mut(&run_id) {
            entry.info = run_info;
            entry.job_states = None; // Clear job states when run completes
        }
    }

    /// Get a run by ID
    /// For active runs (started but not completed), includes real-time active jobs info
    fn get_run(&self, run_id: mq_protocol::RunId) -> Option<mq_protocol::MQRunInfo> {
        let entry = self.runs.get(&run_id)?;
        let mut info = entry.info.clone();

        // For active runs (started but not completed), derive running jobs from steps and completions
        if info.started_at.is_some()
            && info.completed_at.is_none()
            && let Some(ref job_states) = entry.job_states
        {
            // Jobs that have steps but aren't in completions are running
            info.active_jobs = job_states.with(|s| {
                s.steps
                    .iter()
                    .filter(|(job_name, _)| !s.completions.contains_key(*job_name))
                    .map(|(job_name, job_steps)| {
                        // Use first step timestamp as job start time
                        let job_started_at = job_steps
                            .first()
                            .map(|s| s.ts)
                            .unwrap_or_else(SystemTime::now);

                        // Find current step (last one with Running status)
                        let current_step = job_steps
                            .iter()
                            .rev()
                            .find(|s| matches!(s.status, protocol::StepStatus::Running));

                        let (name, step_started_at) = if let Some(step) = current_step {
                            (format!("{}/{}", job_name, step.name), step.ts)
                        } else {
                            (job_name.clone(), job_started_at)
                        };

                        protocol::StepLogEntry {
                            ts: step_started_at,
                            name,
                            status: protocol::StepStatus::Running,
                            job_started_at: Some(job_started_at),
                        }
                    })
                    .collect()
            });
        }

        Some(info)
    }

    /// List all runs, sorted by ID descending, with optional limit
    fn list_runs(&self, limit: Option<usize>) -> Vec<mq_protocol::MQRunInfo> {
        let mut runs: Vec<_> = self.runs.values().map(|e| e.info.clone()).collect();

        runs.sort_by(|a, b| b.id.0.cmp(&a.id.0));

        if let Some(limit) = limit {
            runs.truncate(limit);
        }

        runs
    }

    /// Get the root directory
    fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Get the base branch name
    fn base_branch(&self) -> &str {
        &self.base_branch
    }
}

/// Thread-safe wrapper around MQState that handles locking
#[derive(Clone)]
struct SharedMQState {
    state: Arc<Mutex<MQState>>,
    /// Notified when any run completes
    run_completed: Arc<Condvar>,
}

impl SharedMQState {
    fn new(state: MQState) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            run_completed: Arc::new(Condvar::new()),
        }
    }

    fn queue_run(
        &self,
        candidate: selfci::revision::ResolvedRevision,
        no_merge: bool,
    ) -> mq_protocol::MQRunInfo {
        self.state.lock().unwrap().queue_run(candidate, no_merge)
    }

    fn start_run(
        &self,
        run_id: mq_protocol::RunId,
    ) -> Option<(
        mq_protocol::MQRunInfo,
        selfci::config::MQHooksConfig,
        selfci::config::MergeMode,
        super::check::SharedJobStates,
    )> {
        self.state.lock().unwrap().start_run(run_id)
    }

    fn complete_run(&self, run_id: mq_protocol::RunId, run_info: mq_protocol::MQRunInfo) {
        self.state.lock().unwrap().complete_run(run_id, run_info);
        self.run_completed.notify_all();
    }

    fn get_run(&self, run_id: mq_protocol::RunId) -> Option<mq_protocol::MQRunInfo> {
        self.state.lock().unwrap().get_run(run_id)
    }

    fn list_runs(&self, limit: Option<usize>) -> Vec<mq_protocol::MQRunInfo> {
        self.state.lock().unwrap().list_runs(limit)
    }

    fn root_dir(&self) -> PathBuf {
        self.state.lock().unwrap().root_dir().to_path_buf()
    }

    fn base_branch(&self) -> String {
        self.state.lock().unwrap().base_branch().to_string()
    }
}
/// Try to resolve base branch from config only (no CLI arg), quietly without printing errors
/// Returns Some(branch) if config has base-branch, None otherwise
fn try_resolve_base_branch_from_config(root_dir: &Path) -> Option<String> {
    let config = selfci::config::read_config(root_dir).ok()?;
    config.mq?.base_branch
}

/// Messages handled by the merge queue processor.
enum QueueMessage {
    /// Process a candidate run already sent to the queue processor.
    Run(mq_protocol::RunId),
    /// Finish runs enqueued before this marker and stop the processor.
    Shutdown,
}

/// Result of start_daemon_common
struct StartDaemonResult {
    outcome: DaemonizeOutcome,
    base_branch: String,
}

/// Outcome of daemon startup
enum DaemonizeOutcome {
    /// No base branch configured - can't start daemon
    NoBranch,
    /// Daemon was already running
    AlreadyRunning,
    /// Parent process - socket already bound, child will accept connections
    /// Includes daemon_dir so parent knows where runtime files are
    Parent { daemon_dir: PathBuf },
    /// Child process (or foreground mode) - daemon_dir and listener to run the daemon
    Child {
        daemon_dir: PathBuf,
        listener: UnixListener,
    },
}

/// Check if a daemon is already running for this project and print info if so
fn check_daemon_already_running(root_dir: &Path) -> Result<bool, MainError> {
    if let Some(existing_dir) = get_project_daemon_runtime_dir(root_dir)? {
        if let Some(pid) = verified_daemon_pid(&existing_dir) {
            println!("Merge queue daemon is already running (PID: {})", pid);
            println!("Runtime directory: {}", existing_dir.display());
            println!("Use 'selfci mq stop' to stop it");
            return Ok(true);
        }
        let has_runtime_state = [
            constants::MQ_DIR_FILENAME,
            constants::MQ_PID_FILENAME,
            constants::MQ_SOCK_FILENAME,
        ]
        .iter()
        .any(|name| existing_dir.join(name).exists());
        if std::env::var_os(envs::SELFCI_MQ_RUNTIME_DIR).is_some() && has_runtime_state {
            return Err(MainError::CommunicationFailed);
        }
    }
    Ok(false)
}

/// Daemonize the process in background mode
/// Creates daemon directory and binds socket BEFORE forking, so the socket is
/// immediately ready for connections when the parent process exits.
/// Returns DaemonizeOutcome::Parent in parent process, DaemonizeOutcome::Child in child process
fn daemonize_background(
    root_dir: &Path,
    explicit_runtime_dir: Option<PathBuf>,
    log_file: Option<PathBuf>,
    base_branch: &str,
) -> Result<DaemonizeOutcome, MainError> {
    println!("Base branch: {}", base_branch);

    // Determine daemon directory BEFORE forking
    // Use parent PID for directory name if no explicit dir provided
    let daemon_dir = match explicit_runtime_dir {
        Some(dir) => dir,
        None => get_daemon_dir_for_pid(std::process::id())?,
    };

    // Create directory BEFORE forking
    std::fs::create_dir_all(&daemon_dir).map_err(|error| {
        WorkDirError::CreateWorkDirectoryFailed(selfci::WorkDirectoryCreateError::new(
            daemon_dir.clone(),
            error,
        ))
    })?;

    // Bind socket BEFORE writing mq.dir - this ensures that finding mq.dir
    // guarantees the socket is ready for connections
    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);
    let listener = UnixListener::bind(&socket_path).map_err(WorkDirError::CreateFailed)?;

    // Write mq.dir AFTER socket is bound - discovery via mq.dir means socket is ready
    std::fs::write(
        daemon_dir.join(constants::MQ_DIR_FILENAME),
        root_dir.to_string_lossy().as_bytes(),
    )
    .map_err(WorkDirError::CreateFailed)?;

    println!("Runtime directory: {}", daemon_dir.display());

    // Now fork - mq.pid will be written by child after fork
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child: _ }) => {
            // Parent process - socket is already bound, child will accept connections
            // We can exit immediately - no waiting needed!
            Ok(DaemonizeOutcome::Parent { daemon_dir })
        }
        Err(e) => {
            // Fork failed - clean up
            std::fs::remove_dir_all(&daemon_dir).ok();
            eprintln!("Failed to fork: {}", e);
            Err(MainError::CheckFailed)
        }
        Ok(ForkResult::Child) => {
            // Child process - become session leader and continue as daemon

            // Become session leader
            setsid().map_err(|_| {
                WorkDirError::CreateFailed(std::io::Error::other("Failed to become session leader"))
            })?;

            // Redirect stdin to /dev/null
            let devnull = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .map_err(WorkDirError::CreateFailed)?;
            let devnull_fd = devnull.into_raw_fd();
            dup2(devnull_fd, 0).map_err(|_| {
                WorkDirError::CreateFailed(std::io::Error::other(
                    "Failed to redirect stdin to /dev/null",
                ))
            })?;
            if devnull_fd > 2 {
                close(devnull_fd).ok();
            }

            // Change to working directory
            std::env::set_current_dir(root_dir).map_err(WorkDirError::CreateFailed)?;

            // Set umask
            umask(Mode::from_bits_truncate(0o027));

            // Write mq.pid AFTER fork (child's actual PID)
            // Note: mq.dir was already written before fork for immediate discovery
            let pid = std::process::id();
            std::fs::write(daemon_dir.join(constants::MQ_PID_FILENAME), pid.to_string())
                .map_err(WorkDirError::CreateFailed)?;

            // Set up log file redirection
            let log_path = log_file.unwrap_or_else(|| daemon_dir.join("mq.log"));
            let log_file_handle = match OpenOptions::new().create(true).append(true).open(&log_path)
            {
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

            // Redirect stdout/stderr to log file
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

            Ok(DaemonizeOutcome::Child {
                daemon_dir,
                listener,
            })
        }
    }
}

/// Run daemon in foreground mode (no fork)
/// Returns DaemonizeOutcome::Child since we run the daemon loop directly
fn daemonize_foreground(
    root_dir: &Path,
    explicit_runtime_dir: Option<PathBuf>,
    base_branch: &str,
) -> Result<DaemonizeOutcome, MainError> {
    let pid = std::process::id();
    let daemon_dir = match explicit_runtime_dir {
        Some(dir) => dir,
        None => get_daemon_dir_for_pid(pid)?,
    };

    std::fs::create_dir_all(&daemon_dir).map_err(|error| {
        WorkDirError::CreateWorkDirectoryFailed(selfci::WorkDirectoryCreateError::new(
            daemon_dir.clone(),
            error,
        ))
    })?;

    // Bind socket BEFORE writing mq.dir - finding mq.dir guarantees socket is ready
    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);
    let listener = UnixListener::bind(&socket_path).map_err(WorkDirError::CreateFailed)?;

    // Write mq.dir and mq.pid AFTER socket is bound
    std::fs::write(
        daemon_dir.join(constants::MQ_DIR_FILENAME),
        root_dir.to_string_lossy().as_bytes(),
    )
    .map_err(WorkDirError::CreateFailed)?;
    std::fs::write(daemon_dir.join(constants::MQ_PID_FILENAME), pid.to_string())
        .map_err(WorkDirError::CreateFailed)?;

    println!(
        "Merge queue daemon started for base branch: {}",
        base_branch
    );
    println!("Runtime directory: {}", daemon_dir.display());
    Ok(DaemonizeOutcome::Child {
        daemon_dir,
        listener,
    })
}

/// Set up runtime directory and daemonize (fork) if in background mode
/// Returns DaemonizeOutcome indicating whether we're parent (should not run daemon loop) or child/foreground (should run daemon loop)
fn daemonize(
    root_dir: &Path,
    foreground: bool,
    log_file: Option<PathBuf>,
    base_branch: &str,
) -> Result<DaemonizeOutcome, MainError> {
    // Check for explicit runtime directory from environment
    let explicit_runtime_dir = std::env::var(envs::SELFCI_MQ_RUNTIME_DIR)
        .ok()
        .map(PathBuf::from);

    if foreground {
        daemonize_foreground(root_dir, explicit_runtime_dir, base_branch)
    } else {
        daemonize_background(root_dir, explicit_runtime_dir, log_file, base_branch)
    }
}

/// Common daemon startup logic: resolve base branch, check if running, run pre-start hook, daemonize
/// If base_branch is None, tries to resolve from config; returns NoBranch if not found
fn start_daemon_common(
    root_dir: &Path,
    base_branch: Option<String>,
    foreground: bool,
    log_file: Option<PathBuf>,
) -> Result<StartDaemonResult, MainError> {
    // Resolve base branch: CLI arg takes precedence, then config
    let base_branch = match base_branch {
        Some(branch) => branch,
        None => match try_resolve_base_branch_from_config(root_dir) {
            Some(branch) => branch,
            None => {
                return Ok(StartDaemonResult {
                    outcome: DaemonizeOutcome::NoBranch,
                    base_branch: String::new(),
                });
            }
        },
    };

    if check_daemon_already_running(root_dir)? {
        return Ok(StartDaemonResult {
            outcome: DaemonizeOutcome::AlreadyRunning,
            base_branch,
        });
    }

    // Run pre-start hook BEFORE daemonization with inherited stdio
    // This allows interactive commands (e.g., password prompts, keychain unlock)
    let merged_config = selfci::config::read_merged_mq_config(root_dir)?;
    if !run_hook_interactive(
        merged_config.hooks.pre_start.as_ref(),
        "pre-start",
        root_dir,
    ) {
        eprintln!("Pre-start hook failed, aborting daemon startup");
        return Err(MainError::CheckFailed);
    }

    let outcome = daemonize(root_dir, foreground, log_file, &base_branch)?;
    Ok(StartDaemonResult {
        outcome,
        base_branch,
    })
}

pub fn start_daemon(
    base_branch: Option<String>,
    foreground: bool,
    log_file: Option<PathBuf>,
) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let result = start_daemon_common(&root_dir, base_branch, foreground, log_file)?;

    match result.outcome {
        DaemonizeOutcome::NoBranch => {
            eprintln!("Error: --base-branch not specified and mq.base-branch not set in config");
            eprintln!(
                "Either specify --base-branch or add mq.base-branch to .config/selfci/ci.yaml"
            );
            Err(MainError::CheckFailed)
        }
        DaemonizeOutcome::AlreadyRunning => Ok(()),
        DaemonizeOutcome::Parent { daemon_dir: _ } => {
            // Parent process in background mode - exit now, child will run the daemon
            std::process::exit(0);
        }
        DaemonizeOutcome::Child {
            daemon_dir,
            listener,
        } => run_daemon_loop(daemon_dir, root_dir, result.base_branch, listener),
    }
}

/// Auto-start daemon in background if config has base-branch set
/// Returns Ok(Some(daemon_dir)) if started successfully, Ok(None) if cannot auto-start
pub fn auto_start_daemon(root_dir: &Path) -> Result<Option<PathBuf>, MainError> {
    let result = start_daemon_common(root_dir, None, false, None)?;

    match result.outcome {
        DaemonizeOutcome::NoBranch => Ok(None), // Can't auto-start without config
        DaemonizeOutcome::AlreadyRunning => Ok(None),
        DaemonizeOutcome::Parent { daemon_dir } => {
            println!("Auto-starting merge queue daemon...");
            // Parent process - socket is already bound, immediately usable
            Ok(Some(daemon_dir))
        }
        DaemonizeOutcome::Child {
            daemon_dir,
            listener,
        } => {
            println!("Auto-starting merge queue daemon...");
            // Child process - run the daemon loop (never returns)
            let _ = run_daemon_loop(
                daemon_dir,
                root_dir.to_path_buf(),
                result.base_branch,
                listener,
            );
            std::process::exit(0);
        }
    }
}

/// Run the daemon main loop (socket listener, request handler, etc.)
fn run_daemon_loop(
    daemon_dir: PathBuf,
    root_dir: PathBuf,
    base_branch: String,
    listener: UnixListener,
) -> Result<(), MainError> {
    // Read merged config to get hooks
    let merged_config = selfci::config::read_merged_mq_config(&root_dir)?;
    debug!(
        "Loaded MQ hooks config: pre_start={}, post_start={}, pre_clone={}, post_clone={}, pre_merge={}, post_merge={}",
        merged_config
            .hooks
            .pre_start
            .as_ref()
            .map(|h| h.is_set())
            .unwrap_or(false),
        merged_config
            .hooks
            .post_start
            .as_ref()
            .map(|h| h.is_set())
            .unwrap_or(false),
        merged_config
            .hooks
            .pre_clone
            .as_ref()
            .map(|h| h.is_set())
            .unwrap_or(false),
        merged_config
            .hooks
            .post_clone
            .as_ref()
            .map(|h| h.is_set())
            .unwrap_or(false),
        merged_config
            .hooks
            .pre_merge
            .as_ref()
            .map(|h| h.is_set())
            .unwrap_or(false),
        merged_config
            .hooks
            .post_merge
            .as_ref()
            .map(|h| h.is_set())
            .unwrap_or(false),
    );

    // Run post-start hook if configured (after daemonization, with captured output)
    // Note: pre-start hook runs BEFORE daemonization in start_daemon/auto_start_daemon
    let post_start_result = run_hook(
        merged_config.hooks.post_start.as_ref(),
        "post-start",
        &root_dir,
    );
    if !post_start_result.output.is_empty() {
        eprintln!("Post-start hook output:\n{}", post_start_result.output);
    }
    if !post_start_result.success {
        eprintln!("Post-start hook failed, aborting daemon startup");
        return Err(MainError::CheckFailed);
    }

    // Create shutdown flag
    let shutdown = Arc::new(AtomicBool::new(false));

    // Set up cleanup on exit - remove entire daemon directory
    let daemon_dir_cleanup = daemon_dir.clone();
    let _guard = scopeguard::guard((), move |_| {
        std::fs::remove_dir_all(&daemon_dir_cleanup).ok();
    });

    // Socket is already bound (passed as parameter)
    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);

    // Set up signal handling using signal_hook's iterator API
    // This is more robust than the low-level pipe API
    use signal_hook::iterator::Signals;
    let mut signals = Signals::new([SIGTERM]).map_err(WorkDirError::CreateFailed)?;

    let shutdown_clone = Arc::clone(&shutdown);
    let socket_path_clone = socket_path.clone();
    std::thread::spawn(move || {
        debug!("Signal handler thread started, waiting for SIGTERM");

        // Block until SIGTERM is received (the only signal we registered for)
        if let Some(sig) = signals.forever().next() {
            debug_assert_eq!(sig, SIGTERM);
            debug!("SIGTERM received, waking up listener");
            // Set flag and wake up the blocking accept()
            shutdown_clone.store(true, Ordering::SeqCst);
            let _ = UnixStream::connect(&socket_path_clone);
            debug!("Connected to socket to wake up accept()");
        }

        debug!("Signal handler thread exiting");
    });

    // Initialize state
    let state = SharedMQState::new(MQState {
        root_dir: root_dir.clone(),
        base_branch: base_branch.clone(),
        merge_mode: merged_config.merge_mode,
        hooks: merged_config.hooks,
        next_run_id: mq_protocol::RunId(1),
        runs: HashMap::new(),
    });

    // Create the queue processor's message channel
    let (queue_sender, queue_receiver) = mpsc::channel::<QueueMessage>();

    // Spawn the queue processor
    let state_clone = state.clone();
    let root_dir_clone = root_dir.clone();
    let queue_worker = std::thread::spawn(move || {
        // process_queue creates worker pools for each candidate check via run_candidate_check
        process_queue(state_clone, root_dir_clone, queue_receiver);
    });

    debug!("Entering main daemon loop");

    // Main loop: accept connections and handle requests
    loop {
        let accepted = listener.accept();

        // Check for shutdown after accept returns (could be woken by signal handler)
        if shutdown.load(Ordering::SeqCst) {
            debug!("Shutdown requested, exiting daemon loop");
            break;
        }

        match accepted {
            Ok((mut stream, _)) => {
                let state_clone = state.clone();
                let queue_sender_clone = queue_sender.clone();
                let shutdown_clone = Arc::clone(&shutdown);
                let socket_path_clone = socket_path.clone();
                std::thread::spawn(move || {
                    if let Ok(request) = mq_protocol::read_mq_request(&mut stream) {
                        let shutdown_requested = matches!(
                            request,
                            mq_protocol::MQRequest::Shutdown { expected_pid }
                                if expected_pid == std::process::id() as libc::pid_t
                        );
                        let response = handle_request(&state_clone, request, queue_sender_clone);
                        let _ = mq_protocol::write_mq_response(&mut stream, response);
                        if shutdown_requested {
                            shutdown_clone.store(true, Ordering::SeqCst);
                            let _ = UnixStream::connect(&socket_path_clone);
                        }
                    }
                });
            }
            Err(e) => {
                debug!("Connection error: {}", e);
            }
        }
    }

    // Let work already enqueued before the shutdown marker finish before process
    // exit. Candidate checks own temporary worktrees whose drop cleanup must not
    // be interrupted by daemon shutdown. Late detached request handlers are not
    // part of this queue-drain guarantee.
    let _ = queue_sender.send(QueueMessage::Shutdown);
    queue_worker.join().map_err(|_| MainError::CheckFailed)?;

    Ok(())
}

fn handle_request(
    state: &SharedMQState,
    request: mq_protocol::MQRequest,
    queue_sender: mpsc::Sender<QueueMessage>,
) -> mq_protocol::MQResponse {
    match request {
        mq_protocol::MQRequest::Hello => mq_protocol::MQResponse::HelloAck {
            pid: std::process::id() as libc::pid_t,
        },
        mq_protocol::MQRequest::Shutdown { expected_pid } => {
            let pid = std::process::id() as libc::pid_t;
            if expected_pid == pid {
                mq_protocol::MQResponse::ShutdownAck { pid }
            } else {
                mq_protocol::MQResponse::Error("daemon PID does not match request".to_string())
            }
        }

        mq_protocol::MQRequest::Version => mq_protocol::MQResponse::Version {
            version: crate::version_string(),
        },

        mq_protocol::MQRequest::AddCandidate {
            candidate,
            no_merge,
        } => {
            // Get root_dir and VCS for resolution
            let root_dir = state.root_dir();
            let vcs = match get_vcs(&root_dir, None) {
                Ok(v) => v,
                Err(e) => return mq_protocol::MQResponse::Error(format!("VCS error: {}", e)),
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

            let run = state.queue_run(resolved_candidate.clone(), no_merge);
            let run_id = run.id;

            debug!(
                candidate_user = %resolved_candidate.user,
                candidate_commit = %resolved_candidate.commit_id,
                run_id = %run_id,
                no_merge,
                "Added candidate to queue"
            );

            // Send the run to the queue processor
            match queue_sender.send(QueueMessage::Run(run_id)) {
                Ok(_) => mq_protocol::MQResponse::CandidateAdded { run_id },
                Err(e) => mq_protocol::MQResponse::Error(format!("Failed to queue run: {}", e)),
            }
        }

        mq_protocol::MQRequest::List { limit } => {
            let runs = state.list_runs(limit);
            mq_protocol::MQResponse::RunList { runs }
        }

        mq_protocol::MQRequest::GetStatus { run_id } => {
            let run = state.get_run(run_id);
            mq_protocol::MQResponse::RunStatus { run }
        }

        mq_protocol::MQRequest::WaitForRun { run_id } => {
            let mut guard = state.state.lock().unwrap();
            loop {
                match guard.get_run(run_id) {
                    Some(run) if run.completed_at.is_some() => {
                        return mq_protocol::MQResponse::RunStatus { run: Some(run) };
                    }
                    Some(_) => {
                        guard = state.run_completed.wait(guard).unwrap();
                    }
                    None => {
                        return mq_protocol::MQResponse::RunStatus { run: None };
                    }
                }
            }
        }
    }
}

/// Result of running a hook command
struct HookResult {
    success: bool,
    output: String,
}

/// Environment variables for candidate-specific hooks
struct CandidateHookEnv<'a> {
    /// Original candidate commit ID (what user submitted)
    candidate_commit_id: &'a str,
    /// Original candidate change ID (what user submitted)
    candidate_change_id: &'a str,
    /// Original candidate ID (user-provided revision string)
    candidate_id: &'a str,
    /// Base branch name
    base_branch: &'a str,
    /// Tested commit ID, if test integration has completed.
    tested_commit_id: Option<&'a str>,
    /// Tested change ID, if test integration has completed.
    tested_change_id: Option<&'a str>,
    /// Actual landed commit ID, only after successful publication.
    landed_commit_id: Option<&'a str>,
    /// Actual landed change ID, only after successful publication.
    landed_change_id: Option<&'a str>,
}

/// Run a hook command if configured and capture output
fn run_hook(
    hook: Option<&selfci::config::CommandConfig>,
    hook_name: &str,
    root_dir: &Path,
) -> HookResult {
    run_hook_with_env(hook, hook_name, root_dir, None)
}

/// Run a hook command with optional candidate environment variables
fn run_hook_with_env(
    hook: Option<&selfci::config::CommandConfig>,
    hook_name: &str,
    root_dir: &Path,
    candidate_env: Option<&CandidateHookEnv<'_>>,
) -> HookResult {
    let Some(hook_config) = hook else {
        return HookResult {
            success: true,
            output: String::new(),
        };
    };

    if !hook_config.is_set() {
        return HookResult {
            success: true,
            output: String::new(),
        };
    }

    let full_command = hook_config.full_command();
    debug!(hook = hook_name, command = ?full_command, "Running hook");

    // Build command with optional candidate environment variables
    let mut command = cmd(&full_command[0], &full_command[1..]);
    command = command.dir(root_dir).env(envs::SELFCI_RUN_MODE, "mq");

    if let Some(env) = candidate_env {
        command = command
            .env(envs::SELFCI_CANDIDATE_COMMIT_ID, env.candidate_commit_id)
            .env(envs::SELFCI_CANDIDATE_CHANGE_ID, env.candidate_change_id)
            .env(envs::SELFCI_CANDIDATE_ID, env.candidate_id)
            .env(envs::SELFCI_MQ_BASE_BRANCH, env.base_branch);

        // Keep SELFCI_MERGED_* as compatibility aliases for the tested identity.
        if let Some(tested_commit_id) = env.tested_commit_id {
            command = command
                .env(envs::SELFCI_TESTED_COMMIT_ID, tested_commit_id)
                .env(envs::SELFCI_MERGED_COMMIT_ID, tested_commit_id);
        }
        if let Some(tested_change_id) = env.tested_change_id {
            command = command
                .env(envs::SELFCI_TESTED_CHANGE_ID, tested_change_id)
                .env(envs::SELFCI_MERGED_CHANGE_ID, tested_change_id);
        }
        if let Some(landed_commit_id) = env.landed_commit_id {
            command = command.env(envs::SELFCI_LANDED_COMMIT_ID, landed_commit_id);
        }
        if let Some(landed_change_id) = env.landed_change_id {
            command = command.env(envs::SELFCI_LANDED_CHANGE_ID, landed_change_id);
        }
    }

    // Use stdout_capture() to capture output instead of inheriting parent's stdout
    let result = command
        .stderr_to_stdout()
        .stdout_capture()
        .unchecked()
        .run();

    match result {
        Ok(output) => {
            let success = output.status.success();
            let output_str = String::from_utf8_lossy(&output.stdout).to_string();

            if success {
                debug!(hook = hook_name, "Hook succeeded");
            } else {
                debug!(
                    hook = hook_name,
                    "Hook failed with exit code {:?}",
                    output.status.code()
                );
            }

            HookResult {
                success,
                output: output_str,
            }
        }
        Err(e) => {
            debug!(hook = hook_name, error = %e, "Hook execution error");
            HookResult {
                success: false,
                output: format!("Failed to execute hook: {}", e),
            }
        }
    }
}

/// Run a hook command with inherited stdio (for interactive use before daemonization)
/// Returns true if hook succeeded or was not configured, false if it failed
fn run_hook_interactive(
    hook: Option<&selfci::config::CommandConfig>,
    hook_name: &str,
    root_dir: &Path,
) -> bool {
    let Some(hook_config) = hook else {
        return true;
    };

    if !hook_config.is_set() {
        return true;
    }

    let full_command = hook_config.full_command();
    debug!(hook = hook_name, command = ?full_command, "Running interactive hook");

    // Run with inherited stdio - no capture, allows user interaction
    let result = cmd(&full_command[0], &full_command[1..])
        .dir(root_dir)
        .unchecked()
        .run();

    match result {
        Ok(output) => {
            let success = output.status.success();
            if success {
                debug!(hook = hook_name, "Interactive hook succeeded");
            } else {
                debug!(
                    hook = hook_name,
                    "Interactive hook failed with exit code {:?}",
                    output.status.code()
                );
            }
            success
        }
        Err(e) => {
            eprintln!("Failed to execute {} hook: {}", hook_name, e);
            false
        }
    }
}

fn process_queue(
    state: SharedMQState,
    root_dir: PathBuf,
    queue_receiver: mpsc::Receiver<QueueMessage>,
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
        // Wait for the next queue message
        let run_id = match queue_receiver.recv() {
            Ok(QueueMessage::Run(id)) => id,
            Ok(QueueMessage::Shutdown) => {
                debug!("MQ queue shutdown requested");
                break;
            }
            Err(_) => {
                debug!("MQ queue channel closed, exiting process_queue");
                break;
            }
        };

        // Move run from queued to active and get hooks, merge_mode, and shared job states
        let (mut run_info, hooks, merge_mode, shared_job_states) = match state.start_run(run_id) {
            Some(result) => result,
            None => {
                debug!("Run {} not found in queued map", run_id);
                continue;
            }
        };

        debug!(
            run_id = %run_info.id,
            candidate_user = %run_info.candidate.user,
            candidate_commit = %run_info.candidate.commit_id,
            "Processing MQ candidate check"
        );

        // Get base branch
        let base_branch = state.base_branch();

        // Create candidate environment for hooks (before test merge, no merged info yet)
        let candidate_commit_id = run_info.candidate.commit_id.to_string();
        let candidate_change_id = run_info.candidate.change_id.to_string();
        let candidate_id = run_info.candidate.user.to_string();
        let candidate_env_pre_merge = CandidateHookEnv {
            candidate_commit_id: &candidate_commit_id,
            candidate_change_id: &candidate_change_id,
            candidate_id: &candidate_id,
            base_branch: &base_branch,
            tested_commit_id: None,
            tested_change_id: None,
            landed_commit_id: None,
            landed_change_id: None,
        };

        // Run pre-clone hook if configured (runs before worktrees are created)
        // Uses pre-merge env (no merged info yet)
        let pre_clone_result = run_hook_with_env(
            hooks.pre_clone.as_ref(),
            "pre-clone",
            &root_dir,
            Some(&candidate_env_pre_merge),
        );
        if !pre_clone_result.output.is_empty() {
            run_info.output.push_str("### Pre-Clone Hook\n\n");
            run_info.output.push_str(&pre_clone_result.output);
            run_info.output.push('\n');
        }
        if !pre_clone_result.success {
            run_info.status = mq_protocol::MQRunStatus::Failed(mq_protocol::FailedReason::PreClone);
            run_info.output.push_str("\nPre-clone hook failed\n");
            run_info.completed_at = Some(SystemTime::now());
            state.complete_run(run_id, run_info);
            continue;
        }

        // Resolve base branch to immutable ID
        let resolved_base = match selfci::revision::resolve_revision(&vcs, &root_dir, &base_branch)
        {
            Ok(r) => r,
            Err(e) => {
                run_info.status =
                    mq_protocol::MQRunStatus::Failed(mq_protocol::FailedReason::BaseResolve);
                run_info.output.push_str(&format!(
                    "Failed to resolve base branch '{}': {}",
                    base_branch, e
                ));
                run_info.completed_at = Some(SystemTime::now());
                state.complete_run(run_id, run_info);
                continue;
            }
        };

        // Create test merge/rebase of candidate onto base for CI testing
        let test_merge_result = match create_test_merge(
            &root_dir,
            resolved_base.commit_id.as_str(),
            &run_info.candidate,
            &merge_mode,
        ) {
            Ok(result) => result,
            Err(e) => {
                let fail_reason = match merge_mode {
                    selfci::config::MergeMode::Rebase => mq_protocol::FailedReason::TestRebase,
                    selfci::config::MergeMode::Merge => mq_protocol::FailedReason::TestMerge,
                };
                run_info.status = mq_protocol::MQRunStatus::Failed(fail_reason);
                run_info.output.push_str(&format!(
                    "Failed to create test merge/rebase of candidate onto base: {}",
                    e
                ));
                run_info.completed_at = Some(SystemTime::now());
                state.complete_run(run_id, run_info);
                continue;
            }
        };

        // Store the test merge output in the job info
        run_info.test_merge_output = test_merge_result.output.clone();

        // Use the merged commit for CI testing
        let merged_commit_id = test_merge_result.commit_id.to_string();
        let merged_change_id = test_merge_result.change_id.to_string();

        // Create candidate environment with merged info for post-merge hooks
        let candidate_env_tested = CandidateHookEnv {
            candidate_commit_id: &candidate_commit_id,
            candidate_change_id: &candidate_change_id,
            candidate_id: &candidate_id,
            base_branch: &base_branch,
            tested_commit_id: Some(&merged_commit_id),
            tested_change_id: Some(&merged_change_id),
            landed_commit_id: None,
            landed_change_id: None,
        };

        // Create a ResolvedRevision for the merged commit (keeping original user string for display)
        let merged_candidate = selfci::revision::ResolvedRevision {
            user: run_info.candidate.user.clone(),
            commit_id: test_merge_result.commit_id,
            change_id: test_merge_result.change_id,
        };

        // Determine parallelism (default to 1 for merge queue)
        let parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        // Build post-clone hook config if hook is configured
        // SELFCI_CANDIDATE_* = original candidate (what user submitted)
        // SELFCI_MERGED_* = merged commit (what CI will test)
        let post_clone_hook =
            hooks
                .post_clone
                .as_ref()
                .map(|hook| super::check::PostCloneHookConfig {
                    hook,
                    candidate_commit_id: &candidate_commit_id,
                    candidate_change_id: &candidate_change_id,
                    candidate_id: &candidate_id,
                    base_branch: &base_branch,
                    merged_commit_id: Some(&merged_commit_id),
                    merged_change_id: Some(&merged_change_id),
                });

        // Run the candidate check using the shared implementation
        // Pass the merged commit as the working candidate (what CI will test)
        // Pass the original candidate so SELFCI_CANDIDATE_* env vars refer to what user submitted
        match super::check::run_candidate_check(
            &root_dir,
            &resolved_base,
            &merged_candidate,
            parallelism,
            None,
            super::check::CheckMode::MergeQueue,
            post_clone_hook,
            Some(&run_info.candidate), // Original candidate for SELFCI_CANDIDATE_* env vars
            Some(&shared_job_states),
        ) {
            Ok(result) => {
                // Handle post-clone hook output if present
                if let Some(output) = &result.post_clone_output
                    && !output.is_empty()
                {
                    run_info.output.push_str("### Post-Clone Hook\n\n");
                    run_info.output.push_str(output);
                    run_info.output.push('\n');
                }

                // Check if post-clone hook failed
                if result.post_clone_success == Some(false) {
                    run_info.status =
                        mq_protocol::MQRunStatus::Failed(mq_protocol::FailedReason::PostClone);
                    run_info.output.push_str("\nPost-clone hook failed\n");
                    run_info.completed_at = Some(SystemTime::now());
                    state.complete_run(run_id, run_info);
                    continue;
                }

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

                // Append check output to job output (preserving hook outputs)
                run_info.output.push_str(&result.output);
                run_info.active_jobs = Vec::new(); // Active jobs list is only populated for status queries
                run_info.completed_steps = result.steps.clone(); // Store completed steps for status display
                run_info.completed_jobs = result.jobs.clone(); // Store completed jobs for status display
                run_info.completed_at = Some(SystemTime::now());

                if job_passed {
                    // Merge into base branch if no_merge is false
                    if run_info.no_merge {
                        debug!(
                            "MQ candidate check {} passed (no-merge mode, skipping merge)",
                            run_info.id
                        );
                        run_info.status =
                            mq_protocol::MQRunStatus::Passed(mq_protocol::PassedReason::NoMerge);
                    } else {
                        debug!(
                            "MQ candidate check {} passed, merging into {}",
                            run_info.id, base_branch
                        );

                        // Run pre-merge hook if configured
                        // Uses post-merge env (has merged info)
                        let pre_merge_result = run_hook_with_env(
                            hooks.pre_merge.as_ref(),
                            "pre-merge",
                            &root_dir,
                            Some(&candidate_env_tested),
                        );
                        if !pre_merge_result.output.is_empty() {
                            run_info.output.push_str("\n\n### Pre-Merge Hook\n\n");
                            run_info.output.push_str(&pre_merge_result.output);
                        }

                        if !pre_merge_result.success {
                            run_info
                                .output
                                .push_str("\n\nPre-merge hook failed, skipping merge\n");
                            run_info.status = mq_protocol::MQRunStatus::Failed(
                                mq_protocol::FailedReason::PreMerge,
                            );
                        } else {
                            // Atomically publish the exact integration that passed
                            // against the exact base used to prepare it.
                            match publish_prepared_candidate(
                                &root_dir,
                                &base_branch,
                                &resolved_base.commit_id,
                                &merged_candidate,
                            ) {
                                Ok(PublicationOutcome::Verified(landing)) => {
                                    // Append merge output with separator
                                    let header = match merge_mode {
                                        selfci::config::MergeMode::Rebase => "### Final Rebase",
                                        selfci::config::MergeMode::Merge => "### Final Merge",
                                    };
                                    run_info.output.push_str("\n\n");
                                    run_info.output.push_str(header);
                                    run_info.output.push_str("\n\n");
                                    run_info.output.push_str(&landing.output);

                                    let landed_commit_id = landing.commit_id.to_string();
                                    let landed_change_id = landing.change_id.to_string();
                                    let candidate_env_landed = CandidateHookEnv {
                                        candidate_commit_id: &candidate_commit_id,
                                        candidate_change_id: &candidate_change_id,
                                        candidate_id: &candidate_id,
                                        base_branch: &base_branch,
                                        tested_commit_id: Some(&merged_commit_id),
                                        tested_change_id: Some(&merged_change_id),
                                        landed_commit_id: Some(&landed_commit_id),
                                        landed_change_id: Some(&landed_change_id),
                                    };

                                    // Run post-merge hook if configured
                                    // Uses post-merge env (has merged info)
                                    let post_merge_result = run_hook_with_env(
                                        hooks.post_merge.as_ref(),
                                        "post-merge",
                                        &root_dir,
                                        Some(&candidate_env_landed),
                                    );
                                    if !post_merge_result.output.is_empty() {
                                        run_info.output.push_str("\n\n### Post-Merge Hook\n\n");
                                        run_info.output.push_str(&post_merge_result.output);
                                    }

                                    if !post_merge_result.success {
                                        run_info.output.push_str("\n\nPost-merge hook failed\n");
                                        // Note: merge already happened, so we still report success
                                        // but log the hook failure
                                    }

                                    let passed_reason = match merge_mode {
                                        selfci::config::MergeMode::Rebase => {
                                            mq_protocol::PassedReason::Rebased
                                        }
                                        selfci::config::MergeMode::Merge => {
                                            mq_protocol::PassedReason::Merged
                                        }
                                    };
                                    run_info.status =
                                        mq_protocol::MQRunStatus::Passed(passed_reason);
                                }
                                Ok(PublicationOutcome::AppliedUnverified { commit_id, output }) => {
                                    run_info
                                        .output
                                        .push_str("\n\n### Publication Applied but Unverified\n\n");
                                    run_info.output.push_str(&output);
                                    run_info.output.push_str(&format!(
                                        "\nSelfCI requested publication of checked commit \
                                         {commit_id}, but could not verify the resulting bookmark \
                                         identity. Inspect the repository before retrying.\n"
                                    ));
                                    run_info.status = mq_protocol::MQRunStatus::Failed(
                                        mq_protocol::FailedReason::PublicationUnverified,
                                    );
                                }
                                Err(e) => {
                                    run_info
                                        .output
                                        .push_str(&format!("\n\n### Merge Failed\n\n{}", e));
                                    run_info.status = mq_protocol::MQRunStatus::Failed(
                                        mq_protocol::FailedReason::Merge,
                                    );
                                }
                            }
                        }
                    }
                } else {
                    run_info.status =
                        mq_protocol::MQRunStatus::Failed(mq_protocol::FailedReason::Check);
                    debug!("MQ candidate check {} failed", run_info.id);
                }
            }
            Err(e) => {
                run_info.status =
                    mq_protocol::MQRunStatus::Failed(mq_protocol::FailedReason::Check);
                run_info.output = format!("Check failed: {}", e);
                run_info.completed_at = Some(SystemTime::now());
                debug!("MQ candidate check {} failed: {}", run_info.id, e);
            }
        }

        // Move job from active to completed
        state.complete_run(run_id, run_info);
    }
}

/// Result of a test merge operation (merge/rebase before CI check)
pub(crate) struct TestMergeOutcome {
    /// The commit ID of the merged/rebased commit
    pub(crate) commit_id: selfci::revision::CommitId,
    /// The change ID (for jujutsu, same as commit_id for git)
    pub(crate) change_id: selfci::revision::ChangeId,
    /// Log of commands executed during the test merge
    pub(crate) output: String,
}

/// Test rebase for Git - rebases candidate onto base without updating any refs
/// Returns the resulting commit ID and log of commands executed
fn test_merge_git_rebase(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<TestMergeOutcome, selfci::MergeError> {
    let mut output_log = String::new();

    // Create temporary worktree in detached HEAD state at candidate commit
    let temp_worktree = root_dir.join(format!(".git/selfci-test-worktree-{}", candidate.commit_id));

    output_log.push_str(&format!(
        "Git test rebase: rebasing {} onto {}\n\n",
        candidate.commit_id, base_branch
    ));

    let worktree_cmd = Cmd::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &temp_worktree.display().to_string(),
            candidate.commit_id.as_str(),
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", worktree_cmd.log_line());
    let output = worktree_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::WorktreeCreateFailed)?;
    output_log.push_str(&output);
    output_log.push_str("\n\n");

    // Ensure cleanup on any exit path
    let cleanup = scopeguard::guard((), |_| {
        let _ = cmd!("git", "worktree", "remove", "--force", &temp_worktree)
            .dir(root_dir)
            .run();
    });

    // In the worktree, rebase onto base_branch
    let rebase_cmd = Cmd::new("git")
        .args(["rebase", base_branch])
        .dir(&temp_worktree);
    let _ = write!(output_log, "{}", rebase_cmd.log_line());
    let rebase_result = rebase_cmd
        .to_expression()
        .stderr_to_stdout()
        .stdout_capture()
        .unchecked()
        .run()
        .map_err(|e| selfci::MergeError::RebaseFailed(selfci::CommandOutputError(e.to_string())))?;

    let rebase_output = String::from_utf8_lossy(&rebase_result.stdout).to_string();
    output_log.push_str(&rebase_output);
    output_log.push_str("\n\n");

    if !rebase_result.status.success() {
        // Abort the rebase to clean up
        let _ = cmd!("git", "rebase", "--abort").dir(&temp_worktree).run();
        return Err(selfci::MergeError::RebaseFailed(
            selfci::CommandOutputError(rebase_output),
        ));
    }

    // Get the resulting commit ID (HEAD in worktree)
    let rev_parse_cmd = Cmd::new("git")
        .args(["rev-parse", "HEAD"])
        .dir(&temp_worktree);
    let _ = write!(output_log, "{}", rev_parse_cmd.log_line());
    let commit_id = rev_parse_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?
        .trim()
        .to_string();
    output_log.push_str(&format!("{}\n\n", commit_id));

    // Cleanup is handled by scopeguard
    drop(cleanup);

    Ok(TestMergeOutcome {
        commit_id: selfci::revision::CommitId::new(commit_id.clone())
            .expect("git rev-parse returned invalid commit id"),
        change_id: selfci::revision::ChangeId::new(commit_id),
        output: output_log,
    })
}

/// Test merge for Git - merges candidate into base without updating any refs
/// Returns the resulting commit ID and log of commands executed
fn test_merge_git_merge(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<TestMergeOutcome, selfci::MergeError> {
    let mut output_log = String::new();

    // Create temporary worktree at base branch
    let temp_worktree = root_dir.join(format!(".git/selfci-test-worktree-{}", candidate.commit_id));

    output_log.push_str(&format!(
        "Git test merge: merging {} into {}\n\n",
        candidate.commit_id, base_branch
    ));

    let worktree_cmd = Cmd::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &temp_worktree.display().to_string(),
            base_branch,
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", worktree_cmd.log_line());
    let output = worktree_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::WorktreeCreateFailed)?;
    output_log.push_str(&output);
    output_log.push_str("\n\n");

    // Ensure cleanup on any exit path
    let cleanup = scopeguard::guard((), |_| {
        let _ = cmd!("git", "worktree", "remove", "--force", &temp_worktree)
            .dir(root_dir)
            .run();
    });

    // In the worktree, merge candidate
    let merge_message = format!("Merge commit '{}' by SelfCI", candidate.commit_id);
    let merge_cmd = Cmd::new("git")
        .args([
            "merge",
            "--no-ff",
            "-m",
            &merge_message,
            candidate.commit_id.as_str(),
        ])
        .dir(&temp_worktree);
    let _ = write!(output_log, "{}", merge_cmd.log_line());
    let merge_result = merge_cmd
        .to_expression()
        .stderr_to_stdout()
        .stdout_capture()
        .unchecked()
        .run()
        .map_err(|e| selfci::MergeError::MergeFailed(selfci::CommandOutputError(e.to_string())))?;

    let merge_output = String::from_utf8_lossy(&merge_result.stdout).to_string();
    output_log.push_str(&merge_output);
    output_log.push_str("\n\n");

    if !merge_result.status.success() {
        // Abort the merge to clean up
        let _ = cmd!("git", "merge", "--abort").dir(&temp_worktree).run();
        return Err(selfci::MergeError::MergeFailed(selfci::CommandOutputError(
            merge_output,
        )));
    }

    // Get the resulting commit ID (HEAD in worktree)
    let rev_parse_cmd = Cmd::new("git")
        .args(["rev-parse", "HEAD"])
        .dir(&temp_worktree);
    let _ = write!(output_log, "{}", rev_parse_cmd.log_line());
    let commit_id = rev_parse_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?
        .trim()
        .to_string();
    output_log.push_str(&format!("{}\n\n", commit_id));

    output_log.push_str("Test merge complete\n");

    // Cleanup is handled by scopeguard
    drop(cleanup);

    Ok(TestMergeOutcome {
        commit_id: selfci::revision::CommitId::new(commit_id.clone())
            .expect("git rev-parse returned invalid commit id"),
        change_id: selfci::revision::ChangeId::new(commit_id),
        output: output_log,
    })
}

/// Count revisions before a mutating jj command.
fn count_jj_revisions(
    root_dir: &Path,
    revset: &str,
    output_log: &mut String,
) -> Result<usize, selfci::MergeError> {
    let command = Cmd::new("jj")
        .args([
            "log",
            "-r",
            revset,
            "-T",
            r#""revision\n""#,
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", command.log_line());
    let output = command
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?;
    output_log.push_str(&output);
    output_log.push_str("\n\n");
    Ok(output.lines().count())
}

/// Check whether a string is one full jj change ID.
fn is_full_jj_change_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_lowercase())
}

/// Check whether a string is a full or display-width jj commit ID.
fn is_jj_commit_id(value: &str) -> bool {
    (12..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_full_jj_commit_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn invalid_jj_machine_output(output: &str) -> selfci::MergeError {
    selfci::MergeError::ChangeIdFailed(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("unexpected jj machine output: {output:?}"),
    ))
}

/// Parse exactly one full Jujutsu change ID.
fn parse_single_full_jj_change_id(
    output: &str,
) -> Result<selfci::revision::ChangeId, selfci::MergeError> {
    match output.lines().collect::<Vec<_>>().as_slice() {
        [value] if is_full_jj_change_id(value) => Ok(selfci::revision::ChangeId::new(*value)),
        _ => Err(invalid_jj_machine_output(output)),
    }
}

/// Parse exactly one full Jujutsu commit ID.
fn parse_single_full_jj_commit_id(
    output: &str,
) -> Result<selfci::revision::CommitId, selfci::MergeError> {
    match output.lines().collect::<Vec<_>>().as_slice() {
        [value] if is_full_jj_commit_id(value) => {
            selfci::revision::CommitId::new(*value).map_err(|_| invalid_jj_machine_output(output))
        }
        _ => Err(invalid_jj_machine_output(output)),
    }
}

/// Parse zero or more full Jujutsu commit IDs, one per line.
fn parse_full_jj_commit_ids(
    output: &str,
) -> Result<Vec<selfci::revision::CommitId>, selfci::MergeError> {
    output
        .lines()
        .map(|value| {
            if is_full_jj_commit_id(value) {
                selfci::revision::CommitId::new(value)
                    .map_err(|_| invalid_jj_machine_output(output))
            } else {
                Err(invalid_jj_machine_output(output))
            }
        })
        .collect()
}

/// Parse full Jujutsu change/commit identity pairs, one revision per line.
fn parse_full_jj_revision_ids(
    output: &str,
) -> Result<Vec<(selfci::revision::ChangeId, selfci::revision::CommitId)>, selfci::MergeError> {
    output
        .lines()
        .map(|line| {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() != 2
                || !is_full_jj_change_id(parts[0])
                || !is_full_jj_commit_id(parts[1])
            {
                return Err(invalid_jj_machine_output(output));
            }
            Ok((
                selfci::revision::ChangeId::new(parts[0]),
                selfci::revision::CommitId::new(parts[1])
                    .expect("validated full Jujutsu commit ID"),
            ))
        })
        .collect()
}

/// Require jj support for isolated, unintegrated operations.
fn require_unintegrated_operations(root_dir: &Path) -> Result<(), selfci::MergeError> {
    let supported = cmd!("jj", "--help")
        .dir(root_dir)
        .read()
        .map(|output| output.contains("--no-integrate-operation"))
        .map_err(selfci::MergeError::UnsupportedJj)?;
    if supported {
        Ok(())
    } else {
        Err(selfci::MergeError::UnsupportedJj(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this jj version lacks required --no-integrate-operation support",
        )))
    }
}

/// Extract the operation ID emitted by `--no-integrate-operation`.
fn parse_unintegrated_operation_id(output: &str) -> Option<&str> {
    output.lines().find_map(|line| {
        line.strip_prefix(
            "Operation left uncommitted because --no-integrate-operation was requested: ",
        )
    })
}

/// Integrate a prepared Jujutsu operation into the repository.
fn integrate_jj_operation(
    root_dir: &Path,
    operation_id: &str,
    output_log: &mut String,
) -> Result<(), selfci::MergeError> {
    let command = Cmd::new("jj")
        .args(["--ignore-working-copy", "op", "integrate", operation_id])
        .dir(root_dir);
    let _ = write!(output_log, "{}", command.log_line());
    let output = command
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    output_log.push_str(&output);
    output_log.push_str("\n\n");
    Ok(())
}

struct JjUniqueMutableStack {
    candidate_change_id: selfci::revision::ChangeId,
    unique_stack: Vec<JjStackRevision>,
}

/// One exact submitted-stack revision.
struct JjStackRevision {
    commit_id: selfci::revision::CommitId,
}

fn jj_unique_mutable_stack(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
    output_log: &mut String,
) -> Result<JjUniqueMutableStack, selfci::MergeError> {
    let change_id_cmd = Cmd::new("jj")
        .args([
            "log",
            "-r",
            candidate.commit_id.as_str(),
            "-T",
            "self.change_id()",
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", change_id_cmd.log_line());
    let candidate_change_id_output = change_id_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?;
    let candidate_change_id = parse_single_full_jj_change_id(&candidate_change_id_output)?;
    output_log.push_str(&format!("{}\n\n", candidate_change_id));

    let base_revisions_cmd = Cmd::new("jj")
        .args([
            "log",
            "-r",
            &format!("::{}", base_branch),
            "-T",
            r#"self.change_id() ++ " " ++ self.commit_id() ++ "\n""#,
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", base_revisions_cmd.log_line());
    let base_revisions_output = base_revisions_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?;
    output_log.push_str(&base_revisions_output);
    output_log.push_str("\n\n");
    let mut base_revisions: HashMap<_, Vec<_>> = HashMap::new();
    for (change_id, commit_id) in parse_full_jj_revision_ids(&base_revisions_output)? {
        base_revisions.entry(change_id).or_default().push(commit_id);
    }

    let candidate_ancestry_cmd = Cmd::new("jj")
        .args([
            "log",
            "-r",
            &format!("::{}", candidate.commit_id),
            "-T",
            r#"self.change_id() ++ " " ++ self.commit_id() ++ "\n""#,
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", candidate_ancestry_cmd.log_line());
    let candidate_ancestry_output = candidate_ancestry_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?;
    output_log.push_str(&format!("{}\n", candidate_ancestry_output));
    for (change_id, commit_id) in parse_full_jj_revision_ids(&candidate_ancestry_output)? {
        if base_revisions.get(&change_id).is_some_and(|base_commits| {
            base_commits
                .iter()
                .any(|base_commit| base_commit != &commit_id)
        }) {
            return Err(selfci::MergeError::RebaseFailed(
                selfci::CommandOutputError(format!(
                    "checked base contains a different revision of submitted change {change_id}"
                )),
            ));
        }
    }

    let candidate_stack_cmd = Cmd::new("jj")
        .args([
            "log",
            "-r",
            &format!("::{} ~ ::{}", candidate.commit_id, base_branch),
            "-T",
            r#"self.change_id() ++ " " ++ self.commit_id() ++ "\n""#,
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", candidate_stack_cmd.log_line());
    let candidate_stack_output = candidate_stack_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?;
    output_log.push_str(&format!("{}\n", candidate_stack_output));

    let mut unique_stack = Vec::new();
    for (_, commit_id) in parse_full_jj_revision_ids(&candidate_stack_output)? {
        unique_stack.push(JjStackRevision { commit_id });
    }
    unique_stack.reverse();

    Ok(JjUniqueMutableStack {
        candidate_change_id,
        unique_stack,
    })
}

/// Test rebase for Jujutsu - duplicates and rebases the unique mutable suffix onto base
/// Uses jj duplicate to create a copy, leaving the original candidate untouched
/// Returns the resulting commit and change IDs of the duplicated, rebased commits
fn test_merge_jj_rebase(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<TestMergeOutcome, selfci::MergeError> {
    let mut output_log = String::new();
    let stack = jj_unique_mutable_stack(root_dir, base_branch, candidate, &mut output_log)?;

    // Preserve the exact submitted revision when it already descends from the
    // exact checked base. In particular, do not replace an immutable submitted
    // commit with a synthetic duplicate that has identical content.
    let candidate_on_base = count_jj_revisions(
        root_dir,
        &format!("{} & {}::", candidate.commit_id, base_branch),
        &mut output_log,
    )? == 1;
    if candidate_on_base {
        return Ok(TestMergeOutcome {
            commit_id: candidate.commit_id.clone(),
            change_id: stack.candidate_change_id,
            output: output_log,
        });
    }

    // A submitted strict ancestor is already integrated. Test the current exact
    // base rather than stale candidate content, and publish a no-op CAS.
    let base_on_candidate = count_jj_revisions(
        root_dir,
        &format!("{} & {}::", base_branch, candidate.commit_id),
        &mut output_log,
    )? == 1;
    if base_on_candidate {
        let base_change_cmd = Cmd::new("jj")
            .args([
                "log",
                "-r",
                base_branch,
                "-T",
                "self.change_id()",
                "--no-graph",
                "--color=never",
            ])
            .dir(root_dir);
        let _ = write!(output_log, "{}", base_change_cmd.log_line());
        let base_change_output = base_change_cmd
            .to_expression()
            .read()
            .map_err(selfci::MergeError::ChangeIdFailed)?;
        let base_change_id = parse_single_full_jj_change_id(&base_change_output)?;
        output_log.push_str(&format!("{}\n\n", base_change_id));
        return Ok(TestMergeOutcome {
            commit_id: selfci::revision::CommitId::new(base_branch)
                .expect("merge-queue base is an exact commit ID"),
            change_id: base_change_id,
            output: output_log,
        });
    }

    if stack.unique_stack.is_empty() {
        return Err(selfci::MergeError::RebaseFailed(
            selfci::CommandOutputError(
                "submitted candidate has no exact suffix to rebase".to_string(),
            ),
        ));
    }

    let stack_root = &stack
        .unique_stack
        .first()
        .expect("empty exact suffix returned above")
        .commit_id;
    let dup_revset = format!("{}::{}", stack_root, candidate.commit_id);
    let expected_duplicate_count = count_jj_revisions(root_dir, &dup_revset, &mut output_log)?;
    require_unintegrated_operations(root_dir)?;
    let dup_cmd = Cmd::new("jj")
        .args([
            "--ignore-working-copy",
            "--no-integrate-operation",
            "--color=never",
            "--config",
            JJ_MACHINE_COMMIT_SUMMARY_CONFIG,
            "duplicate",
            &dup_revset,
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", dup_cmd.log_line());
    let dup_output = dup_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(|e| selfci::MergeError::RebaseFailed(selfci::CommandOutputError(e.to_string())))?;
    output_log.push_str(&dup_output);
    output_log.push_str("\n\n");

    let operation_id = parse_unintegrated_operation_id(&dup_output).ok_or_else(|| {
        selfci::MergeError::RebaseFailed(selfci::CommandOutputError(format!(
            "Failed to find unintegrated operation ID in output: {}",
            dup_output
        )))
    })?;

    let mut duplicated_change_ids = Vec::with_capacity(expected_duplicate_count);
    let mut duplicated_commit_ids = Vec::with_capacity(expected_duplicate_count);
    for line in dup_output.lines() {
        let Some(rest) = line.strip_prefix("Duplicated ") else {
            continue;
        };
        let parts: Vec<_> = rest.split_whitespace().collect();
        let valid = parts.len() == 4
            && parts[1] == "as"
            && is_jj_commit_id(parts[0])
            && is_full_jj_change_id(parts[2])
            && selfci::revision::CommitId::new(parts[3]).is_ok();
        if !valid {
            return Err(selfci::MergeError::RebaseFailed(
                selfci::CommandOutputError(format!(
                    "Invalid duplicate output line: {line}\nFull output:\n{dup_output}"
                )),
            ));
        }
        duplicated_change_ids.push(selfci::revision::ChangeId::new(parts[2]));
        duplicated_commit_ids.push(
            selfci::revision::CommitId::new(parts[3]).expect("validated duplicate commit ID"),
        );
    }

    if duplicated_change_ids.len() != expected_duplicate_count {
        return Err(selfci::MergeError::RebaseFailed(
            selfci::CommandOutputError(format!(
                "Expected {expected_duplicate_count} duplicated changes, found {} in output:\n{}",
                duplicated_change_ids.len(),
                dup_output,
            )),
        ));
    }

    let dup_root_commit_id = duplicated_commit_ids
        .first()
        .expect("duplicated commit IDs are non-empty");
    let dup_change_id = duplicated_change_ids
        .last()
        .cloned()
        .expect("duplicated change IDs are non-empty");

    // Rebase within the isolated duplicate operation. Exact duplicate commit
    // IDs select the source, so no integrated rewrite can supersede it.
    let rebase_cmd = Cmd::new("jj")
        .args([
            "--ignore-working-copy",
            "--at-operation",
            operation_id,
            "--no-integrate-operation",
            "rebase",
            "-s",
            dup_root_commit_id.as_str(),
            "-d",
            base_branch,
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", rebase_cmd.log_line());
    let rebase_result = rebase_cmd
        .to_expression()
        .stderr_to_stdout()
        .stdout_capture()
        .unchecked()
        .run()
        .map_err(|e| selfci::MergeError::RebaseFailed(selfci::CommandOutputError(e.to_string())))?;

    let rebase_output = String::from_utf8_lossy(&rebase_result.stdout).to_string();
    output_log.push_str(&rebase_output);
    output_log.push_str("\n\n");

    if !rebase_result.status.success() {
        return Err(selfci::MergeError::RebaseFailed(
            selfci::CommandOutputError(rebase_output),
        ));
    }

    let rebase_operation_id = parse_unintegrated_operation_id(&rebase_output).ok_or_else(|| {
        selfci::MergeError::RebaseFailed(selfci::CommandOutputError(format!(
            "Failed to find unintegrated rebase operation ID in output: {rebase_output}"
        )))
    })?;

    // Resolve every duplicated revision only inside the isolated operation.
    // Change IDs are unambiguous there, and validating the exact resulting set
    // prevents an integrated rewrite from superseding preparation.
    let owned_revset = duplicated_change_ids
        .iter()
        .map(selfci::revision::ChangeId::as_str)
        .collect::<Vec<_>>()
        .join(" | ");
    let owned_commits_cmd = Cmd::new("jj")
        .args([
            "--at-operation",
            rebase_operation_id,
            "log",
            "-r",
            &owned_revset,
            "-T",
            "self.commit_id() ++ '\n'",
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", owned_commits_cmd.log_line());
    let owned_commits_output = owned_commits_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?;
    let owned_commit_ids = parse_full_jj_commit_ids(&owned_commits_output)?;
    if owned_commit_ids.len() != expected_duplicate_count {
        return Err(invalid_jj_machine_output(&owned_commits_output));
    }
    output_log.push_str(&owned_commits_output);
    output_log.push_str("\n\n");

    // Get the commit ID of the rebased duplicate in that same operation.
    let commit_id_cmd = Cmd::new("jj")
        .args([
            "--at-operation",
            rebase_operation_id,
            "log",
            "-r",
            dup_change_id.as_str(),
            "-T",
            "self.commit_id()",
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", commit_id_cmd.log_line());
    let commit_id_output = commit_id_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?;
    let commit_id = parse_single_full_jj_commit_id(&commit_id_output)?;
    output_log.push_str(&format!("{}\n\n", commit_id));

    integrate_jj_operation(root_dir, rebase_operation_id, &mut output_log)?;

    // Update working copy snapshot to avoid stale errors
    let update_cmd = Cmd::new("jj")
        .args(["workspace", "update-stale"])
        .dir(root_dir);
    let _ = write!(output_log, "{}", update_cmd.log_line());
    let update_output = update_cmd
        .to_expression()
        .stdin_null()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    output_log.push_str(&update_output);
    output_log.push_str("\n\n");

    Ok(TestMergeOutcome {
        commit_id,
        change_id: dup_change_id,
        output: output_log,
    })
}

/// Test merge for Jujutsu - creates a merge commit without updating bookmarks
/// Returns the resulting commit and change IDs
fn test_merge_jj_merge(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<TestMergeOutcome, selfci::MergeError> {
    let mut output_log = String::new();

    output_log.push_str(&format!(
        "Jujutsu test merge: merging {} into {}\n\n",
        candidate.commit_id, base_branch
    ));

    // Create a new merge commit with both base and candidate as parents
    require_unintegrated_operations(root_dir)?;
    let new_cmd = Cmd::new("jj")
        .args([
            "--ignore-working-copy",
            "--no-integrate-operation",
            "--color=never",
            "--config",
            JJ_MACHINE_COMMIT_SUMMARY_CONFIG,
            "new",
            "--no-edit",
            "-m",
            &format!("Merge commit '{}' by SelfCI", candidate.commit_id),
            base_branch,
            candidate.commit_id.as_str(),
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", new_cmd.log_line());
    let output = new_cmd
        .to_expression()
        .stdin_null()
        .stderr_to_stdout()
        .read()
        .map_err(|e| selfci::MergeError::MergeFailed(selfci::CommandOutputError(e.to_string())))?;
    output_log.push_str(&output);
    output_log.push_str("\n\n");

    // Keep the synthetic merge invisible until its exact full identity has
    // been discovered.
    let operation_id = parse_unintegrated_operation_id(&output).ok_or_else(|| {
        selfci::MergeError::MergeFailed(selfci::CommandOutputError(format!(
            "Failed to find unintegrated operation ID in output: {}",
            output
        )))
    })?;

    // The command-local template fixes the exact output shape and excludes
    // user-controlled summaries, including embedded newlines.
    let line = output
        .lines()
        .find(|line| line.starts_with("Created new commit "))
        .ok_or_else(|| {
            selfci::MergeError::MergeFailed(selfci::CommandOutputError(format!(
                "Failed to parse merge commit from output: {}",
                output
            )))
        })?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    let valid = parts.len() == 5
        && parts[..3] == ["Created", "new", "commit"]
        && is_full_jj_change_id(parts[3])
        && selfci::revision::CommitId::new(parts[4]).is_ok();
    if !valid {
        return Err(selfci::MergeError::MergeFailed(selfci::CommandOutputError(
            format!("Invalid new commit output line: {line}\nFull output:\n{output}"),
        )));
    }
    let change_id = selfci::revision::ChangeId::new(parts[3]);
    let commit_id = parts[4].to_string();
    integrate_jj_operation(root_dir, operation_id, &mut output_log)?;

    // Update working copy snapshot to avoid stale errors
    let update_cmd = Cmd::new("jj")
        .args(["workspace", "update-stale"])
        .dir(root_dir);
    let _ = write!(output_log, "{}", update_cmd.log_line());
    let update_output = update_cmd
        .to_expression()
        .stdin_null()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    output_log.push_str(&update_output);
    output_log.push_str("\n\n");

    output_log.push_str("Test merge complete\n");

    Ok(TestMergeOutcome {
        commit_id: selfci::revision::CommitId::new(commit_id.to_string())
            .expect("jj new returned invalid commit id"),
        change_id,
        output: output_log,
    })
}

/// Create a test merge/rebase of candidate onto base for CI testing
/// This does NOT update any refs - the resulting commit is dangling (Git) or just exists (jj)
pub(crate) fn create_test_merge(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
    merge_mode: &selfci::config::MergeMode,
) -> Result<TestMergeOutcome, selfci::MergeError> {
    // Detect VCS
    let vcs = get_vcs(root_dir, None).map_err(|e| {
        selfci::MergeError::ConfigReadFailed(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            e.to_string(),
        ))
    })?;

    debug!(vcs = ?vcs, merge_mode = ?merge_mode, "Creating test merge");

    match (vcs, merge_mode) {
        (selfci::VCS::Git, selfci::config::MergeMode::Rebase) => {
            test_merge_git_rebase(root_dir, base_branch, candidate)
        }
        (selfci::VCS::Git, selfci::config::MergeMode::Merge) => {
            test_merge_git_merge(root_dir, base_branch, candidate)
        }
        (selfci::VCS::Jujutsu, selfci::config::MergeMode::Rebase) => {
            test_merge_jj_rebase(root_dir, base_branch, candidate)
        }
        (selfci::VCS::Jujutsu, selfci::config::MergeMode::Merge) => {
            test_merge_jj_merge(root_dir, base_branch, candidate)
        }
    }
}

/// Identity and command log for an integration published to the configured base.
struct LandingOutcome {
    /// Exact commit now named by the base.
    commit_id: selfci::revision::CommitId,
    /// Exact change now named by the base.
    change_id: selfci::revision::ChangeId,
    /// Publication command output.
    output: String,
}

/// Result of attempting to publish a prepared integration.
enum PublicationOutcome {
    /// The expected-old update applied and the actual identity was verified.
    Verified(LandingOutcome),
    /// The update applied, but a later identity verification failed.
    AppliedUnverified {
        /// Exact checked commit requested as the publication target.
        commit_id: selfci::revision::CommitId,
        /// Diagnostic command log and verification error.
        output: String,
    },
}

/// Atomically publish the exact prepared integration if the base still names
/// the commit against which it was checked.
fn publish_prepared_candidate(
    root_dir: &Path,
    base_branch: &str,
    expected_base: &selfci::revision::CommitId,
    prepared: &selfci::revision::ResolvedRevision,
) -> Result<PublicationOutcome, selfci::MergeError> {
    let vcs = get_vcs(root_dir, None).map_err(|e| {
        selfci::MergeError::ConfigReadFailed(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            e.to_string(),
        ))
    })?;
    let mut output_log = String::new();

    match vcs {
        selfci::VCS::Git => {
            let reference = format!("refs/heads/{base_branch}");
            let update_ref = Cmd::new("git")
                .args([
                    "update-ref",
                    &reference,
                    prepared.commit_id.as_str(),
                    expected_base.as_str(),
                ])
                .dir(root_dir);
            let _ = write!(output_log, "{}", update_ref.log_line());
            let output = update_ref
                .to_expression()
                .stderr_to_stdout()
                .read()
                .map_err(selfci::MergeError::BranchUpdateFailed)?;
            output_log.push_str(&output);
        }
        selfci::VCS::Jujutsu => {
            let expected_target = format!("{base_branch} & {expected_base}");
            let bookmark_move = Cmd::new("jj")
                .args([
                    "--ignore-working-copy",
                    "--config",
                    "ui.quiet=false",
                    "--color=never",
                    "--no-pager",
                    "bookmark",
                    "move",
                    base_branch,
                    "--from",
                    &expected_target,
                    "--to",
                    prepared.commit_id.as_str(),
                ])
                .dir(root_dir);
            let _ = write!(output_log, "{}", bookmark_move.log_line());
            let output = match bookmark_move.to_expression().stderr_to_stdout().read() {
                Ok(output) => output,
                Err(error) => {
                    return Err(selfci::MergeError::BranchUpdateFailed(error));
                }
            };
            output_log.push_str(&output);
            output_log.push('\n');
            let no_bookmark_updated = output.lines().any(|line| line == "No bookmarks to update.");
            if no_bookmark_updated && prepared.commit_id != *expected_base {
                return Err(selfci::MergeError::BranchUpdateFailed(
                    std::io::Error::other("checked Jujutsu base no longer has the expected target"),
                ));
            }
            #[cfg(test)]
            if FORCE_JJ_POST_MOVE_VERIFY_FAILURE.swap(false, Ordering::SeqCst) {
                output_log.push_str("forced post-move verification failure\n");
                return Ok(PublicationOutcome::AppliedUnverified {
                    commit_id: prepared.commit_id.clone(),
                    output: output_log,
                });
            }

            // Resolve after publication so concurrent Jujutsu operations are
            // integrated. A conflict or any other non-unique target is a safe
            // landing failure rather than an unsafe identity for hooks.
            let verify = Cmd::new("jj")
                .args([
                    "log",
                    "-r",
                    base_branch,
                    "-T",
                    r#"self.change_id() ++ " " ++ self.commit_id() ++ "\n""#,
                    "--no-graph",
                    "--color=never",
                ])
                .dir(root_dir);
            let _ = write!(output_log, "{}", verify.log_line());
            let identity = match verify.to_expression().read() {
                Ok(identity) => identity,
                Err(error) => {
                    if no_bookmark_updated {
                        return Err(selfci::MergeError::BranchUpdateFailed(error));
                    }
                    output_log.push_str(&format!("identity verification failed: {error}\n"));
                    return Ok(PublicationOutcome::AppliedUnverified {
                        commit_id: prepared.commit_id.clone(),
                        output: output_log,
                    });
                }
            };
            let parts: Vec<_> = identity.split_whitespace().collect();
            if parts.len() != 2
                || !is_full_jj_change_id(parts[0])
                || !is_full_jj_commit_id(parts[1])
                || parts[1] != prepared.commit_id.as_str()
            {
                if no_bookmark_updated {
                    return Err(selfci::MergeError::BranchUpdateFailed(
                        std::io::Error::other(
                            "checked Jujutsu base moved during no-op publication",
                        ),
                    ));
                }
                output_log.push_str(&format!(
                    "identity verification returned an unexpected target: {identity:?}\n"
                ));
                return Ok(PublicationOutcome::AppliedUnverified {
                    commit_id: prepared.commit_id.clone(),
                    output: output_log,
                });
            }
            output_log.push_str(&identity);
            return Ok(PublicationOutcome::Verified(LandingOutcome {
                commit_id: selfci::revision::CommitId::new(parts[1])
                    .expect("validated full Jujutsu commit ID"),
                change_id: selfci::revision::ChangeId::new(parts[0]),
                output: output_log,
            }));
        }
    }

    Ok(PublicationOutcome::Verified(LandingOutcome {
        commit_id: prepared.commit_id.clone(),
        change_id: prepared.change_id.clone(),
        output: output_log,
    }))
}

pub fn add_candidate(
    candidate: Option<String>,
    no_merge: bool,
    wait: bool,
) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let candidate = match candidate {
        Some(c) => c,
        None => {
            let vcs = get_vcs(&root_dir, None)?;
            match vcs {
                selfci::VCS::Jujutsu => "@-".to_string(),
                selfci::VCS::Git => "HEAD".to_string(),
            }
        }
    };

    // Try to get existing daemon, or auto-start if config has base-branch
    let daemon_dir = match get_project_daemon_runtime_dir(&root_dir)? {
        Some(dir) => dir,
        None => {
            // Daemon not running - try to auto-start if config is available
            match auto_start_daemon(&root_dir)? {
                Some(dir) => dir,
                None => {
                    eprintln!("Start it with: selfci mq start --base-branch <branch>");
                    eprintln!("Or set mq.base-branch in .config/selfci/ci.yaml for auto-start");
                    return Err(MainError::DaemonNotRunning);
                }
            }
        }
    };

    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);

    let response = mq_protocol::send_mq_request(
        &socket_path,
        mq_protocol::MQRequest::AddCandidate {
            candidate,
            no_merge,
        },
    )
    .map_err(|_| MainError::CommunicationFailed)?;

    match response {
        mq_protocol::MQResponse::CandidateAdded { run_id } => {
            if no_merge {
                println!(
                    "Added to merge queue with run ID: {} (no-merge mode)",
                    run_id
                );
            } else {
                println!("Added to merge queue with run ID: {}", run_id);
            }
            if wait {
                wait_for_run_id(run_id)?;
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

pub fn list_runs(limit: Option<usize>) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let daemon_dir =
        get_project_daemon_runtime_dir(&root_dir)?.ok_or(MainError::DaemonNotRunning)?;

    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);

    let response =
        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::List { limit })
            .map_err(|_| MainError::CommunicationFailed)?;

    match response {
        mq_protocol::MQResponse::RunList { runs } => {
            if runs.is_empty() {
                println!("No runs in queue");
            } else {
                let mut table = Table::new();
                table
                    .load_preset(presets::NOTHING)
                    .set_content_arrangement(ContentArrangement::Dynamic)
                    .set_header(vec![
                        "ID",
                        "Status",
                        "Change",
                        "Commit",
                        "Candidate",
                        "Queued",
                    ]);

                for run in runs {
                    let status = run.status.display();
                    let queued = humantime::format_rfc3339_seconds(run.queued_at);
                    // Shorten change_id and commit_id to first 8 chars
                    let change_short = &run.candidate.change_id.as_str()
                        [..run.candidate.change_id.as_str().len().min(8)];
                    let commit_short = &run.candidate.commit_id.as_str()
                        [..run.candidate.commit_id.as_str().len().min(8)];
                    table.add_row(vec![
                        run.id.to_string(),
                        status.to_string(),
                        change_short.to_string(),
                        commit_short.to_string(),
                        run.candidate.user.to_string(),
                        queued.to_string(),
                    ]);
                }

                // Print with minimal formatting: header, separator line, data rows
                let output = format!("{table}");
                let mut lines = output.lines();
                if let Some(header) = lines.next() {
                    println!("{}", header);
                    println!("{}", "-".repeat(header.len()));
                    for line in lines {
                        println!("{}", line);
                    }
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

pub fn get_status(run_id: u64) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let daemon_dir =
        get_project_daemon_runtime_dir(&root_dir)?.ok_or(MainError::DaemonNotRunning)?;

    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);

    let run_id = mq_protocol::RunId(run_id);
    let response =
        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::GetStatus { run_id })
            .map_err(|_| MainError::CommunicationFailed)?;

    match response {
        mq_protocol::MQResponse::RunStatus { run: Some(run) } => {
            println!("Run ID: {}", run.id);
            println!(
                "Candidate: {} (commit: {})",
                run.candidate.user, run.candidate.commit_id
            );
            println!("Status: {}", run.status.display());
            println!(
                "Queued at: {}",
                humantime::format_rfc3339_seconds(run.queued_at)
            );

            if let Some(started_at) = run.started_at {
                println!(
                    "Started at: {}",
                    humantime::format_rfc3339_seconds(started_at)
                );

                // Show active steps if the run is still running
                if matches!(run.status, mq_protocol::MQRunStatus::Running) {
                    let now = std::time::SystemTime::now();
                    let active: Vec<_> = run
                        .active_jobs
                        .iter()
                        .filter(|step| matches!(step.status, protocol::StepStatus::Running))
                        .collect();

                    if !active.is_empty() {
                        let active_strs: Vec<String> = active
                            .iter()
                            .map(|step| {
                                let step_elapsed = now.duration_since(step.ts).unwrap_or_default();
                                let job_elapsed = step
                                    .job_started_at
                                    .and_then(|t| now.duration_since(t).ok())
                                    .unwrap_or(step_elapsed);
                                format!(
                                    "{} ({:.1}s/{:.1}s)",
                                    step.name,
                                    step_elapsed.as_secs_f64(),
                                    job_elapsed.as_secs_f64()
                                )
                            })
                            .collect();
                        println!("Active Steps: {}", active_strs.join(", "));
                    }
                }
            }

            if let Some(completed_at) = run.completed_at {
                println!(
                    "Completed at: {}",
                    humantime::format_rfc3339_seconds(completed_at)
                );
            }

            if !run.test_merge_output.is_empty() {
                let header = match run.merge_mode {
                    selfci::config::MergeMode::Rebase => "### Pre-check Rebase",
                    selfci::config::MergeMode::Merge => "### Pre-check Merge",
                };
                println!("\n{}\n", header);
                println!("{}", run.test_merge_output);
            }

            if !run.output.is_empty() {
                println!("\n### Check Output\n");
                println!("{}", run.output);
            }

            // Show failed steps and jobs summary
            let failed_steps: Vec<_> = run
                .completed_steps
                .iter()
                .filter(|step| {
                    matches!(step.status, protocol::StepStatus::Failed { ignored: false })
                })
                .map(|step| step.name.as_str())
                .collect();
            let failed_jobs: Vec<_> = run
                .completed_jobs
                .iter()
                .filter(|job| matches!(job.status, protocol::JobStatus::Failed))
                .map(|job| job.name.as_str())
                .collect();
            if !failed_steps.is_empty() || !failed_jobs.is_empty() {
                let mut failed_items = Vec::new();
                failed_items.extend(failed_steps);
                failed_items.extend(failed_jobs);
                println!("\nFailed: {}", failed_items.join(", "));
            }

            Ok(())
        }
        mq_protocol::MQResponse::RunStatus { run: None } => {
            eprintln!("Run {} not found", run_id);
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

pub fn wait_for_run(run_id: Option<u64>) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let daemon_dir =
        get_project_daemon_runtime_dir(&root_dir)?.ok_or(MainError::DaemonNotRunning)?;

    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);

    // Resolve run_id: use provided value or find the latest
    let run_id = match run_id {
        Some(id) => mq_protocol::RunId(id),
        None => {
            let response = mq_protocol::send_mq_request(
                &socket_path,
                mq_protocol::MQRequest::List { limit: Some(1) },
            )
            .map_err(|_| MainError::CommunicationFailed)?;
            match response {
                mq_protocol::MQResponse::RunList { runs } => {
                    if let Some(run) = runs.first() {
                        run.id
                    } else {
                        eprintln!("No runs in queue");
                        std::process::exit(selfci::exit_codes::EXIT_MQ_RUN_NOT_FOUND);
                    }
                }
                _ => {
                    return Err(MainError::CommunicationFailed);
                }
            }
        }
    };

    wait_for_run_id(run_id)
}

fn wait_for_run_id(run_id: mq_protocol::RunId) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let daemon_dir =
        get_project_daemon_runtime_dir(&root_dir)?.ok_or(MainError::DaemonNotRunning)?;

    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);

    eprintln!("Waiting for run {} ...", run_id);

    // Send blocking wait request — daemon responds when run completes
    let response =
        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::WaitForRun { run_id })
            .map_err(|_| MainError::CommunicationFailed)?;

    match response {
        mq_protocol::MQResponse::RunStatus { run: Some(run) } => {
            eprintln!("Run {} completed: {}", run_id, run.status.display());
            if run.status.is_passed() {
                Ok(())
            } else {
                std::process::exit(selfci::exit_codes::EXIT_MQ_WAIT_FAILED);
            }
        }
        mq_protocol::MQResponse::RunStatus { run: None } => {
            eprintln!("Run {} not found", run_id);
            std::process::exit(selfci::exit_codes::EXIT_MQ_RUN_NOT_FOUND);
        }
        _ => Err(MainError::CommunicationFailed),
    }
}

pub fn stop_daemon() -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    // Find daemon directory
    let daemon_dir = match get_project_daemon_runtime_dir(&root_dir)? {
        Some(dir) => dir,
        None => {
            println!("Merge queue daemon is not running for this project");
            return Ok(());
        }
    };

    let pid_file = daemon_dir.join(constants::MQ_PID_FILENAME);

    // Read and validate the PID before invoking signal APIs, where zero and
    // negative values have process-group semantics.
    let pid = match std::fs::read_to_string(&pid_file) {
        Ok(content) => match content.trim().parse::<libc::pid_t>() {
            Ok(pid) if pid > 0 => pid,
            Err(_) | Ok(_) => {
                return Err(ProcessControlError::new(
                    "validate daemon PID",
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "daemon PID must be a positive platform process ID",
                    ),
                )
                .into());
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProcessControlError::new("read daemon PID", error).into());
        }
        Err(error) => {
            return Err(ProcessControlError::new("read daemon PID", error).into());
        }
    };

    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);
    let identity = match mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::Hello) {
        Ok(identity) => identity,
        Err(message) => {
            return match ProcessExitWatcher::arm(pid)
                .map_err(|error| ProcessControlError::new("observe stale daemon PID", error))?
            {
                WatchState::AlreadyExited => {
                    println!("Process not running, cleaning up");
                    remove_daemon_dir_if_pid(&daemon_dir, pid)
                }
                WatchState::Watching(_) => Err(ProcessControlError::new(
                    "verify daemon identity",
                    std::io::Error::other(message),
                )
                .into()),
            };
        }
    };
    match identity {
        mq_protocol::MQResponse::HelloAck { pid: response_pid } if response_pid == pid => {}
        mq_protocol::MQResponse::HelloAck { .. } => {
            return Err(ProcessControlError::new(
                "verify daemon identity",
                std::io::Error::other("daemon socket PID does not match mq.pid"),
            )
            .into());
        }
        _ => return Err(MainError::CommunicationFailed),
    }

    let watcher = match ProcessExitWatcher::arm(pid)
        .map_err(|error| ProcessControlError::new("observe daemon exit", error))?
    {
        WatchState::Watching(watcher) => watcher,
        WatchState::AlreadyExited => {
            println!("Process not running, cleaning up");
            remove_daemon_dir_if_pid(&daemon_dir, pid)?;
            return Ok(());
        }
    };

    let response = mq_protocol::send_mq_request(
        &socket_path,
        mq_protocol::MQRequest::Shutdown { expected_pid: pid },
    )
    .map_err(|message| {
        ProcessControlError::new("request daemon shutdown", std::io::Error::other(message))
    })?;
    match response {
        mq_protocol::MQResponse::ShutdownAck { pid: response_pid } if response_pid == pid => {}
        mq_protocol::MQResponse::ShutdownAck { .. } => {
            return Err(ProcessControlError::new(
                "verify shutdown responder",
                std::io::Error::other("daemon socket PID does not match mq.pid"),
            )
            .into());
        }
        _ => return Err(MainError::CommunicationFailed),
    }

    match watcher
        .wait(mq_stop_timeout(
            "SELFCI_TEST_MQ_STOP_GRACE_MS",
            MQ_STOP_GRACE_TIMEOUT,
        ))
        .map_err(|error| ProcessControlError::new("wait for daemon exit", error))?
    {
        WaitOutcome::Exited => {
            println!("Daemon stopped successfully");
            return Ok(());
        }
        WaitOutcome::TimedOut => {}
    }

    // Check the already-armed watcher once more immediately before using the
    // raw PID. This narrows macOS's unavoidable PID-reuse race.
    if watcher
        .wait(std::time::Duration::ZERO)
        .map_err(|error| ProcessControlError::new("recheck daemon exit", error))?
        == WaitOutcome::Exited
    {
        println!("Daemon stopped successfully");
        return Ok(());
    }

    eprintln!("Timeout waiting for daemon to stop, sending SIGKILL...");
    if let Err(error) = signal::kill(Pid::from_raw(pid), Signal::SIGKILL)
        && error != nix::errno::Errno::ESRCH
    {
        return Err(ProcessControlError::new(
            "send SIGKILL to daemon",
            std::io::Error::from_raw_os_error(error as i32),
        )
        .into());
    }
    if watcher
        .wait(mq_stop_timeout(
            "SELFCI_TEST_MQ_STOP_KILL_MS",
            MQ_STOP_KILL_TIMEOUT,
        ))
        .map_err(|error| ProcessControlError::new("wait for force-killed daemon", error))?
        == WaitOutcome::TimedOut
    {
        return Err(ProcessControlError::new(
            "wait for force-killed daemon",
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "daemon remained alive after SIGKILL",
            ),
        )
        .into());
    }

    remove_daemon_dir_if_pid(&daemon_dir, pid)?;
    println!("Daemon forcefully terminated");
    Ok(())
}

/// Return a production stop timeout, with a debug-build seam for lifecycle tests.
fn mq_stop_timeout(variable: &str, default: std::time::Duration) -> std::time::Duration {
    if cfg!(debug_assertions)
        && let Ok(value) = std::env::var(variable)
        && let Ok(milliseconds) = value.parse()
    {
        return std::time::Duration::from_millis(milliseconds);
    }
    default
}

/// Remove a stale daemon directory, ignoring only an already-removed directory.
fn remove_daemon_dir(daemon_dir: &Path) -> Result<(), MainError> {
    match std::fs::remove_dir_all(daemon_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(ProcessControlError::new("remove stale daemon runtime directory", error).into())
        }
    }
}

/// Remove runtime state only when it still belongs to the observed daemon PID.
fn remove_daemon_dir_if_pid(daemon_dir: &Path, expected_pid: libc::pid_t) -> Result<(), MainError> {
    let pid_file = daemon_dir.join(constants::MQ_PID_FILENAME);
    match std::fs::read_to_string(pid_file) {
        Ok(contents) if contents.trim().parse::<libc::pid_t>() == Ok(expected_pid) => {
            remove_daemon_dir(daemon_dir)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(ProcessControlError::new("verify stale daemon runtime directory", error).into())
        }
    }
}

/// Print the runtime directory for the daemon
pub fn print_runtime_dir() -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let daemon_dir =
        get_project_daemon_runtime_dir(&root_dir)?.ok_or(MainError::DaemonNotRunning)?;
    println!("{}", daemon_dir.display());
    Ok(())
}

/// Print the PID of the running daemon
pub fn print_pid() -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let daemon_dir =
        get_project_daemon_runtime_dir(&root_dir)?.ok_or(MainError::DaemonNotRunning)?;
    let pid_file = daemon_dir.join(constants::MQ_PID_FILENAME);
    let pid = std::fs::read_to_string(&pid_file).map_err(WorkDirError::CreateFailed)?;
    println!("{}", pid.trim());
    Ok(())
}

pub fn get_daemon_version() -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let daemon_dir = match get_project_daemon_runtime_dir(&root_dir)? {
        Some(dir) => dir,
        None => {
            println!("Merge queue daemon is not running for this project");
            return Ok(());
        }
    };

    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);

    let response = mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::Version)
        .map_err(|_| MainError::CommunicationFailed)?;

    match response {
        mq_protocol::MQResponse::Version { version } => {
            println!("selfci {}", version);
            Ok(())
        }
        mq_protocol::MQResponse::Error(e) => {
            eprintln!("Error: {}", e);
            Err(MainError::CommunicationFailed)
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            Err(MainError::CommunicationFailed)
        }
    }
}
