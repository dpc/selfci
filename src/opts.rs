use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "selfci")]
#[command(about = "A minimalistic local-first unix-philosophy-abiding CI", long_about = None)]
pub struct Cli {
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
    /// Report commands
    Report {
        #[command(subcommand)]
        report_command: ReportCommands,
    },
    /// Log a step (used inside check commands)
    Step {
        /// Name of the step
        name: String,
    },
    /// Start a new job (used inside check commands)
    Job {
        /// Name of the job
        name: String,
    },
}

#[derive(Subcommand)]
pub enum ReportCommands {
    /// Report success
    Success,
    /// Report failure
    Failure,
    /// Report run
    Run,
    /// Report init
    Init,
}
