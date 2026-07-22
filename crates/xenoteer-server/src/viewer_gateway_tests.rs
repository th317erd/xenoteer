//! Focused viewer gateway tests live outside the production module.

use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Error as WebSocketError, Message as BackendMessage,
        client::IntoClientRequest,
        handshake::server::{Request as BackendRequest, Response as BackendResponse},
    },
};
use tower::ServiceExt;
use xenoteer_protocol::{
    DesktopGeneration, DesktopId, OneTimeViewerTicket, ViewerMode, ViewerTicketRequest,
    ViewerTicketSecret,
};

use crate::{
    AllowedOrigins, ApiServices, Authentication, DesktopReadiness, Grant,
    InMemoryViewerTicketRegistry, LoopbackWebsockifyConnector, Principal, ReadinessHandle,
    ReadinessSnapshot, StaticCapabilityProvider, StaticTokenProvider, TransportLimits,
    VIEWER_BINARY_PROTOCOL, VIEWER_TICKET_PROTOCOL_PREFIX, ViewerGateway,
    ViewerGatewayConfigurationError, ViewerGatewayLimits, ViewerTicketRegistryConfig,
    api_router_with_services,
    control::UnavailableControlPlane,
    observation::UnavailableObservationPlane,
    viewer_gateway::{parse_viewer_protocols, required_no_vnc_assets},
};

const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
const ORIGIN: &str = "https://viewer.example";

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn viewer_assets() -> Result<Self, Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!("xenoteer-viewer-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path)?;
        for asset in required_no_vnc_assets() {
            let destination = path.join(asset);
            let Some(parent) = destination.parent() else {
                return Err(std::io::Error::other("viewer fixture path has no parent").into());
            };
            fs::create_dir_all(parent)?;
            fs::write(destination, b"export const xenoteerViewerFixture = true;\n")?;
        }
        Ok(Self(path))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn viewer_protocol_requires_one_binary_and_one_well_formed_ticket()
-> Result<(), Box<dyn std::error::Error>> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(
            "binary, xenoteer.ticket.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ),
    );
    assert!(parse_viewer_protocols(&headers).is_ok());

    for invalid in [
        "binary",
        "xenoteer.ticket.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "binary, binary, xenoteer.ticket.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "binary, xenoteer.ticket.bad",
        "binary, xenoteer.ticket.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA, xenoteer.ticket.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
        "binary, unsupported, xenoteer.ticket.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        let mut headers = HeaderMap::new();
        let value = HeaderValue::from_str(invalid)?;
        headers.insert(header::SEC_WEBSOCKET_PROTOCOL, value);
        assert!(
            parse_viewer_protocols(&headers).is_err(),
            "accepted {invalid}"
        );
    }
    Ok(())
}

#[test]
fn gateway_limits_and_backend_endpoint_are_closed() {
    assert!(
        ViewerGatewayLimits::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(1),
            1024,
            1,
        )
        .is_err()
    );
    assert!(
        ViewerGatewayLimits::new(
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            1024,
            1,
        )
        .is_err()
    );
    assert_eq!(
        LoopbackWebsockifyConnector::new(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            6080,
        )),
        Err(ViewerGatewayConfigurationError::Backend)
    );
    assert_eq!(
        LoopbackWebsockifyConnector::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
        Err(ViewerGatewayConfigurationError::Backend)
    );
}

#[tokio::test]
async fn static_viewer_is_query_free_hardened_and_contains_no_control_ui()
-> Result<(), Box<dyn std::error::Error>> {
    let assets = TemporaryDirectory::viewer_assets()?;
    let backend = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let connector = Arc::new(LoopbackWebsockifyConnector::new(backend.local_addr()?)?);
    drop(backend);
    let gateway = Arc::new(ViewerGateway::new(
        connector,
        assets.path(),
        ViewerGatewayLimits::default(),
    )?);
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let application = test_application(desktop_id, generation, gateway)?.0;

    let response = application
        .clone()
        .oneshot(Request::get(viewer_page_path(desktop_id, generation)).body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_hardened_static_headers(response.headers());
    let html = String::from_utf8(to_bytes(response.into_body(), 128 * 1024).await?.to_vec())?;
    assert!(!html.contains("<style"));
    assert!(!html.contains("innerHTML"));
    assert!(html.contains("role=\"status\""));
    assert!(html.contains("aria-live=\"polite\""));

    let module = application
        .clone()
        .oneshot(Request::get("/viewer/assets/viewer.mjs").body(Body::empty())?)
        .await?;
    assert_eq!(module.status(), StatusCode::OK);
    assert_hardened_static_headers(module.headers());
    let source = String::from_utf8(to_bytes(module.into_body(), 128 * 1024).await?.to_vec())?;
    for required in [
        "history.replaceState",
        "viewOnly = true",
        "resizeSession = false",
        "wsProtocols: [\"binary\", ticketProtocol]",
        "textContent",
    ] {
        assert!(
            source.contains(required),
            "viewer module omitted {required}"
        );
    }
    for forbidden in [
        "innerHTML",
        "Authorization",
        "clipboardPasteFrom",
        "sendCredentials",
        "fileInput",
        "?ticket=",
    ] {
        assert!(
            !source.contains(forbidden),
            "viewer module contains {forbidden}"
        );
    }

    let query = application
        .clone()
        .oneshot(
            Request::get(format!(
                "{}?ticket=must-never-be-in-a-query",
                viewer_page_path(desktop_id, generation)
            ))
            .body(Body::empty())?,
        )
        .await?;
    assert_eq!(query.status(), StatusCode::NOT_FOUND);

    let traversal = application
        .clone()
        .oneshot(Request::get("/viewer/vendor/%2e%2e/static/viewer.mjs").body(Body::empty())?)
        .await?;
    assert_eq!(traversal.status(), StatusCode::NOT_FOUND);

    let vendor = application
        .oneshot(Request::get("/viewer/vendor/core/rfb.js").body(Body::empty())?)
        .await?;
    assert_eq!(vendor.status(), StatusCode::OK);
    assert_hardened_static_headers(vendor.headers());
    Ok(())
}

fn assert_hardened_static_headers(headers: &HeaderMap) {
    assert_eq!(
        headers.get(header::CACHE_CONTROL),
        Some(&HeaderValue::from_static("private, no-store"))
    );
    assert_eq!(
        headers.get(header::X_CONTENT_TYPE_OPTIONS),
        Some(&HeaderValue::from_static("nosniff"))
    );
    assert_eq!(
        headers.get(header::REFERRER_POLICY),
        Some(&HeaderValue::from_static("no-referrer"))
    );
    assert!(
        headers
            .get(header::CONTENT_SECURITY_POLICY)
            .is_some_and(|value| value
                .as_bytes()
                .windows(22)
                .any(|part| part == b"frame-ancestors 'none'"))
    );
}

fn test_limits(
    maximum_frame_bytes: usize,
    maximum_sessions: usize,
) -> Result<ViewerGatewayLimits, ViewerGatewayConfigurationError> {
    ViewerGatewayLimits::new(
        Duration::from_millis(250),
        Duration::from_millis(250),
        Duration::from_secs(5),
        Duration::from_secs(10),
        maximum_frame_bytes,
        maximum_sessions,
    )
}

fn test_application(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    gateway: Arc<ViewerGateway>,
) -> Result<(Router, Arc<InMemoryViewerTicketRegistry>), Box<dyn std::error::Error>> {
    let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
        DesktopReadiness::Ready,
        Some(generation),
        None::<String>,
    ));
    let principal = Principal::new("viewer-principal", [Grant::ViewerRead])?;
    let provider = StaticTokenProvider::single(TOKEN, principal)?;
    let registry = Arc::new(InMemoryViewerTicketRegistry::new(
        ViewerTicketRegistryConfig::default(),
    )?);
    let services = ApiServices::new(
        Arc::new(UnavailableControlPlane),
        Arc::new(UnavailableObservationPlane),
    )
    .with_viewer_ticket_service(registry.clone())
    .with_viewer_gateway(gateway);
    Ok((
        api_router_with_services(
            readiness,
            desktop_id,
            Authentication::bearer(provider),
            StaticCapabilityProvider::empty()?,
            TransportLimits::default(),
            AllowedOrigins::exact([ORIGIN.to_owned()])?,
            services,
        ),
        registry,
    ))
}

fn viewer_page_path(desktop_id: DesktopId, generation: DesktopGeneration) -> String {
    format!("/viewer/{desktop_id}/{generation}/")
}

fn viewer_socket_path(desktop_id: DesktopId, generation: DesktopGeneration) -> String {
    format!("/v1/desktops/{desktop_id}/generations/{generation}/viewer/ws")
}

async fn issue_ticket(
    application: Router,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
) -> Result<OneTimeViewerTicket, Box<dyn std::error::Error>> {
    let request = ViewerTicketRequest {
        desktop_id,
        desktop_generation: generation,
        mode: ViewerMode::ViewOnly,
    };
    let response = application
        .oneshot(
            Request::post(format!("/v1/desktops/{desktop_id}/viewer-tickets"))
                .header(
                    header::AUTHORIZATION,
                    "Bearer 0123456789abcdef0123456789abcdef",
                )
                .header(header::ORIGIN, ORIGIN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&request)?))?,
        )
        .await?;
    if response.status() != StatusCode::CREATED {
        return Err(std::io::Error::other("viewer ticket issuance failed").into());
    }
    let body = to_bytes(response.into_body(), 64 * 1024).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[derive(Clone, Copy)]
enum BackendBehavior {
    Echo,
    Hold,
    Text,
    Oversize(usize),
}

struct TaskGuard(JoinHandle<()>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn spawn_backend(
    behavior: BackendBehavior,
) -> Result<(SocketAddr, Arc<AtomicUsize>, TaskGuard), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let accepts = Arc::new(AtomicUsize::new(0));
    let task_accepts = Arc::clone(&accepts);
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            task_accepts.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(handle_backend(stream, behavior));
        }
    });
    Ok((address, accepts, TaskGuard(task)))
}

#[allow(clippy::result_large_err)] // Required by tungstenite's fixed handshake callback type.
async fn handle_backend(stream: tokio::net::TcpStream, behavior: BackendBehavior) {
    let upgraded = accept_hdr_async(
        stream,
        |request: &BackendRequest, mut response: BackendResponse| {
            let request_is_valid = request.uri().path() == "/websockify"
                && request
                    .headers()
                    .get(header::SEC_WEBSOCKET_PROTOCOL)
                    .map(HeaderValue::as_bytes)
                    == Some(VIEWER_BINARY_PROTOCOL.as_bytes());
            if !request_is_valid {
                return Ok(response);
            }
            response.headers_mut().insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::from_static(VIEWER_BINARY_PROTOCOL),
            );
            Ok(response)
        },
    )
    .await;
    let Ok(mut socket) = upgraded else {
        return;
    };
    match behavior {
        BackendBehavior::Text => {
            let _ignored = socket.send(BackendMessage::Text("forbidden".into())).await;
        }
        BackendBehavior::Oversize(length) => {
            let _ignored = socket
                .send(BackendMessage::Binary(vec![0x5a; length].into()))
                .await;
        }
        BackendBehavior::Echo | BackendBehavior::Hold => {}
    }
    while let Some(message) = socket.next().await {
        match message {
            Ok(BackendMessage::Binary(payload)) if matches!(behavior, BackendBehavior::Echo) => {
                let _ignored = socket.send(BackendMessage::Binary(payload)).await;
            }
            Ok(BackendMessage::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

async fn spawn_application(application: Router) -> Result<(SocketAddr, TaskGuard), std::io::Error> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ignored = axum::serve(listener, application).await;
    });
    Ok((address, TaskGuard(task)))
}

async fn connect_browser(
    address: SocketAddr,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    ticket: &OneTimeViewerTicket,
    origin: &str,
) -> Result<
    (
        WebSocketStream<tokio::net::TcpStream>,
        http::Response<Option<Vec<u8>>>,
    ),
    WebSocketError,
> {
    let stream = tokio::net::TcpStream::connect(address).await?;
    let mut request = format!(
        "ws://{address}{}",
        viewer_socket_path(desktop_id, generation)
    )
    .into_client_request()?;
    request
        .headers_mut()
        .insert(header::ORIGIN, HeaderValue::from_str(origin)?);
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_str(&format!(
            "{VIEWER_BINARY_PROTOCOL}, {VIEWER_TICKET_PROTOCOL_PREFIX}{}",
            ticket.ticket.expose_secret()
        ))?,
    );
    tokio_tungstenite::client_async(request, stream).await
}

#[tokio::test]
async fn binary_bridge_is_bidirectional_and_selects_only_binary()
-> Result<(), Box<dyn std::error::Error>> {
    let (backend_address, accepts, _backend) = spawn_backend(BackendBehavior::Echo).await?;
    let assets = TemporaryDirectory::viewer_assets()?;
    let gateway = Arc::new(ViewerGateway::new(
        Arc::new(LoopbackWebsockifyConnector::new(backend_address)?),
        assets.path(),
        test_limits(1024, 2)?,
    )?);
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let (application, _) = test_application(desktop_id, generation, gateway)?;
    let ticket = issue_ticket(application.clone(), desktop_id, generation).await?;
    let (server_address, _server) = spawn_application(application).await?;

    let (mut browser, response) =
        connect_browser(server_address, desktop_id, generation, &ticket, ORIGIN).await?;
    assert_eq!(
        response.headers().get(header::SEC_WEBSOCKET_PROTOCOL),
        Some(&HeaderValue::from_static(VIEWER_BINARY_PROTOCOL))
    );
    browser
        .send(BackendMessage::Binary(Bytes::from_static(
            b"RFB test bytes",
        )))
        .await?;
    let reply = tokio::time::timeout(Duration::from_secs(2), browser.next())
        .await?
        .ok_or_else(|| std::io::Error::other("viewer bridge closed before reply"))??;
    assert_eq!(
        reply,
        BackendMessage::Binary(Bytes::from_static(b"RFB test bytes"))
    );
    browser.close(None).await?;
    assert_eq!(accepts.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn invalid_replayed_origin_and_route_attempts_never_reach_backend()
-> Result<(), Box<dyn std::error::Error>> {
    let (backend_address, accepts, _backend) = spawn_backend(BackendBehavior::Echo).await?;
    let assets = TemporaryDirectory::viewer_assets()?;
    let gateway = Arc::new(ViewerGateway::new(
        Arc::new(LoopbackWebsockifyConnector::new(backend_address)?),
        assets.path(),
        test_limits(1024, 4)?,
    )?);
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let (application, _) = test_application(desktop_id, generation, gateway)?;
    let ticket = issue_ticket(application.clone(), desktop_id, generation).await?;
    let (server_address, _server) = spawn_application(application).await?;

    let wrong_origin = connect_browser(
        server_address,
        desktop_id,
        generation,
        &ticket,
        "https://wrong.example",
    )
    .await;
    assert_eq!(
        websocket_http_status(&wrong_origin),
        Some(StatusCode::FORBIDDEN)
    );
    let wrong_generation = connect_browser(
        server_address,
        desktop_id,
        DesktopGeneration::new(),
        &ticket,
        ORIGIN,
    )
    .await;
    assert_eq!(
        websocket_http_status(&wrong_generation),
        Some(StatusCode::FORBIDDEN)
    );
    let wrong_desktop = connect_browser(
        server_address,
        DesktopId::new(),
        generation,
        &ticket,
        ORIGIN,
    )
    .await;
    assert_eq!(
        websocket_http_status(&wrong_desktop),
        Some(StatusCode::FORBIDDEN)
    );
    let mut unknown_ticket = ticket.clone();
    unknown_ticket.ticket = ViewerTicketSecret::new("B".repeat(43))?;
    let invalid = connect_browser(
        server_address,
        desktop_id,
        generation,
        &unknown_ticket,
        ORIGIN,
    )
    .await;
    assert_eq!(websocket_http_status(&invalid), Some(StatusCode::FORBIDDEN));
    assert_websocket_error_omits_secret(&invalid, unknown_ticket.ticket.expose_secret());
    assert_eq!(accepts.load(Ordering::SeqCst), 0);

    let (mut valid, _) =
        connect_browser(server_address, desktop_id, generation, &ticket, ORIGIN).await?;
    wait_for_accepts(&accepts, 1).await?;
    assert_eq!(accepts.load(Ordering::SeqCst), 1);
    valid.close(None).await?;

    let replay = connect_browser(server_address, desktop_id, generation, &ticket, ORIGIN).await;
    assert_eq!(websocket_http_status(&replay), Some(StatusCode::FORBIDDEN));
    assert_eq!(accepts.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn concurrent_ticket_use_succeeds_once_and_opens_one_backend()
-> Result<(), Box<dyn std::error::Error>> {
    let (backend_address, accepts, _backend) = spawn_backend(BackendBehavior::Hold).await?;
    let assets = TemporaryDirectory::viewer_assets()?;
    let gateway = Arc::new(ViewerGateway::new(
        Arc::new(LoopbackWebsockifyConnector::new(backend_address)?),
        assets.path(),
        test_limits(1024, 4)?,
    )?);
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let (application, _) = test_application(desktop_id, generation, gateway)?;
    let ticket = issue_ticket(application.clone(), desktop_id, generation).await?;
    let (server_address, _server) = spawn_application(application).await?;

    let (first, second) = tokio::join!(
        connect_browser(server_address, desktop_id, generation, &ticket, ORIGIN),
        connect_browser(server_address, desktop_id, generation, &ticket, ORIGIN),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert_eq!(
        usize::from(websocket_http_status(&first) == Some(StatusCode::FORBIDDEN))
            + usize::from(websocket_http_status(&second) == Some(StatusCode::FORBIDDEN)),
        1
    );
    wait_for_accepts(&accepts, 1).await?;
    assert_eq!(accepts.load(Ordering::SeqCst), 1);
    if let Ok((mut socket, _)) = first {
        socket.close(None).await?;
    }
    if let Ok((mut socket, _)) = second {
        socket.close(None).await?;
    }
    Ok(())
}

#[tokio::test]
async fn session_capacity_rejects_before_ticket_consumption_or_backend_connect()
-> Result<(), Box<dyn std::error::Error>> {
    let (backend_address, accepts, _backend) = spawn_backend(BackendBehavior::Hold).await?;
    let assets = TemporaryDirectory::viewer_assets()?;
    let gateway = Arc::new(ViewerGateway::new(
        Arc::new(LoopbackWebsockifyConnector::new(backend_address)?),
        assets.path(),
        test_limits(1024, 1)?,
    )?);
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let (application, _) = test_application(desktop_id, generation, gateway)?;
    let first_ticket = issue_ticket(application.clone(), desktop_id, generation).await?;
    let second_ticket = issue_ticket(application.clone(), desktop_id, generation).await?;
    let (server_address, _server) = spawn_application(application).await?;
    let (mut first, _) = connect_browser(
        server_address,
        desktop_id,
        generation,
        &first_ticket,
        ORIGIN,
    )
    .await?;

    let full = connect_browser(
        server_address,
        desktop_id,
        generation,
        &second_ticket,
        ORIGIN,
    )
    .await;
    assert_eq!(
        websocket_http_status(&full),
        Some(StatusCode::TOO_MANY_REQUESTS)
    );
    assert_eq!(accepts.load(Ordering::SeqCst), 1);

    first.close(None).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let (mut second, _) = loop {
        match connect_browser(
            server_address,
            desktop_id,
            generation,
            &second_ticket,
            ORIGIN,
        )
        .await
        {
            Ok(connection) => break connection,
            Err(WebSocketError::Http(response))
                if response.status() == StatusCode::TOO_MANY_REQUESTS
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    assert_eq!(accepts.load(Ordering::SeqCst), 2);
    second.close(None).await?;
    Ok(())
}

#[tokio::test]
async fn backend_failure_and_stall_are_bounded_and_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let reserved = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let unavailable_address = reserved.local_addr()?;
    drop(reserved);
    let assets = TemporaryDirectory::viewer_assets()?;
    let gateway = Arc::new(ViewerGateway::new(
        Arc::new(LoopbackWebsockifyConnector::new(unavailable_address)?),
        assets.path(),
        test_limits(1024, 1)?,
    )?);
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let (application, registry) = test_application(desktop_id, generation, gateway)?;
    let ticket = issue_ticket(application.clone(), desktop_id, generation).await?;
    let (server_address, _server) = spawn_application(application).await?;
    let failed = connect_browser(server_address, desktop_id, generation, &ticket, ORIGIN).await;
    assert_eq!(
        websocket_http_status(&failed),
        Some(StatusCode::SERVICE_UNAVAILABLE)
    );
    assert_eq!(registry.retained_ticket_count(), 0);

    let stall_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let stall_address = stall_listener.local_addr()?;
    let stall_task = TaskGuard(tokio::spawn(async move {
        if let Ok((_stream, _)) = stall_listener.accept().await {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }));
    let assets = TemporaryDirectory::viewer_assets()?;
    let limits = ViewerGatewayLimits::new(
        Duration::from_millis(50),
        Duration::from_millis(100),
        Duration::from_secs(1),
        Duration::from_secs(2),
        1024,
        1,
    )?;
    let gateway = Arc::new(ViewerGateway::new(
        Arc::new(LoopbackWebsockifyConnector::new(stall_address)?),
        assets.path(),
        limits,
    )?);
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let (application, registry) = test_application(desktop_id, generation, gateway)?;
    let ticket = issue_ticket(application.clone(), desktop_id, generation).await?;
    let (server_address, _server) = spawn_application(application).await?;
    let stalled = tokio::time::timeout(
        Duration::from_secs(1),
        connect_browser(server_address, desktop_id, generation, &ticket, ORIGIN),
    )
    .await?;
    assert_eq!(
        websocket_http_status(&stalled),
        Some(StatusCode::SERVICE_UNAVAILABLE)
    );
    assert_eq!(registry.retained_ticket_count(), 0);
    drop(stall_task);
    Ok(())
}

#[tokio::test]
async fn text_and_oversized_backend_messages_close_without_forwarding_content()
-> Result<(), Box<dyn std::error::Error>> {
    for behavior in [BackendBehavior::Text, BackendBehavior::Oversize(65)] {
        let (backend_address, _, _backend) = spawn_backend(behavior).await?;
        let assets = TemporaryDirectory::viewer_assets()?;
        let gateway = Arc::new(ViewerGateway::new(
            Arc::new(LoopbackWebsockifyConnector::new(backend_address)?),
            assets.path(),
            test_limits(64, 1)?,
        )?);
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let (application, _) = test_application(desktop_id, generation, gateway)?;
        let ticket = issue_ticket(application.clone(), desktop_id, generation).await?;
        let (server_address, _server) = spawn_application(application).await?;
        let (mut browser, _) =
            connect_browser(server_address, desktop_id, generation, &ticket, ORIGIN).await?;
        let message = tokio::time::timeout(Duration::from_secs(2), browser.next())
            .await?
            .ok_or_else(|| std::io::Error::other("viewer bridge ended without close evidence"))??;
        assert!(matches!(message, BackendMessage::Close(_)));
    }
    Ok(())
}

#[tokio::test]
async fn browser_text_and_oversized_binary_messages_close_the_bounded_bridge()
-> Result<(), Box<dyn std::error::Error>> {
    let (backend_address, _, _backend) = spawn_backend(BackendBehavior::Hold).await?;
    let assets = TemporaryDirectory::viewer_assets()?;
    let gateway = Arc::new(ViewerGateway::new(
        Arc::new(LoopbackWebsockifyConnector::new(backend_address)?),
        assets.path(),
        test_limits(64, 2)?,
    )?);
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let (application, _) = test_application(desktop_id, generation, gateway)?;
    let text_ticket = issue_ticket(application.clone(), desktop_id, generation).await?;
    let oversized_ticket = issue_ticket(application.clone(), desktop_id, generation).await?;
    let (server_address, _server) = spawn_application(application).await?;

    let (mut text_browser, _) =
        connect_browser(server_address, desktop_id, generation, &text_ticket, ORIGIN).await?;
    text_browser
        .send(BackendMessage::Text("forbidden".into()))
        .await?;
    let text_close = tokio::time::timeout(Duration::from_secs(2), text_browser.next())
        .await?
        .ok_or_else(|| std::io::Error::other("text viewer ended without close evidence"))??;
    assert!(matches!(text_close, BackendMessage::Close(_)));

    let (mut oversized_browser, _) = connect_browser(
        server_address,
        desktop_id,
        generation,
        &oversized_ticket,
        ORIGIN,
    )
    .await?;
    oversized_browser
        .send(BackendMessage::Binary(vec![0x44; 65].into()))
        .await?;
    let oversized_close = tokio::time::timeout(Duration::from_secs(2), oversized_browser.next())
        .await?
        .ok_or_else(|| std::io::Error::other("oversized viewer ended without close evidence"))?;
    assert!(matches!(
        oversized_close,
        Ok(BackendMessage::Close(_)) | Err(_)
    ));
    Ok(())
}

fn websocket_http_status<T>(result: &Result<T, WebSocketError>) -> Option<StatusCode> {
    match result {
        Err(WebSocketError::Http(response)) => Some(response.status()),
        Ok(_) | Err(_) => None,
    }
}

async fn wait_for_accepts(
    accepts: &AtomicUsize,
    expected: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while accepts.load(Ordering::SeqCst) < expected {
        if tokio::time::Instant::now() >= deadline {
            return Err(std::io::Error::other("backend accept condition timed out").into());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    Ok(())
}

fn assert_websocket_error_omits_secret<T: core::fmt::Debug>(
    result: &Result<T, WebSocketError>,
    secret: &str,
) {
    let rendered = format!("{result:?}");
    assert!(!rendered.contains(secret));
    if let Err(WebSocketError::Http(response)) = result {
        assert!(response.body().as_ref().is_none_or(|body| {
            !body
                .windows(secret.len())
                .any(|part| part == secret.as_bytes())
        }));
    }
}
