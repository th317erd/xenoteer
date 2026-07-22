//! Linux/Unix process-level graceful shutdown verification.

#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn sigterm_drives_graceful_daemon_shutdown() -> Result<(), Box<dyn std::error::Error>> {
    let reservation = TcpListener::bind("127.0.0.1:0")?;
    let address = reservation.local_addr()?;
    drop(reservation);

    let child = Command::new(env!("CARGO_BIN_EXE_xenoteerd"))
        .args(["--listen", &address.to_string(), "--insecure-disable-auth"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard::new(child);

    wait_for_liveness(address, Duration::from_secs(10))?;
    let readiness = http_get(address, "/readyz")?;
    assert!(readiness.starts_with("HTTP/1.1 503"));
    assert!(readiness.contains(r#"{"status":"not_ready"}"#));
    terminate_and_wait(&mut child, Duration::from_secs(10))?;

    let output = child.finish()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "xenoteerd exited unsuccessfully after SIGTERM: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("shutdown signal received"));
    assert!(stdout.contains("\"signal\":\"terminate\""));
    assert!(stdout.contains("xenoteerd stopped"));
    Ok(())
}

#[test]
fn launcher_config_environment_loads_without_entering_typed_environment()
-> Result<(), Box<dyn std::error::Error>> {
    const CONFIG_VALUE_CANARY: &str = "LAUNCHER_CONFIG_VALUE_SECRET_CANARY";
    let reservation = TcpListener::bind("127.0.0.1:0")?;
    let address = reservation.local_addr()?;
    drop(reservation);
    let config_contents = include_str!("../../../xenoteer.example.toml")
        .replace("127.0.0.1:8080", &address.to_string())
        .replace(
            "insecure_disable_auth = false",
            "insecure_disable_auth = true",
        );
    let config = TestConfigFile::new(CONFIG_VALUE_CANARY, &config_contents)?;

    let child = Command::new(env!("CARGO_BIN_EXE_xenoteerd"))
        .env_clear()
        .env("XENOTEER_CONFIG", config.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard::new(child);

    wait_for_liveness(address, Duration::from_secs(10))?;
    terminate_and_wait(&mut child, Duration::from_secs(10))?;
    let output = child.finish()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "xenoteerd launched through XENOTEER_CONFIG unsuccessfully: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stdout.contains("loaded validated configuration"));
    assert!(!stdout.contains(CONFIG_VALUE_CANARY));
    assert!(!stderr.contains(CONFIG_VALUE_CANARY));
    Ok(())
}

#[test]
fn unknown_xenoteer_environment_fails_without_echoing_canaries()
-> Result<(), Box<dyn std::error::Error>> {
    const KEY_CANARY: &str = "XENOTEER_BAD_KEY_SECRET_CANARY";
    const VALUE_CANARY: &str = "UNKNOWN_ENV_VALUE_SECRET_CANARY";
    let child = Command::new(env!("CARGO_BIN_EXE_xenoteerd"))
        .env_clear()
        .env(KEY_CANARY, VALUE_CANARY)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard::new(child);

    wait_for_exit(&mut child, Duration::from_secs(10))?;
    let output = child.finish()?;
    if output.status.success() {
        return Err(std::io::Error::other(
            "xenoteerd accepted an unknown Xenoteer environment key",
        )
        .into());
    }
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("invalid Xenoteer environment configuration key"));
    assert!(!stderr.contains(KEY_CANARY));
    assert!(!stderr.contains(VALUE_CANARY));
    Ok(())
}

fn terminate_and_wait(
    child: &mut ChildGuard,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let pid = child.child_mut()?.id().to_string();
    let kill_status = Command::new("kill").args(["-TERM", &pid]).status()?;
    if !kill_status.success() {
        return Err(std::io::Error::other("kill -TERM command failed").into());
    }
    wait_for_exit(child, timeout)
}

fn wait_for_exit(
    child: &mut ChildGuard,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.child_mut()?.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::other("xenoteerd did not exit before timeout").into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_liveness(
    address: SocketAddr,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(response) = http_get(address, "/livez")
            && response.starts_with("HTTP/1.1 200")
            && response.contains(r#"{"status":"alive"}"#)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::other("xenoteerd /livez did not become available").into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn http_get(address: SocketAddr, path: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(100))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> std::io::Result<&mut Child> {
        self.child
            .as_mut()
            .ok_or_else(|| std::io::Error::other("child process already consumed"))
    }

    fn finish(mut self) -> std::io::Result<Output> {
        let child = self
            .child
            .take()
            .ok_or_else(|| std::io::Error::other("child process already consumed"))?;
        child.wait_with_output()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct TestConfigFile {
    path: PathBuf,
}

impl TestConfigFile {
    fn new(name: &str, contents: &str) -> std::io::Result<Self> {
        let path =
            std::env::temp_dir().join(format!("xenoteerd-{name}-{}.toml", std::process::id()));
        fs::write(&path, contents)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestConfigFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
