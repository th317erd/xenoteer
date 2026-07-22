//! Bounded daemon adapter from raw X11 selection reads to authorized delivery.

use core::fmt;
use std::{
    fmt::Write as _,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use xenoteer_protocol::{
    ArtifactContentType, ArtifactPurpose, ArtifactRef, ClipboardReadDelivery, ClipboardReadRequest,
    ClipboardReadResult, ClipboardTarget, DesktopGeneration, DesktopId, MAX_INLINE_CLIPBOARD_BYTES,
    RequestId, SecretInlineBinary, SecretInlineText, SelectionName, SelectionTransferEvidence,
    SelectionTransferMode, SelectionTransferTerminal, Sha256Digest,
};
use xenoteer_server::{
    ClipboardReadFuture, ClipboardReadRequestContext, ClipboardReadService, ControlPlaneError,
    Grant, Principal,
};
use xenoteer_x11::{
    ClipboardActorFailureKind, ClipboardActorHandle, ClipboardPayloadKind, ClipboardReadRawRequest,
    ClipboardSubmitError, RawClipboardReadResult, RawClipboardTarget,
};

use crate::artifact_service::{
    GeneratedArtifactRequest, InternalArtifactContext, StoreArtifactService,
};

const DEFAULT_CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_secs(12);
#[cfg(test)]
const MAX_CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_secs(30);
const UTF8_TEXT_CONTENT_TYPE: &str = "text/plain;charset=utf-8";
const BINARY_CONTENT_TYPE: &str = "application/octet-stream";
const PNG_CONTENT_TYPE: &str = "image/png";

type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Bounded total actor-read and optional artifact-publication policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClipboardReadServiceSettings {
    total_timeout: Duration,
}

impl ClipboardReadServiceSettings {
    /// Creates a non-zero timeout within the immutable daemon ceiling.
    #[cfg(test)]
    pub(crate) fn new(total_timeout: Duration) -> Result<Self, ClipboardReadSettingsError> {
        if total_timeout.is_zero() || total_timeout > MAX_CLIPBOARD_READ_TIMEOUT {
            return Err(ClipboardReadSettingsError);
        }
        Ok(Self { total_timeout })
    }
}

impl Default for ClipboardReadServiceSettings {
    fn default() -> Self {
        Self {
            total_timeout: DEFAULT_CLIPBOARD_READ_TIMEOUT,
        }
    }
}

/// Invalid daemon clipboard-read timeout policy.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClipboardReadSettingsError;

#[cfg(test)]
impl fmt::Display for ClipboardReadSettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("clipboard read timeout is zero or exceeds its fixed ceiling")
    }
}

#[cfg(test)]
impl std::error::Error for ClipboardReadSettingsError {}

/// Daemon clipboard service composed from one X11 actor and private artifact store.
pub(crate) struct DaemonClipboardReadService {
    reader: Arc<dyn RawClipboardReader>,
    artifacts: Arc<dyn ClipboardArtifactPublisher>,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    settings: ClipboardReadServiceSettings,
}

impl fmt::Debug for DaemonClipboardReadService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DaemonClipboardReadService")
            .field("desktop_id", &self.desktop_id)
            .field("desktop_generation", &self.desktop_generation)
            .field("settings", &self.settings)
            .field("reader", &"[PRIVATE ACTOR]")
            .field("artifacts", &"[PRIVATE STORE]")
            .finish()
    }
}

impl DaemonClipboardReadService {
    /// Composes the production clipboard actor and generated-artifact boundary.
    pub(crate) fn new(
        actor: ClipboardActorHandle,
        artifacts: Arc<StoreArtifactService>,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Self {
        Self {
            reader: Arc::new(ActorClipboardReader { actor }),
            artifacts: Arc::new(StoreClipboardArtifactPublisher { service: artifacts }),
            desktop_id,
            desktop_generation,
            settings: ClipboardReadServiceSettings::default(),
        }
    }

    #[cfg(test)]
    fn with_components(
        reader: Arc<dyn RawClipboardReader>,
        artifacts: Arc<dyn ClipboardArtifactPublisher>,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        settings: ClipboardReadServiceSettings,
    ) -> Self {
        Self {
            reader,
            artifacts,
            desktop_id,
            desktop_generation,
            settings,
        }
    }

    async fn read_authorized(
        &self,
        context: ClipboardExecutionContext,
        request: ClipboardReadRequest,
    ) -> Result<ClipboardReadResult, ControlPlaneError> {
        context.validate(self.desktop_id, self.desktop_generation)?;
        request
            .validate()
            .map_err(|_| ControlPlaneError::InvalidRequest)?;
        let raw_request = map_request(&request)?;
        let deadline = Instant::now()
            .checked_add(self.settings.total_timeout)
            .ok_or(ControlPlaneError::Internal)?;
        let raw = tokio::time::timeout(
            self.settings.total_timeout,
            self.reader.read(raw_request.clone(), deadline),
        )
        .await
        .map_err(|_| ControlPlaneError::CapabilityUnavailable)?
        .map_err(map_read_error)?;
        let validated = validate_raw_read(raw, &raw_request)?;
        let content = if validated.bytes.len() <= MAX_INLINE_CLIPBOARD_BYTES {
            inline_delivery(validated.payload_kind, validated.bytes, &validated.sha256)?
        } else {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ControlPlaneError::CapabilityUnavailable);
            }
            let content_type = content_type_for(validated.payload_kind)?;
            let expected_length =
                u64::try_from(validated.bytes.len()).map_err(|_| ControlPlaneError::Internal)?;
            let artifact_context = ClipboardArtifactContext {
                principal_id: context.principal_id,
                request_id: context.request_id,
                desktop_id: context.desktop_id,
                desktop_generation: context.desktop_generation,
            };
            let publication = GeneratedClipboardArtifact {
                content_type: content_type.clone(),
                bytes: SecretClipboardBytes(validated.bytes),
            };
            let artifact = tokio::time::timeout(
                remaining,
                self.artifacts.publish(artifact_context, publication),
            )
            .await
            .map_err(|_| ControlPlaneError::CapabilityUnavailable)??;
            validate_artifact(
                &artifact,
                &content_type,
                expected_length,
                &validated.sha256,
                context.desktop_id,
                context.desktop_generation,
            )?;
            ClipboardReadDelivery::Artifact { artifact }
        };
        let result = ClipboardReadResult {
            selection: validated.selection,
            revision: validated.revision,
            evidence: validated.evidence,
            content,
        };
        result
            .validate_for_desktop(context.desktop_id, context.desktop_generation)
            .map_err(|_| ControlPlaneError::Internal)?;
        Ok(result)
    }
}

impl ClipboardReadService for DaemonClipboardReadService {
    fn read<'a>(
        &'a self,
        context: ClipboardReadRequestContext,
        request: ClipboardReadRequest,
    ) -> ClipboardReadFuture<'a, Result<ClipboardReadResult, ControlPlaneError>> {
        Box::pin(async move {
            let execution = authorize_context(
                context.principal(),
                context.request_id(),
                context.desktop_id(),
                context.desktop_generation(),
                self.desktop_id,
                self.desktop_generation,
            )?;
            self.read_authorized(execution, request).await
        })
    }
}

#[derive(Clone)]
struct ClipboardExecutionContext {
    principal_id: String,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
}

impl ClipboardExecutionContext {
    fn validate(
        &self,
        expected_desktop_id: DesktopId,
        expected_generation: DesktopGeneration,
    ) -> Result<(), ControlPlaneError> {
        if self.request_id.as_uuid().is_nil()
            || self.desktop_id.as_uuid().is_nil()
            || self.desktop_generation.as_uuid().is_nil()
        {
            return Err(ControlPlaneError::InvalidRequest);
        }
        if self.desktop_id != expected_desktop_id {
            return Err(ControlPlaneError::PermissionDenied);
        }
        if self.desktop_generation != expected_generation {
            return Err(ControlPlaneError::StaleReference {
                current_generation: Some(expected_generation),
            });
        }
        InternalArtifactContext::new(
            self.principal_id.clone(),
            self.desktop_id,
            self.desktop_generation,
        )?;
        Ok(())
    }
}

impl fmt::Debug for ClipboardExecutionContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClipboardExecutionContext")
            .field("principal_id", &self.principal_id)
            .field("request_id", &self.request_id)
            .field("desktop_id", &self.desktop_id)
            .field("desktop_generation", &self.desktop_generation)
            .finish()
    }
}

fn authorize_context(
    principal: &Principal,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    expected_desktop_id: DesktopId,
    expected_generation: DesktopGeneration,
) -> Result<ClipboardExecutionContext, ControlPlaneError> {
    if !principal.has_grant(Grant::ClipboardRead) {
        return Err(ControlPlaneError::PermissionDenied);
    }
    let context = ClipboardExecutionContext {
        principal_id: principal.id().to_owned(),
        request_id,
        desktop_id,
        desktop_generation,
    };
    context.validate(expected_desktop_id, expected_generation)?;
    Ok(context)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClipboardReadInvocationError {
    Submit(ClipboardSubmitError),
    Operation(ClipboardActorFailureKind),
    ReplyTimedOut,
    ReplyClosed,
    BlockingTaskFailed,
}

#[derive(Clone)]
struct RawClipboardRead {
    selection: SelectionName,
    revision: u64,
    payload_kind: ClipboardPayloadKind,
    bytes: Vec<u8>,
    evidence: RawClipboardEvidence,
}

impl fmt::Debug for RawClipboardRead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawClipboardRead")
            .field("selection", &self.selection)
            .field("revision", &self.revision)
            .field("payload_kind", &self.payload_kind)
            .field("bytes", &self.bytes.len())
            .field("content", &"[REDACTED]")
            .field("evidence", &self.evidence)
            .finish()
    }
}

#[derive(Clone)]
struct RawClipboardEvidence {
    target: RawClipboardTarget,
    transfer: SelectionTransferMode,
    content_length: u64,
    sha256: [u8; 32],
    owner_changed: bool,
    terminal_chunk_observed: bool,
    terminal: SelectionTransferTerminal,
}

impl fmt::Debug for RawClipboardEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawClipboardEvidence")
            .field("target", &self.target)
            .field("transfer", &self.transfer)
            .field("content_length", &self.content_length)
            .field("sha256", &"[REDACTED]")
            .field("owner_changed", &self.owner_changed)
            .field("terminal_chunk_observed", &self.terminal_chunk_observed)
            .field("terminal", &self.terminal)
            .finish()
    }
}

impl From<RawClipboardReadResult> for RawClipboardRead {
    fn from(raw: RawClipboardReadResult) -> Self {
        Self {
            selection: raw.selection,
            revision: raw.revision,
            payload_kind: raw.payload.kind(),
            bytes: raw.payload.expose_secret().to_vec(),
            evidence: RawClipboardEvidence {
                target: raw.evidence.target,
                transfer: raw.evidence.transfer,
                content_length: raw.evidence.content_length,
                sha256: *raw.evidence.sha256.as_bytes(),
                owner_changed: raw.evidence.owner_changed,
                terminal_chunk_observed: raw.evidence.terminal_chunk_observed,
                terminal: raw.evidence.terminal,
            },
        }
    }
}

trait RawClipboardReader: Send + Sync + 'static {
    fn read<'a>(
        &'a self,
        request: ClipboardReadRawRequest,
        deadline: Instant,
    ) -> AdapterFuture<'a, Result<RawClipboardRead, ClipboardReadInvocationError>>;
}

struct ActorClipboardReader {
    actor: ClipboardActorHandle,
}

impl RawClipboardReader for ActorClipboardReader {
    fn read<'a>(
        &'a self,
        request: ClipboardReadRawRequest,
        deadline: Instant,
    ) -> AdapterFuture<'a, Result<RawClipboardRead, ClipboardReadInvocationError>> {
        let submitted = self
            .actor
            .try_read(request)
            .map_err(ClipboardReadInvocationError::Submit);
        Box::pin(async move {
            let reply = submitted?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ClipboardReadInvocationError::ReplyTimedOut);
            }
            tokio::task::spawn_blocking(move || reply.recv_timeout(remaining))
                .await
                .map_err(|_| ClipboardReadInvocationError::BlockingTaskFailed)?
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                        ClipboardReadInvocationError::ReplyTimedOut
                    }
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        ClipboardReadInvocationError::ReplyClosed
                    }
                })?
                .map(Into::into)
                .map_err(|error| ClipboardReadInvocationError::Operation(error.kind))
        })
    }
}

#[derive(Clone, Debug)]
struct ClipboardArtifactContext {
    principal_id: String,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
}

struct SecretClipboardBytes(Vec<u8>);

impl fmt::Debug for SecretClipboardBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretClipboardBytes")
            .field("bytes", &self.0.len())
            .field("content", &"[REDACTED]")
            .finish()
    }
}

struct GeneratedClipboardArtifact {
    content_type: ArtifactContentType,
    bytes: SecretClipboardBytes,
}

impl fmt::Debug for GeneratedClipboardArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeneratedClipboardArtifact")
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes)
            .finish()
    }
}

trait ClipboardArtifactPublisher: Send + Sync + 'static {
    fn publish<'a>(
        &'a self,
        context: ClipboardArtifactContext,
        artifact: GeneratedClipboardArtifact,
    ) -> AdapterFuture<'a, Result<ArtifactRef, ControlPlaneError>>;
}

struct StoreClipboardArtifactPublisher {
    service: Arc<StoreArtifactService>,
}

impl ClipboardArtifactPublisher for StoreClipboardArtifactPublisher {
    fn publish<'a>(
        &'a self,
        context: ClipboardArtifactContext,
        artifact: GeneratedClipboardArtifact,
    ) -> AdapterFuture<'a, Result<ArtifactRef, ControlPlaneError>> {
        Box::pin(async move {
            if context.request_id.as_uuid().is_nil() {
                return Err(ControlPlaneError::InvalidRequest);
            }
            let context = InternalArtifactContext::new(
                context.principal_id,
                context.desktop_id,
                context.desktop_generation,
            )?;
            let request = GeneratedArtifactRequest::new(
                ArtifactPurpose::ClipboardOutput,
                artifact.content_type,
                artifact.bytes.0,
            )?;
            self.service.publish_generated(context, request).await
        })
    }
}

#[derive(Clone, Copy)]
enum ValidatedPayloadKind {
    Text,
    Binary(RawClipboardTarget),
}

struct ValidatedClipboardRead {
    selection: SelectionName,
    revision: u64,
    payload_kind: ValidatedPayloadKind,
    bytes: Vec<u8>,
    evidence: SelectionTransferEvidence,
    sha256: Sha256Digest,
}

fn map_request(
    request: &ClipboardReadRequest,
) -> Result<ClipboardReadRawRequest, ControlPlaneError> {
    let preferred_targets = request
        .preferred_targets
        .iter()
        .map(public_to_raw_target)
        .collect::<Result<Vec<_>, _>>()?;
    let raw = ClipboardReadRawRequest {
        selection: request.selection,
        preferred_targets,
        allow_binary_fallback: request.allow_binary_fallback,
    };
    raw.validate()
        .map_err(|_| ControlPlaneError::InvalidRequest)?;
    Ok(raw)
}

fn public_to_raw_target(target: &ClipboardTarget) -> Result<RawClipboardTarget, ControlPlaneError> {
    match target.as_str() {
        "UTF8_STRING" => Ok(RawClipboardTarget::Utf8String),
        "text/plain;charset=utf-8" => Ok(RawClipboardTarget::TextPlainUtf8),
        "text/plain" => Ok(RawClipboardTarget::TextPlain),
        "STRING" => Ok(RawClipboardTarget::String),
        "image/png" => Ok(RawClipboardTarget::ImagePng),
        "application/octet-stream" => Ok(RawClipboardTarget::ApplicationOctetStream),
        _ => Err(ControlPlaneError::InvalidRequest),
    }
}

fn raw_to_public_target(target: RawClipboardTarget) -> Result<ClipboardTarget, ControlPlaneError> {
    ClipboardTarget::new(match target {
        RawClipboardTarget::Utf8String => "UTF8_STRING",
        RawClipboardTarget::TextPlainUtf8 => "text/plain;charset=utf-8",
        RawClipboardTarget::TextPlain => "text/plain",
        RawClipboardTarget::String => "STRING",
        RawClipboardTarget::ImagePng => "image/png",
        RawClipboardTarget::ApplicationOctetStream => "application/octet-stream",
        RawClipboardTarget::Targets
        | RawClipboardTarget::Timestamp
        | RawClipboardTarget::Multiple => {
            return Err(ControlPlaneError::Internal);
        }
    })
    .map_err(|_| ControlPlaneError::Internal)
}

fn validate_raw_read(
    raw: RawClipboardRead,
    request: &ClipboardReadRawRequest,
) -> Result<ValidatedClipboardRead, ControlPlaneError> {
    let target = raw.evidence.target;
    let target_allowed = if request.preferred_targets.is_empty() {
        is_text_target(target)
    } else {
        request.preferred_targets.contains(&target)
    };
    let payload_kind = match raw.payload_kind {
        ClipboardPayloadKind::Utf8Text
            if is_text_target(target) && std::str::from_utf8(&raw.bytes).is_ok() =>
        {
            ValidatedPayloadKind::Text
        }
        ClipboardPayloadKind::Binary(binary_target)
            if is_binary_target(target) && binary_target == target =>
        {
            ValidatedPayloadKind::Binary(binary_target)
        }
        ClipboardPayloadKind::Binary(RawClipboardTarget::ApplicationOctetStream)
            if is_text_target(target)
                && request.allow_binary_fallback
                && std::str::from_utf8(&raw.bytes).is_err() =>
        {
            ValidatedPayloadKind::Binary(RawClipboardTarget::ApplicationOctetStream)
        }
        _ => return Err(ControlPlaneError::Internal),
    };
    let actual_digest: [u8; 32] = Sha256::digest(&raw.bytes).into();
    if raw.selection != request.selection
        || raw.revision == 0
        || !target_allowed
        || raw.evidence.content_length != u64::try_from(raw.bytes.len()).unwrap_or(u64::MAX)
        || raw.evidence.sha256 != actual_digest
    {
        return Err(ControlPlaneError::Internal);
    }
    let sha256 = digest_to_protocol(raw.evidence.sha256)?;
    let evidence = SelectionTransferEvidence {
        target: raw_to_public_target(target)?,
        transfer: raw.evidence.transfer,
        content_length: raw.evidence.content_length,
        sha256: sha256.clone(),
        owner_changed: raw.evidence.owner_changed,
        terminal_chunk_observed: raw.evidence.terminal_chunk_observed,
        terminal: raw.evidence.terminal,
    };
    if evidence.validate().is_err() || !evidence.completed() {
        return Err(ControlPlaneError::Internal);
    }
    Ok(ValidatedClipboardRead {
        selection: raw.selection,
        revision: raw.revision,
        payload_kind,
        bytes: raw.bytes,
        evidence,
        sha256,
    })
}

fn inline_delivery(
    kind: ValidatedPayloadKind,
    bytes: Vec<u8>,
    sha256: &Sha256Digest,
) -> Result<ClipboardReadDelivery, ControlPlaneError> {
    match kind {
        ValidatedPayloadKind::Text => {
            let text = String::from_utf8(bytes).map_err(|_| ControlPlaneError::Internal)?;
            Ok(ClipboardReadDelivery::InlineText {
                text: SecretInlineText::new(text).map_err(|_| ControlPlaneError::Internal)?,
            })
        }
        ValidatedPayloadKind::Binary(_) => {
            let decoded_length =
                u32::try_from(bytes.len()).map_err(|_| ControlPlaneError::Internal)?;
            let data =
                SecretInlineBinary::new(STANDARD.encode(bytes), decoded_length, sha256.clone())
                    .map_err(|_| ControlPlaneError::Internal)?;
            Ok(ClipboardReadDelivery::InlineBinary { data })
        }
    }
}

fn content_type_for(kind: ValidatedPayloadKind) -> Result<ArtifactContentType, ControlPlaneError> {
    ArtifactContentType::new(match kind {
        ValidatedPayloadKind::Text => UTF8_TEXT_CONTENT_TYPE,
        ValidatedPayloadKind::Binary(RawClipboardTarget::ImagePng) => PNG_CONTENT_TYPE,
        ValidatedPayloadKind::Binary(RawClipboardTarget::ApplicationOctetStream) => {
            BINARY_CONTENT_TYPE
        }
        ValidatedPayloadKind::Binary(_) => return Err(ControlPlaneError::Internal),
    })
    .map_err(|_| ControlPlaneError::Internal)
}

fn validate_artifact(
    artifact: &ArtifactRef,
    content_type: &ArtifactContentType,
    content_length: u64,
    sha256: &Sha256Digest,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<(), ControlPlaneError> {
    if artifact.validate().is_err()
        || artifact.purpose != ArtifactPurpose::ClipboardOutput
        || artifact.desktop_id != desktop_id
        || artifact.desktop_generation != desktop_generation
        || artifact.content_type != *content_type
        || artifact.content_length != content_length
        || artifact.sha256 != *sha256
    {
        return Err(ControlPlaneError::Internal);
    }
    Ok(())
}

fn is_text_target(target: RawClipboardTarget) -> bool {
    matches!(
        target,
        RawClipboardTarget::Utf8String
            | RawClipboardTarget::TextPlainUtf8
            | RawClipboardTarget::TextPlain
            | RawClipboardTarget::String
    )
}

fn is_binary_target(target: RawClipboardTarget) -> bool {
    matches!(
        target,
        RawClipboardTarget::ImagePng | RawClipboardTarget::ApplicationOctetStream
    )
}

fn digest_to_protocol(bytes: [u8; 32]) -> Result<Sha256Digest, ControlPlaneError> {
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").map_err(|_| ControlPlaneError::Internal)?;
    }
    Sha256Digest::new(encoded).map_err(|_| ControlPlaneError::Internal)
}

fn map_read_error(error: ClipboardReadInvocationError) -> ControlPlaneError {
    match error {
        ClipboardReadInvocationError::Submit(ClipboardSubmitError::QueueFull)
        | ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::ControlQueueFull) => {
            ControlPlaneError::ResourceExhausted
        }
        ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::SelectionHasNoOwner) => {
            ControlPlaneError::NotFound
        }
        ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::TargetUnsupported)
        | ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::ProtocolViolation)
        | ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::RequestorDestroyed) => {
            ControlPlaneError::UnsupportedByTarget
        }
        ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::OwnerChanged)
        | ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::OwnershipRace) => {
            ControlPlaneError::LeaseConflict
        }
        ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::SelectionTooLarge)
        | ClipboardReadInvocationError::Submit(ClipboardSubmitError::InvalidRequest(_)) => {
            ControlPlaneError::InvalidRequest
        }
        ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::TransferTimeout)
        | ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::BackendUnavailable)
        | ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::ActorPoisoned)
        | ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::ActorStopped)
        | ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::ActorPanicked)
        | ClipboardReadInvocationError::Submit(ClipboardSubmitError::Closed)
        | ClipboardReadInvocationError::ReplyTimedOut
        | ClipboardReadInvocationError::ReplyClosed
        | ClipboardReadInvocationError::BlockingTaskFailed => {
            ControlPlaneError::CapabilityUnavailable
        }
    }
}

#[cfg(test)]
#[path = "clipboard_service_tests.rs"]
mod tests;
