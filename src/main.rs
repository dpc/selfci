mod cmd;
mod opts;

use clap::Parser;
use opts::{Cli, Commands, JobCommands, StepCommands};
use selfci::{MainError, detect_vcs, envs, init_config, protocol};
use std::error::Error;
use std::path::PathBuf;

fn main() {
    // Initialize tracing subscriber
    // Use SELFCI_LOG env var (falls back to INFO level if not set)
    let env_filter = std::env::var(envs::SELFCI_LOG)
        .ok()
        .and_then(|v| v.parse::<tracing_subscriber::EnvFilter>().ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));

    // Use compact format by default (level + message only)
    // Set SELFCI_LOG_FULL for verbose format with timestamps and targets
    let use_full_format = std::env::var(envs::SELFCI_LOG_FULL).is_ok();

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr);

    if use_full_format {
        subscriber.init();
    } else {
        subscriber
            .without_time()
            .with_target(false)
            .with_level(false)
            .init();
    }

    if let Err(err) = main_inner() {
        // Print the error chain
        eprintln!("Error: {}", &err);

        let mut source = err.source();
        while let Some(err) = source {
            eprintln!("Caused by: {}", err);
            source = err.source();
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
            let job_name = match std::env::var(envs::SELFCI_JOB_NAME) {
                Ok(name) => name,
                Err(_) => {
                    eprintln!(
                        "Error: {} environment variable not set",
                        envs::SELFCI_JOB_NAME
                    );
                    std::process::exit(1);
                }
            };

            // Get socket path from environment
            let socket_path = match std::env::var(envs::SELFCI_JOB_SOCK_PATH) {
                Ok(path) => PathBuf::from(path),
                Err(_) => {
                    eprintln!(
                        "Error: {} environment variable not set",
                        envs::SELFCI_JOB_SOCK_PATH
                    );
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
            let socket_path = match std::env::var(envs::SELFCI_JOB_SOCK_PATH) {
                Ok(path) => PathBuf::from(path),
                Err(_) => {
                    eprintln!(
                        "Error: {} environment variable not set",
                        envs::SELFCI_JOB_SOCK_PATH
                    );
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
        Commands::Mq { command, run_id } => match command {
            Some(opts::MQCommands::Start {
                base_branch,
                foreground,
                log_file,
            }) => {
                cmd::mq::start_daemon(base_branch, foreground, log_file)?;
            }
            Some(opts::MQCommands::Add {
                candidate,
                no_merge,
            }) => {
                cmd::mq::add_candidate(candidate, no_merge)?;
            }
            Some(opts::MQCommands::Check { candidate }) => {
                cmd::mq::add_candidate(candidate, true)?;
            }
            Some(opts::MQCommands::List { limit }) => {
                cmd::mq::list_runs(limit)?;
            }
            None => {
                // If run_id provided, show status; otherwise list runs
                if let Some(id) = run_id {
                    cmd::mq::get_status(id)?;
                } else {
                    cmd::mq::list_runs(None)?;
                }
            }
            Some(opts::MQCommands::Status { run_id }) => {
                cmd::mq::get_status(run_id)?;
            }
            Some(opts::MQCommands::Stop) => {
                cmd::mq::stop_daemon()?;
            }
            Some(opts::MQCommands::RuntimeDir) => {
                cmd::mq::print_runtime_dir()?;
            }
            Some(opts::MQCommands::Pid) => {
                cmd::mq::print_pid()?;
            }
        },
    }

    Ok(())
}
