//! PatchArena command-line entry point and exit-code adapter.

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use patcharena_cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: could not start async runtime: {error}");
            return ExitCode::from(1);
        }
    };

    match runtime.block_on(patcharena_cli::run(cli.command)) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn init_tracing(verbosity: u8) {
    let fallback = match verbosity {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact()
        .init();
}
