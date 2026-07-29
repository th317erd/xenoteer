//! Process boundary for the language-neutral Rust SDK adapter.

use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

fn run_adapter_output(value: &serde_json::Value) -> Result<Output, Box<dyn std::error::Error>> {
    let mut adapter = Command::new(env!("CARGO_BIN_EXE_xenoteer-sdk-conformance"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    serde_json::to_writer(
        adapter.stdin.as_mut().ok_or("adapter stdin missing")?,
        value,
    )?;
    adapter
        .stdin
        .as_mut()
        .ok_or("adapter stdin missing")?
        .write_all(b"\n")?;
    drop(adapter.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    while adapter.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            adapter.kill()?;
            return Err("adapter exceeded ten-second test bound".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(adapter.wait_with_output()?)
}

fn run_adapter(value: &serde_json::Value) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let output = run_adapter_output(value)?;
    assert!(output.status.success());
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn official_runner_passes_every_v1_case_without_skips() -> Result<(), Box<dyn std::error::Error>> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(repository.join("scripts/conformance/run.py"))
        .arg("--root")
        .arg(repository.join("conformance/v1"))
        .arg("--timeout-seconds")
        .arg("10")
        .arg("--adapter")
        .arg(env!("CARGO_BIN_EXE_xenoteer-sdk-conformance"))
        .output()?;
    assert!(
        output.status.success(),
        "runner failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout)?.trim(),
        "adapter passed 73 Xenoteer v1 conformance cases"
    );
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn mutated_command_fixture_fails_against_concrete_envelope_outcome()
-> Result<(), Box<dyn std::error::Error>> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let payload = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(repository.join("scripts/conformance/run.py"))
        .arg("--root")
        .arg(repository.join("conformance/v1"))
        .arg("--operation")
        .arg("command_reconnect")
        .arg("--emit-payload")
        .output()?;
    assert!(payload.status.success());
    let mut value: serde_json::Value = serde_json::from_slice(&payload.stdout)?;
    let case = value["cases"]
        .as_array_mut()
        .and_then(|cases| cases.first_mut())
        .ok_or("missing reconnect case")?;
    case["input"]["command"] = serde_json::json!({
        "type": "application_launch",
        "application": "mutated",
        "arguments": []
    });

    let response = run_adapter(&value)?;
    let result = &response["results"][0];
    assert_eq!(result["id"], "command.reconnect.after-acceptance");
    assert_eq!(result["status"], "failed");
    assert!(
        result["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("submission_envelopes")),
        "unexpected mutation result: {result}"
    );
    Ok(())
}

#[test]
fn mutated_event_frame_fails_against_production_event_ingestion()
-> Result<(), Box<dyn std::error::Error>> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let payload = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(repository.join("scripts/conformance/run.py"))
        .arg("--root")
        .arg(repository.join("conformance/v1"))
        .arg("--operation")
        .arg("event_continuity")
        .arg("--emit-payload")
        .output()?;
    assert!(payload.status.success());
    let mut value: serde_json::Value = serde_json::from_slice(&payload.stdout)?;
    let case = value["cases"]
        .as_array_mut()
        .and_then(|cases| {
            cases
                .iter_mut()
                .find(|case| case["id"] == "event.filtered-sequence-jump")
        })
        .ok_or("missing filtered sequence event case")?;
    case["input"]["frames"][0]["event"]["sequence"] = serde_json::json!("11");
    let response = run_adapter(&value)?;
    let result = response["results"]
        .as_array()
        .and_then(|results| {
            results
                .iter()
                .find(|result| result["id"] == "event.filtered-sequence-jump")
        })
        .ok_or("missing filtered sequence event result")?;
    assert_eq!(result["status"], "failed");
    assert!(
        result["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("delivered_sequences")),
        "unexpected event mutation result: {}",
        result
    );
    Ok(())
}

#[test]
fn adapter_rejects_replaced_corpus_identity_and_protocol() -> Result<(), Box<dyn std::error::Error>>
{
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let payload = Command::new("python3")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg(repository.join("scripts/conformance/run.py"))
        .arg("--root")
        .arg(repository.join("conformance/v1"))
        .arg("--operation")
        .arg("decode_uint64_string")
        .arg("--emit-payload")
        .output()?;
    assert!(payload.status.success());
    let value: serde_json::Value = serde_json::from_slice(&payload.stdout)?;

    let mut replaced_corpus = value.clone();
    replaced_corpus["corpus"] = serde_json::json!("xenoteer-conformance-v2");
    let output = run_adapter_output(&replaced_corpus)?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid conformance"));

    let mut replaced_hash = value.clone();
    replaced_hash["corpus_sha256"] =
        serde_json::json!("0000000000000000000000000000000000000000000000000000000000000000");
    let output = run_adapter_output(&replaced_hash)?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid conformance"));

    let mut replaced_protocol = value;
    replaced_protocol["protocol"] = serde_json::json!({"major": 1, "min_minor": 0, "max_minor": 1});
    let output = run_adapter_output(&replaced_protocol)?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid conformance"));
    Ok(())
}
