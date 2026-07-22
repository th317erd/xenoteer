//! Bounded daemon adapter from raw X11 capture to private screenshot artifacts.

use core::fmt;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use xenoteer_protocol::{
    ArtifactContentType, ArtifactPurpose, ArtifactRef, CoordinateSpace, CursorCaptureEvidence,
    DesktopGeneration, DesktopId, RawAlphaMode, RawBgraMetadata, RawChannelOrder, Rect, RequestId,
    SCREENSHOT_PNG_CONTENT_TYPE, SCREENSHOT_RAW_BGRA_CONTENT_TYPE, ScreenshotDelivery,
    ScreenshotFormat, ScreenshotRequest, ScreenshotResult, ScreenshotSourceLimitation,
    ScreenshotTarget, Sha256Digest, Size, WindowRect, WindowRef,
};
use xenoteer_server::{
    ControlPlaneError, Grant, ScreenshotFuture, ScreenshotRequestContext, ScreenshotService,
};
use xenoteer_x11::capture::{
    CaptureActorFailureKind, CaptureActorHandle, CaptureSubmitError, RawCaptureLimitation,
    RawCaptureResult, RawCaptureRevalidationError,
};

use crate::artifact_service::{
    GeneratedArtifactRequest, InternalArtifactContext, StoreArtifactService,
};
use crate::observation_plane::DaemonObservationService;

const DEFAULT_SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_WINDOW_REVALIDATION_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const MAX_SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const MAX_WINDOW_REVALIDATION_TIMEOUT: Duration = Duration::from_secs(5);

type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type ExactRevalidator =
    Box<dyn FnOnce(&WindowRef) -> Result<(), RawCaptureRevalidationError> + Send + 'static>;

/// Bounded daemon policy for one screenshot capture, validation, and publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScreenshotServiceSettings {
    total_timeout: Duration,
    revalidation_timeout: Duration,
}

impl ScreenshotServiceSettings {
    /// Creates non-zero settings within the daemon's immutable hard ceilings.
    #[cfg(test)]
    pub(crate) fn new(
        total_timeout: Duration,
        revalidation_timeout: Duration,
    ) -> Result<Self, ScreenshotServiceSettingsError> {
        if total_timeout.is_zero()
            || total_timeout > MAX_SCREENSHOT_TIMEOUT
            || revalidation_timeout.is_zero()
            || revalidation_timeout > MAX_WINDOW_REVALIDATION_TIMEOUT
            || revalidation_timeout > total_timeout
        {
            return Err(ScreenshotServiceSettingsError);
        }
        Ok(Self {
            total_timeout,
            revalidation_timeout,
        })
    }
}

impl Default for ScreenshotServiceSettings {
    fn default() -> Self {
        Self {
            total_timeout: DEFAULT_SCREENSHOT_TIMEOUT,
            revalidation_timeout: DEFAULT_WINDOW_REVALIDATION_TIMEOUT,
        }
    }
}

/// Invalid screenshot-service timeout policy.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScreenshotServiceSettingsError;

#[cfg(test)]
impl fmt::Display for ScreenshotServiceSettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("screenshot timeouts are zero, excessive, or inconsistent")
    }
}

#[cfg(test)]
impl std::error::Error for ScreenshotServiceSettingsError {}

/// Screenshot service composed by the daemon after all three private actors are healthy.
pub(crate) struct DaemonScreenshotService {
    capture: Arc<dyn RawCaptureRuntime>,
    observation: Arc<dyn ExactWindowRevalidator>,
    artifacts: Arc<dyn ScreenshotArtifactPublisher>,
    settings: ScreenshotServiceSettings,
}

impl fmt::Debug for DaemonScreenshotService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DaemonScreenshotService")
            .field("settings", &self.settings)
            .field("capture", &"[PRIVATE ACTOR]")
            .field("observation", &"[PRIVATE ACTOR]")
            .field("artifacts", &"[PRIVATE STORE]")
            .finish()
    }
}

impl DaemonScreenshotService {
    /// Composes the production capture, exact-observation, and artifact boundaries.
    pub(crate) fn new(
        capture: CaptureActorHandle,
        observation: Arc<DaemonObservationService>,
        artifacts: Arc<StoreArtifactService>,
    ) -> Self {
        Self {
            capture: Arc::new(ActorCaptureRuntime { handle: capture }),
            observation,
            artifacts: Arc::new(StoreScreenshotArtifactPublisher { service: artifacts }),
            settings: ScreenshotServiceSettings::default(),
        }
    }

    #[cfg(test)]
    fn with_components(
        capture: Arc<dyn RawCaptureRuntime>,
        observation: Arc<dyn ExactWindowRevalidator>,
        artifacts: Arc<dyn ScreenshotArtifactPublisher>,
        settings: ScreenshotServiceSettings,
    ) -> Self {
        Self {
            capture,
            observation,
            artifacts,
            settings,
        }
    }

    async fn capture_authorized(
        &self,
        context: ScreenshotExecutionContext,
        request: ScreenshotRequest,
    ) -> Result<ScreenshotResult, ControlPlaneError> {
        context.validate()?;
        request
            .validate_for_desktop(context.desktop_id, context.desktop_generation)
            .map_err(|_| ControlPlaneError::InvalidRequest)?;
        let deadline = Instant::now()
            .checked_add(self.settings.total_timeout)
            .ok_or(ControlPlaneError::Internal)?;
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let revalidate = self.revalidator_for(&request);
        let captured = self
            .capture
            .capture(request.clone(), deadline, cancellation, revalidate)
            .await
            .map_err(|error| map_capture_error(error, context.desktop_generation))?;
        validate_capture_evidence(&captured, &request)?;

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ControlPlaneError::CapabilityUnavailable);
        }
        let digest = digest_to_protocol(captured.sha256)?;
        let captured_length =
            u64::try_from(captured.bytes.len()).map_err(|_| ControlPlaneError::Internal)?;
        let source_region = WindowRect::new(CoordinateSpace::RootPhysical, captured.source_region)
            .map_err(|_| ControlPlaneError::Internal)?;
        let limitation = limitation_to_protocol(captured.limitation);
        let artifact_context = ScreenshotArtifactContext {
            principal_id: context.principal_id.clone(),
            desktop_id: context.desktop_id,
            desktop_generation: context.desktop_generation,
        };
        let publication = GeneratedScreenshotArtifact {
            content_type: content_type_for(captured.format)?,
            bytes: SecretScreenshotBytes(captured.bytes),
        };
        let artifact = tokio::time::timeout(
            remaining,
            self.artifacts.publish(artifact_context, publication),
        )
        .await
        .map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
        validate_artifact_evidence(
            &artifact,
            captured.format,
            captured_length,
            &context,
            &digest,
        )?;

        let raw = match captured.format {
            ScreenshotFormat::RawBgra => Some(RawBgraMetadata {
                size: captured.output_size,
                stride_bytes: captured
                    .raw_stride_bytes
                    .ok_or(ControlPlaneError::Internal)?,
                channel_order: RawChannelOrder::Bgra8,
                alpha_mode: RawAlphaMode::Unpremultiplied,
            }),
            ScreenshotFormat::Png => None,
        };
        let result = ScreenshotResult {
            target: captured.target,
            source_region,
            source_size: captured.source_size,
            limitation,
            format: captured.format,
            size: captured.output_size,
            raw,
            cursor: captured.cursor,
            sha256: digest,
            delivery: ScreenshotDelivery::Artifact { artifact },
        };
        result
            .validate_against(
                &request,
                context.desktop_id,
                context.desktop_generation,
                captured.source_size,
            )
            .map_err(|_| ControlPlaneError::Internal)?;
        Ok(result)
    }

    fn revalidator_for(&self, request: &ScreenshotRequest) -> Option<ExactRevalidator> {
        let window = match &request.target {
            ScreenshotTarget::Root => return None,
            ScreenshotTarget::WindowVisible { window, .. }
            | ScreenshotTarget::WindowDrawable { window } => window.clone(),
        };
        let observation = Arc::clone(&self.observation);
        let timeout = self.settings.revalidation_timeout;
        Some(Box::new(move |candidate| {
            if candidate != &window {
                return Err(RawCaptureRevalidationError::StaleReference);
            }
            let xid = observation
                .revalidate_exact(window, timeout)
                .map_err(map_revalidation_error)?;
            if xid != candidate.xid {
                return Err(RawCaptureRevalidationError::StaleReference);
            }
            Ok(())
        }))
    }
}

impl ScreenshotService for DaemonScreenshotService {
    fn capture<'a>(
        &'a self,
        context: ScreenshotRequestContext,
        request: ScreenshotRequest,
    ) -> ScreenshotFuture<'a, Result<ScreenshotResult, ControlPlaneError>> {
        Box::pin(async move {
            if !context.principal().has_grant(Grant::CaptureRead) {
                return Err(ControlPlaneError::PermissionDenied);
            }
            let context = ScreenshotExecutionContext {
                principal_id: context.principal().id().to_owned(),
                request_id: context.request_id(),
                desktop_id: context.desktop_id(),
                desktop_generation: context.desktop_generation(),
            };
            self.capture_authorized(context, request).await
        })
    }
}

#[derive(Clone)]
struct ScreenshotExecutionContext {
    principal_id: String,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
}

impl ScreenshotExecutionContext {
    fn validate(&self) -> Result<(), ControlPlaneError> {
        InternalArtifactContext::new(
            self.principal_id.clone(),
            self.desktop_id,
            self.desktop_generation,
        )?;
        if self.request_id.as_uuid().is_nil() {
            return Err(ControlPlaneError::InvalidRequest);
        }
        Ok(())
    }
}

impl fmt::Debug for ScreenshotExecutionContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScreenshotExecutionContext")
            .field("principal_id", &self.principal_id)
            .field("request_id", &self.request_id)
            .field("desktop_id", &self.desktop_id)
            .field("desktop_generation", &self.desktop_generation)
            .finish()
    }
}

trait ExactWindowRevalidator: Send + Sync + 'static {
    fn revalidate_exact(
        &self,
        window: WindowRef,
        timeout: Duration,
    ) -> Result<u32, ControlPlaneError>;
}

impl ExactWindowRevalidator for DaemonObservationService {
    fn revalidate_exact(
        &self,
        window: WindowRef,
        timeout: Duration,
    ) -> Result<u32, ControlPlaneError> {
        self.revalidate_exact_blocking(window, timeout)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureInvocationError {
    Submit(CaptureSubmitError),
    Operation(CaptureActorFailureKind),
    ReplyTimedOut,
    ReplyClosed,
    BlockingTaskFailed,
}

#[derive(Clone)]
struct CapturedScreenshot {
    target: ScreenshotTarget,
    format: ScreenshotFormat,
    source_region: Rect,
    source_size: Size,
    output_size: Size,
    raw_stride_bytes: Option<u32>,
    cursor: CursorCaptureEvidence,
    limitation: RawCaptureLimitation,
    sha256: [u8; 32],
    bytes: Vec<u8>,
}

impl fmt::Debug for CapturedScreenshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedScreenshot")
            .field("target", &self.target)
            .field("format", &self.format)
            .field("source_region", &self.source_region)
            .field("source_size", &self.source_size)
            .field("output_size", &self.output_size)
            .field("raw_stride_bytes", &self.raw_stride_bytes)
            .field("cursor", &self.cursor)
            .field("limitation", &self.limitation)
            .field("sha256", &"[REDACTED]")
            .field("bytes", &self.bytes.len())
            .field("content", &"[REDACTED]")
            .finish()
    }
}

impl From<RawCaptureResult> for CapturedScreenshot {
    fn from(raw: RawCaptureResult) -> Self {
        Self {
            target: raw.target,
            format: raw.format,
            source_region: raw.source_region,
            source_size: raw.source_size,
            output_size: raw.output_size,
            raw_stride_bytes: raw.raw_stride_bytes,
            cursor: raw.cursor,
            limitation: raw.limitation,
            sha256: *raw.sha256.as_bytes(),
            bytes: raw.bytes.expose_secret().to_vec(),
        }
    }
}

trait RawCaptureRuntime: Send + Sync + 'static {
    fn capture<'a>(
        &'a self,
        request: ScreenshotRequest,
        deadline: Instant,
        cancellation: CancellationToken,
        revalidate: Option<ExactRevalidator>,
    ) -> AdapterFuture<'a, Result<CapturedScreenshot, CaptureInvocationError>>;
}

struct ActorCaptureRuntime {
    handle: CaptureActorHandle,
}

impl RawCaptureRuntime for ActorCaptureRuntime {
    fn capture<'a>(
        &'a self,
        request: ScreenshotRequest,
        deadline: Instant,
        cancellation: CancellationToken,
        revalidate: Option<ExactRevalidator>,
    ) -> AdapterFuture<'a, Result<CapturedScreenshot, CaptureInvocationError>> {
        let submitted = match &request.target {
            ScreenshotTarget::Root => self
                .handle
                .try_capture_root(request, deadline, cancellation),
            ScreenshotTarget::WindowVisible { .. } | ScreenshotTarget::WindowDrawable { .. } => {
                let Some(revalidate) = revalidate else {
                    return Box::pin(async {
                        Err(CaptureInvocationError::Operation(
                            CaptureActorFailureKind::StaleReference,
                        ))
                    });
                };
                self.handle
                    .try_capture_window(request, deadline, cancellation, revalidate)
            }
        }
        .map_err(CaptureInvocationError::Submit);
        Box::pin(async move {
            let reply = submitted?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CaptureInvocationError::ReplyTimedOut);
            }
            tokio::task::spawn_blocking(move || reply.recv_timeout(remaining))
                .await
                .map_err(|_| CaptureInvocationError::BlockingTaskFailed)?
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                        CaptureInvocationError::ReplyTimedOut
                    }
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        CaptureInvocationError::ReplyClosed
                    }
                })?
                .map(Into::into)
                .map_err(|error| CaptureInvocationError::Operation(error.kind))
        })
    }
}

#[derive(Clone, Debug)]
struct ScreenshotArtifactContext {
    principal_id: String,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
}

struct SecretScreenshotBytes(Vec<u8>);

impl fmt::Debug for SecretScreenshotBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretScreenshotBytes")
            .field("bytes", &self.0.len())
            .field("content", &"[REDACTED]")
            .finish()
    }
}

struct GeneratedScreenshotArtifact {
    content_type: ArtifactContentType,
    bytes: SecretScreenshotBytes,
}

impl fmt::Debug for GeneratedScreenshotArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeneratedScreenshotArtifact")
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes)
            .finish()
    }
}

trait ScreenshotArtifactPublisher: Send + Sync + 'static {
    fn publish<'a>(
        &'a self,
        context: ScreenshotArtifactContext,
        artifact: GeneratedScreenshotArtifact,
    ) -> AdapterFuture<'a, Result<ArtifactRef, ControlPlaneError>>;
}

struct StoreScreenshotArtifactPublisher {
    service: Arc<StoreArtifactService>,
}

impl ScreenshotArtifactPublisher for StoreScreenshotArtifactPublisher {
    fn publish<'a>(
        &'a self,
        context: ScreenshotArtifactContext,
        artifact: GeneratedScreenshotArtifact,
    ) -> AdapterFuture<'a, Result<ArtifactRef, ControlPlaneError>> {
        Box::pin(async move {
            let context = InternalArtifactContext::new(
                context.principal_id,
                context.desktop_id,
                context.desktop_generation,
            )?;
            let request = GeneratedArtifactRequest::new(
                ArtifactPurpose::Screenshot,
                artifact.content_type,
                artifact.bytes.0,
            )?;
            self.service.publish_generated(context, request).await
        })
    }
}

fn validate_capture_evidence(
    captured: &CapturedScreenshot,
    request: &ScreenshotRequest,
) -> Result<(), ControlPlaneError> {
    let source_region_size = captured
        .source_region
        .size()
        .map_err(|_| ControlPlaneError::Internal)?;
    let expected_output = request
        .validate_for_source(captured.source_size)
        .map_err(|_| ControlPlaneError::Internal)?;
    let limitation_matches = matches!(
        (&captured.target, captured.limitation),
        (
            ScreenshotTarget::Root,
            RawCaptureLimitation::RootVisibleFramebuffer
        ) | (
            ScreenshotTarget::WindowVisible { .. },
            RawCaptureLimitation::WindowVisibleIncludesOccluders
        ) | (
            ScreenshotTarget::WindowDrawable { .. },
            RawCaptureLimitation::WindowDrawableObscuredUndefined
        )
    );
    let root_region_matches = match (&request.target, request.region) {
        (ScreenshotTarget::Root, Some(region)) => captured.source_region == region,
        (ScreenshotTarget::Root, None) => {
            captured.source_region.origin().x() == 0 && captured.source_region.origin().y() == 0
        }
        _ => true,
    };
    let expected_raw_bytes = u64::from(captured.output_size.width())
        .checked_mul(u64::from(captured.output_size.height()))
        .and_then(|pixels| pixels.checked_mul(4));
    let format_matches = match captured.format {
        ScreenshotFormat::Png => captured.raw_stride_bytes.is_none() && !captured.bytes.is_empty(),
        ScreenshotFormat::RawBgra => {
            captured.raw_stride_bytes == captured.output_size.width().checked_mul(4)
                && expected_raw_bytes == u64::try_from(captured.bytes.len()).ok()
        }
    };
    let ceiling = request
        .max_bytes
        .unwrap_or(xenoteer_protocol::MAX_SCREENSHOT_BYTES);
    let digest: [u8; 32] = Sha256::digest(&captured.bytes).into();
    if captured.target != request.target
        || captured.format != request.format
        || source_region_size != captured.source_size
        || captured.output_size != expected_output
        || captured.cursor.requested != request.include_cursor
        || captured.cursor.validate().is_err()
        || !limitation_matches
        || !root_region_matches
        || !format_matches
        || u64::try_from(captured.bytes.len()).map_or(true, |length| length > ceiling)
        || captured.sha256 != digest
    {
        return Err(ControlPlaneError::Internal);
    }
    Ok(())
}

fn validate_artifact_evidence(
    artifact: &ArtifactRef,
    format: ScreenshotFormat,
    captured_length: u64,
    context: &ScreenshotExecutionContext,
    digest: &Sha256Digest,
) -> Result<(), ControlPlaneError> {
    let expected_content_type = match format {
        ScreenshotFormat::Png => SCREENSHOT_PNG_CONTENT_TYPE,
        ScreenshotFormat::RawBgra => SCREENSHOT_RAW_BGRA_CONTENT_TYPE,
    };
    if artifact.validate().is_err()
        || artifact.purpose != ArtifactPurpose::Screenshot
        || artifact.desktop_id != context.desktop_id
        || artifact.desktop_generation != context.desktop_generation
        || artifact.content_type.as_str() != expected_content_type
        || artifact.content_length != captured_length
        || artifact.sha256 != *digest
    {
        return Err(ControlPlaneError::Internal);
    }
    Ok(())
}

fn content_type_for(format: ScreenshotFormat) -> Result<ArtifactContentType, ControlPlaneError> {
    ArtifactContentType::new(match format {
        ScreenshotFormat::Png => SCREENSHOT_PNG_CONTENT_TYPE,
        ScreenshotFormat::RawBgra => SCREENSHOT_RAW_BGRA_CONTENT_TYPE,
    })
    .map_err(|_| ControlPlaneError::Internal)
}

const fn limitation_to_protocol(limitation: RawCaptureLimitation) -> ScreenshotSourceLimitation {
    match limitation {
        RawCaptureLimitation::RootVisibleFramebuffer => {
            ScreenshotSourceLimitation::RootVisibleFramebuffer
        }
        RawCaptureLimitation::WindowVisibleIncludesOccluders => {
            ScreenshotSourceLimitation::WindowVisibleIncludesOccluders
        }
        RawCaptureLimitation::WindowDrawableObscuredUndefined => {
            ScreenshotSourceLimitation::WindowDrawableObscuredUndefined
        }
    }
}

fn digest_to_protocol(bytes: [u8; 32]) -> Result<Sha256Digest, ControlPlaneError> {
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").map_err(|_| ControlPlaneError::Internal)?;
    }
    Sha256Digest::new(encoded).map_err(|_| ControlPlaneError::Internal)
}

fn map_revalidation_error(error: ControlPlaneError) -> RawCaptureRevalidationError {
    match error {
        ControlPlaneError::NotFound
        | ControlPlaneError::StaleReference { .. }
        | ControlPlaneError::PermissionDenied => RawCaptureRevalidationError::StaleReference,
        _ => RawCaptureRevalidationError::Unavailable,
    }
}

fn map_capture_error(
    error: CaptureInvocationError,
    generation: DesktopGeneration,
) -> ControlPlaneError {
    match error {
        CaptureInvocationError::Submit(CaptureSubmitError::InvalidRequest(_))
        | CaptureInvocationError::Operation(CaptureActorFailureKind::RegionOutOfBounds)
        | CaptureInvocationError::Operation(CaptureActorFailureKind::OutputTooLarge) => {
            ControlPlaneError::InvalidRequest
        }
        CaptureInvocationError::Submit(CaptureSubmitError::QueueFull)
        | CaptureInvocationError::Operation(CaptureActorFailureKind::ControlQueueFull) => {
            ControlPlaneError::ResourceExhausted
        }
        CaptureInvocationError::Operation(CaptureActorFailureKind::StaleReference) => {
            ControlPlaneError::StaleReference {
                current_generation: Some(generation),
            }
        }
        CaptureInvocationError::Operation(CaptureActorFailureKind::TargetVanished) => {
            ControlPlaneError::NotFound
        }
        CaptureInvocationError::Operation(CaptureActorFailureKind::WindowNotViewable) => {
            ControlPlaneError::UnsupportedByTarget
        }
        CaptureInvocationError::Operation(CaptureActorFailureKind::Cancelled)
        | CaptureInvocationError::Operation(CaptureActorFailureKind::DeadlineExceeded)
        | CaptureInvocationError::Operation(CaptureActorFailureKind::CursorUnavailable)
        | CaptureInvocationError::Operation(CaptureActorFailureKind::BackendUnavailable)
        | CaptureInvocationError::Operation(CaptureActorFailureKind::ActorPoisoned)
        | CaptureInvocationError::Operation(CaptureActorFailureKind::ActorStopped)
        | CaptureInvocationError::Operation(CaptureActorFailureKind::ActorPanicked)
        | CaptureInvocationError::Submit(CaptureSubmitError::Closed)
        | CaptureInvocationError::ReplyTimedOut
        | CaptureInvocationError::ReplyClosed
        | CaptureInvocationError::BlockingTaskFailed => ControlPlaneError::CapabilityUnavailable,
        CaptureInvocationError::Operation(CaptureActorFailureKind::EncodeFailed)
        | CaptureInvocationError::Operation(CaptureActorFailureKind::InvalidTarget)
        | CaptureInvocationError::Submit(CaptureSubmitError::InvalidDeadline)
        | CaptureInvocationError::Submit(CaptureSubmitError::InvalidTarget) => {
            ControlPlaneError::Internal
        }
    }
}

#[cfg(test)]
#[path = "screenshot_service_tests.rs"]
mod tests;
