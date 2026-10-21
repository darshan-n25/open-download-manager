//! Open Download Manager binary entry point.

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use odm::cli::{self, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose, cli.quiet);

    match cli::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("odm-cli: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Log to stderr so stdout stays clean for piping (`odm-cli list --json`, `odm-cli grab`).
fn init_tracing(verbose: u8, quiet: bool) {
    let level = if quiet {
        "error"
    } else {
        match verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("odm-cli={level}")));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
