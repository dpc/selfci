mod cmd;
mod opts;

use clap::Parser;
use opts::{Cli, Commands, JobCommands, StepCommands};
use selfci::{MainError, detect_vcs, init_config, protocol};
use std::path::PathBuf;

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

            println!(
                "Initialized selfci config at .config/selfci/{}",
                selfci::constants::CONFIG_FILENAME
            );
            println!("Edit this file to configure your CI command.");
        }
        Commands::Check {
            root,
            base,
            candidate,
            print_output,
            jobs,
        } => {
            cmd::check::check(
                root,
                base,
                candidate,
                print_output,
                jobs,
                cli.vcs.as_deref(),
            )?;
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
                    let request = protocol::JobControlRequest::MarkStepFailed { job_name, ignore };
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
                JobCommands::Wait { name, success } => {
                    // Send request to wait for job
                    let request = protocol::JobControlRequest::WaitForJob { name: name.clone() };
                    match protocol::send_request(&socket_path, request) {
                        Ok(protocol::JobControlResponse::JobCompleted { status }) => {
                            match status {
                                protocol::JobStatus::Succeeded => {
                                    // Job succeeded - exit 0
                                    std::process::exit(0);
                                }
                                protocol::JobStatus::Failed => {
                                    if success {
                                        eprintln!("Job '{}' failed", name);
                                        std::process::exit(
                                            selfci::exit_codes::EXIT_JOB_WAIT_FAILED,
                                        );
                                    } else {
                                        // --success not specified, just report status
                                        std::process::exit(0);
                                    }
                                }
                                protocol::JobStatus::Running => {
                                    // Shouldn't happen - job should not be reported as completed while running
                                    eprintln!("Job '{}' is still running", name);
                                    std::process::exit(1);
                                }
                            }
                        }
                        Ok(protocol::JobControlResponse::JobNotFound) => {
                            eprintln!("Job '{}' not found", name);
                            std::process::exit(selfci::exit_codes::EXIT_JOB_NOT_FOUND);
                        }
                        Ok(protocol::JobControlResponse::Error(err)) => {
                            eprintln!("Error waiting for job: {}", err);
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
        Commands::Mq(mq_command) => match mq_command {
            opts::MQCommands::Start { base_branch } => {
                cmd::mq::start_daemon(base_branch)?;
            }
            opts::MQCommands::Add {
                candidate,
                no_merge,
            } => {
                cmd::mq::add_candidate(candidate, no_merge)?;
            }
            opts::MQCommands::List { limit } => {
                cmd::mq::list_jobs(limit)?;
            }
            opts::MQCommands::Status { run_id: job_id } => {
                cmd::mq::get_status(job_id)?;
            }
        },
    }

    Ok(())
}
