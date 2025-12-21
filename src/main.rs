mod cmd;
mod opts;

use clap::Parser;
use opts::{Cli, Commands, ReportCommands};
use selfci::{detect_vcs, init_config, step, MainError};
use tracing::debug;

fn main() {
    // Initialize tracing subscriber
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    if let Err(err) = main_inner() {
        // Print the error chain
        eprintln!("Error: {}", err);

        // Print source chain if available
        let mut source = std::error::Error::source(&err);
        while let Some(err) = source {
            eprintln!("Caused by: {}", err);
            source = std::error::Error::source(err);
        }

        std::process::exit(err.exit_code());
    }
}

fn main_inner() -> Result<(), MainError> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init => {
            // Get current directory
            let root_dir = std::env::current_dir().expect("Failed to get current directory");

            // Detect VCS first to ensure we're in a VCS-managed directory
            let _vcs = detect_vcs(&root_dir)?;

            // Initialize config
            init_config(&root_dir)?;

            println!("Initialized selfci config at .config/selfci/config.yml");
            println!("Edit this file to configure your CI command.");
        }
        Commands::Check { root, base, candidate, print_output, jobs } => {
            cmd::check::check(root, base, candidate, print_output, jobs)?;
        }
        Commands::Report { report_command } => {
            match report_command {
                ReportCommands::Success => {
                    debug!(command = "success", "Report command invoked");
                    // TODO: Implement report success logic
                }
                ReportCommands::Failure => {
                    debug!(command = "failure", "Report command invoked");
                    // TODO: Implement report failure logic
                }
                ReportCommands::Run => {
                    debug!(command = "run", "Report command invoked");
                    // TODO: Implement report run logic
                }
                ReportCommands::Init => {
                    debug!(command = "init", "Report command invoked");
                    // TODO: Implement report init logic
                }
            }
        }
        Commands::Step { name } => {
            if let Err(err) = step::log_step(name) {
                eprintln!("Error logging step: {}", err);
                std::process::exit(1);
            }
        }
    }

    Ok(())
}
