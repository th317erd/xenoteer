//! Public Rust SDK connection-configuration contract.

use std::{
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};
use xenoteer_sdk::{
    Client, ClientOptions, ProtocolVersion, ReconnectPolicy, SafeLogEvent, SafeLogHookError,
    SdkError, TlsPolicy, VersionRange, XenoteerClient,
};

type TestError = Box<dyn Error + Send + Sync>;

const TOKEN_ONE: &str = "connection-token-one-0123456789abcdef";
const TOKEN_TWO: &str = "connection-token-two-0123456789abcde";

fn valid_status() -> serde_json::Value {
    serde_json::json!({
        "server_version": "0.1.0",
        "protocol_min": {"major": 1, "minor": 0},
        "protocol_max": {"major": 1, "minor": 0},
        "server_time": "2030-01-01T00:00:00Z",
        "desktop": {
            "id": "20000000-0000-4000-8000-000000000001",
            "generation": "30000000-0000-4000-8000-000000000001",
            "state": "ready",
            "reason_code": null
        },
        "capabilities": {"capabilities": []}
    })
}

async fn two_request_server() -> Result<
    (
        String,
        tokio::task::JoinHandle<Result<Vec<String>, TestError>>,
    ),
    TestError,
> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let mut authorizations = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await?;
            let mut request = vec![0_u8; 4096];
            let read = timeout(Duration::from_secs(2), stream.read(&mut request)).await??;
            let request = String::from_utf8(request[..read].to_vec())?;
            let authorization = request
                .lines()
                .find_map(|line| {
                    line.strip_prefix("authorization: ")
                        .or_else(|| line.strip_prefix("Authorization: "))
                })
                .ok_or("missing authorization header")?
                .to_owned();
            authorizations.push(authorization);
            let body = serde_json::to_vec(&valid_status())?;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.shutdown().await?;
        }
        Ok(authorizations)
    });
    Ok((format!("http://{address}"), task))
}

async fn one_status_server(
    status: serde_json::Value,
) -> Result<(String, tokio::task::JoinHandle<Result<(), TestError>>), TestError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let (mut stream, _) = timeout(Duration::from_secs(2), listener.accept()).await??;
        let mut request = [0_u8; 4096];
        let _read = timeout(Duration::from_secs(2), stream.read(&mut request)).await??;
        let body = serde_json::to_vec(&status)?;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.shutdown().await?;
        Ok(())
    });
    Ok((format!("http://{address}"), task))
}

#[test]
fn builder_validates_every_public_bound_and_redacts_configuration() -> Result<(), TestError> {
    let policy = ReconnectPolicy::new(3, Duration::from_millis(10), Duration::from_millis(40))?;
    let options = ClientOptions::builder("http://127.0.0.1:9", TOKEN_ONE)
        .connect_timeout(Duration::from_secs(2))
        .request_timeout(Duration::from_secs(3))
        .reconnect_policy(policy)
        .client_metadata("connection-options-test", "1.2.3")
        .protocol_range(VersionRange::V1)
        .tls_policy(TlsPolicy::native_roots())
        .build()?;
    let client = Client::from_options(options)?;
    let debug = format!("{client:?}");
    assert!(!debug.contains(TOKEN_ONE));
    assert!(!debug.contains("authorization"));

    for timeout_value in [Duration::ZERO, Duration::from_secs(301)] {
        assert!(matches!(
            ClientOptions::builder("http://127.0.0.1:9", TOKEN_ONE)
                .request_timeout(timeout_value)
                .build(),
            Err(SdkError::InvalidRequest)
        ));
    }
    for timeout_value in [Duration::ZERO, Duration::from_secs(61)] {
        assert!(matches!(
            ClientOptions::builder("http://127.0.0.1:9", TOKEN_ONE)
                .connect_timeout(timeout_value)
                .build(),
            Err(SdkError::InvalidRequest)
        ));
    }
    for (attempts, initial, maximum) in [
        (0, Duration::from_millis(1), Duration::from_millis(1)),
        (101, Duration::from_millis(1), Duration::from_millis(1)),
        (1, Duration::ZERO, Duration::from_millis(1)),
        (1, Duration::from_millis(2), Duration::from_millis(1)),
        (1, Duration::from_millis(1), Duration::from_secs(61)),
    ] {
        assert!(matches!(
            ReconnectPolicy::new(attempts, initial, maximum),
            Err(SdkError::InvalidRequest)
        ));
    }
    for (name, version) in [
        ("", "1"),
        ("client", ""),
        ("\n", "1"),
        ("client", "\u{0000}"),
        (&"a".repeat(129), "1"),
        ("client", &"1".repeat(129)),
    ] {
        assert!(matches!(
            ClientOptions::builder("http://127.0.0.1:9", TOKEN_ONE)
                .client_metadata(name, version)
                .build(),
            Err(SdkError::InvalidRequest)
        ));
    }
    Ok(())
}

#[test]
fn crate_features_are_rustls_only_and_native_roots_are_optional() -> Result<(), TestError> {
    let manifest = std::fs::read_to_string(format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")))?;
    for required in [
        "default = [\"rustls-tls\", \"native-roots\"]",
        "rustls-tls = [",
        "native-roots = [\"rustls-tls\", \"dep:rustls-native-certs\"]",
    ] {
        assert!(manifest.contains(required));
    }
    assert!(!manifest.contains("openssl"));
    assert!(!manifest.contains("native-tls"));
    Ok(())
}

#[tokio::test]
async fn async_token_provider_rotates_without_caching_and_redacts_failures() -> Result<(), TestError>
{
    let (base, server) = two_request_server().await?;
    let invocation = Arc::new(AtomicUsize::new(0));
    let provider_invocation = invocation.clone();
    let options = ClientOptions::builder_with_token_provider(base, move || {
        let index = provider_invocation.fetch_add(1, Ordering::SeqCst);
        async move {
            Ok::<_, &'static str>(if index == 0 {
                TOKEN_ONE.to_owned()
            } else {
                TOKEN_TWO.to_owned()
            })
        }
    })
    .build()?;
    let client = Client::from_options(options)?;
    client.status().await?;
    client.status().await?;
    assert_eq!(invocation.load(Ordering::SeqCst), 2);
    assert_eq!(
        timeout(Duration::from_secs(2), server).await???,
        [format!("Bearer {TOKEN_ONE}"), format!("Bearer {TOKEN_TWO}")]
    );

    let secret_error = "provider-secret-error-should-never-escape";
    let options =
        ClientOptions::builder_with_token_provider("http://127.0.0.1:9", move || async move {
            Err::<String, _>(secret_error)
        })
        .build()?;
    let Err(error) = Client::from_options(options)?.status().await else {
        return Err("failing token provider unexpectedly succeeded".into());
    };
    assert!(matches!(error, SdkError::TokenProvider));
    assert!(!error.to_string().contains(secret_error));
    assert!(!format!("{error:?}").contains(secret_error));

    let invalid = ClientOptions::builder_with_token_provider("http://127.0.0.1:9", || async {
        Ok::<_, ()>("short".to_owned())
    })
    .build()?;
    assert!(matches!(
        Client::from_options(invalid)?.status().await,
        Err(SdkError::InvalidBearerToken)
    ));

    let hanging = ClientOptions::builder_with_token_provider("http://127.0.0.1:9", || async {
        std::future::pending::<Result<String, ()>>().await
    })
    .request_timeout(Duration::from_millis(25))
    .build()?;
    let started = std::time::Instant::now();
    assert!(matches!(
        Client::from_options(hanging)?.status().await,
        Err(SdkError::RequestTimeout)
    ));
    assert!(started.elapsed() < Duration::from_millis(500));
    Ok(())
}

#[tokio::test]
#[allow(clippy::panic)]
async fn safe_log_hook_failure_and_panic_cannot_change_transport_outcome() -> Result<(), TestError>
{
    let (base, server) = two_request_server().await?;
    let events = Arc::new(Mutex::new(Vec::<SafeLogEvent>::new()));
    let captured = events.clone();
    let options = ClientOptions::builder(base, TOKEN_ONE)
        .safe_log(move |event| {
            if let Ok(mut events) = captured.lock() {
                events.push(event);
            }
            Err(SafeLogHookError)
        })
        .build()?;
    let client = Client::from_options(options)?;
    client.status().await?;

    let panic_options = ClientOptions::builder("http://127.0.0.1:9", TOKEN_ONE)
        .safe_log(|_| -> Result<(), SafeLogHookError> { panic!("log canary") })
        .build()?;
    let panic_client = Client::from_options(panic_options)?;
    if panic_client.status().await.is_ok() {
        return Err("unreachable direct-origin request unexpectedly succeeded".into());
    }

    client.status().await?;
    timeout(Duration::from_secs(2), server).await???;
    let events = events
        .lock()
        .map_err(|_| -> TestError { "event log lock was poisoned".into() })?;
    assert!(events.len() >= 4);
    let rendered = format!("{events:?}");
    assert!(!rendered.contains(TOKEN_ONE));
    assert!(!rendered.contains("/v1/status"));
    Ok(())
}

#[tokio::test]
#[allow(clippy::panic)]
async fn panic_hook_contract_subprocess_helper() -> Result<(), TestError> {
    let Ok(mode) = std::env::var("XENOTEER_PANIC_CONTRACT_MODE") else {
        return Ok(());
    };
    match mode.as_str() {
        "provider" => {
            let options =
                ClientOptions::builder_with_token_provider("http://127.0.0.1:9", || async move {
                    panic!("provider-panic-hook-canary");
                    #[allow(unreachable_code)]
                    Ok::<String, ()>(TOKEN_ONE.to_owned())
                })
                .build()?;
            assert!(matches!(
                Client::from_options(options)?.status().await,
                Err(SdkError::TokenProvider)
            ));
        }
        "hook" => {
            let options = ClientOptions::builder("http://127.0.0.1:9", TOKEN_ONE)
                .safe_log(|_| -> Result<(), SafeLogHookError> {
                    panic!("safe-log-panic-hook-canary")
                })
                .request_timeout(Duration::from_millis(20))
                .build()?;
            assert!(Client::from_options(options)?.status().await.is_err());
        }
        _ => return Err("unknown panic-contract subprocess mode".into()),
    }
    Ok(())
}

#[test]
fn panic_payload_erasure_and_panic_hook_output_are_explicitly_separate_contracts()
-> Result<(), TestError> {
    for (mode, canary) in [
        ("provider", "provider-panic-hook-canary"),
        ("hook", "safe-log-panic-hook-canary"),
    ] {
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "panic_hook_contract_subprocess_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("XENOTEER_PANIC_CONTRACT_MODE", mode)
            .output()?;
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(canary),
            "{mode}: the installed panic hook did not retain responsibility for output"
        );
    }

    let readme = std::fs::read_to_string(format!("{}/README.md", env!("CARGO_MANIFEST_DIR")))?;
    for required in [
        "errors and panic payloads are erased from SDK errors and safe logs",
        "panic-hook output remains the caller/runtime's responsibility",
        "must never place secrets in panic payloads",
    ] {
        assert!(
            readme.contains(required),
            "missing README contract: {required}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn configured_protocol_range_drives_negotiation() -> Result<(), TestError> {
    let mut status = valid_status();
    status["protocol_max"] = serde_json::json!({"major": 1, "minor": 2});
    let (base, server) = one_status_server(status).await?;
    let options = ClientOptions::builder(base, TOKEN_ONE)
        .protocol_range(VersionRange::new(1, 1, 1)?)
        .build()?;
    let client = XenoteerClient::from_transport(Client::from_options(options)?).await?;
    assert_eq!(client.negotiated_protocol(), ProtocolVersion::new(1, 1));
    timeout(Duration::from_secs(2), server).await???;

    assert!(matches!(
        ClientOptions::builder("http://127.0.0.1:9", TOKEN_ONE)
            .protocol_range(VersionRange::new(2, 0, 0)?)
            .build(),
        Err(SdkError::InvalidRequest)
    ));
    Ok(())
}
