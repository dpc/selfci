mod cmd;
mod opts;

use clap::Parser;
use opts::{Cli, Commands, JobCommands, ReportCommands, StepCommands};
use selfci::{detect_vcs, init_config, protocol, MainError};
use std::path::PathBuf;
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
        Commands::Step { step_command } => {
            // Get job name from environment
            let job_name = match std::env::var("SELFCI_JOB_NAME") {
                Ok(name) => name,
                Err(_) => {
                    eprintln!("Error: SELFCI_JOB_NAME environment variable not set");
                    std::process::exit(1);
                }
            };

            // Get socket path from environment
            let socket_path = match std::env::var("SELFCI_JOB_SOCK_PATH") {
                Ok(path) => PathBuf::from(path),
                Err(_) => {
                    eprintln!("Error: SELFCI_JOB_SOCK_PATH environment variable not set");
                    std::process::exit(1);
                }
            };

            match step_command {
                StepCommands::Start { name } => {
                    // Send request to log step
                    let request = protocol::JobControlRequest::LogStep {
                        job_name,
                        step_name: name,
                    };
                    match protocol::send_request(&socket_path, request) {
                        Ok(protocol::JobControlResponse::StepLogged) => {
                            // Success - silent exit
                        }
                        Ok(protocol::JobControlResponse::Error(err)) => {
                            eprintln!("Error logging step: {}", err);
                            std::process::exit(1);
                        }
                        Ok(_) => {
                            eprintln!("Unexpected response from control socket");
                            std::process::exit(1);
                        }
                        Err(err) => {
                            eprintln!("Error communicating with control socket: {}", err);
                            std::process::exit(1);
                        }
                    }
                }
                StepCommands::Fail { ignore } => {
                    // Send request to mark step as failed
                    let request = protocol::JobControlRequest::MarkStepFailed {
                        job_name,
                        ignore,
                    };
                    match protocol::send_request(&socket_path, request) {
                        Ok(protocol::JobControlResponse::StepMarkedFailed) => {
                            // Success - silent exit
                        }
                        Ok(protocol::JobControlResponse::Error(err)) => {
                            eprintln!("Error marking step as failed: {}", err);
                            std::process::exit(1);
                        }
                        Ok(_) => {
                            eprintln!("Unexpected response from control socket");
                            std::process::exit(1);
                        }
                        Err(err) => {
                            eprintln!("Error communicating with control socket: {}", err);
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        Commands::Job { job_command } => {
            // Get socket path from environment
            let socket_path = match std::env::var("SELFCI_JOB_SOCK_PATH") {
                Ok(path) => PathBuf::from(path),
                Err(_) => {
                    eprintln!("Error: SELFCI_JOB_SOCK_PATH environment variable not set");
                    std::process::exit(1);
                }
            };

            match job_command {
                JobCommands::Start { name } => {
                    // Send request to start job
                    let request = protocol::JobControlRequest::StartJob { name };
                    match protocol::send_request(&socket_path, request) {
                        Ok(protocol::JobControlResponse::JobStarted) => {
                            // Success - silent exit
                        }
                        Ok(protocol::JobControlResponse::Error(err)) => {
                            eprintln!("Error starting job: {}", err);
                            std::process::exit(1);
                        }
                        Ok(_) => {
                            eprintln!("Unexpected response from control socket");
                            std::process::exit(1);
                        }
                        Err(err) => {
                            eprintln!("Error communicating with control socket: {}", err);
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
