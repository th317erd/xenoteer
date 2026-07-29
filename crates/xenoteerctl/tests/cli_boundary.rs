//! Process-level checks for the scriptable command-line boundary.

use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn xenoteerctl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_xenoteerctl"))
}

fn run_bounded(mut command: Command) -> Result<Output, Box<dyn std::error::Error>> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            child.kill()?;
            return Err("xenoteerctl exceeded ten-second process-test bound".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(child.wait_with_output()?)
}

fn accept_bounded(listener: &TcpListener) -> std::io::Result<(TcpStream, SocketAddr)> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, address)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                return Ok((stream, address));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "fixture accept exceeded ten seconds",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Ok(request)
}

fn write_http_response(
    stream: &mut TcpStream,
    content_type: &str,
    extra_headers: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

#[test]
fn version_identifies_the_exact_binary() -> Result<(), Box<dyn std::error::Error>> {
    let output = xenoteerctl().arg("--version").output()?;
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)?.starts_with("xenoteerctl 0.1.0"));
    Ok(())
}

#[test]
fn online_commands_require_an_explicit_safe_token_source() -> Result<(), Box<dyn std::error::Error>>
{
    for command in ["status", "doctor"] {
        let output = xenoteerctl()
            .env_remove("XENOTEER_TOKEN")
            .env_remove("XENOTEER_TOKEN_FILE")
            .arg(command)
            .output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("token environment variable XENOTEER_TOKEN is unset"));
    }
    Ok(())
}

#[test]
fn transport_failures_never_print_the_bearer() -> Result<(), Box<dyn std::error::Error>> {
    let secret = "cli-secret-canary-0123456789abcdef";
    let output = xenoteerctl()
        .env("XENOTEER_TOKEN", secret)
        .arg("--base-url")
        .arg("http://127.0.0.1:9")
        .arg("status")
        .output()?;
    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("transport failed"));
    assert!(!stderr.contains(secret));
    Ok(())
}

#[test]
fn help_exposes_the_phase_six_command_families() -> Result<(), Box<dyn std::error::Error>> {
    let output = xenoteerctl().arg("--help").output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    for command in [
        "status",
        "capabilities",
        "doctor",
        "lease",
        "command",
        "mouse",
        "keyboard",
        "windows",
        "elements",
        "clipboard",
        "app",
        "screenshot",
        "events",
        "artifact",
        "viewer-ticket",
    ] {
        assert!(stdout.contains(command), "missing command family {command}");
    }
    let screenshot = xenoteerctl().args(["screenshot", "--help"]).output()?;
    assert!(screenshot.status.success());
    assert!(String::from_utf8(screenshot.stdout)?.contains("--output"));
    assert!(stdout.contains("--json"));
    Ok(())
}

#[test]
fn help_exposes_the_complete_planned_command_names() -> Result<(), Box<dyn std::error::Error>> {
    for (family, commands) in [
        (
            "mouse",
            &["move", "click", "drag", "scroll", "position", "reset"][..],
        ),
        ("key", &["press", "down", "up", "chord", "text", "reset"]),
        ("clipboard", &["get", "set", "clear", "paste"]),
        (
            "window",
            &[
                "list", "find", "show", "activate", "close", "move", "resize", "state", "capture",
                "wait",
            ],
        ),
        (
            "element",
            &[
                "query", "show", "invoke", "click", "focus", "text", "value", "wait",
            ],
        ),
        ("app", &["list", "launch", "terminate", "logs"]),
        ("events", &["watch"]),
        ("viewer", &["url"]),
        ("command", &["show", "wait", "cancel"]),
        ("lease", &["acquire", "renew", "release", "show"]),
    ] {
        let output = xenoteerctl().args([family, "--help"]).output()?;
        assert!(output.status.success(), "{family} help failed");
        let help = String::from_utf8(output.stdout)?;
        for command in commands {
            assert!(
                help.contains(command),
                "{family} help is missing subcommand {command}"
            );
        }
    }
    let command_help = xenoteerctl()
        .args(["command", "submit", "--help"])
        .output()?;
    let command_help = String::from_utf8(command_help.stdout)?;
    for option in ["--command-id", "--lease-id", "--with-lease"] {
        assert!(command_help.contains(option), "missing {option}");
    }
    for command in ["launch", "status", "terminate"] {
        let output = xenoteerctl().args(["app", command, "--help"]).output()?;
        assert!(output.status.success(), "app {command} help failed");
        let help = String::from_utf8(output.stdout)?;
        assert!(
            help.contains("--command-id"),
            "app {command} is missing --command-id"
        );
    }
    Ok(())
}

#[test]
fn doctor_false_is_json_on_stdout_and_a_nonzero_exit() -> Result<(), Box<dyn std::error::Error>> {
    let mut status: serde_json::Value = serde_json::from_str(include_str!(
        "../../../docs/api/v1/examples/status-response.json"
    ))?;
    status["desktop"]["state"] = serde_json::json!("failed");
    status["desktop"]["reason_code"] = serde_json::json!("fixture_failed");
    let body = serde_json::to_vec(&status)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = thread::spawn(move || -> std::io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes())?;
        stream.write_all(&body)
    });
    let output = xenoteerctl()
        .env("XENOTEER_TOKEN", "doctor-test-token-0123456789abcdef")
        .args(["--base-url", &base, "--json", "doctor"])
        .output()?;
    assert_eq!(output.status.code(), Some(7));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["ok"], false);
    assert!(String::from_utf8(output.stderr)?.contains("doctor checks failed"));
    server
        .join()
        .map_err(|_| "doctor server thread panicked")??;
    Ok(())
}

#[test]
fn doctor_browser_requires_a_browser_specific_capability() -> Result<(), Box<dyn std::error::Error>>
{
    let body = include_bytes!("../../../docs/api/v1/examples/status-response.json").to_vec();
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = thread::spawn(move || -> std::io::Result<()> {
        let (mut stream, _) = accept_bounded(&listener)?;
        read_http_request(&mut stream)?;
        write_http_response(&mut stream, "application/json", "", &body)
    });
    let mut command = xenoteerctl();
    command
        .env("XENOTEER_TOKEN", "doctor-browser-token-0123456789abcdef")
        .args(["--base-url", &base, "--json", "doctor", "--browser"]);
    let output = run_bounded(command)?;
    assert_eq!(output.status.code(), Some(7));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["ok"], false);
    assert_eq!(report["browser_registered_available"], false);
    server
        .join()
        .map_err(|_| "doctor browser server thread panicked")??;
    Ok(())
}

#[test]
fn doctor_viewer_issues_a_real_origin_bound_ticket_probe() -> Result<(), Box<dyn std::error::Error>>
{
    const DESKTOP_ID: &str = "018f3e58-78c0-7d8e-a701-3a6ca29a0001";
    const GENERATION: &str = "018f3e58-78c0-7d8e-a701-3a6ca29a0002";
    const ORIGIN: &str = "https://viewer.example";
    let status = include_bytes!("../../../docs/api/v1/examples/status-response.json").to_vec();
    let ticket = serde_json::to_vec(&serde_json::json!({
        "ticket": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "principal_id": "automation:doctor",
        "audience": "viewer_websocket",
        "desktop_id": DESKTOP_ID,
        "desktop_generation": GENERATION,
        "origin": ORIGIN,
        "mode": "view_only",
        "issued_at": "2030-01-01T00:00:00Z",
        "expires_at": "2030-01-01T00:00:30Z",
        "use_policy": "single_use"
    }))?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = thread::spawn(move || -> std::io::Result<()> {
        let (mut status_stream, _) = accept_bounded(&listener)?;
        read_http_request(&mut status_stream)?;
        write_http_response(&mut status_stream, "application/json", "", &status)?;
        let (mut ticket_stream, _) = accept_bounded(&listener)?;
        let request = String::from_utf8(read_http_request(&mut ticket_stream)?)
            .map_err(std::io::Error::other)?;
        if !request.starts_with(&format!("POST /v1/desktops/{DESKTOP_ID}/viewer-tickets "))
            || !request
                .to_ascii_lowercase()
                .contains(&format!("origin: {ORIGIN}"))
        {
            return Err(std::io::Error::other("viewer ticket probe request differs"));
        }
        write_http_response(&mut ticket_stream, "application/json", "", &ticket)
    });
    let mut command = xenoteerctl();
    command
        .env("XENOTEER_TOKEN", "doctor-viewer-token-0123456789abcdef")
        .args([
            "--base-url",
            &base,
            "--json",
            "doctor",
            "--viewer",
            "--viewer-origin",
            ORIGIN,
        ]);
    let output = run_bounded(command)?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["ok"], true);
    assert_eq!(report["viewer_ticket_issued"], true);
    server
        .join()
        .map_err(|_| "doctor viewer server thread panicked")??;
    Ok(())
}

#[test]
fn doctor_help_describes_viewer_ticket_issuance_not_gateway_health()
-> Result<(), Box<dyn std::error::Error>> {
    let output = xenoteerctl().args(["doctor", "--help"]).output()?;
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout)?;
    assert!(help.contains("one-time viewer ticket issuance"));
    assert!(!help.contains("gateway capability"));
    Ok(())
}

#[test]
fn caller_command_id_reaches_the_http_boundary_exactly() -> Result<(), Box<dyn std::error::Error>> {
    const COMMAND_ID: &str = "40000000-0000-4000-8000-000000000777";
    let status = include_bytes!("../../../docs/api/v1/examples/status-response.json").to_vec();
    let result = serde_json::to_vec(&serde_json::json!({
        "command_id": COMMAND_ID,
        "lifecycle": "accepted",
        "effect_stage": "accepted",
        "accepted_at": "2030-01-01T00:00:00Z",
        "started_at": null,
        "finished_at": null,
        "outcome": null,
        "error": null,
        "warnings": []
    }))?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let (mut status_stream, _) = accept_bounded(&listener)?;
        let status_request = read_http_request(&mut status_stream)?;
        if !String::from_utf8_lossy(&status_request).starts_with("GET /v1/status ") {
            return Err(std::io::Error::other("status request missing"));
        }
        write_http_response(&mut status_stream, "application/json", "", &status)?;

        let (mut command_stream, _) = accept_bounded(&listener)?;
        let command_request = read_http_request(&mut command_stream)?;
        write_http_response(&mut command_stream, "application/json", "", &result)?;
        Ok(command_request)
    });
    let mut command = xenoteerctl();
    command
        .env("XENOTEER_TOKEN", "command-id-test-token-0123456789abcdef")
        .args([
            "--base-url",
            &base,
            "--json",
            "app",
            "launch",
            "--profile",
            "conformance",
            "--command-id",
            COMMAND_ID,
        ]);
    let output = run_bounded(command)?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains(&format!("xenoteerctl: command_id={COMMAND_ID}"))
    );
    let request = String::from_utf8(
        server
            .join()
            .map_err(|_| "command ID server thread panicked")??,
    )?;
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("idempotency-key: {COMMAND_ID}"))
    );
    let body: serde_json::Value = serde_json::from_str(
        request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .ok_or("command body missing")?,
    )?;
    assert_eq!(body["command_id"], COMMAND_ID);
    assert_eq!(body["command"]["type"], "application_launch");
    assert_eq!(body["command"]["application"], "conformance");
    Ok(())
}

#[cfg(unix)]
#[test]
fn token_file_symlinks_are_rejected_before_network_io() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("xenoteerctl-token-{}-{unique}", std::process::id()));
    fs::create_dir(&directory)?;
    let target = directory.join("token");
    let link = directory.join("token-link");
    let secret = "symlink-secret-canary-0123456789abcdef";
    fs::write(&target, secret)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    symlink(&target, &link)?;

    let output = xenoteerctl()
        .arg("--token-file")
        .arg(&link)
        .arg("status")
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8(output.stderr)?.contains(secret));

    fs::remove_file(&link)?;
    fs::remove_file(&target)?;
    fs::remove_dir(&directory)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn token_files_owned_by_another_user_are_rejected_before_network_io()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::{PermissionsExt, chown};

    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "xenoteerctl-token-owner-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&directory)?;
    let token = directory.join("token");
    let secret = "wrong-owner-secret-canary-0123456789abcdef";
    fs::write(&token, secret)?;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600))?;
    let euid = nix::unistd::geteuid().as_raw();
    let other_uid = if euid == 1 { 2 } else { 1 };
    if let Err(error) = chown(&token, Some(other_uid), None) {
        if euid == 0 {
            return Err(format!(
                "root fixture setup must deterministically chown the token: {error}"
            )
            .into());
        }
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            eprintln!("platform coverage: euid {euid} cannot create a wrong-owner fixture");
            fs::remove_file(&token)?;
            fs::remove_dir(&directory)?;
            return Ok(());
        }
        return Err(error.into());
    }

    let output = xenoteerctl()
        .arg("--token-file")
        .arg(&token)
        .arg("status")
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("must be owned by the effective user"));
    assert!(!stderr.contains(secret));

    fs::remove_file(&token)?;
    fs::remove_dir(&directory)?;
    Ok(())
}

#[test]
fn screenshot_dash_writes_only_verified_binary_to_stdout() -> Result<(), Box<dyn std::error::Error>>
{
    const BODY: &[u8] = b"\x89PNG\r\n\x1a\nfixture-binary\0\xff";
    const DIGEST: &str = "66ae8eb36c55705536318c686e219bce0facd7006e3b94c03643a6fee1eb0f79";
    const DESKTOP_ID: &str = "018f3e58-78c0-7d8e-a701-3a6ca29a0001";
    const GENERATION: &str = "018f3e58-78c0-7d8e-a701-3a6ca29a0002";
    const ARTIFACT_ID: &str = "018f3e58-78c0-7d8e-a701-3a6ca29a0099";

    let status = include_bytes!("../../../docs/api/v1/examples/status-response.json").to_vec();
    let screenshot = serde_json::to_vec(&serde_json::json!({
        "target": {"kind": "root"},
        "source_region": {
            "coordinate_space": "root_physical",
            "rect": {"x": 0, "y": 0, "width": 1, "height": 1}
        },
        "source_size": {"width": 1, "height": 1},
        "limitation": "root_visible_framebuffer",
        "format": "png",
        "size": {"width": 1, "height": 1},
        "raw": null,
        "cursor": {
            "requested": false,
            "composited": false,
            "serial_before": null,
            "serial_after": null,
            "moved_during_capture": false
        },
        "sha256": DIGEST,
        "delivery": {
            "delivery": "artifact",
            "artifact": {
                "artifact_id": ARTIFACT_ID,
                "purpose": "screenshot",
                "desktop_id": DESKTOP_ID,
                "desktop_generation": GENERATION,
                "content_type": "image/png",
                "content_length": BODY.len(),
                "sha256": DIGEST,
                "created_at": "2030-01-01T00:00:00Z",
                "expires_at": "2030-01-01T00:05:00Z"
            }
        }
    }))?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = thread::spawn(move || -> std::io::Result<()> {
        for (expected_path, content_type, headers, body) in [
            ("/v1/status", "application/json", "", status.as_slice()),
            (
                "/screenshots",
                "application/json",
                "",
                screenshot.as_slice(),
            ),
            (
                "/v1/artifacts/",
                "image/png",
                concat!(
                    "X-Content-Sha256: ",
                    "66ae8eb36c55705536318c686e219bce0facd7006e3b94c03643a6fee1eb0f79",
                    "\r\n"
                ),
                BODY,
            ),
        ] {
            let (mut stream, _) = listener.accept()?;
            let request = read_http_request(&mut stream)?;
            let request_line_end = request
                .windows(2)
                .position(|window| window == b"\r\n")
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
            let request_line = String::from_utf8_lossy(&request[..request_line_end]);
            if !request_line.contains(expected_path) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unexpected request line: {request_line}"),
                ));
            }
            write_http_response(&mut stream, content_type, headers, body)?;
        }
        Ok(())
    });

    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let request_path = std::env::temp_dir().join(format!(
        "xenoteerctl-screenshot-{}-{unique}.json",
        std::process::id()
    ));
    fs::write(
        &request_path,
        br#"{"target":{"kind":"root"},"region":null,"format":"png","include_cursor":false,"scale":null,"max_bytes":1024}"#,
    )?;
    let output = xenoteerctl()
        .env("XENOTEER_TOKEN", "screenshot-test-token-0123456789abcdef")
        .args(["--base-url", &base, "screenshot", "--input"])
        .arg(&request_path)
        .args(["--output", "-"])
        .output()?;
    fs::remove_file(&request_path)?;
    server
        .join()
        .map_err(|_| "screenshot server thread panicked")??;

    assert!(
        output.status.success(),
        "{:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, BODY);
    assert!(output.stderr.is_empty());
    Ok(())
}
