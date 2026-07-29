//! Xenoteer language-neutral corpus adapter for the Rust SDK.

use std::{io, process::ExitCode};

#[tokio::main]
async fn main() -> ExitCode {
    match xenoteer_sdk::conformance::run_adapter(io::stdin().lock(), io::stdout().lock()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xenoteer-sdk-conformance: {error}");
            ExitCode::from(2)
        }
    }
}
