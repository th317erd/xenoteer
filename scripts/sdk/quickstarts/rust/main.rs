// SPDX-License-Identifier: Apache-2.0

use std::{env, path::PathBuf, process::ExitCode, time::Duration};

use xenoteer_sdk::{Client, DesktopState, ErrorCode, ProtocolVersion, SdkError, XenoteerClient};

fn required(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("required environment is missing: {name}"))
}

fn expected_artifact_root() -> Result<PathBuf, String> {
    let root = PathBuf::from(required("XENOTEER_EXPECTED_INSTALL_ROOT")?);
    let root = root
        .canonicalize()
        .map_err(|_| "staged Rust archive root is unavailable".to_owned())?;
    for package in ["xenoteer-sdk", "xenoteer-protocol"] {
        let manifest = root.join(package).join("Cargo.toml");
        if !manifest.is_file() {
            return Err(format!("staged Rust archive omitted {package}/Cargo.toml"));
        }
    }
    Ok(root)
}

async fn exercise() -> Result<(), String> {
    let _artifact_root = expected_artifact_root()?;
    let language = required("XENOTEER_QUICKSTART_LANGUAGE")?;
    if !language
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
    {
        return Err("quick-start language label is invalid".to_owned());
    }
    let base = required("XENOTEER_API_BASE")?;
    let token = required("XENOTEER_TOKEN")?;
    let expect_auth_failure = required("XENOTEER_EXPECT_AUTH_FAILURE")? == "1";
    let transport = Client::new(base, token.as_bytes())
        .and_then(|client| client.with_request_timeout(Duration::from_secs(5)))
        .map_err(|error| format!("could not prepare bounded SDK transport: {error}"))?;
    let connection = tokio::time::timeout(
        Duration::from_secs(6),
        XenoteerClient::from_transport(transport),
    )
    .await
    .map_err(|_| "SDK connection exceeded its outer bound".to_owned())?;

    if expect_auth_failure {
        match connection {
            Err(SdkError::Problem(problem))
                if problem.status() == 401
                    && problem.code() == ErrorCode::AuthenticationRequired =>
            {
                println!("quickstart-ok language={language} mode=auth-failure");
                return Ok(());
            }
            Ok(client) => {
                client.close().await;
                return Err("invalid bearer unexpectedly authenticated".to_owned());
            }
            Err(error) => {
                return Err(format!(
                    "invalid bearer returned the wrong safe SDK failure: {error}"
                ));
            }
        }
    }

    let client = connection.map_err(|error| format!("SDK connection failed: {error}"))?;
    if client.negotiated_protocol() != ProtocolVersion::V1_0 {
        client.close().await;
        return Err("server did not negotiate frozen protocol v1.0".to_owned());
    }
    if client.status().desktop.state != DesktopState::Ready {
        client.close().await;
        return Err("desktop was not ready".to_owned());
    }
    client
        .desktop()
        .map_err(|error| format!("ready status omitted a live desktop: {error}"))?;
    client.close().await;
    println!("quickstart-ok language={language} mode=success");
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match exercise().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("public Rust quick-start failed: {error}");
            ExitCode::FAILURE
        }
    }
}
