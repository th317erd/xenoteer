//! Local deterministic TLS policy integration tests.

use std::{
    env,
    error::Error,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use rustls::{
    RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer},
    server::WebPkiClientVerifier,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};
use xenoteer_sdk::{
    Client, ClientHello, ClientOptions, ConnectionId, EventResumeStatus, EventStreamCloseReason,
    EventStreamItem, EventTopic, ReconnectPolicy, SafeLogEvent, SafeLogOperation, SafeLogOutcome,
    SafeLogTransport, SdkError, TlsPolicy, WebSocketClientMessage, WebSocketServerMessage,
    WelcomeDesktop, WelcomeDesktopState, WelcomeLimits, WelcomePrincipal, WelcomeResume,
    XenoteerClient,
};

type TestError = Box<dyn Error + Send + Sync>;

const TOKEN: &str = "tls-options-token-0123456789abcdef";

fn fixture(encoded: &str) -> Result<Vec<u8>, TestError> {
    Ok(STANDARD.decode(encoded.split_whitespace().collect::<String>())?)
}

fn ca_certificate() -> Result<Vec<u8>, TestError> {
    fixture(include_str!("fixtures/tls/ca.cert.der.b64"))
}

fn server_certificate() -> Result<Vec<u8>, TestError> {
    fixture(include_str!("fixtures/tls/server.cert.der.b64"))
}

fn server_key() -> Result<Vec<u8>, TestError> {
    fixture(include_str!("fixtures/tls/server.key.der.b64"))
}

fn client_certificate() -> Result<Vec<u8>, TestError> {
    fixture(include_str!("fixtures/tls/client.cert.der.b64"))
}

fn client_key() -> Result<Vec<u8>, TestError> {
    fixture(include_str!("fixtures/tls/client.key.der.b64"))
}

fn tls_acceptor(require_client_identity: bool) -> Result<TlsAcceptor, TestError> {
    let certificates = vec![CertificateDer::from(server_certificate()?)];
    let private_key = PrivateKeyDer::try_from(server_key()?)?;
    let configuration = if require_client_identity {
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(ca_certificate()?))?;
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
        ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certificates, private_key)?
    } else {
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, private_key)?
    };
    Ok(TlsAcceptor::from(Arc::new(configuration)))
}

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

async fn https_status_server(
    require_client_identity: bool,
) -> Result<(String, JoinHandle<Result<bool, TestError>>), TestError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let acceptor = tls_acceptor(require_client_identity)?;
    let task = tokio::spawn(async move {
        let (stream, _) = timeout(Duration::from_secs(2), listener.accept()).await??;
        let Ok(mut stream) = timeout(Duration::from_secs(2), acceptor.accept(stream)).await? else {
            return Ok(false);
        };
        let mut request = [0_u8; 4096];
        let read = timeout(Duration::from_secs(2), stream.read(&mut request)).await??;
        let request = std::str::from_utf8(&request[..read])?;
        if !request.starts_with("GET /v1/status HTTP/1.1\r\n") {
            return Err("unexpected HTTPS request target".into());
        }
        let body = serde_json::to_vec(&valid_status())?;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.shutdown().await?;
        Ok(true)
    });
    Ok((format!("https://{address}"), task))
}

fn welcome() -> Result<WebSocketServerMessage, TestError> {
    Ok(WebSocketServerMessage::Welcome {
        protocol: xenoteer_sdk::ProtocolVersion::V1_0,
        connection_id: ConnectionId::new(),
        principal: WelcomePrincipal {
            id: "tls-options-test".to_owned(),
            capabilities: vec!["desktop:observe".to_owned()],
        },
        desktop: WelcomeDesktop {
            id: serde_json::from_value(serde_json::json!("20000000-0000-4000-8000-000000000001"))?,
            generation: Some(serde_json::from_value(serde_json::json!(
                "30000000-0000-4000-8000-000000000001"
            ))?),
            state: WelcomeDesktopState::Ready,
        },
        limits: WelcomeLimits {
            max_message_bytes: 1_048_576,
            heartbeat_ms: 1_000,
            normal_outbound_capacity: 64,
            reserved_outbound_capacity: 8,
            max_command_watches: 8,
        },
        resume: WelcomeResume {
            status: EventResumeStatus::NotRequested,
        },
    })
}

#[allow(clippy::result_large_err)] // tungstenite's handshake callback error contains a response.
async fn https_then_reconnecting_wss_server() -> Result<
    (
        String,
        JoinHandle<Result<(Vec<String>, Vec<(String, String)>), TestError>>,
    ),
    TestError,
> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let acceptor = tls_acceptor(true)?;
    let task = tokio::spawn(async move {
        let (stream, _) = timeout(Duration::from_secs(2), listener.accept()).await??;
        let mut stream = timeout(Duration::from_secs(2), acceptor.accept(stream)).await??;
        let mut request = [0_u8; 4096];
        let _read = timeout(Duration::from_secs(2), stream.read(&mut request)).await??;
        let body = serde_json::to_vec(&valid_status())?;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.shutdown().await?;

        let mut authorizations = Vec::new();
        let mut metadata = Vec::new();
        for connection_index in 0..2 {
            let (stream, _) = timeout(Duration::from_secs(2), listener.accept()).await??;
            let stream = timeout(Duration::from_secs(2), acceptor.accept(stream)).await??;
            let captured = Arc::new(Mutex::new(None::<String>));
            let callback_capture = captured.clone();
            let mut socket =
                accept_hdr_async(stream, move |request: &Request, response: Response| {
                    let value = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    if let Ok(mut target) = callback_capture.lock() {
                        *target = value;
                    }
                    Ok(response)
                })
                .await?;
            authorizations.push(
                captured
                    .lock()
                    .map_err(|_| "authorization capture poisoned")?
                    .clone()
                    .ok_or("WSS authorization missing")?,
            );

            let Message::Text(hello) = timeout(Duration::from_secs(2), socket.next())
                .await?
                .ok_or("WSS hello missing")??
            else {
                return Err("WSS hello was not text".into());
            };
            let hello: ClientHello = serde_json::from_str(hello.as_ref())?;
            metadata.push((hello.client.name, hello.client.version));
            socket
                .send(Message::Text(serde_json::to_string(&welcome()?)?.into()))
                .await?;
            let Message::Text(subscription) = timeout(Duration::from_secs(2), socket.next())
                .await?
                .ok_or("WSS subscription missing")??
            else {
                return Err("WSS subscription was not text".into());
            };
            let subscription: WebSocketClientMessage = serde_json::from_str(subscription.as_ref())?;
            let WebSocketClientMessage::EventsSubscribe {
                request_id, topics, ..
            } = subscription
            else {
                return Err("unexpected WSS subscription message".into());
            };
            socket
                .send(Message::Text(
                    serde_json::to_string(&WebSocketServerMessage::EventsSubscribed {
                        request_id,
                        topics,
                    })?
                    .into(),
                ))
                .await?;
            let code = if connection_index == 0 {
                CloseCode::Restart
            } else {
                CloseCode::Normal
            };
            socket
                .send(Message::Close(Some(CloseFrame {
                    code,
                    reason: "bounded fixture".into(),
                })))
                .await?;
        }
        Ok((authorizations, metadata))
    });
    Ok((format!("https://{address}"), task))
}

#[test]
fn malformed_roots_keys_and_mismatched_identity_fail_closed() -> Result<(), TestError> {
    for policy in [
        TlsPolicy::custom_roots().with_root_certificate_der(Vec::new()),
        TlsPolicy::custom_roots().with_root_certificate_der([1_u8, 2, 3]),
        TlsPolicy::custom_roots()
            .with_root_certificate_der(ca_certificate()?)
            .with_client_identity_der(Vec::new(), client_key()?),
        TlsPolicy::custom_roots()
            .with_root_certificate_der(ca_certificate()?)
            .with_client_identity_der(vec![client_certificate()?], [1_u8, 2, 3]),
        TlsPolicy::custom_roots()
            .with_root_certificate_der(ca_certificate()?)
            .with_client_identity_der(vec![client_certificate()?], server_key()?),
    ] {
        let options = ClientOptions::builder("https://127.0.0.1:9", TOKEN)
            .tls_policy(policy)
            .build();
        assert!(matches!(options, Err(SdkError::TlsConfiguration)));
    }
    Ok(())
}

#[tokio::test]
async fn custom_ca_and_mtls_are_required_and_accepted_for_https() -> Result<(), TestError> {
    #[cfg(feature = "native-roots")]
    {
        let (default_base, default_server) = https_status_server(false).await?;
        assert!(matches!(
            Client::new(default_base, TOKEN)?.status().await,
            Err(SdkError::Transport)
        ));
        assert!(!timeout(Duration::from_secs(2), default_server).await???);
    }

    let (mtls_base, mtls_server) = https_status_server(true).await?;
    let policy = TlsPolicy::custom_roots()
        .with_root_certificate_der(ca_certificate()?)
        .with_client_identity_der(vec![client_certificate()?], client_key()?);
    let options = ClientOptions::builder(mtls_base, TOKEN)
        .tls_policy(policy)
        .build()?;
    Client::from_options(options)?.status().await?;
    assert!(timeout(Duration::from_secs(2), mtls_server).await???);

    let (missing_identity_base, missing_identity_server) = https_status_server(true).await?;
    let policy = TlsPolicy::custom_roots().with_root_certificate_der(ca_certificate()?);
    let options = ClientOptions::builder(missing_identity_base, TOKEN)
        .tls_policy(policy)
        .build()?;
    assert!(matches!(
        Client::from_options(options)?.status().await,
        Err(SdkError::Transport)
    ));
    assert!(!timeout(Duration::from_secs(2), missing_identity_server).await???);
    Ok(())
}

#[tokio::test]
async fn connect_timeout_bounds_a_stalled_tls_handshake() -> Result<(), TestError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok::<(), std::io::Error>(())
    });
    let policy = TlsPolicy::custom_roots().with_root_certificate_der(ca_certificate()?);
    let options = ClientOptions::builder(format!("https://{address}"), TOKEN)
        .tls_policy(policy)
        .connect_timeout(Duration::from_millis(25))
        .request_timeout(Duration::from_secs(2))
        .build()?;
    let started = Instant::now();
    assert!(matches!(
        Client::from_options(options)?.status().await,
        Err(SdkError::Transport)
    ));
    assert!(started.elapsed() < Duration::from_millis(500));
    timeout(Duration::from_secs(2), server).await???;
    Ok(())
}

#[tokio::test]
async fn one_mtls_policy_and_rotating_provider_cover_https_initial_wss_and_reconnect()
-> Result<(), TestError> {
    const TOKENS: [&str; 3] = [
        "tls-rotation-http-0123456789abcdef",
        "tls-rotation-wss1-0123456789abcdef",
        "tls-rotation-wss2-0123456789abcdef",
    ];
    let (base, server) = https_then_reconnecting_wss_server().await?;
    let invocation = Arc::new(AtomicUsize::new(0));
    let provider_invocation = invocation.clone();
    let safe_events = Arc::new(Mutex::new(Vec::<SafeLogEvent>::new()));
    let safe_events_hook = safe_events.clone();
    let policy = TlsPolicy::custom_roots()
        .with_root_certificate_der(ca_certificate()?)
        .with_client_identity_der(vec![client_certificate()?], client_key()?);
    let options = ClientOptions::builder_with_token_provider(base, move || {
        let index = provider_invocation.fetch_add(1, Ordering::SeqCst);
        async move {
            TOKENS
                .get(index)
                .map(|token| (*token).to_owned())
                .ok_or("unexpected token resolution")
        }
    })
    .tls_policy(policy)
    .connect_timeout(Duration::from_secs(2))
    .request_timeout(Duration::from_secs(2))
    .reconnect_policy(ReconnectPolicy::new(
        1,
        Duration::from_millis(10),
        Duration::from_millis(10),
    )?)
    .client_metadata("tls-options-client", "9.8.7")
    .safe_log(move |event| {
        safe_events_hook
            .lock()
            .map_err(|_| xenoteer_sdk::SafeLogHookError)?
            .push(event);
        Ok(())
    })
    .build()?;
    let client = XenoteerClient::from_transport(Client::from_options(options)?).await?;
    let mut events = client
        .desktop()?
        .events(vec![EventTopic::new("window.changed")?], None)
        .await?;
    let terminal = timeout(Duration::from_secs(3), async {
        while let Some(item) = events.next().await {
            if let EventStreamItem::Closed { reason } = item {
                return reason;
            }
        }
        EventStreamCloseReason::ClientClosed
    })
    .await?;
    assert_eq!(terminal, EventStreamCloseReason::PeerClosed(1000));
    let (authorizations, metadata) = timeout(Duration::from_secs(3), server).await???;
    assert_eq!(invocation.load(Ordering::SeqCst), 3);
    assert_eq!(
        authorizations,
        [
            format!("Bearer {}", TOKENS[1]),
            format!("Bearer {}", TOKENS[2])
        ]
    );
    assert_eq!(
        metadata,
        [
            ("tls-options-client".to_owned(), "9.8.7".to_owned()),
            ("tls-options-client".to_owned(), "9.8.7".to_owned())
        ]
    );
    let safe_events = safe_events
        .lock()
        .map_err(|_| "safe event capture poisoned")?;
    for required in [
        SafeLogEvent {
            transport: SafeLogTransport::Http,
            operation: SafeLogOperation::HttpExchange,
            outcome: SafeLogOutcome::Succeeded,
        },
        SafeLogEvent {
            transport: SafeLogTransport::WebSocket,
            operation: SafeLogOperation::WebSocketConnect,
            outcome: SafeLogOutcome::Succeeded,
        },
        SafeLogEvent {
            transport: SafeLogTransport::WebSocket,
            operation: SafeLogOperation::WebSocketReconnect,
            outcome: SafeLogOutcome::Succeeded,
        },
    ] {
        assert!(safe_events.contains(&required));
    }
    Ok(())
}

#[tokio::test]
async fn proxy_environment_cannot_silently_redirect_direct_origin_connections()
-> Result<(), TestError> {
    const CHILD_MARKER: &str = "XENOTEER_DIRECT_ORIGIN_PROXY_TEST_CHILD";
    if env::var_os(CHILD_MARKER).is_none() {
        let output = Command::new(env::current_exe()?)
            .arg("--exact")
            .arg("proxy_environment_cannot_silently_redirect_direct_origin_connections")
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .env("NO_PROXY", "")
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "direct-origin child failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        return Ok(());
    }

    let (base, server) = https_status_server(true).await?;
    let policy = TlsPolicy::custom_roots()
        .with_root_certificate_der(ca_certificate()?)
        .with_client_identity_der(vec![client_certificate()?], client_key()?);
    let options = ClientOptions::builder(base, TOKEN)
        .tls_policy(policy)
        .build()?;
    Client::from_options(options)?.status().await?;
    assert!(timeout(Duration::from_secs(2), server).await???);
    Ok(())
}
