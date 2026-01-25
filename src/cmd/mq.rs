use duct::cmd;
use nix::sys::signal::{self, Signal};
use nix::sys::stat::{Mode, umask};
use nix::unistd::{ForkResult, Pid, close, dup2, fork, setsid};
use selfci::duct_util::Cmd;
use selfci::{MainError, WorkDirError, constants, envs, get_vcs, mq_protocol, protocol};
use signal_hook::consts::SIGTERM;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::os::unix::io::IntoRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::SystemTime;
use tracing::debug;

/// Get the selfci root runtime directory, where individual runtime for each
/// started instance are located.
fn get_selfci_root_runtime_dir() -> Result<PathBuf, MainError> {
    let base = dirs::runtime_dir().unwrap_or_else(|| {
        // Fallback to /tmp/selfci-{uid} if XDG_RUNTIME_DIR not available
        let uid = nix::unistd::getuid();
        PathBuf::from(format!("/tmp/selfci-{}", uid))
    });

    Ok(base.join("selfci"))
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
        let dir = PathBuf::from(explicit_dir);

        // Verify it's for our project (if initialized)
        let dir_file = dir.join(constants::MQ_DIR_FILENAME);
        if dir_file.exists() {
            let stored_root =
                std::fs::read_to_string(&dir_file).map_err(WorkDirError::CreateFailed)?;
            if paths_equal(Path::new(stored_root.trim()), project_root) {
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
    let runtime_dir = get_selfci_root_runtime_dir()?;
    if !runtime_dir.exists() {
        return Ok(None);
    }

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
            // Found matching project, verify daemon is running
            if verify_daemon_running(&pid_dir) {
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
fn verify_daemon_running(daemon_dir: &Path) -> bool {
    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);

    // If daemon responds to Hello, it's running
    matches!(
        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::Hello),
        Ok(mq_protocol::MQResponse::HelloAck)
    )
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
    merge_style: selfci::config::MergeStyle,
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
            merge_style: None,
            test_merge_output: String::new(),
            output: String::new(),
            active_jobs: Vec::new(),
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
    /// Returns the run info, hooks, merge_style, and shared job states for processing
    fn start_run(
        &mut self,
        run_id: mq_protocol::RunId,
    ) -> Option<(
        mq_protocol::MQRunInfo,
        selfci::config::MQHooksConfig,
        selfci::config::MergeStyle,
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
            self.merge_style,
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
                        let started_at = job_steps
                            .first()
                            .map(|s| s.ts)
                            .unwrap_or_else(SystemTime::now);

                        // Find current step (last one with Running status)
                        let current_step = job_steps
                            .iter()
                            .rev()
                            .find(|s| matches!(s.status, protocol::StepStatus::Running))
                            .map(|s| s.name.clone());

                        let name = if let Some(step) = current_step {
                            format!("{}/{}", job_name, step)
                        } else {
                            job_name.clone()
                        };

                        protocol::StepLogEntry {
                            ts: started_at,
                            name,
                            status: protocol::StepStatus::Running,
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
struct SharedMQState(Arc<Mutex<MQState>>);

impl SharedMQState {
    fn new(state: MQState) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }

    fn queue_run(
        &self,
        candidate: selfci::revision::ResolvedRevision,
        no_merge: bool,
    ) -> mq_protocol::MQRunInfo {
        self.0.lock().unwrap().queue_run(candidate, no_merge)
    }

    fn start_run(
        &self,
        run_id: mq_protocol::RunId,
    ) -> Option<(
        mq_protocol::MQRunInfo,
        selfci::config::MQHooksConfig,
        selfci::config::MergeStyle,
        super::check::SharedJobStates,
    )> {
        self.0.lock().unwrap().start_run(run_id)
    }

    fn complete_run(&self, run_id: mq_protocol::RunId, run_info: mq_protocol::MQRunInfo) {
        self.0.lock().unwrap().complete_run(run_id, run_info)
    }

    fn get_run(&self, run_id: mq_protocol::RunId) -> Option<mq_protocol::MQRunInfo> {
        self.0.lock().unwrap().get_run(run_id)
    }

    fn list_runs(&self, limit: Option<usize>) -> Vec<mq_protocol::MQRunInfo> {
        self.0.lock().unwrap().list_runs(limit)
    }

    fn root_dir(&self) -> PathBuf {
        self.0.lock().unwrap().root_dir().to_path_buf()
    }

    fn base_branch(&self) -> String {
        self.0.lock().unwrap().base_branch().to_string()
    }
}
/// Try to resolve base branch from config only (no CLI arg), quietly without printing errors
/// Returns Some(branch) if config has base-branch, None otherwise
fn try_resolve_base_branch_from_config(root_dir: &Path) -> Option<String> {
    let config = selfci::config::read_config(root_dir).ok()?;
    config.mq?.base_branch
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
    if let Some(existing_dir) = get_project_daemon_runtime_dir(root_dir)?
        && let Ok(pid_str) = std::fs::read_to_string(existing_dir.join(constants::MQ_PID_FILENAME))
        && let Ok(pid) = pid_str.trim().parse::<u32>()
    {
        println!("Merge queue daemon is already running (PID: {})", pid);
        println!("Runtime directory: {}", existing_dir.display());
        println!("Use 'selfci mq stop' to stop it");
        Ok(true)
    } else {
        Ok(false)
    }
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
    std::fs::create_dir_all(&daemon_dir).map_err(|e| {
        eprintln!(
            "ERROR: Failed to create daemon directory {}: {}",
            daemon_dir.display(),
            e
        );
        WorkDirError::CreateFailed(e)
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

    std::fs::create_dir_all(&daemon_dir).map_err(WorkDirError::CreateFailed)?;

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
    let merged_config = selfci::config::read_merged_mq_config(root_dir).unwrap_or_default();
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
    let merged_config = selfci::config::read_merged_mq_config(&root_dir).unwrap_or_default();
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
        merge_style: merged_config.merge_style,
        hooks: merged_config.hooks,
        next_run_id: mq_protocol::RunId(1),
        runs: HashMap::new(),
    });

    // Create channel for queueing jobs
    let (mq_jobs_sender, mq_jobs_receiver) = mpsc::channel::<mq_protocol::RunId>();

    // Spawn worker thread to process queue
    let state_clone = state.clone();
    let root_dir_clone = root_dir.clone();
    std::thread::spawn(move || {
        // process_queue creates worker pools for each candidate check via run_candidate_check
        process_queue(state_clone, root_dir_clone, mq_jobs_receiver);
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
                let mq_runs_sender_clone = mq_jobs_sender.clone();
                std::thread::spawn(move || {
                    if let Ok(request) = mq_protocol::read_mq_request(&mut stream) {
                        let response = handle_request(&state_clone, request, mq_runs_sender_clone);
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
    state: &SharedMQState,
    request: mq_protocol::MQRequest,
    mq_runs_sender: mpsc::Sender<mq_protocol::RunId>,
) -> mq_protocol::MQResponse {
    match request {
        mq_protocol::MQRequest::Hello => mq_protocol::MQResponse::HelloAck,

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

            // Send run ID to process_queue
            match mq_runs_sender.send(run_id) {
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
    /// Merged commit ID (after test merge/rebase), None before test merge
    merged_commit_id: Option<&'a str>,
    /// Merged change ID (after test merge/rebase), None before test merge
    merged_change_id: Option<&'a str>,
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
    command = command
        .dir(root_dir)
        .env(envs::SELFCI_VERSION, env!("CARGO_PKG_VERSION"));

    if let Some(env) = candidate_env {
        command = command
            .env(envs::SELFCI_CANDIDATE_COMMIT_ID, env.candidate_commit_id)
            .env(envs::SELFCI_CANDIDATE_CHANGE_ID, env.candidate_change_id)
            .env(envs::SELFCI_CANDIDATE_ID, env.candidate_id)
            .env(envs::SELFCI_MQ_BASE_BRANCH, env.base_branch);

        // Add merged env vars if present (after test merge)
        if let Some(merged_commit_id) = env.merged_commit_id {
            command = command.env(envs::SELFCI_MERGED_COMMIT_ID, merged_commit_id);
        }
        if let Some(merged_change_id) = env.merged_change_id {
            command = command.env(envs::SELFCI_MERGED_CHANGE_ID, merged_change_id);
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
        .env(envs::SELFCI_VERSION, env!("CARGO_PKG_VERSION"))
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
    mq_runs_receiver: mpsc::Receiver<mq_protocol::RunId>,
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
        // Wait for next run ID from channel
        let run_id = match mq_runs_receiver.recv() {
            Ok(id) => id,
            Err(_) => {
                debug!("MQ runs channel closed, exiting process_queue");
                break;
            }
        };

        // Move run from queued to active and get hooks, merge_style, and shared job states
        let (mut run_info, hooks, merge_style, shared_job_states) = match state.start_run(run_id) {
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
            merged_commit_id: None,
            merged_change_id: None,
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
        let test_merge_result =
            match create_test_merge(&root_dir, &base_branch, &run_info.candidate, &merge_style) {
                Ok(result) => result,
                Err(e) => {
                    let fail_reason = match merge_style {
                        selfci::config::MergeStyle::Rebase => mq_protocol::FailedReason::TestRebase,
                        selfci::config::MergeStyle::Merge => mq_protocol::FailedReason::TestMerge,
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

        // Store the test merge output and style in the job info
        run_info.merge_style = Some(merge_style);
        run_info.test_merge_output = test_merge_result.output.clone();

        // Use the merged commit for CI testing
        let merged_commit_id = test_merge_result.commit_id.to_string();
        let merged_change_id = test_merge_result.change_id.to_string();

        // Create candidate environment with merged info for post-merge hooks
        let candidate_env_post_merge = CandidateHookEnv {
            candidate_commit_id: &candidate_commit_id,
            candidate_change_id: &candidate_change_id,
            candidate_id: &candidate_id,
            base_branch: &base_branch,
            merged_commit_id: Some(&merged_commit_id),
            merged_change_id: Some(&merged_change_id),
        };

        // Set up cleanup guard for jj test merge commits
        // This ensures cleanup happens regardless of check success/failure
        // NOTE: We use base_branch (ref name) rather than commit ID because the actual
        // merge (if check passes) will advance main, and we want cleanup to exclude
        // ancestors of the NEW main (post-merge), not the old one.
        let _jj_cleanup_guard = if matches!(vcs, selfci::VCS::Jujutsu) {
            let cleanup_root_dir = root_dir.clone();
            let cleanup_change_id = merged_change_id.clone();
            let cleanup_base_ref = base_branch.to_string();
            Some(scopeguard::guard((), move |_| {
                cleanup_jj_test_merge(&cleanup_root_dir, &cleanup_change_id, &cleanup_base_ref);
            }))
        } else {
            None
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

                // Store check output separately - this is what goes into the merge commit
                // (without hook outputs)
                let check_output = result.output.clone();

                // Append check output to job output (preserving hook outputs)
                run_info.output.push_str(&result.output);
                run_info.active_jobs = Vec::new(); // Active jobs list is only populated for status queries
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
                            Some(&candidate_env_post_merge),
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
                            // Perform the final merge using the original candidate
                            // (test merge used duplicated commits that were cleaned up)
                            match merge_candidate(
                                &root_dir,
                                &base_branch,
                                &run_info.candidate,
                                &check_output,
                                &merge_style,
                            ) {
                                Ok(merge_log) => {
                                    // Append merge output with separator
                                    run_info.output.push_str("\n\n### Merge Output\n\n");
                                    run_info.output.push_str(&merge_log);

                                    // Run post-merge hook if configured
                                    // Uses post-merge env (has merged info)
                                    let post_merge_result = run_hook_with_env(
                                        hooks.post_merge.as_ref(),
                                        "post-merge",
                                        &root_dir,
                                        Some(&candidate_env_post_merge),
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

                                    run_info.status = mq_protocol::MQRunStatus::Passed(
                                        mq_protocol::PassedReason::Merged,
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
    let merge_cmd = Cmd::new("git")
        .args([
            "merge",
            "--no-ff",
            "-m",
            "Test merge by SelfCI",
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

/// Clean up temporary jj commits created by test merge
/// For rebase mode: abandons the duplicated commits (entire branch)
/// For merge mode: abandons the temporary merge commit
///
/// The `base_ref` parameter is used to exclude ancestors of the base from abandoning.
/// It can be a branch name (like "main") or a commit ID.
pub(crate) fn cleanup_jj_test_merge(root_dir: &Path, test_change_id: &str, base_ref: &str) {
    debug!(change_id = %test_change_id, base = %base_ref, "Cleaning up jj test merge commits");

    // Abandon the test merge commit(s)
    // For rebase mode, we need to abandon the entire duplicated branch, not just the tip
    // Use revset ::change_id to get the change and all its ancestors
    // The "~ ::base_ref" part excludes ancestors of base, leaving only the duplicated commits
    let revset = format!("::{} ~ ::{}", test_change_id, base_ref);
    match cmd!("jj", "--ignore-working-copy", "abandon", &revset)
        .dir(root_dir)
        .stderr_to_stdout()
        .run()
    {
        Ok(_) => {
            debug!("Successfully abandoned test merge commit");
        }
        Err(e) => {
            // Non-fatal - the commits will be garbage collected eventually
            debug!(error = %e, "Failed to abandon test merge commit (non-fatal)");
        }
    }

    // Update working copy snapshot
    let _ = cmd!("jj", "workspace", "update-stale")
        .dir(root_dir)
        .stdin_null()
        .stderr_to_stdout()
        .run();
}

/// Test rebase for Jujutsu - duplicates and rebases candidate onto base
/// Uses jj duplicate to create a copy, leaving the original candidate untouched
/// Returns the resulting commit and change IDs of the duplicated, rebased commits
fn test_merge_jj_rebase(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<TestMergeOutcome, selfci::MergeError> {
    let mut output_log = String::new();

    // Duplicate the candidate branch (commits from base to candidate, exclusive of base)
    // This creates copies with new change IDs, leaving the original untouched
    let revset = format!("{}..{}", base_branch, candidate.commit_id);
    let dup_cmd = Cmd::new("jj")
        .args(["--ignore-working-copy", "duplicate", &revset])
        .dir(root_dir);
    let _ = write!(output_log, "{}", dup_cmd.log_line());
    let dup_output = dup_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(|e| selfci::MergeError::RebaseFailed(selfci::CommandOutputError(e.to_string())))?;
    output_log.push_str(&dup_output);
    output_log.push_str("\n\n");

    // Parse the output to find the duplicate of the candidate tip
    // Output format: "Duplicated <short_commit_id> as <new_change_id> <new_short_commit_id> <description>"
    // The duplicates are output in topological order (ancestors first), so the last one is the tip
    let mut duplicated_tip_change_id = None;
    for line in dup_output.lines() {
        if line.starts_with("Duplicated") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Format: "Duplicated <short_commit> as <new_change_id> ..."
            if parts.len() >= 4 {
                duplicated_tip_change_id = Some(parts[3].to_string());
                // Keep going to get the last one (the tip)
            }
        }
    }

    let dup_change_id = duplicated_tip_change_id.ok_or_else(|| {
        selfci::MergeError::RebaseFailed(selfci::CommandOutputError(format!(
            "Failed to find duplicated tip in output: {}",
            dup_output
        )))
    })?;

    // Rebase the duplicated commits onto base branch
    let rebase_cmd = Cmd::new("jj")
        .args([
            "--ignore-working-copy",
            "rebase",
            "-b",
            &dup_change_id,
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

    // Get the commit ID of the rebased duplicate
    let commit_id_cmd = Cmd::new("jj")
        .args([
            "log",
            "-r",
            &dup_change_id,
            "-T",
            "commit_id",
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", commit_id_cmd.log_line());
    let commit_id = commit_id_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?
        .trim()
        .to_string();
    output_log.push_str(&format!("{}\n\n", commit_id));

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
        commit_id: selfci::revision::CommitId::new(commit_id)
            .expect("jj log returned invalid commit id"),
        change_id: selfci::revision::ChangeId::new(dup_change_id),
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
    let new_cmd = Cmd::new("jj")
        .args([
            "--ignore-working-copy",
            "new",
            "--no-edit",
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

    // Parse the output to get the change ID
    // Output format: "Created new commit <change_id> <short_commit_id> ..."
    let parts: Vec<&str> = output
        .lines()
        .find(|line| line.starts_with("Created new commit"))
        .ok_or_else(|| {
            selfci::MergeError::MergeFailed(selfci::CommandOutputError(format!(
                "Failed to parse merge commit from output: {}",
                output
            )))
        })?
        .split_whitespace()
        .collect();

    let change_id = parts.get(3).ok_or_else(|| {
        selfci::MergeError::MergeFailed(selfci::CommandOutputError(format!(
            "Failed to parse change ID from output: {}",
            output
        )))
    })?;

    // Get the full commit ID using jj log (the output only has short ID)
    let commit_id_cmd = Cmd::new("jj")
        .args([
            "log",
            "-r",
            *change_id,
            "-T",
            "commit_id",
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(output_log, "{}", commit_id_cmd.log_line());
    let commit_id = commit_id_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?
        .trim()
        .to_string();
    output_log.push_str(&format!("{}\n\n", commit_id));

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
        change_id: selfci::revision::ChangeId::new(change_id.to_string()),
        output: output_log,
    })
}

/// Create a test merge/rebase of candidate onto base for CI testing
/// This does NOT update any refs - the resulting commit is dangling (Git) or just exists (jj)
pub(crate) fn create_test_merge(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
    merge_style: &selfci::config::MergeStyle,
) -> Result<TestMergeOutcome, selfci::MergeError> {
    // Detect VCS
    let vcs = get_vcs(root_dir, None).map_err(|e| {
        selfci::MergeError::ConfigReadFailed(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            e.to_string(),
        ))
    })?;

    debug!(vcs = ?vcs, merge_style = ?merge_style, "Creating test merge");

    match (vcs, merge_style) {
        (selfci::VCS::Git, selfci::config::MergeStyle::Rebase) => {
            test_merge_git_rebase(root_dir, base_branch, candidate)
        }
        (selfci::VCS::Git, selfci::config::MergeStyle::Merge) => {
            test_merge_git_merge(root_dir, base_branch, candidate)
        }
        (selfci::VCS::Jujutsu, selfci::config::MergeStyle::Rebase) => {
            test_merge_jj_rebase(root_dir, base_branch, candidate)
        }
        (selfci::VCS::Jujutsu, selfci::config::MergeStyle::Merge) => {
            test_merge_jj_merge(root_dir, base_branch, candidate)
        }
    }
}

fn merge_candidate_git_rebase(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<String, selfci::MergeError> {
    let mut merge_log = String::new();

    // Git rebase mode - use a temporary worktree to avoid touching user's working directory
    let temp_worktree = root_dir.join(format!(".git/selfci-worktree-{}", candidate.commit_id));

    merge_log.push_str(&format!(
        "Git rebase mode: rebasing {} onto {}\n\n",
        candidate.commit_id, base_branch
    ));

    // Create temporary worktree in detached HEAD state at candidate commit
    let worktree_cmd = Cmd::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &temp_worktree.display().to_string(),
            candidate.commit_id.as_str(),
        ])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", worktree_cmd.log_line());
    let output = worktree_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::WorktreeCreateFailed)?;
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

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
    let _ = write!(merge_log, "{}", rebase_cmd.log_line());
    let rebase_result = rebase_cmd
        .to_expression()
        .stderr_to_stdout()
        .stdout_capture()
        .unchecked()
        .run()
        .map_err(|e| selfci::MergeError::RebaseFailed(selfci::CommandOutputError(e.to_string())))?;
    let output = String::from_utf8_lossy(&rebase_result.stdout).to_string();
    if !rebase_result.status.success() {
        let _ = cmd!("git", "rebase", "--abort").dir(&temp_worktree).run();
        return Err(selfci::MergeError::RebaseFailed(
            selfci::CommandOutputError(output),
        ));
    }
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    // Update base_branch to point to the rebased commits (HEAD in worktree)
    let update_ref_cmd = Cmd::new("git")
        .args(["update-ref", &format!("refs/heads/{}", base_branch), "HEAD"])
        .dir(&temp_worktree);
    let _ = write!(merge_log, "{}", update_ref_cmd.log_line());
    let output = update_ref_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    // Cleanup is handled by scopeguard
    drop(cleanup);

    Ok(merge_log)
}

fn merge_candidate_git_merge(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
    test_output: &str,
) -> Result<String, selfci::MergeError> {
    let mut merge_log = String::new();

    // Git merge mode - use a temporary worktree to avoid touching user's working directory
    let temp_worktree = root_dir.join(format!(".git/selfci-worktree-{}", candidate.commit_id));

    // Create temporary worktree in detached HEAD state at base branch
    let worktree_cmd = Cmd::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &temp_worktree.display().to_string(),
            base_branch,
        ])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", worktree_cmd.log_line());
    let output = worktree_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::WorktreeCreateFailed)?;
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    // Ensure cleanup on any exit path
    let cleanup = scopeguard::guard((), |_| {
        let _ = cmd!("git", "worktree", "remove", "--force", &temp_worktree)
            .dir(root_dir)
            .run();
    });

    // Build the merge commit message
    let merge_message = format!(
        "Merge commit '{}' by SelfCI\n\n### Check Output\n\n{}",
        candidate.commit_id, test_output
    );

    // In the worktree, merge candidate with a custom message
    let merge_cmd = Cmd::new("git")
        .args([
            "merge",
            "--no-ff",
            "-m",
            &merge_message,
            candidate.commit_id.as_str(),
        ])
        .dir(&temp_worktree);
    let _ = write!(merge_log, "{}", merge_cmd.log_line());
    let merge_result = merge_cmd
        .to_expression()
        .stderr_to_stdout()
        .stdout_capture()
        .unchecked()
        .run()
        .map_err(|e| selfci::MergeError::MergeFailed(selfci::CommandOutputError(e.to_string())))?;
    let output = String::from_utf8_lossy(&merge_result.stdout).to_string();
    if !merge_result.status.success() {
        let _ = cmd!("git", "merge", "--abort").dir(&temp_worktree).run();
        return Err(selfci::MergeError::MergeFailed(selfci::CommandOutputError(
            output,
        )));
    }
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    // Update base_branch to point to the merge commit (HEAD in worktree)
    let update_ref_cmd = Cmd::new("git")
        .args(["update-ref", &format!("refs/heads/{}", base_branch), "HEAD"])
        .dir(&temp_worktree);
    let _ = write!(merge_log, "{}", update_ref_cmd.log_line());
    let output = update_ref_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    // Cleanup is handled by scopeguard
    drop(cleanup);

    merge_log.push_str("Merge completed successfully\n");
    Ok(merge_log)
}

fn merge_candidate_jj_rebase(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<String, selfci::MergeError> {
    let mut merge_log = String::new();

    // Get the change ID of the candidate (stable across rebases)
    let change_id_cmd = Cmd::new("jj")
        .args([
            "log",
            "-r",
            candidate.commit_id.as_str(),
            "-T",
            "change_id",
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", change_id_cmd.log_line());
    let change_id = change_id_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?
        .trim()
        .to_string();
    merge_log.push_str(&format!("{}\n\n", change_id));

    // Rebase the candidate branch onto base
    // The test merge used a duplicate, so the original candidate is still untouched
    let rebase_cmd = Cmd::new("jj")
        .args([
            "--ignore-working-copy",
            "rebase",
            "-b",
            candidate.commit_id.as_str(),
            "-d",
            base_branch,
        ])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", rebase_cmd.log_line());
    let rebase_result = rebase_cmd
        .to_expression()
        .stderr_to_stdout()
        .stdout_capture()
        .unchecked()
        .run()
        .map_err(|e| selfci::MergeError::RebaseFailed(selfci::CommandOutputError(e.to_string())))?;
    let output = String::from_utf8_lossy(&rebase_result.stdout).to_string();
    if !rebase_result.status.success() {
        return Err(selfci::MergeError::RebaseFailed(
            selfci::CommandOutputError(output),
        ));
    }
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    // Move the base branch bookmark to the rebased commit using change ID
    let bookmark_cmd = Cmd::new("jj")
        .args([
            "--ignore-working-copy",
            "bookmark",
            "set",
            base_branch,
            "-r",
            &change_id,
        ])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", bookmark_cmd.log_line());
    let output = bookmark_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    // Update the working copy snapshot to avoid "stale working copy" errors
    let update_cmd = Cmd::new("jj")
        .args(["workspace", "update-stale"])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", update_cmd.log_line());
    let output = update_cmd
        .to_expression()
        .stdin_null()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    Ok(merge_log)
}

fn merge_candidate_jj_merge(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
    test_output: &str,
) -> Result<String, selfci::MergeError> {
    let mut merge_log = String::new();

    // Create a new merge commit with both base and candidate as parents
    // Use --ignore-working-copy and --no-edit to avoid changing the working directory or @
    let new_cmd = Cmd::new("jj")
        .args([
            "--ignore-working-copy",
            "new",
            "--no-edit",
            base_branch,
            candidate.commit_id.as_str(),
        ])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", new_cmd.log_line());
    let output = new_cmd
        .to_expression()
        .stdin_null()
        .stderr_to_stdout()
        .read()
        .map_err(|e| selfci::MergeError::MergeFailed(selfci::CommandOutputError(e.to_string())))?;
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    // Parse the output to get the merge commit ID
    // Output format: "Created new commit <change_id> <commit_id> ..."
    let merge_commit_change_id = output
        .lines()
        .find(|line| line.starts_with("Created new commit"))
        .and_then(|line| line.split_whitespace().nth(3))
        .ok_or_else(|| {
            selfci::MergeError::MergeFailed(selfci::CommandOutputError(format!(
                "Failed to parse merge commit ID from output: {}",
                output
            )))
        })?
        .to_string();

    // Set the merge commit description
    let merge_message = format!(
        "Merge commit '{}' by SelfCI\n\n### Check Output\n\n{}",
        candidate.commit_id, test_output
    );
    let describe_cmd = Cmd::new("jj")
        .args([
            "--ignore-working-copy",
            "describe",
            "-r",
            &merge_commit_change_id,
            "-m",
            &merge_message,
        ])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", describe_cmd.log_line());
    let output = describe_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(|e| selfci::MergeError::MergeFailed(selfci::CommandOutputError(e.to_string())))?;
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    // Get the final commit ID (describe may have changed it)
    let commit_id_cmd = Cmd::new("jj")
        .args([
            "log",
            "-r",
            &merge_commit_change_id,
            "-T",
            "commit_id",
            "--no-graph",
            "--color=never",
        ])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", commit_id_cmd.log_line());
    let final_commit_id = commit_id_cmd
        .to_expression()
        .read()
        .map_err(selfci::MergeError::ChangeIdFailed)?
        .trim()
        .to_string();
    merge_log.push_str(&format!("{}\n\n", final_commit_id));

    // Move the base branch bookmark to the merge commit
    let bookmark_cmd = Cmd::new("jj")
        .args([
            "--ignore-working-copy",
            "bookmark",
            "set",
            base_branch,
            "-r",
            &final_commit_id,
        ])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", bookmark_cmd.log_line());
    let output = bookmark_cmd
        .to_expression()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    // Update the working copy snapshot to avoid "stale working copy" errors
    let update_cmd = Cmd::new("jj")
        .args(["workspace", "update-stale"])
        .dir(root_dir);
    let _ = write!(merge_log, "{}", update_cmd.log_line());
    let output = update_cmd
        .to_expression()
        .stdin_null()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push_str("\n\n");

    Ok(merge_log)
}

fn merge_candidate(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
    test_output: &str,
    merge_style: &selfci::config::MergeStyle,
) -> Result<String, selfci::MergeError> {
    // Detect VCS
    let vcs = get_vcs(root_dir, None).map_err(|e| {
        selfci::MergeError::ConfigReadFailed(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            e.to_string(),
        ))
    })?;

    match (vcs, merge_style) {
        (selfci::VCS::Git, selfci::config::MergeStyle::Rebase) => {
            merge_candidate_git_rebase(root_dir, base_branch, candidate)
        }
        (selfci::VCS::Git, selfci::config::MergeStyle::Merge) => {
            merge_candidate_git_merge(root_dir, base_branch, candidate, test_output)
        }
        (selfci::VCS::Jujutsu, selfci::config::MergeStyle::Rebase) => {
            merge_candidate_jj_rebase(root_dir, base_branch, candidate)
        }
        (selfci::VCS::Jujutsu, selfci::config::MergeStyle::Merge) => {
            merge_candidate_jj_merge(root_dir, base_branch, candidate, test_output)
        }
    }
}

pub fn add_candidate(candidate: String, no_merge: bool) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    // Try to get existing daemon, or auto-start if config has base-branch
    let daemon_dir = match get_project_daemon_runtime_dir(&root_dir)? {
        Some(dir) => dir,
        None => {
            // Daemon not running - try to auto-start if config is available
            match auto_start_daemon(&root_dir)? {
                Some(dir) => dir,
                None => {
                    eprintln!("Merge queue daemon is not running for this project");
                    eprintln!("Start it with: selfci mq start --base-branch <branch>");
                    eprintln!("Or set mq.base-branch in .config/selfci/ci.yaml for auto-start");
                    return Err(MainError::CheckFailed);
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
    .map_err(|e| {
        eprintln!("Error: {}", e);
        MainError::CheckFailed
    })?;

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

    let daemon_dir = get_project_daemon_runtime_dir(&root_dir)?.ok_or_else(|| {
        eprintln!("Merge queue daemon is not running for this project");
        MainError::CheckFailed
    })?;

    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);

    let response =
        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::List { limit })
            .map_err(|e| {
                eprintln!("Error: {}", e);
                MainError::CheckFailed
            })?;

    match response {
        mq_protocol::MQResponse::RunList { runs } => {
            if runs.is_empty() {
                println!("No runs in queue");
            } else {
                println!(
                    "{:<6} {:<20} {:<12} {:<10} {:<20} {:<20}",
                    "ID", "Status", "Change", "Commit", "Candidate", "Queued"
                );
                println!("{}", "-".repeat(92));
                for run in runs {
                    let status = run.status.display();

                    let queued = humantime::format_rfc3339_seconds(run.queued_at);
                    // Shorten change_id and commit_id to first 8 chars
                    let change_short = &run.candidate.change_id.as_str()
                        [..run.candidate.change_id.as_str().len().min(8)];
                    let commit_short = &run.candidate.commit_id.as_str()
                        [..run.candidate.commit_id.as_str().len().min(8)];
                    println!(
                        "{:<6} {:<20} {:<12} {:<10} {:<20} {:<20}",
                        run.id,
                        status,
                        change_short,
                        commit_short,
                        run.candidate.user.as_str(),
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

pub fn get_status(run_id: u64) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let daemon_dir = get_project_daemon_runtime_dir(&root_dir)?.ok_or_else(|| {
        eprintln!("Merge queue daemon is not running for this project");
        MainError::CheckFailed
    })?;

    let socket_path = daemon_dir.join(constants::MQ_SOCK_FILENAME);

    let run_id = mq_protocol::RunId(run_id);
    let response =
        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::GetStatus { run_id })
            .map_err(|e| {
                eprintln!("Error: {}", e);
                MainError::CheckFailed
            })?;

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

                // Show active jobs if the run is still running
                if matches!(run.status, mq_protocol::MQRunStatus::Running) {
                    let now = std::time::SystemTime::now();
                    let active: Vec<_> = run
                        .active_jobs
                        .iter()
                        .filter(|job| matches!(job.status, protocol::StepStatus::Running))
                        .collect();

                    if !active.is_empty() {
                        let active_strs: Vec<String> = active
                            .iter()
                            .map(|job| {
                                let elapsed = now.duration_since(job.ts).unwrap_or_default();
                                format!("{} ({:.3}s)", job.name, elapsed.as_secs_f64())
                            })
                            .collect();
                        println!("Active Jobs: {}", active_strs.join(", "));
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
                let header = match run.merge_style {
                    Some(selfci::config::MergeStyle::Rebase) => "### Test Rebase",
                    Some(selfci::config::MergeStyle::Merge) => "### Test Merge",
                    None => "### Test Merge/Rebase",
                };
                println!("\n{}\n", header);
                println!("{}", run.test_merge_output);
            }

            if !run.output.is_empty() {
                println!("\n### Check Output\n");
                println!("{}", run.output);
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

pub fn stop_daemon() -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    // Find daemon directory
    let daemon_dir = get_project_daemon_runtime_dir(&root_dir)?.ok_or_else(|| {
        eprintln!("Merge queue daemon is not running for this project");
        MainError::CheckFailed
    })?;

    let pid_file = daemon_dir.join(constants::MQ_PID_FILENAME);

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
    // We check if the daemon directory was cleaned up by the daemon's scopeguard,
    // which is more reliable than signal::kill(pid, None) which returns success
    // for zombie processes.
    let timeout = std::time::Duration::from_secs(30);
    let start = std::time::Instant::now();

    loop {
        // Check if daemon directory was cleaned up (daemon exited and ran scopeguard)
        if !daemon_dir.exists() {
            println!("Daemon stopped successfully");
            return Ok(());
        }

        // Also check if process still exists (not a zombie)
        // If process doesn't exist at all, clean up the directory
        if signal::kill(Pid::from_raw(pid as i32), None).is_err() {
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

/// Print the runtime directory for the daemon
pub fn print_runtime_dir() -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    match get_project_daemon_runtime_dir(&root_dir)? {
        Some(daemon_dir) => {
            println!("{}", daemon_dir.display());
            Ok(())
        }
        None => {
            eprintln!("Merge queue daemon is not running for this project");
            Err(MainError::CheckFailed)
        }
    }
}

/// Print the PID of the running daemon
pub fn print_pid() -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    match get_project_daemon_runtime_dir(&root_dir)? {
        Some(daemon_dir) => {
            let pid_file = daemon_dir.join(constants::MQ_PID_FILENAME);
            let pid = std::fs::read_to_string(&pid_file).map_err(WorkDirError::CreateFailed)?;
            println!("{}", pid.trim());
            Ok(())
        }
        None => {
            eprintln!("Merge queue daemon is not running for this project");
            Err(MainError::CheckFailed)
        }
    }
}
