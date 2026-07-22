//! Adversarial HTTP tests for authenticated clipboard-read admission.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use tower::ServiceExt;
use xenoteer_protocol::{
    ArtifactContentType, ArtifactId, ArtifactPurpose, ArtifactRef, ClipboardReadDelivery,
    ClipboardReadRequest, ClipboardReadResult, ClipboardTarget, DesktopGeneration, DesktopId,
    RequestId, SecretInlineBinary, SecretInlineText, SelectionName, SelectionTransferEvidence,
    SelectionTransferMode, SelectionTransferTerminal, Sha256Digest, Timestamp,
};

use crate::{
    AllowedOrigins, ApiServices, Authentication, ClipboardReadFuture, ClipboardReadRequestContext,
    ClipboardReadService, ControlPlaneError, DesktopReadiness, Grant, Principal, ReadinessHandle,
    ReadinessSnapshot, StaticCapabilityProvider, StaticTokenProvider, TransportLimits,
    api_router_with_services, control::UnavailableControlPlane,
    observation::UnavailableObservationPlane,
};

const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const BYTES_SHA256: &str = "039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81";

#[derive(Debug, Clone)]
struct CapturedCall {
    principal_id: String,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    request: ClipboardReadRequest,
}

struct FixtureClipboardReadService {
    result: ClipboardReadResult,
    error: Option<ControlPlaneError>,
    calls: AtomicUsize,
    captured: Mutex<Vec<CapturedCall>>,
}

impl FixtureClipboardReadService {
    fn returning(result: ClipboardReadResult) -> Self {
        Self {
            result,
            error: None,
            calls: AtomicUsize::new(0),
            captured: Mutex::new(Vec::new()),
        }
    }

    fn failing(result: ClipboardReadResult, error: ControlPlaneError) -> Self {
        Self {
            result,
            error: Some(error),
            calls: AtomicUsize::new(0),
            captured: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn captured(&self) -> Vec<CapturedCall> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ClipboardReadService for FixtureClipboardReadService {
    fn read<'a>(
        &'a self,
        context: ClipboardReadRequestContext,
        request: ClipboardReadRequest,
    ) -> ClipboardReadFuture<'a, Result<ClipboardReadResult, ControlPlaneError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(CapturedCall {
                principal_id: context.principal().id().to_owned(),
                request_id: context.request_id(),
                desktop_id: context.desktop_id(),
                desktop_generation: context.desktop_generation(),
                request,
            });
        Box::pin(async move {
            match self.error {
                Some(error) => Err(error),
                None => Ok(self.result.clone()),
            }
        })
    }
}

fn principal(
    grants: impl IntoIterator<Item = Grant>,
) -> Result<Principal, Box<dyn std::error::Error>> {
    Ok(Principal::new("clipboard-reader", grants)?)
}

fn application(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    principal: Principal,
    clipboard_reads: Option<Arc<dyn ClipboardReadService>>,
) -> Result<Router, Box<dyn std::error::Error>> {
    let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
        DesktopReadiness::Ready,
        Some(generation),
        None::<String>,
    ));
    let provider = StaticTokenProvider::single(TOKEN, principal)?;
    let mut services = ApiServices::new(
        Arc::new(UnavailableControlPlane),
        Arc::new(UnavailableObservationPlane),
    );
    if let Some(clipboard_reads) = clipboard_reads {
        services = services.with_clipboard_read_service(clipboard_reads);
    }
    Ok(api_router_with_services(
        readiness,
        desktop_id,
        Authentication::bearer(provider),
        StaticCapabilityProvider::empty()?,
        TransportLimits::default(),
        AllowedOrigins::default(),
        services,
    ))
}

fn read_request(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    body: Vec<u8>,
) -> Result<Request<Body>, axum::http::Error> {
    request_at(
        format!("/v1/desktops/{desktop_id}/clipboard/read?desktop_generation={generation}"),
        body,
    )
}

fn request_at(uri: String, body: Vec<u8>) -> Result<Request<Body>, axum::http::Error> {
    Request::post(uri)
        .header(
            header::AUTHORIZATION,
            "Bearer 0123456789abcdef0123456789abcdef",
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
}

fn valid_request() -> Result<ClipboardReadRequest, Box<dyn std::error::Error>> {
    Ok(ClipboardReadRequest {
        selection: SelectionName::Clipboard,
        preferred_targets: vec![ClipboardTarget::new("UTF8_STRING")?],
        allow_binary_fallback: false,
    })
}

fn text_result() -> Result<ClipboardReadResult, Box<dyn std::error::Error>> {
    Ok(ClipboardReadResult {
        selection: SelectionName::Clipboard,
        revision: 1,
        evidence: evidence("UTF8_STRING", 5, HELLO_SHA256)?,
        content: ClipboardReadDelivery::InlineText {
            text: SecretInlineText::new("hello")?,
        },
    })
}

fn binary_result(target: &str) -> Result<ClipboardReadResult, Box<dyn std::error::Error>> {
    Ok(ClipboardReadResult {
        selection: SelectionName::Clipboard,
        revision: 1,
        evidence: evidence(target, 3, BYTES_SHA256)?,
        content: ClipboardReadDelivery::InlineBinary {
            data: SecretInlineBinary::new("AQID", 3, Sha256Digest::new(BYTES_SHA256)?)?,
        },
    })
}

fn artifact_result(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
) -> Result<ClipboardReadResult, Box<dyn std::error::Error>> {
    Ok(ClipboardReadResult {
        selection: SelectionName::Clipboard,
        revision: 1,
        evidence: evidence("UTF8_STRING", 5, HELLO_SHA256)?,
        content: ClipboardReadDelivery::Artifact {
            artifact: ArtifactRef {
                artifact_id: ArtifactId::new(),
                purpose: ArtifactPurpose::ClipboardOutput,
                desktop_id,
                desktop_generation: generation,
                content_type: ArtifactContentType::new("text/plain;charset=utf-8")?,
                content_length: 5,
                sha256: Sha256Digest::new(HELLO_SHA256)?,
                created_at: Timestamp::parse("2026-07-21T00:00:00Z")?,
                expires_at: Timestamp::parse("2026-07-21T01:00:00Z")?,
            },
        },
    })
}

fn evidence(
    target: &str,
    content_length: u64,
    digest: &str,
) -> Result<SelectionTransferEvidence, Box<dyn std::error::Error>> {
    Ok(SelectionTransferEvidence {
        target: ClipboardTarget::new(target)?,
        transfer: SelectionTransferMode::Direct,
        content_length,
        sha256: Sha256Digest::new(digest)?,
        owner_changed: false,
        terminal_chunk_observed: false,
        terminal: SelectionTransferTerminal::Completed,
    })
}

async fn assert_output_rejected(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    request: &ClipboardReadRequest,
    result: ClipboardReadResult,
) -> Result<(), Box<dyn std::error::Error>> {
    let service = Arc::new(FixtureClipboardReadService::returning(result));
    let response = application(
        desktop_id,
        generation,
        principal([Grant::ClipboardRead])?,
        Some(service.clone()),
    )?
    .oneshot(read_request(
        desktop_id,
        generation,
        serde_json::to_vec(request)?,
    )?)
    .await?;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(service.call_count(), 1);
    Ok(())
}

#[tokio::test]
async fn valid_read_dispatches_bound_context_and_returns_private_json()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let request = valid_request()?;
    let expected = text_result()?;
    let service = Arc::new(FixtureClipboardReadService::returning(expected.clone()));
    let response = application(
        desktop_id,
        generation,
        principal([Grant::ClipboardRead])?,
        Some(service.clone()),
    )?
    .oneshot(read_request(
        desktop_id,
        generation,
        serde_json::to_vec(&request)?,
    )?)
    .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL),
        Some(&"private, no-store".parse()?)
    );
    let body = to_bytes(response.into_body(), 512 * 1024).await?;
    assert_eq!(
        serde_json::from_slice::<ClipboardReadResult>(&body)?,
        expected
    );
    let captured = service.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].principal_id, "clipboard-reader");
    assert!(!captured[0].request_id.as_uuid().is_nil());
    assert_eq!(captured[0].desktop_id, desktop_id);
    assert_eq!(captured[0].desktop_generation, generation);
    assert_eq!(captured[0].request, request);
    Ok(())
}

#[tokio::test]
async fn missing_clipboard_read_grant_never_dispatches() -> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let service = Arc::new(FixtureClipboardReadService::returning(text_result()?));
    let response = application(
        desktop_id,
        generation,
        principal([Grant::ClipboardWrite])?,
        Some(service.clone()),
    )?
    .oneshot(read_request(
        desktop_id,
        generation,
        serde_json::to_vec(&valid_request()?)?,
    )?)
    .await?;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(service.call_count(), 0);
    Ok(())
}

#[tokio::test]
async fn malformed_body_query_and_path_never_dispatch() -> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let service = Arc::new(FixtureClipboardReadService::returning(text_result()?));
    let application = application(
        desktop_id,
        generation,
        principal([Grant::ClipboardRead])?,
        Some(service.clone()),
    )?;
    let body = serde_json::to_vec(&valid_request()?)?;

    let unknown_body = application
        .clone()
        .oneshot(read_request(
            desktop_id,
            generation,
            br#"{"selection":"clipboard","preferred_targets":["UTF8_STRING"],"allow_binary_fallback":false,"unknown":true}"#.to_vec(),
        )?)
        .await?;
    assert_eq!(unknown_body.status(), StatusCode::BAD_REQUEST);

    let duplicate_targets = ClipboardReadRequest {
        preferred_targets: vec![
            ClipboardTarget::new("UTF8_STRING")?,
            ClipboardTarget::new("UTF8_STRING")?,
        ],
        ..valid_request()?
    };
    let duplicate_response = application
        .clone()
        .oneshot(read_request(
            desktop_id,
            generation,
            serde_json::to_vec(&duplicate_targets)?,
        )?)
        .await?;
    assert_eq!(duplicate_response.status(), StatusCode::BAD_REQUEST);

    for uri in [
        format!("/v1/desktops/{desktop_id}/clipboard/read"),
        format!(
            "/v1/desktops/{desktop_id}/clipboard/read?desktop_generation={generation}&extra=true"
        ),
        format!("/v1/desktops/not-a-desktop/clipboard/read?desktop_generation={generation}"),
    ] {
        let response = application
            .clone()
            .oneshot(request_at(uri, body.clone())?)
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(service.call_count(), 0);
    Ok(())
}

#[tokio::test]
async fn wrong_desktop_and_stale_generation_never_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let service = Arc::new(FixtureClipboardReadService::returning(text_result()?));
    let application = application(
        desktop_id,
        generation,
        principal([Grant::ClipboardRead])?,
        Some(service.clone()),
    )?;
    let body = serde_json::to_vec(&valid_request()?)?;

    let wrong_desktop = application
        .clone()
        .oneshot(read_request(DesktopId::new(), generation, body.clone())?)
        .await?;
    assert_eq!(wrong_desktop.status(), StatusCode::NOT_FOUND);
    let stale = application
        .oneshot(read_request(desktop_id, DesktopGeneration::new(), body)?)
        .await?;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(service.call_count(), 0);
    Ok(())
}

#[tokio::test]
async fn service_result_must_match_selection_target_and_valid_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let request = valid_request()?;

    let mut wrong_selection = text_result()?;
    wrong_selection.selection = SelectionName::Primary;
    assert_output_rejected(desktop_id, generation, &request, wrong_selection).await?;

    let wrong_target = ClipboardReadResult {
        evidence: evidence("text/plain", 5, HELLO_SHA256)?,
        ..text_result()?
    };
    assert_output_rejected(desktop_id, generation, &request, wrong_target).await?;

    let mut malformed = text_result()?;
    malformed.revision = 0;
    assert_output_rejected(desktop_id, generation, &request, malformed).await?;
    Ok(())
}

#[tokio::test]
async fn artifact_result_must_have_clipboard_output_purpose_and_exact_scope()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let request = valid_request()?;

    let mut wrong_purpose = artifact_result(desktop_id, generation)?;
    let ClipboardReadDelivery::Artifact { artifact } = &mut wrong_purpose.content else {
        unreachable!();
    };
    artifact.purpose = ArtifactPurpose::Screenshot;
    assert_output_rejected(desktop_id, generation, &request, wrong_purpose).await?;

    let wrong_scope = artifact_result(DesktopId::new(), generation)?;
    assert_output_rejected(desktop_id, generation, &request, wrong_scope).await?;

    let wrong_generation = artifact_result(desktop_id, DesktopGeneration::new())?;
    assert_output_rejected(desktop_id, generation, &request, wrong_generation).await?;

    let mut wrong_media_type = artifact_result(desktop_id, generation)?;
    let ClipboardReadDelivery::Artifact { artifact } = &mut wrong_media_type.content else {
        unreachable!();
    };
    artifact.content_type = ArtifactContentType::new("image/png")?;
    assert_output_rejected(desktop_id, generation, &request, wrong_media_type).await?;
    Ok(())
}

#[tokio::test]
async fn binary_fallback_requires_opt_in_but_explicit_binary_target_does_not()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();

    let mut fallback = valid_request()?;
    assert_output_rejected(
        desktop_id,
        generation,
        &fallback,
        binary_result("UTF8_STRING")?,
    )
    .await?;

    fallback.allow_binary_fallback = true;
    let fallback_service = Arc::new(FixtureClipboardReadService::returning(binary_result(
        "UTF8_STRING",
    )?));
    let fallback_response = application(
        desktop_id,
        generation,
        principal([Grant::ClipboardRead])?,
        Some(fallback_service.clone()),
    )?
    .oneshot(read_request(
        desktop_id,
        generation,
        serde_json::to_vec(&fallback)?,
    )?)
    .await?;
    assert_eq!(fallback_response.status(), StatusCode::OK);

    let explicit_binary = ClipboardReadRequest {
        preferred_targets: vec![ClipboardTarget::new("application/octet-stream")?],
        allow_binary_fallback: false,
        ..valid_request()?
    };
    let binary_service = Arc::new(FixtureClipboardReadService::returning(binary_result(
        "application/octet-stream",
    )?));
    let binary_response = application(
        desktop_id,
        generation,
        principal([Grant::ClipboardRead])?,
        Some(binary_service.clone()),
    )?
    .oneshot(read_request(
        desktop_id,
        generation,
        serde_json::to_vec(&explicit_binary)?,
    )?)
    .await?;
    assert_eq!(binary_response.status(), StatusCode::OK);
    assert_eq!(fallback_service.call_count(), 1);
    assert_eq!(binary_service.call_count(), 1);
    Ok(())
}

#[tokio::test]
async fn typed_service_errors_and_unavailable_default_are_mapped()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let body = serde_json::to_vec(&valid_request()?)?;
    let service = Arc::new(FixtureClipboardReadService::failing(
        text_result()?,
        ControlPlaneError::ResourceExhausted,
    ));
    let exhausted = application(
        desktop_id,
        generation,
        principal([Grant::ClipboardRead])?,
        Some(service.clone()),
    )?
    .oneshot(read_request(desktop_id, generation, body.clone())?)
    .await?;
    assert_eq!(exhausted.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(service.call_count(), 1);

    let unavailable = application(
        desktop_id,
        generation,
        principal([Grant::ClipboardRead])?,
        None,
    )?
    .oneshot(read_request(desktop_id, generation, body)?)
    .await?;
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    Ok(())
}
