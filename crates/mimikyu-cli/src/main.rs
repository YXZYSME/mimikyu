mod batch;
mod cli;
mod client;
mod commands;
mod output;

use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use cli::Cli;
use output::{print_json_err, CliError};

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() -> ExitCode {
    init_tracing();

    let raw_json_requested = std::env::args_os().any(|a| a == "--json");

    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(err) => {
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                let _ = err.print();
                return ExitCode::SUCCESS;
            }
            if raw_json_requested {
                print_json_err(&err.to_string(), None);
            } else {
                let _ = err.print();
            }
            return ExitCode::FAILURE;
        }
    };

    let json_mode = cli.json;

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            let msg = format!("failed to initialize async runtime: {err}");
            if json_mode {
                print_json_err(&msg, None);
            } else {
                eprintln!("error: {msg}");
            }
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(commands::dispatch(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if json_mode {
                if let Some(cli_err) = err.downcast_ref::<CliError>() {
                    print_json_err(&cli_err.message, cli_err.data.as_ref());
                } else {
                    print_json_err(&err.to_string(), None);
                }
            } else {
                eprintln!("error: {err}");
            }
            ExitCode::FAILURE
        }
    }
}
