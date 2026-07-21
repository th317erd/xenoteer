//! Phase-0 diagnostic CLI package boundary.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xenoteerctl", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report daemon and desktop status once the public transport is available.
    Status,
    /// Diagnose the deployment once public probes are available.
    Doctor,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let operation = match cli.command {
        Command::Status => "status",
        Command::Doctor => "doctor",
    };
    eprintln!(
        "xenoteerctl {operation}: not yet wired in Phase 0; the public SDK transport is scheduled for Phase 6"
    );
    ExitCode::from(7)
}
