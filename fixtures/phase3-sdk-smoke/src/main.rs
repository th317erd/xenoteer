// SPDX-License-Identifier: BUSL-1.1

//! Internal black-box SDK probe for the Phase 3 control image.

use std::{env, error::Error, fs, io, str::FromStr};

use xenoteer_sdk::{
    Client, Command, CommandEnvelope, CommandLifecycle, DesktopGeneration, DesktopId,
    DesktopProbeCommand, ProtocolVersion, RequestId,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut arguments = env::args();
    let _program = arguments.next();
    let base_uri = required_argument(&mut arguments, "base URI")?;
    let token_path = required_argument(&mut arguments, "token file")?;
    let desktop_id = DesktopId::from_str(&required_argument(&mut arguments, "desktop ID")?)?;
    let generation =
        DesktopGeneration::from_str(&required_argument(&mut arguments, "desktop generation")?)?;
    if arguments.next().is_some() {
        return Err(
            "usage: xenoteer-phase3-sdk-smoke BASE_URI TOKEN_FILE DESKTOP_ID GENERATION".into(),
        );
    }

    let token = fs::read(token_path)?;
    let client = Client::new(base_uri, token)?;
    let command = CommandEnvelope::new(
        ProtocolVersion::V1_0,
        RequestId::new(),
        xenoteer_sdk::CommandId::new(),
        desktop_id,
        generation,
        Command::DesktopProbe(DesktopProbeCommand {}),
    )?;
    let submitted = client.submit_command(&command).await?;
    let result = if submitted.lifecycle().is_terminal() {
        submitted
    } else {
        client
            .wait_command(desktop_id, command.command_id, 10_000)
            .await?
    };
    if !matches!(result.lifecycle(), CommandLifecycle::Succeeded) {
        return Err("SDK probe command did not reach successful terminal state".into());
    }
    serde_json::to_writer(io::stdout().lock(), &result)?;
    println!();
    Ok(())
}

fn required_argument(
    arguments: &mut impl Iterator<Item = String>,
    name: &'static str,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    arguments
        .next()
        .ok_or_else(|| format!("missing required {name}").into())
}
