#![allow(clippy::expect_used, clippy::panic)]

use std::{
    collections::VecDeque,
    future::pending,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use xenoteer_protocol::{
    ArtifactContentType, ArtifactId, ArtifactPurpose, ArtifactRef, ClipboardReadDelivery,
    ClipboardReadRequest, DesktopGeneration, DesktopId, MAX_INLINE_CLIPBOARD_BYTES, RequestId,
    SelectionName, SelectionTransferFailureReason, SelectionTransferMode,
    SelectionTransferTerminal, Timestamp,
};
use xenoteer_server::{ControlPlaneError, Grant, Principal};
use xenoteer_x11::{
    ClipboardActorFailureKind, ClipboardPayloadKind, ClipboardReadRawRequest,
    ClipboardRequestError, ClipboardSubmitError, RawClipboardTarget,
};

use super::*;

enum ReaderResponse {
    Ready(Result<RawClipboardRead, ClipboardReadInvocationError>),
    Pending,
}

struct FakeReader {
    responses: Mutex<VecDeque<ReaderResponse>>,
    calls: Mutex<Vec<ClipboardReadRawRequest>>,
}

impl FakeReader {
    fn new(responses: impl IntoIterator<Item = ReaderResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("calls lock").len()
    }

    fn calls(&self) -> Vec<ClipboardReadRawRequest> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl RawClipboardReader for FakeReader {
    fn read<'a>(
        &'a self,
        request: ClipboardReadRawRequest,
        _deadline: Instant,
    ) -> AdapterFuture<'a, Result<RawClipboardRead, ClipboardReadInvocationError>> {
        self.calls.lock().expect("calls lock").push(request);
        let response = self
            .responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .expect("scripted reader response");
        Box::pin(async move {
            match response {
                ReaderResponse::Ready(result) => result,
                ReaderResponse::Pending => pending().await,
            }
        })
    }
}

#[derive(Clone)]
enum PublisherMode {
    Correct,
    Fixed(Result<ArtifactRef, ControlPlaneError>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PublishedCall {
    principal_id: String,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    content_type: String,
    content_length: u64,
    sha256: [u8; 32],
}

struct FakePublisher {
    mode: Mutex<PublisherMode>,
    calls: Mutex<Vec<PublishedCall>>,
}

impl FakePublisher {
    fn new(mode: PublisherMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<PublishedCall> {
        self.calls.lock().expect("publisher calls lock").clone()
    }
}

impl ClipboardArtifactPublisher for FakePublisher {
    fn publish<'a>(
        &'a self,
        context: ClipboardArtifactContext,
        artifact: GeneratedClipboardArtifact,
    ) -> AdapterFuture<'a, Result<ArtifactRef, ControlPlaneError>> {
        let sha256: [u8; 32] = Sha256::digest(&artifact.bytes.0).into();
        let content_length = u64::try_from(artifact.bytes.0.len()).expect("test length fits");
        let call = PublishedCall {
            principal_id: context.principal_id,
            request_id: context.request_id,
            desktop_id: context.desktop_id,
            desktop_generation: context.desktop_generation,
            content_type: artifact.content_type.as_str().to_owned(),
            content_length,
            sha256,
        };
        self.calls
            .lock()
            .expect("publisher calls lock")
            .push(call.clone());
        let result = match self.mode.lock().expect("publisher mode lock").clone() {
            PublisherMode::Correct => Ok(artifact_ref(
                call.desktop_id,
                call.desktop_generation,
                ArtifactPurpose::ClipboardOutput,
                &call.content_type,
                call.content_length,
                call.sha256,
            )),
            PublisherMode::Fixed(result) => result,
        };
        Box::pin(async move { result })
    }
}

fn scope() -> (DesktopId, DesktopGeneration) {
    (DesktopId::new(), DesktopGeneration::new())
}

fn execution(desktop_id: DesktopId, generation: DesktopGeneration) -> ClipboardExecutionContext {
    ClipboardExecutionContext {
        principal_id: "alice".to_owned(),
        request_id: RequestId::new(),
        desktop_id,
        desktop_generation: generation,
    }
}

fn request(
    selection: SelectionName,
    targets: &[&str],
    allow_binary_fallback: bool,
) -> ClipboardReadRequest {
    ClipboardReadRequest {
        selection,
        preferred_targets: targets
            .iter()
            .map(|target| ClipboardTarget::new(*target).expect("supported target"))
            .collect(),
        allow_binary_fallback,
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn raw_read(
    selection: SelectionName,
    payload_kind: ClipboardPayloadKind,
    bytes: Vec<u8>,
    target: RawClipboardTarget,
) -> RawClipboardRead {
    let sha256 = digest(&bytes);
    RawClipboardRead {
        selection,
        revision: 7,
        payload_kind,
        evidence: RawClipboardEvidence {
            target,
            transfer: SelectionTransferMode::Direct,
            content_length: u64::try_from(bytes.len()).expect("test length fits"),
            sha256,
            owner_changed: false,
            terminal_chunk_observed: false,
            terminal: SelectionTransferTerminal::Completed,
        },
        bytes,
    }
}

fn artifact_ref(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    purpose: ArtifactPurpose,
    content_type: &str,
    content_length: u64,
    sha256: [u8; 32],
) -> ArtifactRef {
    ArtifactRef {
        artifact_id: ArtifactId::new(),
        purpose,
        desktop_id,
        desktop_generation,
        content_type: ArtifactContentType::new(content_type).expect("test content type"),
        content_length,
        sha256: digest_to_protocol(sha256).expect("test digest"),
        created_at: Timestamp::parse("2026-07-21T00:00:00Z").expect("created timestamp"),
        expires_at: Timestamp::parse("2026-07-21T00:05:00Z").expect("expiry timestamp"),
    }
}

fn fixture_service(
    response: ReaderResponse,
    publisher_mode: PublisherMode,
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    settings: ClipboardReadServiceSettings,
) -> (
    DaemonClipboardReadService,
    Arc<FakeReader>,
    Arc<FakePublisher>,
) {
    let reader = Arc::new(FakeReader::new([response]));
    let publisher = Arc::new(FakePublisher::new(publisher_mode));
    let service = DaemonClipboardReadService::with_components(
        reader.clone(),
        publisher.clone(),
        desktop_id,
        generation,
        settings,
    );
    (service, reader, publisher)
}

#[test]
fn all_public_target_mappings_and_default_preferences_are_exact() {
    let cases = [
        ("UTF8_STRING", RawClipboardTarget::Utf8String),
        (
            "text/plain;charset=utf-8",
            RawClipboardTarget::TextPlainUtf8,
        ),
        ("text/plain", RawClipboardTarget::TextPlain),
        ("STRING", RawClipboardTarget::String),
        ("image/png", RawClipboardTarget::ImagePng),
        (
            "application/octet-stream",
            RawClipboardTarget::ApplicationOctetStream,
        ),
    ];
    for (public, raw_target) in cases {
        let mapped = map_request(&request(SelectionName::Clipboard, &[public], false))
            .expect("public target maps");
        assert_eq!(mapped.preferred_targets, vec![raw_target]);
        assert_eq!(
            raw_to_public_target(raw_target)
                .expect("content target maps")
                .as_str(),
            public
        );
    }

    let default = map_request(&request(SelectionName::Primary, &[], false))
        .expect("empty preferences retain actor default");
    assert_eq!(default.selection, SelectionName::Primary);
    assert!(default.preferred_targets.is_empty());
    for internal in [
        RawClipboardTarget::Targets,
        RawClipboardTarget::Timestamp,
        RawClipboardTarget::Multiple,
    ] {
        assert_eq!(
            raw_to_public_target(internal),
            Err(ControlPlaneError::Internal)
        );
    }
}

#[tokio::test]
async fn inline_text_and_reviewed_binary_are_integrity_checked() {
    let (desktop_id, generation) = scope();
    let text = raw_read(
        SelectionName::Clipboard,
        ClipboardPayloadKind::Utf8Text,
        b"hello clipboard".to_vec(),
        RawClipboardTarget::Utf8String,
    );
    let (service, _, publisher) = fixture_service(
        ReaderResponse::Ready(Ok(text)),
        PublisherMode::Correct,
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    let result = service
        .read_authorized(
            execution(desktop_id, generation),
            request(SelectionName::Clipboard, &["UTF8_STRING"], false),
        )
        .await
        .expect("valid text read");
    match result.content {
        ClipboardReadDelivery::InlineText { text } => {
            assert_eq!(text.expose_secret(), "hello clipboard");
        }
        other => panic!("unexpected delivery: {other:?}"),
    }
    assert!(publisher.calls().is_empty());

    let binary = raw_read(
        SelectionName::Clipboard,
        ClipboardPayloadKind::Binary(RawClipboardTarget::ImagePng),
        vec![1, 2],
        RawClipboardTarget::ImagePng,
    );
    let (service, _, publisher) = fixture_service(
        ReaderResponse::Ready(Ok(binary)),
        PublisherMode::Correct,
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    let result = service
        .read_authorized(
            execution(desktop_id, generation),
            request(SelectionName::Clipboard, &["image/png"], false),
        )
        .await
        .expect("valid binary read");
    match result.content {
        ClipboardReadDelivery::InlineBinary { data } => {
            assert_eq!(data.expose_base64_secret(), "AQI=");
            assert_eq!(data.decoded_length(), 2);
            data.validate().expect("checked base64");
        }
        other => panic!("unexpected delivery: {other:?}"),
    }
    assert!(publisher.calls().is_empty());
}

#[tokio::test]
async fn completed_incr_terminal_evidence_is_preserved_exactly() {
    let (desktop_id, generation) = scope();
    let mut raw = raw_read(
        SelectionName::Primary,
        ClipboardPayloadKind::Utf8Text,
        b"chunked".to_vec(),
        RawClipboardTarget::TextPlain,
    );
    raw.evidence.transfer = SelectionTransferMode::Incr {
        announced_minimum_bytes: 7,
        chunks: 2,
    };
    raw.evidence.terminal_chunk_observed = true;
    let (service, _, publisher) = fixture_service(
        ReaderResponse::Ready(Ok(raw)),
        PublisherMode::Correct,
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    let result = service
        .read_authorized(
            execution(desktop_id, generation),
            request(SelectionName::Primary, &["text/plain"], false),
        )
        .await
        .expect("completed INCR read");
    assert_eq!(
        result.evidence.transfer,
        SelectionTransferMode::Incr {
            announced_minimum_bytes: 7,
            chunks: 2,
        }
    );
    assert!(result.evidence.terminal_chunk_observed);
    assert_eq!(
        result.evidence.terminal,
        SelectionTransferTerminal::Completed
    );
    assert!(publisher.calls().is_empty());
}

#[tokio::test]
async fn invalid_utf8_binary_fallback_requires_explicit_opt_in() {
    let (desktop_id, generation) = scope();
    let fallback = || {
        raw_read(
            SelectionName::Clipboard,
            ClipboardPayloadKind::Binary(RawClipboardTarget::ApplicationOctetStream),
            vec![0xff, 0xfe],
            RawClipboardTarget::Utf8String,
        )
    };
    let (service, _, publisher) = fixture_service(
        ReaderResponse::Ready(Ok(fallback())),
        PublisherMode::Correct,
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    assert_eq!(
        service
            .read_authorized(
                execution(desktop_id, generation),
                request(SelectionName::Clipboard, &["UTF8_STRING"], false),
            )
            .await,
        Err(ControlPlaneError::Internal)
    );
    assert!(publisher.calls().is_empty());

    let (service, _, publisher) = fixture_service(
        ReaderResponse::Ready(Ok(fallback())),
        PublisherMode::Correct,
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    let result = service
        .read_authorized(
            execution(desktop_id, generation),
            request(SelectionName::Clipboard, &["UTF8_STRING"], true),
        )
        .await
        .expect("explicit binary fallback");
    assert!(matches!(
        result.content,
        ClipboardReadDelivery::InlineBinary { .. }
    ));
    assert!(publisher.calls().is_empty());

    let incorrectly_classified = raw_read(
        SelectionName::Clipboard,
        ClipboardPayloadKind::Binary(RawClipboardTarget::ApplicationOctetStream),
        b"valid UTF-8 must remain text".to_vec(),
        RawClipboardTarget::Utf8String,
    );
    let (service, _, publisher) = fixture_service(
        ReaderResponse::Ready(Ok(incorrectly_classified)),
        PublisherMode::Correct,
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    assert_eq!(
        service
            .read_authorized(
                execution(desktop_id, generation),
                request(SelectionName::Clipboard, &["UTF8_STRING"], true),
            )
            .await,
        Err(ControlPlaneError::Internal)
    );
    assert!(publisher.calls().is_empty());
}

#[tokio::test]
async fn inline_threshold_and_artifact_boundary_are_exact_and_private() {
    let (desktop_id, generation) = scope();
    let inline_bytes = vec![0x42; MAX_INLINE_CLIPBOARD_BYTES];
    let inline = raw_read(
        SelectionName::Clipboard,
        ClipboardPayloadKind::Binary(RawClipboardTarget::ApplicationOctetStream),
        inline_bytes,
        RawClipboardTarget::ApplicationOctetStream,
    );
    let (service, _, publisher) = fixture_service(
        ReaderResponse::Ready(Ok(inline)),
        PublisherMode::Correct,
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    let result = service
        .read_authorized(
            execution(desktop_id, generation),
            request(
                SelectionName::Clipboard,
                &["application/octet-stream"],
                false,
            ),
        )
        .await
        .expect("ceiling remains inline");
    assert!(matches!(
        result.content,
        ClipboardReadDelivery::InlineBinary { .. }
    ));
    assert!(publisher.calls().is_empty());

    let artifact_bytes = vec![0x51; MAX_INLINE_CLIPBOARD_BYTES + 1];
    let expected_digest = digest(&artifact_bytes);
    let artifact_read = raw_read(
        SelectionName::Clipboard,
        ClipboardPayloadKind::Binary(RawClipboardTarget::ApplicationOctetStream),
        artifact_bytes,
        RawClipboardTarget::ApplicationOctetStream,
    );
    let (service, _, publisher) = fixture_service(
        ReaderResponse::Ready(Ok(artifact_read)),
        PublisherMode::Correct,
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    let context = execution(desktop_id, generation);
    let expected_request_id = context.request_id;
    let result = service
        .read_authorized(
            context,
            request(
                SelectionName::Clipboard,
                &["application/octet-stream"],
                false,
            ),
        )
        .await
        .expect("over-ceiling body becomes artifact");
    let ClipboardReadDelivery::Artifact { artifact } = result.content else {
        panic!("expected artifact delivery")
    };
    assert_eq!(artifact.purpose, ArtifactPurpose::ClipboardOutput);
    assert_eq!(artifact.desktop_id, desktop_id);
    assert_eq!(artifact.desktop_generation, generation);
    assert_eq!(artifact.content_type.as_str(), BINARY_CONTENT_TYPE);
    assert_eq!(
        artifact.content_length,
        u64::try_from(MAX_INLINE_CLIPBOARD_BYTES + 1).expect("test length fits")
    );
    let calls = publisher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].principal_id, "alice");
    assert_eq!(calls[0].request_id, expected_request_id);
    assert_eq!(calls[0].desktop_id, desktop_id);
    assert_eq!(calls[0].desktop_generation, generation);
    assert_eq!(calls[0].content_type, BINARY_CONTENT_TYPE);
    assert_eq!(calls[0].sha256, expected_digest);
}

#[tokio::test]
async fn artifact_publication_uses_truthful_media_type_for_every_payload_class() {
    let (desktop_id, generation) = scope();
    let cases = [
        (
            ClipboardPayloadKind::Utf8Text,
            RawClipboardTarget::Utf8String,
            "UTF8_STRING",
            false,
            b't',
            UTF8_TEXT_CONTENT_TYPE,
        ),
        (
            ClipboardPayloadKind::Binary(RawClipboardTarget::ImagePng),
            RawClipboardTarget::ImagePng,
            "image/png",
            false,
            0x89,
            PNG_CONTENT_TYPE,
        ),
        (
            ClipboardPayloadKind::Binary(RawClipboardTarget::ApplicationOctetStream),
            RawClipboardTarget::ApplicationOctetStream,
            "application/octet-stream",
            false,
            0x44,
            BINARY_CONTENT_TYPE,
        ),
        (
            ClipboardPayloadKind::Binary(RawClipboardTarget::ApplicationOctetStream),
            RawClipboardTarget::Utf8String,
            "UTF8_STRING",
            true,
            0xff,
            BINARY_CONTENT_TYPE,
        ),
    ];
    for (payload_kind, raw_target, public_target, fallback, fill, expected_type) in cases {
        let raw = raw_read(
            SelectionName::Clipboard,
            payload_kind,
            vec![fill; MAX_INLINE_CLIPBOARD_BYTES + 1],
            raw_target,
        );
        let (service, _, publisher) = fixture_service(
            ReaderResponse::Ready(Ok(raw)),
            PublisherMode::Correct,
            desktop_id,
            generation,
            ClipboardReadServiceSettings::default(),
        );
        let result = service
            .read_authorized(
                execution(desktop_id, generation),
                request(SelectionName::Clipboard, &[public_target], fallback),
            )
            .await
            .expect("large payload publishes");
        let ClipboardReadDelivery::Artifact { artifact } = result.content else {
            panic!("expected artifact delivery")
        };
        assert_eq!(artifact.content_type.as_str(), expected_type);
        let calls = publisher.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].content_type, expected_type);
    }
}

#[tokio::test]
async fn malformed_raw_evidence_and_kind_combinations_are_rejected() {
    let (desktop_id, generation) = scope();
    let valid = || {
        raw_read(
            SelectionName::Clipboard,
            ClipboardPayloadKind::Utf8Text,
            b"evidence".to_vec(),
            RawClipboardTarget::Utf8String,
        )
    };
    let mut malformed = Vec::new();

    let mut value = valid();
    value.selection = SelectionName::Primary;
    malformed.push(value);
    let mut value = valid();
    value.revision = 0;
    malformed.push(value);
    let mut value = valid();
    value.evidence.target = RawClipboardTarget::ImagePng;
    malformed.push(value);
    let mut value = valid();
    value.evidence.content_length += 1;
    malformed.push(value);
    let mut value = valid();
    value.evidence.sha256 = [0x44; 32];
    malformed.push(value);
    let mut value = valid();
    value.evidence.owner_changed = true;
    malformed.push(value);
    let mut value = valid();
    value.evidence.terminal_chunk_observed = true;
    malformed.push(value);
    let mut value = valid();
    value.evidence.terminal = SelectionTransferTerminal::Failed {
        reason: SelectionTransferFailureReason::Timeout,
    };
    malformed.push(value);
    let mut value = valid();
    value.payload_kind = ClipboardPayloadKind::Binary(RawClipboardTarget::ImagePng);
    malformed.push(value);

    for raw in malformed {
        let (service, reader, publisher) = fixture_service(
            ReaderResponse::Ready(Ok(raw)),
            PublisherMode::Correct,
            desktop_id,
            generation,
            ClipboardReadServiceSettings::default(),
        );
        assert_eq!(
            service
                .read_authorized(
                    execution(desktop_id, generation),
                    request(SelectionName::Clipboard, &["UTF8_STRING"], false),
                )
                .await,
            Err(ControlPlaneError::Internal)
        );
        assert_eq!(reader.call_count(), 1);
        assert!(publisher.calls().is_empty());
    }
}

#[tokio::test]
async fn read_timeout_and_typed_actor_errors_map_without_publication() {
    let (desktop_id, generation) = scope();
    let mappings = [
        (
            ClipboardReadInvocationError::Submit(ClipboardSubmitError::QueueFull),
            ControlPlaneError::ResourceExhausted,
        ),
        (
            ClipboardReadInvocationError::Submit(ClipboardSubmitError::InvalidRequest(
                ClipboardRequestError::InvalidTarget,
            )),
            ControlPlaneError::InvalidRequest,
        ),
        (
            ClipboardReadInvocationError::Submit(ClipboardSubmitError::Closed),
            ControlPlaneError::CapabilityUnavailable,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::SelectionHasNoOwner),
            ControlPlaneError::NotFound,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::TargetUnsupported),
            ControlPlaneError::UnsupportedByTarget,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::OwnerChanged),
            ControlPlaneError::LeaseConflict,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::SelectionTooLarge),
            ControlPlaneError::InvalidRequest,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::TransferTimeout),
            ControlPlaneError::CapabilityUnavailable,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::ProtocolViolation),
            ControlPlaneError::UnsupportedByTarget,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::RequestorDestroyed),
            ControlPlaneError::UnsupportedByTarget,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::OwnershipRace),
            ControlPlaneError::LeaseConflict,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::BackendUnavailable),
            ControlPlaneError::CapabilityUnavailable,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::ActorPoisoned),
            ControlPlaneError::CapabilityUnavailable,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::ActorStopped),
            ControlPlaneError::CapabilityUnavailable,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::ActorPanicked),
            ControlPlaneError::CapabilityUnavailable,
        ),
        (
            ClipboardReadInvocationError::Operation(ClipboardActorFailureKind::ControlQueueFull),
            ControlPlaneError::ResourceExhausted,
        ),
        (
            ClipboardReadInvocationError::ReplyTimedOut,
            ControlPlaneError::CapabilityUnavailable,
        ),
        (
            ClipboardReadInvocationError::ReplyClosed,
            ControlPlaneError::CapabilityUnavailable,
        ),
        (
            ClipboardReadInvocationError::BlockingTaskFailed,
            ControlPlaneError::CapabilityUnavailable,
        ),
    ];
    for (input, expected) in mappings {
        assert_eq!(map_read_error(input), expected);
        let (service, reader, publisher) = fixture_service(
            ReaderResponse::Ready(Err(input)),
            PublisherMode::Correct,
            desktop_id,
            generation,
            ClipboardReadServiceSettings::default(),
        );
        assert_eq!(
            service
                .read_authorized(
                    execution(desktop_id, generation),
                    request(SelectionName::Clipboard, &[], false),
                )
                .await,
            Err(expected)
        );
        assert_eq!(reader.call_count(), 1);
        assert!(publisher.calls().is_empty());
    }

    let settings = ClipboardReadServiceSettings::new(Duration::from_millis(5))
        .expect("small nonzero test timeout");
    let (service, reader, publisher) = fixture_service(
        ReaderResponse::Pending,
        PublisherMode::Correct,
        desktop_id,
        generation,
        settings,
    );
    assert_eq!(
        service
            .read_authorized(
                execution(desktop_id, generation),
                request(SelectionName::Clipboard, &[], false),
            )
            .await,
        Err(ControlPlaneError::CapabilityUnavailable)
    );
    assert_eq!(reader.call_count(), 1);
    assert!(publisher.calls().is_empty());
}

#[tokio::test]
async fn publisher_failures_and_mismatched_references_are_rejected() {
    let (desktop_id, generation) = scope();
    let bytes = vec![0x61; MAX_INLINE_CLIPBOARD_BYTES + 1];
    let expected_digest = digest(&bytes);
    let valid_ref = || {
        artifact_ref(
            desktop_id,
            generation,
            ArtifactPurpose::ClipboardOutput,
            UTF8_TEXT_CONTENT_TYPE,
            u64::try_from(bytes.len()).expect("test length fits"),
            expected_digest,
        )
    };
    let mut mismatches = Vec::new();
    let mut artifact = valid_ref();
    artifact.purpose = ArtifactPurpose::Screenshot;
    mismatches.push(artifact);
    let mut artifact = valid_ref();
    artifact.desktop_id = DesktopId::new();
    mismatches.push(artifact);
    let mut artifact = valid_ref();
    artifact.desktop_generation = DesktopGeneration::new();
    mismatches.push(artifact);
    let mut artifact = valid_ref();
    artifact.content_type = ArtifactContentType::new(PNG_CONTENT_TYPE).expect("PNG type");
    mismatches.push(artifact);
    let mut artifact = valid_ref();
    artifact.content_length += 1;
    mismatches.push(artifact);
    let mut artifact = valid_ref();
    artifact.sha256 = digest_to_protocol([0x11; 32]).expect("alternate digest");
    mismatches.push(artifact);

    for artifact in mismatches {
        let raw = raw_read(
            SelectionName::Clipboard,
            ClipboardPayloadKind::Utf8Text,
            bytes.clone(),
            RawClipboardTarget::Utf8String,
        );
        let (service, _, publisher) = fixture_service(
            ReaderResponse::Ready(Ok(raw)),
            PublisherMode::Fixed(Ok(artifact)),
            desktop_id,
            generation,
            ClipboardReadServiceSettings::default(),
        );
        assert_eq!(
            service
                .read_authorized(
                    execution(desktop_id, generation),
                    request(SelectionName::Clipboard, &["UTF8_STRING"], false),
                )
                .await,
            Err(ControlPlaneError::Internal)
        );
        assert_eq!(publisher.calls().len(), 1);
    }

    let raw = raw_read(
        SelectionName::Clipboard,
        ClipboardPayloadKind::Utf8Text,
        bytes,
        RawClipboardTarget::Utf8String,
    );
    let (service, _, publisher) = fixture_service(
        ReaderResponse::Ready(Ok(raw)),
        PublisherMode::Fixed(Err(ControlPlaneError::ResourceExhausted)),
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    assert_eq!(
        service
            .read_authorized(
                execution(desktop_id, generation),
                request(SelectionName::Clipboard, &["UTF8_STRING"], false),
            )
            .await,
        Err(ControlPlaneError::ResourceExhausted)
    );
    assert_eq!(publisher.calls().len(), 1);
}

#[test]
fn grant_generation_and_owner_scope_are_revalidated_near_effect() {
    let (desktop_id, generation) = scope();
    let request_id = RequestId::new();
    let no_grant = Principal::new("alice", []).expect("principal");
    assert_eq!(
        authorize_context(
            &no_grant, request_id, desktop_id, generation, desktop_id, generation,
        )
        .expect_err("missing grant is rejected"),
        ControlPlaneError::PermissionDenied
    );

    let principal = Principal::new("alice", [Grant::ClipboardRead]).expect("principal");
    assert_eq!(
        authorize_context(
            &principal,
            request_id,
            DesktopId::new(),
            generation,
            desktop_id,
            generation,
        )
        .expect_err("wrong desktop is rejected"),
        ControlPlaneError::PermissionDenied
    );
    assert_eq!(
        authorize_context(
            &principal,
            request_id,
            desktop_id,
            DesktopGeneration::new(),
            desktop_id,
            generation,
        )
        .expect_err("stale generation is rejected"),
        ControlPlaneError::StaleReference {
            current_generation: Some(generation),
        }
    );
    let context = authorize_context(
        &principal, request_id, desktop_id, generation, desktop_id, generation,
    )
    .expect("authorized scope");
    assert_eq!(context.principal_id, "alice");
    assert_eq!(context.request_id, request_id);
    assert_eq!(context.desktop_id, desktop_id);
    assert_eq!(context.desktop_generation, generation);
}

#[tokio::test]
async fn invalid_execution_scope_stops_before_actor_dispatch() {
    let (desktop_id, generation) = scope();
    let raw = raw_read(
        SelectionName::Clipboard,
        ClipboardPayloadKind::Utf8Text,
        b"never dispatched".to_vec(),
        RawClipboardTarget::Utf8String,
    );
    let (service, reader, publisher) = fixture_service(
        ReaderResponse::Ready(Ok(raw)),
        PublisherMode::Correct,
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    let mut context = execution(desktop_id, generation);
    context.desktop_generation = DesktopGeneration::new();
    assert_eq!(
        service
            .read_authorized(
                context,
                request(SelectionName::Clipboard, &["UTF8_STRING"], false),
            )
            .await,
        Err(ControlPlaneError::StaleReference {
            current_generation: Some(generation),
        })
    );
    assert_eq!(reader.call_count(), 0);
    assert!(publisher.calls().is_empty());
}

#[test]
fn diagnostics_redact_clipboard_bytes_and_encoded_content() {
    let canary = b"clipboard-canary-private".to_vec();
    let raw = raw_read(
        SelectionName::Clipboard,
        ClipboardPayloadKind::Utf8Text,
        canary.clone(),
        RawClipboardTarget::Utf8String,
    );
    let raw_debug = format!("{raw:?}");
    assert!(!raw_debug.contains("clipboard-canary-private"));
    assert!(raw_debug.contains("[REDACTED]"));

    let bytes = SecretClipboardBytes(canary);
    let bytes_debug = format!("{bytes:?}");
    assert!(!bytes_debug.contains("clipboard-canary-private"));
    assert!(bytes_debug.contains("[REDACTED]"));
    let publication = GeneratedClipboardArtifact {
        content_type: ArtifactContentType::new(UTF8_TEXT_CONTENT_TYPE).expect("text type"),
        bytes,
    };
    let publication_debug = format!("{publication:?}");
    assert!(!publication_debug.contains("clipboard-canary-private"));
    assert!(publication_debug.contains("[REDACTED]"));
}

#[test]
fn settings_reject_zero_and_over_ceiling_timeouts() {
    assert_eq!(
        ClipboardReadServiceSettings::new(Duration::ZERO),
        Err(ClipboardReadSettingsError)
    );
    assert_eq!(
        ClipboardReadServiceSettings::new(MAX_CLIPBOARD_READ_TIMEOUT + Duration::from_nanos(1)),
        Err(ClipboardReadSettingsError)
    );
    assert_eq!(
        ClipboardReadServiceSettings::new(MAX_CLIPBOARD_READ_TIMEOUT)
            .expect("ceiling is allowed")
            .total_timeout,
        MAX_CLIPBOARD_READ_TIMEOUT
    );
}

#[test]
fn request_mapping_preserves_order_selection_and_fallback_flag() {
    let mapped = map_request(&request(
        SelectionName::Primary,
        &["image/png", "UTF8_STRING", "application/octet-stream"],
        true,
    ))
    .expect("request maps");
    assert_eq!(mapped.selection, SelectionName::Primary);
    assert_eq!(
        mapped.preferred_targets,
        vec![
            RawClipboardTarget::ImagePng,
            RawClipboardTarget::Utf8String,
            RawClipboardTarget::ApplicationOctetStream,
        ]
    );
    assert!(mapped.allow_binary_fallback);
}

#[tokio::test]
async fn reader_receives_one_bounded_request_only() {
    let (desktop_id, generation) = scope();
    let raw = raw_read(
        SelectionName::Primary,
        ClipboardPayloadKind::Utf8Text,
        b"one call".to_vec(),
        RawClipboardTarget::TextPlainUtf8,
    );
    let (service, reader, _) = fixture_service(
        ReaderResponse::Ready(Ok(raw)),
        PublisherMode::Correct,
        desktop_id,
        generation,
        ClipboardReadServiceSettings::default(),
    );
    service
        .read_authorized(
            execution(desktop_id, generation),
            request(SelectionName::Primary, &["text/plain;charset=utf-8"], false),
        )
        .await
        .expect("read succeeds");
    let calls = reader.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].selection, SelectionName::Primary);
    assert_eq!(
        calls[0].preferred_targets,
        vec![RawClipboardTarget::TextPlainUtf8]
    );
}
