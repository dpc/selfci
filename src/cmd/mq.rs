use duct::cmd;
use nix::sys::signal::{self, Signal};
use nix::sys::stat::{Mode, umask};
use nix::unistd::{ForkResult, Pid, close, dup2, fork, setsid};
use selfci::{MainError, WorkDirError, envs, get_vcs, mq_protocol, protocol};
use signal_hook::consts::SIGTERM;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::os::unix::io::IntoRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    merge_style: selfci::config::MergeStyle,
    hooks: selfci::config::MQHooksConfig,
    next_job_id: u64,
    queued: HashMap<u64, mq_protocol::MQJobInfo>,
    active: HashMap<u64, mq_protocol::MQJobInfo>,
    completed: Vec<mq_protocol::MQJobInfo>,
}

impl MQState {
    /// Create and queue a new job, returning the job ID
    fn queue_job(
        &mut self,
        candidate: selfci::revision::ResolvedRevision,
        no_merge: bool,
    ) -> mq_protocol::MQJobInfo {
        let job_id = self.next_job_id;
        self.next_job_id += 1;

        let job = mq_protocol::MQJobInfo {
            id: job_id,
            candidate,
            status: mq_protocol::MQJobStatus::Queued,
            queued_at: SystemTime::now(),
            started_at: None,
            completed_at: None,
            output: String::new(),
            steps: Vec::new(),
            no_merge,
        };

        self.queued.insert(job_id, job.clone());
        job
    }

    /// Move a job from queued to active (start processing it)
    /// Returns the job info along with hooks and merge_style needed for processing
    fn start_job(
        &mut self,
        job_id: u64,
    ) -> Option<(
        mq_protocol::MQJobInfo,
        selfci::config::MQHooksConfig,
        selfci::config::MergeStyle,
    )> {
        let mut job = self.queued.remove(&job_id)?;
        job.status = mq_protocol::MQJobStatus::Running;
        job.started_at = Some(SystemTime::now());
        self.active.insert(job_id, job.clone());
        Some((job, self.hooks.clone(), self.merge_style.clone()))
    }

    /// Move a job from active to completed
    fn complete_job(&mut self, job_id: u64, job_info: mq_protocol::MQJobInfo) {
        self.active.remove(&job_id);
        self.completed.push(job_info);
    }

    /// Get a job by ID (searches queued, active, and completed)
    fn get_job(&self, job_id: u64) -> Option<mq_protocol::MQJobInfo> {
        self.queued
            .get(&job_id)
            .or_else(|| self.active.get(&job_id))
            .cloned()
            .or_else(|| self.completed.iter().find(|j| j.id == job_id).cloned())
    }

    /// List all jobs (queued, active, completed), sorted by ID descending, with optional limit
    fn list_jobs(&self, limit: Option<usize>) -> Vec<mq_protocol::MQJobInfo> {
        let mut jobs: Vec<_> = self
            .queued
            .values()
            .chain(self.active.values())
            .chain(self.completed.iter())
            .cloned()
            .collect();

        jobs.sort_by(|a, b| b.id.cmp(&a.id));

        if let Some(limit) = limit {
            jobs.truncate(limit);
        }

        jobs
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

    fn queue_job(
        &self,
        candidate: selfci::revision::ResolvedRevision,
        no_merge: bool,
    ) -> mq_protocol::MQJobInfo {
        self.0.lock().unwrap().queue_job(candidate, no_merge)
    }

    fn start_job(
        &self,
        job_id: u64,
    ) -> Option<(
        mq_protocol::MQJobInfo,
        selfci::config::MQHooksConfig,
        selfci::config::MergeStyle,
    )> {
        self.0.lock().unwrap().start_job(job_id)
    }

    fn complete_job(&self, job_id: u64, job_info: mq_protocol::MQJobInfo) {
        self.0.lock().unwrap().complete_job(job_id, job_info)
    }

    fn get_job(&self, job_id: u64) -> Option<mq_protocol::MQJobInfo> {
        self.0.lock().unwrap().get_job(job_id)
    }

    fn list_jobs(&self, limit: Option<usize>) -> Vec<mq_protocol::MQJobInfo> {
        self.0.lock().unwrap().list_jobs(limit)
    }

    fn root_dir(&self) -> PathBuf {
        self.0.lock().unwrap().root_dir().to_path_buf()
    }

    fn base_branch(&self) -> String {
        self.0.lock().unwrap().base_branch().to_string()
    }
}

/// Resolve the base branch from CLI argument or config file
fn resolve_base_branch(base_branch: Option<String>, root_dir: &Path) -> Result<String, MainError> {
    if let Some(branch) = base_branch {
        return Ok(branch);
    }

    // Try to read config from current directory
    match selfci::config::read_config(root_dir) {
        Ok(config) => {
            if let Some(mq_config) = config.mq {
                if let Some(branch) = mq_config.base_branch {
                    Ok(branch)
                } else {
                    eprintln!(
                        "Error: --base-branch not specified and mq.base-branch not set in config"
                    );
                    eprintln!(
                        "Either provide --base-branch or set mq.base-branch in .config/selfci/ci.yaml"
                    );
                    Err(MainError::CheckFailed)
                }
            } else {
                eprintln!(
                    "Error: --base-branch not specified and mq.base-branch not set in config"
                );
                eprintln!(
                    "Either provide --base-branch or set mq.base-branch in .config/selfci/ci.yaml"
                );
                Err(MainError::CheckFailed)
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
            Err(MainError::CheckFailed)
        }
    }
}

/// Try to resolve base branch from config only (no CLI arg), quietly without printing errors
/// Returns Some(branch) if config has base-branch, None otherwise
fn try_resolve_base_branch_from_config(root_dir: &Path) -> Option<String> {
    let config = selfci::config::read_config(root_dir).ok()?;
    config.mq?.base_branch
}

/// Result of daemonize_background indicating whether we're in parent or child process
enum DaemonizeResult {
    /// Parent process - daemon_dir where child daemon is running
    Parent(PathBuf),
    /// Child process - daemon_dir where we should run the daemon
    Child(PathBuf),
}

/// Check if a daemon is already running for this project and print info if so
fn check_daemon_already_running(root_dir: &Path) -> Result<bool, MainError> {
    if let Some(existing_dir) = get_daemon_runtime_dir(root_dir)?
        && let Ok(pid_str) = std::fs::read_to_string(existing_dir.join("mq.pid"))
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
/// Forks the process, sets up session, redirects I/O
/// Returns DaemonizeResult::Parent in parent process, DaemonizeResult::Child in child process
fn daemonize_background(
    root_dir: &Path,
    daemon_dir_initial: PathBuf,
    log_file: Option<PathBuf>,
    base_branch: &str,
) -> Result<DaemonizeResult, MainError> {
    // Background mode - manual fork so we can report PID back to parent
    println!("Base branch: {}", base_branch);

    // Create a pipe for the child to send its info back to the parent
    let (reader, mut writer) =
        std::os::unix::net::UnixStream::pair().map_err(WorkDirError::CreateFailed)?;

    // Manual fork (instead of using daemonize crate, so parent can read from pipe)
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child: _ }) => {
            // Parent process - read daemon dir from pipe and return
            drop(writer);

            let reader = std::io::BufReader::new(reader);
            if let Ok(Some(daemon_dir)) = reader.lines().next().transpose() {
                println!("Runtime directory: {}", daemon_dir);
                Ok(DaemonizeResult::Parent(PathBuf::from(daemon_dir)))
            } else {
                eprintln!("Failed to read daemon directory from child process");
                Err(MainError::CheckFailed)
            }
        }
        Err(e) => {
            eprintln!("Failed to fork: {}", e);
            Err(MainError::CheckFailed)
        }
        Ok(ForkResult::Child) => {
            // Child process - become session leader and continue as daemon
            drop(reader);

            // Become session leader
            setsid().map_err(|_| {
                WorkDirError::CreateFailed(std::io::Error::other("Failed to become session leader"))
            })?;

            // Redirect stdin to /dev/null instead of closing it
            // This prevents fd 0 from being accidentally reused
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

            let pid = std::process::id();
            let daemon_dir = if daemon_dir_initial.as_os_str().is_empty() {
                // Auto mode: create PID-based directory
                get_selfci_runtime_dir()?.join(pid.to_string())
            } else {
                // Explicit mode: use provided directory
                daemon_dir_initial
            };

            // Send daemon directory path back to parent
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

            // Redirect stdout/stderr using nix
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
            Ok(DaemonizeResult::Child(daemon_dir))
        }
    }
}

/// Initialize the daemon runtime directory and daemonize the process if in background mode
/// Returns DaemonizeResult indicating whether we're parent (should not run daemon loop) or child/foreground (should run daemon loop)
fn initialize_daemon_dir(
    root_dir: &Path,
    foreground: bool,
    log_file: Option<PathBuf>,
    base_branch: &str,
) -> Result<DaemonizeResult, MainError> {
    // Determine initial daemon directory (may be modified after fork in background mode)
    let daemon_dir_initial = if let Ok(explicit_dir) = std::env::var(envs::SELFCI_MQ_RUNTIME_DIR) {
        // Mode 1: Use explicit directory
        PathBuf::from(explicit_dir)
    } else {
        // Mode 2: Will determine after getting PID
        PathBuf::new()
    };

    // Daemonize and set up runtime directory
    if !foreground {
        daemonize_background(root_dir, daemon_dir_initial, log_file, base_branch)
    } else {
        // Foreground mode - simpler, treat as "child" since we run the daemon loop
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
        Ok(DaemonizeResult::Child(daemon_dir))
    }
}

pub fn start_daemon(
    base_branch: Option<String>,
    foreground: bool,
    log_file: Option<PathBuf>,
) -> Result<(), MainError> {
    let root_dir = std::env::current_dir().map_err(WorkDirError::CreateFailed)?;

    let base_branch = resolve_base_branch(base_branch, &root_dir)?;

    if check_daemon_already_running(&root_dir)? {
        return Ok(());
    }

    // Run pre-start hook BEFORE daemonization with inherited stdio
    // This allows interactive commands (e.g., password prompts, keychain unlock)
    let merged_config = selfci::config::read_merged_mq_config(&root_dir).unwrap_or_default();
    if !run_hook_interactive(
        merged_config.hooks.pre_start.as_ref(),
        "pre-start",
        &root_dir,
    ) {
        eprintln!("Pre-start hook failed, aborting daemon startup");
        return Err(MainError::CheckFailed);
    }

    let result = initialize_daemon_dir(&root_dir, foreground, log_file, &base_branch)?;

    // Handle parent vs child process
    let daemon_dir = match result {
        DaemonizeResult::Parent(_) => {
            // Parent process in background mode - exit now, child will run the daemon
            std::process::exit(0);
        }
        DaemonizeResult::Child(dir) => dir,
    };

    run_daemon_loop(daemon_dir, root_dir, base_branch)
}

/// Auto-start daemon in background if config has base-branch set
/// Returns Ok(Some(daemon_dir)) if started successfully, Ok(None) if cannot auto-start
pub fn auto_start_daemon(root_dir: &Path) -> Result<Option<PathBuf>, MainError> {
    // Try to resolve base branch from config (not CLI)
    let base_branch = match try_resolve_base_branch_from_config(root_dir) {
        Some(branch) => branch,
        None => return Ok(None), // Can't auto-start without config
    };

    // Check if already running
    if get_daemon_runtime_dir(root_dir)?.is_some() {
        // Already running, nothing to do
        return Ok(None);
    }

    println!("Auto-starting merge queue daemon...");

    // Run pre-start hook BEFORE daemonization with inherited stdio
    let merged_config = selfci::config::read_merged_mq_config(root_dir).unwrap_or_default();
    if !run_hook_interactive(
        merged_config.hooks.pre_start.as_ref(),
        "pre-start",
        root_dir,
    ) {
        eprintln!("Pre-start hook failed, aborting daemon startup");
        return Err(MainError::CheckFailed);
    }

    let result = initialize_daemon_dir(root_dir, false, None, &base_branch)?;

    match result {
        DaemonizeResult::Parent(daemon_dir) => {
            // Parent process - wait for daemon to be ready and return
            let socket_path = daemon_dir.join("mq.sock");
            for _ in 0..50 {
                // Wait up to 5 seconds
                std::thread::sleep(std::time::Duration::from_millis(100));
                if socket_path.exists()
                    && let Ok(mq_protocol::MQResponse::HelloAck) =
                        mq_protocol::send_mq_request(&socket_path, mq_protocol::MQRequest::Hello)
                {
                    return Ok(Some(daemon_dir));
                }
            }
            eprintln!("Warning: Daemon started but not responding");
            Ok(Some(daemon_dir))
        }
        DaemonizeResult::Child(daemon_dir) => {
            // Child process - run the daemon loop (never returns)
            let _ = run_daemon_loop(daemon_dir, root_dir.to_path_buf(), base_branch);
            std::process::exit(0);
        }
    }
}

/// Run the daemon main loop (socket listener, request handler, etc.)
fn run_daemon_loop(
    daemon_dir: PathBuf,
    root_dir: PathBuf,
    base_branch: String,
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

    // Bind socket
    let socket_path = daemon_dir.join("mq.sock");
    let listener = UnixListener::bind(&socket_path).map_err(WorkDirError::CreateFailed)?;

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
        next_job_id: 1,
        queued: HashMap::new(),
        active: HashMap::new(),
        completed: Vec::new(),
    });

    // Create channel for queueing jobs
    let (mq_jobs_sender, mq_jobs_receiver) = mpsc::channel::<u64>();

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
                let mq_jobs_sender_clone = mq_jobs_sender.clone();
                std::thread::spawn(move || {
                    if let Ok(request) = mq_protocol::read_mq_request(&mut stream) {
                        let response = handle_request(&state_clone, request, mq_jobs_sender_clone);
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
    mq_jobs_sender: mpsc::Sender<u64>,
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

            let job = state.queue_job(resolved_candidate.clone(), no_merge);
            let job_id = job.id;

            debug!(
                candidate_user = %resolved_candidate.user,
                candidate_commit = %resolved_candidate.commit_id,
                job_id,
                no_merge,
                "Added candidate to queue"
            );

            // Send job ID to process_queue
            match mq_jobs_sender.send(job_id) {
                Ok(_) => mq_protocol::MQResponse::CandidateAdded { job_id },
                Err(e) => mq_protocol::MQResponse::Error(format!("Failed to queue job: {}", e)),
            }
        }

        mq_protocol::MQRequest::List { limit } => {
            let jobs = state.list_jobs(limit);
            mq_protocol::MQResponse::JobList { jobs }
        }

        mq_protocol::MQRequest::GetStatus { job_id } => {
            let job = state.get_job(job_id);
            mq_protocol::MQResponse::JobStatus { job }
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

fn process_queue(state: SharedMQState, root_dir: PathBuf, mq_jobs_receiver: mpsc::Receiver<u64>) {
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

        // Move job from queued to active and get hooks and merge_style
        let (mut job_info, hooks, merge_style) = match state.start_job(job_id) {
            Some(result) => result,
            None => {
                debug!("Job {} not found in queued map", job_id);
                continue;
            }
        };

        debug!(
            job_id = job_info.id,
            candidate_user = %job_info.candidate.user,
            candidate_commit = %job_info.candidate.commit_id,
            "Processing MQ candidate check"
        );

        // Get base branch
        let base_branch = state.base_branch();

        // Create candidate environment for hooks (before test merge, no merged info yet)
        let candidate_commit_id = job_info.candidate.commit_id.to_string();
        let candidate_change_id = job_info.candidate.change_id.to_string();
        let candidate_id = job_info.candidate.user.to_string();
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
            job_info.output.push_str("### Pre-Clone Hook\n\n");
            job_info.output.push_str(&pre_clone_result.output);
            job_info.output.push('\n');
        }
        if !pre_clone_result.success {
            job_info.status = mq_protocol::MQJobStatus::Failed(mq_protocol::FailedReason::PreClone);
            job_info.output.push_str("\nPre-clone hook failed\n");
            job_info.completed_at = Some(SystemTime::now());
            state.complete_job(job_id, job_info);
            continue;
        }

        // Resolve base branch to immutable ID
        let resolved_base = match selfci::revision::resolve_revision(&vcs, &root_dir, &base_branch)
        {
            Ok(r) => r,
            Err(e) => {
                job_info.status =
                    mq_protocol::MQJobStatus::Failed(mq_protocol::FailedReason::BaseResolve);
                job_info.output.push_str(&format!(
                    "Failed to resolve base branch '{}': {}",
                    base_branch, e
                ));
                job_info.completed_at = Some(SystemTime::now());
                state.complete_job(job_id, job_info);
                continue;
            }
        };

        // Create test merge/rebase of candidate onto base for CI testing
        let test_merge_result =
            match create_test_merge(&root_dir, &base_branch, &job_info.candidate, &merge_style) {
                Ok(result) => result,
                Err(e) => {
                    let fail_reason = match merge_style {
                        selfci::config::MergeStyle::Rebase => mq_protocol::FailedReason::TestRebase,
                        selfci::config::MergeStyle::Merge => mq_protocol::FailedReason::TestMerge,
                    };
                    job_info.status = mq_protocol::MQJobStatus::Failed(fail_reason);
                    job_info.output.push_str(&format!(
                        "Failed to create test merge/rebase of candidate onto base: {}",
                        e
                    ));
                    job_info.completed_at = Some(SystemTime::now());
                    state.complete_job(job_id, job_info);
                    continue;
                }
            };

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
            user: job_info.candidate.user.clone(),
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
            Some(&job_info.candidate), // Original candidate for SELFCI_CANDIDATE_* env vars
        ) {
            Ok(result) => {
                // Handle post-clone hook output if present
                if let Some(output) = &result.post_clone_output
                    && !output.is_empty()
                {
                    job_info.output.push_str("### Post-Clone Hook\n\n");
                    job_info.output.push_str(output);
                    job_info.output.push('\n');
                }

                // Check if post-clone hook failed
                if result.post_clone_success == Some(false) {
                    job_info.status =
                        mq_protocol::MQJobStatus::Failed(mq_protocol::FailedReason::PostClone);
                    job_info.output.push_str("\nPost-clone hook failed\n");
                    job_info.completed_at = Some(SystemTime::now());
                    state.complete_job(job_id, job_info);
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
                job_info.output.push_str("### Check Output\n\n");
                job_info.output.push_str(&result.output);
                job_info.steps = result.steps;
                job_info.completed_at = Some(SystemTime::now());

                if job_passed {
                    // Merge into base branch if no_merge is false
                    if job_info.no_merge {
                        debug!(
                            "MQ candidate check {} passed (no-merge mode, skipping merge)",
                            job_info.id
                        );
                        job_info.status =
                            mq_protocol::MQJobStatus::Passed(mq_protocol::PassedReason::NoMerge);
                    } else {
                        debug!(
                            "MQ candidate check {} passed, merging into {}",
                            job_info.id, base_branch
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
                            job_info.output.push_str("\n\n### Pre-Merge Hook\n\n");
                            job_info.output.push_str(&pre_merge_result.output);
                        }

                        if !pre_merge_result.success {
                            job_info
                                .output
                                .push_str("\n\nPre-merge hook failed, skipping merge\n");
                            job_info.status = mq_protocol::MQJobStatus::Failed(
                                mq_protocol::FailedReason::PreMerge,
                            );
                        } else {
                            // Perform the final merge using the original candidate
                            // (test merge used duplicated commits that were cleaned up)
                            match merge_candidate(
                                &root_dir,
                                &base_branch,
                                &job_info.candidate,
                                &check_output,
                                &merge_style,
                            ) {
                                Ok(merge_log) => {
                                    // Append merge output with separator
                                    job_info.output.push_str("\n\n### Merge Output\n\n");
                                    job_info.output.push_str(&merge_log);

                                    // Run post-merge hook if configured
                                    // Uses post-merge env (has merged info)
                                    let post_merge_result = run_hook_with_env(
                                        hooks.post_merge.as_ref(),
                                        "post-merge",
                                        &root_dir,
                                        Some(&candidate_env_post_merge),
                                    );
                                    if !post_merge_result.output.is_empty() {
                                        job_info.output.push_str("\n\n### Post-Merge Hook\n\n");
                                        job_info.output.push_str(&post_merge_result.output);
                                    }

                                    if !post_merge_result.success {
                                        job_info.output.push_str("\n\nPost-merge hook failed\n");
                                        // Note: merge already happened, so we still report success
                                        // but log the hook failure
                                    }

                                    job_info.status = mq_protocol::MQJobStatus::Passed(
                                        mq_protocol::PassedReason::Merged,
                                    );
                                }
                                Err(e) => {
                                    job_info
                                        .output
                                        .push_str(&format!("\n\n### Merge Failed\n\n{}", e));
                                    job_info.status = mq_protocol::MQJobStatus::Failed(
                                        mq_protocol::FailedReason::Merge,
                                    );
                                }
                            }
                        }
                    }
                } else {
                    job_info.status =
                        mq_protocol::MQJobStatus::Failed(mq_protocol::FailedReason::Check);
                    debug!("MQ candidate check {} failed", job_info.id);
                }
            }
            Err(e) => {
                job_info.status =
                    mq_protocol::MQJobStatus::Failed(mq_protocol::FailedReason::Check);
                job_info.output = format!("Check failed: {}", e);
                job_info.completed_at = Some(SystemTime::now());
                debug!("MQ candidate check {} failed: {}", job_info.id, e);
            }
        }

        // Move job from active to completed
        state.complete_job(job_id, job_info);
    }
}

/// Result of a test merge operation (merge/rebase before CI check)
pub(crate) struct TestMergeResult {
    /// The commit ID of the merged/rebased commit
    pub(crate) commit_id: selfci::revision::CommitId,
    /// The change ID (for jujutsu, same as commit_id for git)
    pub(crate) change_id: selfci::revision::ChangeId,
}

/// Test rebase for Git - rebases candidate onto base without updating any refs
/// Returns the resulting commit ID
fn test_merge_git_rebase(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<TestMergeResult, selfci::MergeError> {
    // Create temporary worktree in detached HEAD state at candidate commit
    let temp_worktree = root_dir.join(format!(".git/selfci-test-worktree-{}", candidate.commit_id));

    debug!(
        worktree = %temp_worktree.display(),
        candidate = %candidate.commit_id,
        base = %base_branch,
        "Creating test rebase worktree"
    );

    cmd!(
        "git",
        "worktree",
        "add",
        "--detach",
        &temp_worktree,
        candidate.commit_id.as_str()
    )
    .dir(root_dir)
    .stderr_to_stdout()
    .run()
    .map_err(selfci::MergeError::WorktreeCreateFailed)?;

    // Ensure cleanup on any exit path
    let cleanup = scopeguard::guard((), |_| {
        let _ = cmd!("git", "worktree", "remove", "--force", &temp_worktree)
            .dir(root_dir)
            .run();
    });

    // In the worktree, rebase onto base_branch
    debug!(base = %base_branch, "Rebasing onto base branch");
    cmd!("git", "rebase", base_branch)
        .dir(&temp_worktree)
        .stderr_to_stdout()
        .run()
        .map_err(selfci::MergeError::RebaseFailed)?;

    // Get the resulting commit ID (HEAD in worktree)
    let commit_id = cmd!("git", "rev-parse", "HEAD")
        .dir(&temp_worktree)
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?
        .trim()
        .to_string();

    debug!(commit_id = %commit_id, "Test rebase complete");

    // Cleanup is handled by scopeguard
    drop(cleanup);

    Ok(TestMergeResult {
        commit_id: selfci::revision::CommitId::new(commit_id.clone())
            .expect("git rev-parse returned invalid commit id"),
        change_id: selfci::revision::ChangeId::new(commit_id),
    })
}

/// Test merge for Git - merges candidate into base without updating any refs
/// Returns the resulting commit ID
fn test_merge_git_merge(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<TestMergeResult, selfci::MergeError> {
    // Create temporary worktree at base branch
    let temp_worktree = root_dir.join(format!(".git/selfci-test-worktree-{}", candidate.commit_id));

    debug!(
        worktree = %temp_worktree.display(),
        candidate = %candidate.commit_id,
        base = %base_branch,
        "Creating test merge worktree"
    );

    cmd!(
        "git",
        "worktree",
        "add",
        "--detach",
        &temp_worktree,
        base_branch
    )
    .dir(root_dir)
    .stderr_to_stdout()
    .run()
    .map_err(selfci::MergeError::WorktreeCreateFailed)?;

    // Ensure cleanup on any exit path
    let cleanup = scopeguard::guard((), |_| {
        let _ = cmd!("git", "worktree", "remove", "--force", &temp_worktree)
            .dir(root_dir)
            .run();
    });

    // In the worktree, merge candidate
    debug!(candidate = %candidate.commit_id, "Merging candidate");
    cmd!(
        "git",
        "merge",
        "--no-ff",
        "-m",
        "Test merge by SelfCI",
        candidate.commit_id.as_str()
    )
    .dir(&temp_worktree)
    .stderr_to_stdout()
    .run()
    .map_err(selfci::MergeError::MergeFailed)?;

    // Get the resulting commit ID (HEAD in worktree)
    let commit_id = cmd!("git", "rev-parse", "HEAD")
        .dir(&temp_worktree)
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?
        .trim()
        .to_string();

    debug!(commit_id = %commit_id, "Test merge complete");

    // Cleanup is handled by scopeguard
    drop(cleanup);

    Ok(TestMergeResult {
        commit_id: selfci::revision::CommitId::new(commit_id.clone())
            .expect("git rev-parse returned invalid commit id"),
        change_id: selfci::revision::ChangeId::new(commit_id),
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
) -> Result<TestMergeResult, selfci::MergeError> {
    debug!(
        candidate = %candidate.commit_id,
        base = %base_branch,
        "Duplicating candidate branch for test rebase"
    );

    // Duplicate the candidate branch (commits from base to candidate, exclusive of base)
    // This creates copies with new change IDs, leaving the original untouched
    let revset = format!("{}..{}", base_branch, candidate.commit_id);
    let dup_output = cmd!("jj", "--ignore-working-copy", "duplicate", &revset)
        .dir(root_dir)
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::RebaseFailed)?;

    debug!(dup_output = %dup_output, "Duplicate output");

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
        selfci::MergeError::RebaseFailed(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Failed to find duplicated tip in output: {}", dup_output),
        ))
    })?;

    debug!(dup_change_id = %dup_change_id, "Found duplicated tip change ID");

    // Rebase the duplicated commits onto base branch
    cmd!(
        "jj",
        "--ignore-working-copy",
        "rebase",
        "-b",
        &dup_change_id,
        "-d",
        base_branch
    )
    .dir(root_dir)
    .stderr_to_stdout()
    .run()
    .map_err(selfci::MergeError::RebaseFailed)?;

    // Get the commit ID of the rebased duplicate
    let commit_id = cmd!(
        "jj",
        "log",
        "-r",
        &dup_change_id,
        "-T",
        "commit_id",
        "--no-graph",
        "--color=never"
    )
    .dir(root_dir)
    .read()
    .map_err(selfci::MergeError::ChangeIdFailed)?
    .trim()
    .to_string();

    debug!(commit_id = %commit_id, dup_change_id = %dup_change_id, "Test rebase complete");

    // Update working copy snapshot to avoid stale errors
    cmd!("jj", "workspace", "update-stale")
        .dir(root_dir)
        .stdin_null()
        .stderr_to_stdout()
        .run()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;

    Ok(TestMergeResult {
        commit_id: selfci::revision::CommitId::new(commit_id)
            .expect("jj log returned invalid commit id"),
        change_id: selfci::revision::ChangeId::new(dup_change_id),
    })
}

/// Test merge for Jujutsu - creates a merge commit without updating bookmarks
/// Returns the resulting commit and change IDs
fn test_merge_jj_merge(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
) -> Result<TestMergeResult, selfci::MergeError> {
    debug!(
        candidate = %candidate.commit_id,
        base = %base_branch,
        "Creating test merge commit"
    );

    // Create a new merge commit with both base and candidate as parents
    let output = cmd!(
        "jj",
        "--ignore-working-copy",
        "new",
        "--no-edit",
        base_branch,
        candidate.commit_id.as_str()
    )
    .dir(root_dir)
    .stdin_null()
    .stderr_to_stdout()
    .read()
    .map_err(selfci::MergeError::MergeFailed)?;

    // Parse the output to get the change ID
    // Output format: "Created new commit <change_id> <short_commit_id> ..."
    let parts: Vec<&str> = output
        .lines()
        .find(|line| line.starts_with("Created new commit"))
        .ok_or_else(|| {
            selfci::MergeError::MergeFailed(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to parse merge commit from output: {}", output),
            ))
        })?
        .split_whitespace()
        .collect();

    let change_id = parts.get(3).ok_or_else(|| {
        selfci::MergeError::MergeFailed(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Failed to parse change ID from output: {}", output),
        ))
    })?;

    // Get the full commit ID using jj log (the output only has short ID)
    let commit_id = cmd!(
        "jj",
        "log",
        "-r",
        *change_id,
        "-T",
        "commit_id",
        "--no-graph",
        "--color=never"
    )
    .dir(root_dir)
    .read()
    .map_err(selfci::MergeError::ChangeIdFailed)?
    .trim()
    .to_string();

    debug!(commit_id = %commit_id, change_id = %change_id, "Test merge complete");

    // Update working copy snapshot to avoid stale errors
    cmd!("jj", "workspace", "update-stale")
        .dir(root_dir)
        .stdin_null()
        .stderr_to_stdout()
        .run()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;

    Ok(TestMergeResult {
        commit_id: selfci::revision::CommitId::new(commit_id.to_string())
            .expect("jj new returned invalid commit id"),
        change_id: selfci::revision::ChangeId::new(change_id.to_string()),
    })
}

/// Create a test merge/rebase of candidate onto base for CI testing
/// This does NOT update any refs - the resulting commit is dangling (Git) or just exists (jj)
pub(crate) fn create_test_merge(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
    merge_style: &selfci::config::MergeStyle,
) -> Result<TestMergeResult, selfci::MergeError> {
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
    .map_err(selfci::MergeError::WorktreeCreateFailed)?;
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
        .map_err(selfci::MergeError::RebaseFailed)?;
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
    .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push('\n');

    // Cleanup is handled by scopeguard
    drop(cleanup);

    merge_log.push_str("Rebase completed successfully\n");
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
    .map_err(selfci::MergeError::WorktreeCreateFailed)?;
    merge_log.push_str(&output);
    merge_log.push('\n');

    // Ensure cleanup on any exit path
    let cleanup = scopeguard::guard((), |_| {
        let _ = cmd!("git", "worktree", "remove", "--force", &temp_worktree)
            .dir(root_dir)
            .run();
    });

    // In the worktree, merge candidate with a custom message
    merge_log.push_str(&format!("Merging {} with --no-ff\n", candidate.commit_id));

    // Build the merge commit message
    let merge_message = format!(
        "Merge commit '{}' by SelfCI\n\n### Check Output\n\n{}",
        candidate.commit_id, test_output
    );

    let output = cmd!(
        "git",
        "merge",
        "--no-ff",
        "-m",
        &merge_message,
        candidate.commit_id.as_str()
    )
    .dir(&temp_worktree)
    .stderr_to_stdout()
    .read()
    .map_err(selfci::MergeError::MergeFailed)?;
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
    .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push('\n');

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
    .map_err(selfci::MergeError::ChangeIdFailed)?
    .trim()
    .to_string();
    merge_log.push_str(&format!("Change ID: {}\n", change_id));

    // Rebase the candidate branch onto base
    // The test merge used a duplicate, so the original candidate is still untouched
    merge_log.push_str("Rebasing branch\n");
    let output = cmd!(
        "jj",
        "--ignore-working-copy",
        "rebase",
        "-b",
        candidate.commit_id.as_str(),
        "-d",
        base_branch
    )
    .dir(root_dir)
    .stderr_to_stdout()
    .read()
    .map_err(selfci::MergeError::RebaseFailed)?;
    merge_log.push_str(&output);
    merge_log.push('\n');

    // Move the base branch bookmark to the rebased commit using change ID
    merge_log.push_str(&format!(
        "Moving {} bookmark to rebased commit\n",
        base_branch
    ));
    let output = cmd!(
        "jj",
        "--ignore-working-copy",
        "bookmark",
        "set",
        base_branch,
        "-r",
        &change_id
    )
    .dir(root_dir)
    .stderr_to_stdout()
    .read()
    .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push('\n');

    // Update the working copy snapshot to avoid "stale working copy" errors
    merge_log.push_str("Updating working copy snapshot\n");
    let output = cmd!("jj", "workspace", "update-stale")
        .dir(root_dir)
        .stdin_null()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);

    merge_log.push_str("Rebase completed successfully\n");
    Ok(merge_log)
}

fn merge_candidate_jj_merge(
    root_dir: &Path,
    base_branch: &str,
    candidate: &selfci::revision::ResolvedRevision,
    test_output: &str,
) -> Result<String, selfci::MergeError> {
    let mut merge_log = String::new();

    // Jujutsu merge mode - create a merge commit
    merge_log.push_str(&format!(
        "Jujutsu merge mode: creating merge commit of {} into {}\n",
        candidate.commit_id, base_branch
    ));

    // Create a new merge commit with both base and candidate as parents
    // Use --ignore-working-copy and --no-edit to avoid changing the working directory or @
    merge_log.push_str("Creating merge commit\n");
    let output = cmd!(
        "jj",
        "--ignore-working-copy",
        "new",
        "--no-edit",
        base_branch,
        candidate.commit_id.as_str()
    )
    .dir(root_dir)
    .stdin_null()
    .stderr_to_stdout()
    .read()
    .map_err(selfci::MergeError::MergeFailed)?;
    merge_log.push_str(&output);
    merge_log.push('\n');

    // Parse the output to get the merge commit ID
    // Output format: "Created new commit <change_id> <commit_id> ..."
    let merge_commit_change_id = output
        .lines()
        .find(|line| line.starts_with("Created new commit"))
        .and_then(|line| line.split_whitespace().nth(3))
        .ok_or_else(|| {
            selfci::MergeError::MergeFailed(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to parse merge commit ID from output: {}", output),
            ))
        })?
        .to_string();
    merge_log.push_str(&format!(
        "Merge commit change ID: {}\n",
        merge_commit_change_id
    ));

    // Set the merge commit description
    let merge_message = format!(
        "Merge commit '{}' by SelfCI\n\n### Check Output\n\n{}",
        candidate.commit_id, test_output
    );
    merge_log.push_str("Setting merge commit description\n");
    let output = cmd!(
        "jj",
        "--ignore-working-copy",
        "describe",
        "-r",
        &merge_commit_change_id,
        "-m",
        &merge_message
    )
    .dir(root_dir)
    .stderr_to_stdout()
    .read()
    .map_err(selfci::MergeError::MergeFailed)?;
    merge_log.push_str(&output);
    merge_log.push('\n');

    // Get the final commit ID (describe may have changed it)
    let final_commit_id = cmd!(
        "jj",
        "log",
        "-r",
        &merge_commit_change_id,
        "-T",
        "commit_id",
        "--no-graph",
        "--color=never"
    )
    .dir(root_dir)
    .read()
    .map_err(selfci::MergeError::ChangeIdFailed)?
    .trim()
    .to_string();
    merge_log.push_str(&format!("Final merge commit ID: {}\n", final_commit_id));

    // Move the base branch bookmark to the merge commit
    merge_log.push_str(&format!(
        "Moving {} bookmark to merge commit\n",
        base_branch
    ));
    let output = cmd!(
        "jj",
        "--ignore-working-copy",
        "bookmark",
        "set",
        base_branch,
        "-r",
        &final_commit_id
    )
    .dir(root_dir)
    .stderr_to_stdout()
    .read()
    .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push('\n');

    // Update the working copy snapshot to avoid "stale working copy" errors
    merge_log.push_str("Updating working copy snapshot\n");
    let output = cmd!("jj", "workspace", "update-stale")
        .dir(root_dir)
        .stdin_null()
        .stderr_to_stdout()
        .read()
        .map_err(selfci::MergeError::BranchUpdateFailed)?;
    merge_log.push_str(&output);
    merge_log.push('\n');

    merge_log.push_str("Merge completed successfully\n");
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
    let daemon_dir = match get_daemon_runtime_dir(&root_dir)? {
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
                    "{:<6} {:<20} {:<12} {:<10} {:<20} {:<20}",
                    "ID", "Status", "Change", "Commit", "Candidate", "Queued"
                );
                println!("{}", "-".repeat(92));
                for job in jobs {
                    let status = job.status.display();

                    let queued = humantime::format_rfc3339_seconds(job.queued_at);
                    // Shorten change_id and commit_id to first 8 chars
                    let change_short = &job.candidate.change_id.as_str()
                        [..job.candidate.change_id.as_str().len().min(8)];
                    let commit_short = &job.candidate.commit_id.as_str()
                        [..job.candidate.commit_id.as_str().len().min(8)];
                    println!(
                        "{:<6} {:<20} {:<12} {:<10} {:<20} {:<20}",
                        job.id,
                        status,
                        change_short,
                        commit_short,
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
            println!("Status: {}", job.status.display());
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
