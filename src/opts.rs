use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "selfci")]
#[command(about = "A minimalistic local-first unix-philosophy-abiding CI", long_about = None)]
pub struct Cli {
    /// Force VCS type (jj or git)
    #[arg(long, global = true, env = "SELFCI_VCS_FORCE", value_name = "VCS")]
    pub vcs: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize selfci in the current repository
    Init,
    /// Run CI checks
    Check {
        /// Root working directory
        #[arg(long, env = "SELFCI_ROOT_DIR")]
        root: Option<String>,

        /// Base revision to compare against
        #[arg(long)]
        base: Option<String>,

        /// Candidate revision to check
        #[arg(long)]
        candidate: Option<String>,

        /// Print output to stdout in real-time (otherwise only on failure)
        #[arg(long)]
        print_output: bool,

        /// Number of checks to run in parallel (default: number of CPUs)
        #[arg(long, short = 'j')]
        jobs: Option<usize>,
    },
    /// Step reporting commands (used from inside the CI check)
    Step {
        #[command(subcommand)]
        step_command: StepCommands,
    },
    /// Job control commands (used from inside the CI check)
    Job {
        #[command(subcommand)]
        job_command: JobCommands,
    },
    /// Merge queue commands
    #[command(subcommand)]
    Mq(MQCommands),
}

#[derive(Subcommand)]
pub enum StepCommands {
    /// Start a new step
    Start {
        /// Name of the step
        name: String,
    },
    /// Mark the last started step as failed
    Fail {
        /// Ignore this failure (don't fail the job)
        #[arg(long)]
        ignore: bool,
    },
}

#[derive(Subcommand)]
pub enum JobCommands {
    /// Start a new job
    Start {
        /// Name of the job
        name: String,
    },
    /// Wait for a job to complete
    Wait {
        /// Name of the job to wait for
        name: String,
        /// Wait for successful completion (fail if job fails)
        #[arg(long)]
        success: bool,
    },
}

#[derive(Subcommand)]
pub enum MQCommands {
    /// Start the merge queue daemon
    Start {
        /// Base branch to merge into
        #[arg(long)]
        base_branch: String,
    },
    /// Add a candidate to the merge queue
    Add {
        /// Candidate revision to check and merge
        #[arg(long)]
        candidate: String,

        /// Don't merge even if check passes (dry-run)
        #[arg(long)]
        no_merge: bool,
    },
    /// List jobs in the merge queue
    List {
        /// Number of recent jobs to show
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
    /// Get status of a specific job
    Status {
        /// Job ID to query
        job_id: u64,
    },
}
