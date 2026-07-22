#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::VecDeque;
use std::future::pending;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;
use uuid::Uuid;
use xenoteer_protocol::{Timestamp, WindowCaptureSpace, WindowIdentityHash};

use super::*;

#[derive(Clone)]
struct CaptureCall {
    request: ScreenshotRequest,
    deadline: Instant,
    had_revalidator: bool,
}

struct FakeCaptureRuntime {
    responses: Mutex<VecDeque<Result<CapturedScreenshot, CaptureInvocationError>>>,
    calls: Mutex<Vec<CaptureCall>>,
    tokens: Mutex<Vec<CancellationToken>>,
    pending: AtomicBool,
    entered: Notify,
}

impl FakeCaptureRuntime {
    fn returning(captured: CapturedScreenshot) -> Self {
        Self::responses([Ok(captured)])
    }

    fn responses(
        responses: impl IntoIterator<Item = Result<CapturedScreenshot, CaptureInvocationError>>,
    ) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
            tokens: Mutex::new(Vec::new()),
            pending: AtomicBool::new(false),
            entered: Notify::new(),
        }
    }

    fn pending() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            tokens: Mutex::new(Vec::new()),
            pending: AtomicBool::new(true),
            entered: Notify::new(),
        }
    }

    fn calls(&self) -> Vec<CaptureCall> {
        lock(&self.calls).clone()
    }

    fn latest_token(&self) -> Option<CancellationToken> {
        lock(&self.tokens).last().cloned()
    }
}

impl RawCaptureRuntime for FakeCaptureRuntime {
    fn capture<'a>(
        &'a self,
        request: ScreenshotRequest,
        deadline: Instant,
        cancellation: CancellationToken,
        revalidate: Option<ExactRevalidator>,
    ) -> AdapterFuture<'a, Result<CapturedScreenshot, CaptureInvocationError>> {
        let had_revalidator = revalidate.is_some();
        lock(&self.calls).push(CaptureCall {
            request: request.clone(),
            deadline,
            had_revalidator,
        });
        lock(&self.tokens).push(cancellation);
        if let Some(revalidate) = revalidate {
            let window = match &request.target {
                ScreenshotTarget::WindowVisible { window, .. }
                | ScreenshotTarget::WindowDrawable { window } => window,
                ScreenshotTarget::Root => {
                    return Box::pin(async {
                        Err(CaptureInvocationError::Operation(
                            CaptureActorFailureKind::InvalidTarget,
                        ))
                    });
                }
            };
            if let Err(error) = revalidate(window) {
                let kind = match error {
                    RawCaptureRevalidationError::StaleReference => {
                        CaptureActorFailureKind::StaleReference
                    }
                    RawCaptureRevalidationError::TargetVanished => {
                        CaptureActorFailureKind::TargetVanished
                    }
                    RawCaptureRevalidationError::Unavailable => {
                        CaptureActorFailureKind::BackendUnavailable
                    }
                };
                return Box::pin(async move { Err(CaptureInvocationError::Operation(kind)) });
            }
        }
        if self.pending.load(Ordering::Acquire) {
            self.entered.notify_one();
            return Box::pin(async { pending().await });
        }
        let response =
            lock(&self.responses)
                .pop_front()
                .unwrap_or(Err(CaptureInvocationError::Operation(
                    CaptureActorFailureKind::ActorStopped,
                )));
        Box::pin(async move { response })
    }
}

struct FakeObservation {
    responses: Mutex<VecDeque<Result<u32, ControlPlaneError>>>,
    calls: Mutex<Vec<(WindowRef, Duration)>>,
}

impl FakeObservation {
    fn returning(response: Result<u32, ControlPlaneError>) -> Self {
        Self {
            responses: Mutex::new([response].into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(WindowRef, Duration)> {
        lock(&self.calls).clone()
    }
}

impl ExactWindowRevalidator for FakeObservation {
    fn revalidate_exact(
        &self,
        window: WindowRef,
        timeout: Duration,
    ) -> Result<u32, ControlPlaneError> {
        lock(&self.calls).push((window, timeout));
        lock(&self.responses)
            .pop_front()
            .unwrap_or(Err(ControlPlaneError::CapabilityUnavailable))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArtifactMutation {
    None,
    WrongDigest,
    WrongLength,
    WrongType,
    WrongDesktop,
    WrongPurpose,
}

struct PublishedCall {
    context: ScreenshotArtifactContext,
    content_type: ArtifactContentType,
    bytes: Vec<u8>,
}

struct FakeArtifactPublisher {
    calls: Mutex<Vec<PublishedCall>>,
    mutation: ArtifactMutation,
    delay: Duration,
    failure: Option<ControlPlaneError>,
}

impl FakeArtifactPublisher {
    fn healthy() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            mutation: ArtifactMutation::None,
            delay: Duration::ZERO,
            failure: None,
        }
    }

    fn mutating(mutation: ArtifactMutation) -> Self {
        Self {
            mutation,
            ..Self::healthy()
        }
    }

    fn delayed(delay: Duration) -> Self {
        Self {
            delay,
            ..Self::healthy()
        }
    }

    fn call_count(&self) -> usize {
        lock(&self.calls).len()
    }
}

impl ScreenshotArtifactPublisher for FakeArtifactPublisher {
    fn publish<'a>(
        &'a self,
        context: ScreenshotArtifactContext,
        artifact: GeneratedScreenshotArtifact,
    ) -> AdapterFuture<'a, Result<ArtifactRef, ControlPlaneError>> {
        let digest = digest_to_protocol(Sha256::digest(&artifact.bytes.0).into());
        let length = u64::try_from(artifact.bytes.0.len()).unwrap_or(u64::MAX);
        let content_type = artifact.content_type.clone();
        lock(&self.calls).push(PublishedCall {
            context: context.clone(),
            content_type: content_type.clone(),
            bytes: artifact.bytes.0,
        });
        let mutation = self.mutation;
        let delay = self.delay;
        let failure = self.failure;
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if let Some(error) = failure {
                return Err(error);
            }
            let mut result = ArtifactRef {
                artifact_id: xenoteer_protocol::ArtifactId::new(),
                purpose: ArtifactPurpose::Screenshot,
                desktop_id: context.desktop_id,
                desktop_generation: context.desktop_generation,
                content_type,
                content_length: length,
                sha256: digest?,
                created_at: Timestamp::parse("2026-07-21T00:00:00Z")
                    .map_err(|_| ControlPlaneError::Internal)?,
                expires_at: Timestamp::parse("2026-07-21T01:00:00Z")
                    .map_err(|_| ControlPlaneError::Internal)?,
            };
            match mutation {
                ArtifactMutation::None => {}
                ArtifactMutation::WrongDigest => {
                    result.sha256 = Sha256Digest::new("f".repeat(64))
                        .map_err(|_| ControlPlaneError::Internal)?;
                }
                ArtifactMutation::WrongLength => result.content_length += 1,
                ArtifactMutation::WrongType => {
                    result.content_type = ArtifactContentType::new("application/octet-stream")
                        .map_err(|_| ControlPlaneError::Internal)?;
                }
                ArtifactMutation::WrongDesktop => result.desktop_id = DesktopId::new(),
                ArtifactMutation::WrongPurpose => {
                    result.purpose = ArtifactPurpose::ClipboardOutput;
                }
            }
            Ok(result)
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn adapter_result<T>(result: Result<T, ControlPlaneError>) -> Result<T, std::io::Error> {
    result.map_err(|error| std::io::Error::other(format!("adapter failed: {error:?}")))
}

fn context() -> ScreenshotExecutionContext {
    ScreenshotExecutionContext {
        principal_id: "capture-reader".to_owned(),
        request_id: RequestId::new(),
        desktop_id: DesktopId::new(),
        desktop_generation: DesktopGeneration::new(),
    }
}

fn window(context: &ScreenshotExecutionContext) -> WindowRef {
    WindowRef {
        desktop_id: context.desktop_id,
        desktop_generation: context.desktop_generation,
        xid: 17,
        observed_generation: 3,
        identity_hash: WindowIdentityHash::new("a".repeat(64)).unwrap(),
    }
}

fn root_request(format: ScreenshotFormat) -> ScreenshotRequest {
    ScreenshotRequest {
        target: ScreenshotTarget::Root,
        region: Some(Rect::new(0, 0, 2, 1).unwrap()),
        format,
        include_cursor: false,
        scale: None,
        max_bytes: Some(1_024),
    }
}

fn window_request(context: &ScreenshotExecutionContext) -> ScreenshotRequest {
    ScreenshotRequest {
        target: ScreenshotTarget::WindowVisible {
            window: window(context),
            coordinate_space: WindowCaptureSpace::Client,
        },
        region: Some(Rect::new(0, 0, 2, 1).unwrap()),
        format: ScreenshotFormat::RawBgra,
        include_cursor: false,
        scale: None,
        max_bytes: Some(1_024),
    }
}

fn captured(request: &ScreenshotRequest) -> CapturedScreenshot {
    let bytes = match request.format {
        ScreenshotFormat::Png => b"\x89PNG\r\n\x1a\nfixture".to_vec(),
        ScreenshotFormat::RawBgra => vec![0, 0, 255, 255, 255, 0, 0, 255],
    };
    let limitation = match request.target {
        ScreenshotTarget::Root => RawCaptureLimitation::RootVisibleFramebuffer,
        ScreenshotTarget::WindowVisible { .. } => {
            RawCaptureLimitation::WindowVisibleIncludesOccluders
        }
        ScreenshotTarget::WindowDrawable { .. } => {
            RawCaptureLimitation::WindowDrawableObscuredUndefined
        }
    };
    CapturedScreenshot {
        target: request.target.clone(),
        format: request.format,
        source_region: match request.target {
            ScreenshotTarget::Root => Rect::new(0, 0, 2, 1).unwrap(),
            _ => Rect::new(10, 20, 2, 1).unwrap(),
        },
        source_size: Size::new(2, 1).unwrap(),
        output_size: Size::new(2, 1).unwrap(),
        raw_stride_bytes: (request.format == ScreenshotFormat::RawBgra).then_some(8),
        cursor: CursorCaptureEvidence {
            requested: false,
            composited: false,
            serial_before: None,
            serial_after: None,
            moved_during_capture: false,
        },
        limitation,
        sha256: Sha256::digest(&bytes).into(),
        bytes,
    }
}

fn settings() -> ScreenshotServiceSettings {
    ScreenshotServiceSettings::new(Duration::from_secs(1), Duration::from_millis(100)).unwrap()
}

fn service(
    capture: Arc<dyn RawCaptureRuntime>,
    observation: Arc<dyn ExactWindowRevalidator>,
    artifacts: Arc<dyn ScreenshotArtifactPublisher>,
) -> DaemonScreenshotService {
    DaemonScreenshotService::with_components(capture, observation, artifacts, settings())
}

#[tokio::test]
async fn root_capture_publishes_only_private_artifact_metadata_with_exact_context()
-> Result<(), Box<dyn std::error::Error>> {
    let context = context();
    let request = root_request(ScreenshotFormat::Png);
    let canary = captured(&request);
    assert!(!format!("{canary:?}").contains("fixture"));
    let capture = Arc::new(FakeCaptureRuntime::returning(canary));
    let observation = Arc::new(FakeObservation::returning(Ok(17)));
    let artifacts = Arc::new(FakeArtifactPublisher::healthy());
    let service = service(capture.clone(), observation.clone(), artifacts.clone());

    let result = adapter_result(
        service
            .capture_authorized(context.clone(), request.clone())
            .await,
    )?;
    assert_eq!(capture.calls().len(), 1);
    assert_eq!(capture.calls()[0].request, request);
    assert!(capture.calls()[0].deadline > Instant::now());
    assert!(!capture.calls()[0].had_revalidator);
    assert!(observation.calls().is_empty());
    assert_eq!(artifacts.call_count(), 1);
    let calls = lock(&artifacts.calls);
    assert_eq!(calls[0].context.principal_id, context.principal_id);
    assert_eq!(calls[0].context.desktop_id, context.desktop_id);
    assert_eq!(calls[0].content_type.as_str(), SCREENSHOT_PNG_CONTENT_TYPE);
    assert_eq!(calls[0].bytes, b"\x89PNG\r\n\x1a\nfixture");
    drop(calls);
    assert!(matches!(
        result.delivery,
        ScreenshotDelivery::Artifact { .. }
    ));
    assert_eq!(result.target, ScreenshotTarget::Root);
    assert!(result.raw.is_none());
    Ok(())
}

#[tokio::test]
async fn window_capture_revalidates_exact_birth_once_before_fake_actor_result()
-> Result<(), Box<dyn std::error::Error>> {
    let context = context();
    let request = window_request(&context);
    let expected_window = window(&context);
    let capture = Arc::new(FakeCaptureRuntime::returning(captured(&request)));
    let observation = Arc::new(FakeObservation::returning(Ok(expected_window.xid)));
    let artifacts = Arc::new(FakeArtifactPublisher::healthy());
    let service = service(capture.clone(), observation.clone(), artifacts);

    let result = adapter_result(service.capture_authorized(context, request.clone()).await)?;
    assert!(capture.calls()[0].had_revalidator);
    assert_eq!(
        observation.calls(),
        vec![(expected_window, Duration::from_millis(100))]
    );
    assert_eq!(result.target, request.target);
    assert_eq!(result.raw.map(|raw| raw.stride_bytes), Some(8));
    Ok(())
}

#[tokio::test]
async fn stale_exact_revalidation_never_publishes() -> Result<(), Box<dyn std::error::Error>> {
    let context = context();
    let request = window_request(&context);
    let capture = Arc::new(FakeCaptureRuntime::returning(captured(&request)));
    let observation = Arc::new(FakeObservation::returning(Err(ControlPlaneError::NotFound)));
    let artifacts = Arc::new(FakeArtifactPublisher::healthy());
    let service = service(capture, observation, artifacts.clone());

    let error = service
        .capture_authorized(context.clone(), request)
        .await
        .err();
    assert_eq!(
        error,
        Some(ControlPlaneError::StaleReference {
            current_generation: Some(context.desktop_generation),
        })
    );
    assert_eq!(artifacts.call_count(), 0);
    Ok(())
}

#[tokio::test]
async fn malformed_actor_evidence_fails_closed_before_artifact_publication()
-> Result<(), Box<dyn std::error::Error>> {
    let context = context();
    let request = root_request(ScreenshotFormat::RawBgra);
    let mut cases = Vec::new();

    let mut wrong_region = captured(&request);
    wrong_region.source_region = Rect::new(0, 0, 2, 2)?;
    cases.push(wrong_region);
    let mut wrong_limitation = captured(&request);
    wrong_limitation.limitation = RawCaptureLimitation::WindowDrawableObscuredUndefined;
    cases.push(wrong_limitation);
    let mut wrong_digest = captured(&request);
    wrong_digest.sha256[0] ^= 0xff;
    cases.push(wrong_digest);
    let mut wrong_length = captured(&request);
    wrong_length.bytes.pop();
    cases.push(wrong_length);
    let mut wrong_output = captured(&request);
    wrong_output.output_size = Size::new(1, 1)?;
    cases.push(wrong_output);
    let mut wrong_cursor = captured(&request);
    wrong_cursor.cursor.requested = true;
    cases.push(wrong_cursor);

    for case in cases {
        let capture = Arc::new(FakeCaptureRuntime::returning(case));
        let observation = Arc::new(FakeObservation::returning(Ok(17)));
        let artifacts = Arc::new(FakeArtifactPublisher::healthy());
        let service = service(capture, observation, artifacts.clone());
        assert_eq!(
            service
                .capture_authorized(context.clone(), request.clone())
                .await
                .err(),
            Some(ControlPlaneError::Internal)
        );
        assert_eq!(artifacts.call_count(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn malformed_artifact_evidence_is_never_returned() -> Result<(), Box<dyn std::error::Error>> {
    let context = context();
    let request = root_request(ScreenshotFormat::Png);
    for mutation in [
        ArtifactMutation::WrongDigest,
        ArtifactMutation::WrongLength,
        ArtifactMutation::WrongType,
        ArtifactMutation::WrongDesktop,
        ArtifactMutation::WrongPurpose,
    ] {
        let capture = Arc::new(FakeCaptureRuntime::returning(captured(&request)));
        let observation = Arc::new(FakeObservation::returning(Ok(17)));
        let artifacts = Arc::new(FakeArtifactPublisher::mutating(mutation));
        let service = service(capture, observation, artifacts.clone());
        assert_eq!(
            service
                .capture_authorized(context.clone(), request.clone())
                .await
                .err(),
            Some(ControlPlaneError::Internal),
            "mutation {mutation:?} escaped"
        );
        assert_eq!(artifacts.call_count(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn publication_timeout_is_bounded_and_cancels_capture_token()
-> Result<(), Box<dyn std::error::Error>> {
    let context = context();
    let request = root_request(ScreenshotFormat::Png);
    let capture = Arc::new(FakeCaptureRuntime::returning(captured(&request)));
    let observation = Arc::new(FakeObservation::returning(Ok(17)));
    let artifacts = Arc::new(FakeArtifactPublisher::delayed(Duration::from_millis(100)));
    let bounded =
        ScreenshotServiceSettings::new(Duration::from_millis(20), Duration::from_millis(10))?;
    let service = DaemonScreenshotService::with_components(
        capture.clone(),
        observation,
        artifacts.clone(),
        bounded,
    );
    assert_eq!(
        service.capture_authorized(context, request).await.err(),
        Some(ControlPlaneError::CapabilityUnavailable)
    );
    assert_eq!(artifacts.call_count(), 1);
    assert!(
        capture
            .latest_token()
            .is_some_and(|token| token.is_cancelled())
    );
    Ok(())
}

#[tokio::test]
async fn dropping_service_future_cancels_pending_actor_work()
-> Result<(), Box<dyn std::error::Error>> {
    let context = context();
    let request = root_request(ScreenshotFormat::Png);
    let capture = Arc::new(FakeCaptureRuntime::pending());
    let observation = Arc::new(FakeObservation::returning(Ok(17)));
    let artifacts = Arc::new(FakeArtifactPublisher::healthy());
    let service = Arc::new(service(capture.clone(), observation, artifacts));
    let task_service = Arc::clone(&service);
    let task = tokio::spawn(async move { task_service.capture_authorized(context, request).await });
    capture.entered.notified().await;
    task.abort();
    let _cancelled = task.await;
    assert!(
        capture
            .latest_token()
            .is_some_and(|token| token.is_cancelled())
    );
    Ok(())
}

#[test]
fn actor_failure_mapping_is_closed_and_stable() {
    let generation = DesktopGeneration::new();
    for (kind, expected) in [
        (
            CaptureActorFailureKind::RegionOutOfBounds,
            ControlPlaneError::InvalidRequest,
        ),
        (
            CaptureActorFailureKind::ControlQueueFull,
            ControlPlaneError::ResourceExhausted,
        ),
        (
            CaptureActorFailureKind::StaleReference,
            ControlPlaneError::StaleReference {
                current_generation: Some(generation),
            },
        ),
        (
            CaptureActorFailureKind::TargetVanished,
            ControlPlaneError::NotFound,
        ),
        (
            CaptureActorFailureKind::WindowNotViewable,
            ControlPlaneError::UnsupportedByTarget,
        ),
        (
            CaptureActorFailureKind::BackendUnavailable,
            ControlPlaneError::CapabilityUnavailable,
        ),
        (
            CaptureActorFailureKind::EncodeFailed,
            ControlPlaneError::Internal,
        ),
    ] {
        assert_eq!(
            map_capture_error(CaptureInvocationError::Operation(kind), generation),
            expected
        );
    }
}

#[tokio::test]
async fn public_result_contains_exact_root_physical_source_and_limitation_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    let context = context();
    let request = root_request(ScreenshotFormat::Png);
    let capture = Arc::new(FakeCaptureRuntime::returning(captured(&request)));
    let service = service(
        capture,
        Arc::new(FakeObservation::returning(Ok(17))),
        Arc::new(FakeArtifactPublisher::healthy()),
    );
    let result = adapter_result(service.capture_authorized(context, request).await)?;
    assert_eq!(
        result.source_region,
        WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(0, 0, 2, 1)?)?
    );
    assert_eq!(result.source_size, Size::new(2, 1)?);
    assert_eq!(
        result.limitation,
        ScreenshotSourceLimitation::RootVisibleFramebuffer
    );
    let encoded = serde_json::to_value(&result)?;
    assert_eq!(
        encoded["source_region"]["coordinate_space"],
        "root_physical"
    );
    assert_eq!(encoded["limitation"], "root_visible_framebuffer");

    let mut wrong_space = result.clone();
    wrong_space.source_region.coordinate_space = CoordinateSpace::WindowClient;
    assert!(wrong_space.validate().is_err());
    let mut wrong_size = result.clone();
    wrong_size.source_size = Size::new(1, 1)?;
    assert!(wrong_size.validate().is_err());
    let mut wrong_limitation = result;
    wrong_limitation.limitation = ScreenshotSourceLimitation::WindowVisibleIncludesOccluders;
    assert!(wrong_limitation.validate().is_err());
    Ok(())
}

#[test]
fn settings_and_debug_surfaces_are_bounded_and_secret_free()
-> Result<(), Box<dyn std::error::Error>> {
    assert!(ScreenshotServiceSettings::new(Duration::ZERO, Duration::from_secs(1)).is_err());
    assert!(
        ScreenshotServiceSettings::new(Duration::from_secs(1), Duration::from_secs(2)).is_err()
    );
    assert!(
        ScreenshotServiceSettings::new(Duration::from_secs(31), Duration::from_secs(1)).is_err()
    );
    let secret = SecretScreenshotBytes(b"SCREENSHOT-CANARY".to_vec());
    let debug = format!("{secret:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("SCREENSHOT-CANARY"));
    assert!(DesktopId::from_uuid(Uuid::nil()).as_uuid().is_nil());
    Ok(())
}
