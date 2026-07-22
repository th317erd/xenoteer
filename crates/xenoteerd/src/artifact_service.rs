//! Async, bounded adapter between artifact HTTP bodies and the private store.

use std::{
    error::Error,
    fmt,
    fs::File,
    io::{self, Cursor, Read},
    str::FromStr,
    sync::{Arc, mpsc as std_mpsc},
    time::Duration,
};

use axum::body::{Body, Bytes};
use futures_util::{StreamExt, stream};
use tokio::sync::{mpsc, oneshot};
use xenoteer_artifacts::{
    ArtifactCreate as StoreCreate, ArtifactId as StoreArtifactId,
    ArtifactMetadata as StoreMetadata, ArtifactOwner, ArtifactPurpose as StorePurpose,
    ArtifactScope, ArtifactStore, CapabilityProvenance, Clock,
    DesktopGeneration as StoreGeneration, Sha256Digest as StoreDigest, StoreError, SystemClock,
    TimestampMillis,
};
use xenoteer_protocol::{
    ArtifactContentType, ArtifactId, ArtifactPurpose, ArtifactRef, DesktopGeneration, DesktopId,
    Sha256Digest, Timestamp,
};
use xenoteer_server::{
    ArtifactAccessRequest, ArtifactDownload, ArtifactFuture, ArtifactPurposeSet,
    ArtifactRequestContext, ArtifactService, ArtifactUploadRequest, ControlPlaneError, Grant,
};

const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const STREAM_CHANNEL_CHUNKS: usize = 2;
const DEFAULT_CLIPBOARD_INPUT_RETENTION: Duration = Duration::from_secs(15 * 60);
const DEFAULT_GENERATED_ARTIFACT_RETENTION: Duration = Duration::from_secs(60 * 60);
const DEFAULT_UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Retention selected by the daemon for caller-supplied clipboard bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactRetentionPolicy {
    clipboard_input: Duration,
    generated: Duration,
}

impl ArtifactRetentionPolicy {
    /// Creates a non-zero clipboard-input retention policy representable in
    /// the store's millisecond timestamp format.
    pub fn new(clipboard_input: Duration) -> Result<Self, RetentionPolicyError> {
        let millis = clipboard_input.as_millis();
        if millis == 0 || u64::try_from(millis).is_err() {
            return Err(RetentionPolicyError);
        }
        Ok(Self {
            clipboard_input,
            generated: DEFAULT_GENERATED_ARTIFACT_RETENTION,
        })
    }

    /// Selects the retention used for daemon-produced clipboard-output and
    /// screenshot artifacts. The configured store maximum remains the final
    /// hard ceiling enforced by the artifact store.
    pub fn with_generated_retention(
        mut self,
        generated: Duration,
    ) -> Result<Self, RetentionPolicyError> {
        let millis = generated.as_millis();
        if millis == 0 || u64::try_from(millis).is_err() {
            return Err(RetentionPolicyError);
        }
        self.generated = generated;
        Ok(self)
    }

    fn retention_millis(self, purpose: ArtifactPurpose) -> Result<u64, ControlPlaneError> {
        match purpose {
            ArtifactPurpose::ClipboardInput => u64::try_from(self.clipboard_input.as_millis())
                .map_err(|_| ControlPlaneError::Internal),
            ArtifactPurpose::ClipboardOutput | ArtifactPurpose::Screenshot => {
                u64::try_from(self.generated.as_millis()).map_err(|_| ControlPlaneError::Internal)
            }
            _ => Err(ControlPlaneError::InvalidRequest),
        }
    }
}

impl Default for ArtifactRetentionPolicy {
    fn default() -> Self {
        Self {
            clipboard_input: DEFAULT_CLIPBOARD_INPUT_RETENTION,
            generated: DEFAULT_GENERATED_ARTIFACT_RETENTION,
        }
    }
}

/// Invalid daemon artifact-retention configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicyError;

impl fmt::Display for RetentionPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("artifact retentions must be positive whole-millisecond ranges")
    }
}

impl Error for RetentionPolicyError {}

/// Total and between-chunk deadlines for one caller-supplied artifact body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactUploadTimeoutPolicy {
    total: Duration,
    idle: Duration,
}

impl ArtifactUploadTimeoutPolicy {
    /// Creates a timeout policy. Both deadlines are mandatory; total may be
    /// shorter than idle and then deliberately dominates it.
    pub fn new(total: Duration, idle: Duration) -> Result<Self, UploadTimeoutPolicyError> {
        if total.is_zero() || idle.is_zero() {
            return Err(UploadTimeoutPolicyError);
        }
        Ok(Self { total, idle })
    }

    /// Maximum wall-clock duration from admission through durable publication.
    #[must_use]
    pub const fn total(self) -> Duration {
        self.total
    }

    /// Maximum wall-clock duration between body chunks.
    #[must_use]
    pub const fn idle(self) -> Duration {
        self.idle
    }
}

impl Default for ArtifactUploadTimeoutPolicy {
    fn default() -> Self {
        Self {
            total: DEFAULT_UPLOAD_TOTAL_TIMEOUT,
            idle: DEFAULT_UPLOAD_IDLE_TIMEOUT,
        }
    }
}

/// Invalid daemon artifact-upload timeout configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UploadTimeoutPolicyError;

impl fmt::Display for UploadTimeoutPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("artifact upload total and idle timeouts must be non-zero")
    }
}

impl Error for UploadTimeoutPolicyError {}

/// Filesystem artifact service used by the daemon's authenticated HTTP API.
pub struct StoreArtifactService<C = SystemClock> {
    store: Arc<ArtifactStore<C>>,
    retention: ArtifactRetentionPolicy,
    upload_timeouts: ArtifactUploadTimeoutPolicy,
}

/// Authenticated principal and exact desktop lifetime for daemon-internal
/// artifact production or command consumption.
#[derive(Clone, Debug)]
pub(crate) struct InternalArtifactContext {
    principal_id: String,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
}

impl InternalArtifactContext {
    pub(crate) fn new(
        principal_id: impl Into<String>,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<Self, ControlPlaneError> {
        let principal_id = principal_id.into();
        ArtifactOwner::new(&principal_id).map_err(|_| ControlPlaneError::InvalidRequest)?;
        if desktop_id.as_uuid().is_nil() || desktop_generation.as_uuid().is_nil() {
            return Err(ControlPlaneError::InvalidRequest);
        }
        Ok(Self {
            principal_id,
            desktop_id,
            desktop_generation,
        })
    }

    fn facts(&self, purpose: ArtifactPurpose) -> Result<RequestFacts, ControlPlaneError> {
        Ok(RequestFacts {
            owner: ArtifactOwner::new(&self.principal_id)
                .map_err(|_| ControlPlaneError::InvalidRequest)?,
            desktop_id: self.desktop_id,
            generation: StoreGeneration::from_uuid(self.desktop_generation.as_uuid()),
            allowed_purposes: ArtifactPurposeSet::only(purpose),
        })
    }
}

/// Fully materialized, already-authorized bytes produced by a daemon actor.
/// Debug intentionally omits the potentially secret body.
pub(crate) struct GeneratedArtifactRequest {
    purpose: ArtifactPurpose,
    content_type: ArtifactContentType,
    bytes: Vec<u8>,
}

impl GeneratedArtifactRequest {
    pub(crate) fn new(
        purpose: ArtifactPurpose,
        content_type: ArtifactContentType,
        bytes: Vec<u8>,
    ) -> Result<Self, ControlPlaneError> {
        if !matches!(
            purpose,
            ArtifactPurpose::ClipboardOutput | ArtifactPurpose::Screenshot
        ) || bytes.is_empty()
            || u64::try_from(bytes.len()).map_or(true, |len| len > purpose.maximum_bytes())
        {
            return Err(ControlPlaneError::InvalidRequest);
        }
        Ok(Self {
            purpose,
            content_type,
            bytes,
        })
    }
}

impl fmt::Debug for GeneratedArtifactRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeneratedArtifactRequest")
            .field("purpose", &self.purpose)
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .field("content", &"[REDACTED]")
            .finish()
    }
}

impl<C> StoreArtifactService<C> {
    /// Binds a private artifact store to an explicit daemon retention policy.
    pub fn new(
        store: Arc<ArtifactStore<C>>,
        retention: ArtifactRetentionPolicy,
        upload_timeouts: ArtifactUploadTimeoutPolicy,
    ) -> Self {
        Self {
            store,
            retention,
            upload_timeouts,
        }
    }
}

impl<C: Clock> StoreArtifactService<C> {
    /// Performs a content-free, non-mutating check of the private store root.
    pub(crate) fn probe_backend(&self) -> Result<(), StoreError> {
        self.store.probe()
    }
}

impl<C: Clock + 'static> ArtifactService for StoreArtifactService<C> {
    fn upload<'a>(
        &'a self,
        context: ArtifactRequestContext,
        request: ArtifactUploadRequest,
        body: Body,
    ) -> ArtifactFuture<'a, Result<ArtifactRef, ControlPlaneError>> {
        Box::pin(async move {
            let facts = RequestFacts::from_context(&context)?;
            self.upload_authorized(facts, request, body).await
        })
    }

    fn download<'a>(
        &'a self,
        context: ArtifactRequestContext,
        request: ArtifactAccessRequest,
    ) -> ArtifactFuture<'a, Result<ArtifactDownload, ControlPlaneError>> {
        Box::pin(async move {
            let facts = RequestFacts::from_context(&context)?;
            self.download_authorized(facts, request).await
        })
    }

    fn delete<'a>(
        &'a self,
        context: ArtifactRequestContext,
        request: ArtifactAccessRequest,
    ) -> ArtifactFuture<'a, Result<ArtifactRef, ControlPlaneError>> {
        Box::pin(async move {
            let facts = RequestFacts::from_context(&context)?;
            self.delete_authorized(facts, request).await
        })
    }
}

#[derive(Clone)]
struct RequestFacts {
    owner: ArtifactOwner,
    desktop_id: DesktopId,
    generation: StoreGeneration,
    allowed_purposes: ArtifactPurposeSet,
}

impl RequestFacts {
    fn from_context(context: &ArtifactRequestContext) -> Result<Self, ControlPlaneError> {
        let owner = ArtifactOwner::new(context.principal().id())
            .map_err(|_| ControlPlaneError::Internal)?;
        let generation = StoreGeneration::from_uuid(context.desktop_generation().as_uuid());
        if generation.as_uuid().is_nil() || context.desktop_id().as_uuid().is_nil() {
            return Err(ControlPlaneError::InvalidRequest);
        }
        Ok(Self {
            owner,
            desktop_id: context.desktop_id(),
            generation,
            allowed_purposes: context.allowed_purposes(),
        })
    }
}

impl<C: Clock + 'static> StoreArtifactService<C> {
    /// Publishes daemon-produced bytes as a private, purpose-bound artifact.
    ///
    /// Cancellation and timeout drop the acknowledgement channel; the blocking
    /// worker then removes any publication whose metadata was not returned to
    /// the caller. Actor-produced bytes never pass through an HTTP body.
    pub(crate) async fn publish_generated(
        &self,
        context: InternalArtifactContext,
        request: GeneratedArtifactRequest,
    ) -> Result<ArtifactRef, ControlPlaneError> {
        let facts = context.facts(request.purpose)?;
        let expected_size =
            u64::try_from(request.bytes.len()).map_err(|_| ControlPlaneError::InvalidRequest)?;
        if expected_size == 0 || expected_size > request.purpose.maximum_bytes() {
            return Err(ControlPlaneError::InvalidRequest);
        }
        let purpose = purpose_to_store(request.purpose);
        let provenance = provenance_for(purpose)?;
        let scope = ArtifactScope::new(
            facts.owner.clone(),
            facts.generation,
            purpose,
            provenance.clone(),
        );
        let now = self.store.current_time().as_unix_millis();
        let expires_at = now
            .checked_add(self.retention.retention_millis(request.purpose)?)
            .map(TimestampMillis::from_unix_millis)
            .ok_or(ControlPlaneError::Internal)?;
        let create = StoreCreate::new(
            facts.owner.clone(),
            purpose,
            facts.generation,
            provenance,
            request.content_type.as_str(),
            expected_size,
            expires_at,
        )
        .map_err(|_| ControlPlaneError::Internal)?;

        let (result_tx, result_rx) = oneshot::channel();
        let (ack_tx, ack_rx) = std_mpsc::channel();
        let worker_store = Arc::clone(&self.store);
        let cleanup_scope = scope;
        let bytes = request.bytes;
        let _worker = tokio::task::spawn_blocking(move || {
            let result = worker_store.create(create, Cursor::new(bytes));
            match result {
                Ok(metadata) => {
                    let artifact_id = metadata.id();
                    if result_tx.send(Ok(metadata)).is_ok() && ack_rx.recv().is_ok() {
                        return;
                    }
                    if worker_store.delete(artifact_id, &cleanup_scope).is_err() {
                        tracing::error!(
                            artifact_id = %artifact_id,
                            "failed to remove an unacknowledged generated artifact"
                        );
                    }
                }
                Err(error) => {
                    if let StoreError::DurabilityUncertain { artifact_id, .. } = &error {
                        let _ = worker_store.delete(*artifact_id, &cleanup_scope);
                    }
                    let _ = result_tx.send(Err(error));
                }
            }
        });

        let publication = async {
            let metadata = result_rx
                .await
                .map_err(|_| ControlPlaneError::Internal)?
                .map_err(map_generated_store_error)?;
            if !metadata_matches(&metadata, &facts, purpose)? {
                return Err(ControlPlaneError::Internal);
            }
            let artifact = metadata_to_ref(&metadata, facts.desktop_id)?;
            ack_tx.send(()).map_err(|_| ControlPlaneError::Internal)?;
            Ok(artifact)
        };
        tokio::time::timeout(self.upload_timeouts.total(), publication)
            .await
            .map_err(|_| ControlPlaneError::CapabilityUnavailable)?
    }

    /// Reads a private clipboard-input artifact for command execution only
    /// after rechecking its complete public reference, owner, generation,
    /// purpose, provenance, expiry, immutable metadata, and byte ceiling.
    pub(crate) async fn read_clipboard_input(
        &self,
        context: &InternalArtifactContext,
        expected: &ArtifactRef,
        maximum_bytes: u64,
    ) -> Result<Vec<u8>, ControlPlaneError> {
        if maximum_bytes == 0
            || maximum_bytes > ArtifactPurpose::ClipboardInput.maximum_bytes()
            || expected.validate().is_err()
            || expected.purpose != ArtifactPurpose::ClipboardInput
            || expected.desktop_id != context.desktop_id
            || expected.desktop_generation != context.desktop_generation
            || expected.content_length > maximum_bytes
        {
            return Err(ControlPlaneError::InvalidRequest);
        }
        let facts = context.facts(ArtifactPurpose::ClipboardInput)?;
        let purpose = StorePurpose::ClipboardInput;
        let scope = ArtifactScope::new(
            facts.owner.clone(),
            facts.generation,
            purpose,
            provenance_for(purpose)?,
        );
        let id = StoreArtifactId::from_uuid(expected.artifact_id.as_uuid())
            .map_err(|_| ControlPlaneError::InvalidRequest)?;
        let store = Arc::clone(&self.store);
        let expected = expected.clone();
        tokio::task::spawn_blocking(move || {
            let opened = store
                .open_body(id, &scope)
                .map_err(map_access_store_error)?;
            let metadata = opened.metadata().clone();
            if !metadata_matches(&metadata, &facts, purpose)?
                || metadata_to_ref(&metadata, facts.desktop_id)? != expected
                || metadata.size() > maximum_bytes
            {
                return Err(ControlPlaneError::NotFound);
            }
            let capacity = usize::try_from(metadata.size())
                .map_err(|_| ControlPlaneError::ResourceExhausted)?;
            let read_limit = maximum_bytes
                .checked_add(1)
                .ok_or(ControlPlaneError::Internal)?;
            let mut bytes = Vec::with_capacity(capacity);
            opened
                .into_file()
                .take(read_limit)
                .read_to_end(&mut bytes)
                .map_err(|_| ControlPlaneError::Internal)?;
            if u64::try_from(bytes.len()).ok() != Some(metadata.size()) {
                return Err(ControlPlaneError::Internal);
            }
            Ok(bytes)
        })
        .await
        .map_err(|_| ControlPlaneError::Internal)?
    }

    async fn upload_authorized(
        &self,
        facts: RequestFacts,
        request: ArtifactUploadRequest,
        body: Body,
    ) -> Result<ArtifactRef, ControlPlaneError> {
        let deadline = tokio::time::Instant::now()
            .checked_add(self.upload_timeouts.total())
            .ok_or(ControlPlaneError::Internal)?;
        if !facts.allowed_purposes.contains(request.purpose) {
            return Err(ControlPlaneError::PermissionDenied);
        }
        if request.purpose != ArtifactPurpose::ClipboardInput
            || request.content_length == 0
            || request.content_length > request.purpose.maximum_bytes()
        {
            return Err(ControlPlaneError::InvalidRequest);
        }

        let purpose = purpose_to_store(request.purpose);
        let provenance = provenance_for(purpose)?;
        let scope = ArtifactScope::new(
            facts.owner.clone(),
            facts.generation,
            purpose,
            provenance.clone(),
        );
        let now = self.store.current_time().as_unix_millis();
        let expires_at = now
            .checked_add(self.retention.retention_millis(request.purpose)?)
            .map(TimestampMillis::from_unix_millis)
            .ok_or(ControlPlaneError::Internal)?;
        let mut create = StoreCreate::new(
            facts.owner.clone(),
            purpose,
            facts.generation,
            provenance,
            request.content_type.as_str(),
            request.content_length,
            expires_at,
        )
        .map_err(|_| ControlPlaneError::Internal)?;
        if let Some(expected) = request.expected_sha256.as_ref() {
            let digest = StoreDigest::from_str(expected.as_str())
                .map_err(|_| ControlPlaneError::Internal)?;
            create = create.with_expected_sha256(digest);
        }

        let (chunk_tx, chunk_rx) = mpsc::channel(STREAM_CHANNEL_CHUNKS);
        let (result_tx, result_rx) = oneshot::channel();
        let (ack_tx, ack_rx) = std_mpsc::channel();
        let worker_store = Arc::clone(&self.store);
        let cleanup_scope = scope.clone();
        let _worker = tokio::task::spawn_blocking(move || {
            let result = worker_store.create(create, UploadReader::new(chunk_rx));
            match result {
                Ok(metadata) => {
                    let artifact_id = metadata.id();
                    if result_tx.send(Ok(metadata)).is_ok() && ack_rx.recv().is_ok() {
                        return;
                    }
                    if worker_store.delete(artifact_id, &cleanup_scope).is_err() {
                        tracing::error!(
                            artifact_id = %artifact_id,
                            "failed to remove an unacknowledged artifact publication"
                        );
                    }
                }
                Err(error) => {
                    if let StoreError::DurabilityUncertain { artifact_id, .. } = &error {
                        let _ = worker_store.delete(*artifact_id, &cleanup_scope);
                    }
                    let _ = result_tx.send(Err(error));
                }
            }
        });

        let transfer = async {
            stream_upload_body(
                body,
                request.content_length,
                chunk_tx,
                self.upload_timeouts.idle(),
            )
            .await;
            let metadata = result_rx
                .await
                .map_err(|_| ControlPlaneError::Internal)?
                .map_err(map_upload_store_error)?;
            if !metadata_matches(&metadata, &facts, purpose)? {
                return Err(ControlPlaneError::Internal);
            }
            let artifact = metadata_to_ref(&metadata, facts.desktop_id)?;
            ack_tx.send(()).map_err(|_| ControlPlaneError::Internal)?;
            Ok(artifact)
        };
        tokio::time::timeout_at(deadline, transfer)
            .await
            .map_err(|_| ControlPlaneError::InvalidRequest)?
    }

    async fn download_authorized(
        &self,
        facts: RequestFacts,
        request: ArtifactAccessRequest,
    ) -> Result<ArtifactDownload, ControlPlaneError> {
        let id = StoreArtifactId::from_uuid(request.artifact_id.as_uuid())
            .map_err(|_| ControlPlaneError::InvalidRequest)?;
        let candidates = access_scopes(&facts)?;
        if candidates.is_empty() {
            return Err(ControlPlaneError::PermissionDenied);
        }
        let store = Arc::clone(&self.store);
        let (purpose, opened) = tokio::task::spawn_blocking(move || {
            for (purpose, scope) in candidates {
                match store.open_body(id, &scope) {
                    Ok(opened) => return Ok((purpose, opened)),
                    Err(StoreError::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(StoreError::NotFound)
        })
        .await
        .map_err(|_| ControlPlaneError::Internal)?
        .map_err(map_access_store_error)?;
        let metadata = opened.metadata().clone();
        if !metadata_matches(&metadata, &facts, purpose)? {
            return Err(ControlPlaneError::Internal);
        }
        let artifact = metadata_to_ref(&metadata, facts.desktop_id)?;
        Ok(ArtifactDownload {
            artifact,
            body: file_body(opened.into_file()),
        })
    }

    async fn delete_authorized(
        &self,
        facts: RequestFacts,
        request: ArtifactAccessRequest,
    ) -> Result<ArtifactRef, ControlPlaneError> {
        let id = StoreArtifactId::from_uuid(request.artifact_id.as_uuid())
            .map_err(|_| ControlPlaneError::InvalidRequest)?;
        let candidates = access_scopes(&facts)?;
        if candidates.is_empty() {
            return Err(ControlPlaneError::PermissionDenied);
        }
        let store = Arc::clone(&self.store);
        let (purpose, metadata) = tokio::task::spawn_blocking(move || {
            for (purpose, scope) in candidates {
                match store.delete_with_metadata(id, &scope) {
                    Ok(metadata) => return Ok((purpose, metadata)),
                    Err(StoreError::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
            Err(StoreError::NotFound)
        })
        .await
        .map_err(|_| ControlPlaneError::Internal)?
        .map_err(map_access_store_error)?;
        if !metadata_matches(&metadata, &facts, purpose)? {
            return Err(ControlPlaneError::Internal);
        }
        metadata_to_ref(&metadata, facts.desktop_id)
    }
}

async fn stream_upload_body(
    body: Body,
    declared: u64,
    sender: mpsc::Sender<UploadMessage>,
    idle_timeout: Duration,
) {
    let mut stream = body.into_data_stream();
    let mut accepted = 0_u64;
    loop {
        let frame = match tokio::time::timeout(idle_timeout, stream.next()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(_) => {
                let _ = sender.send(UploadMessage::Failed).await;
                return;
            }
        };
        let bytes = match frame {
            Ok(bytes) => bytes,
            Err(_) => {
                let _ = sender.send(UploadMessage::Failed).await;
                return;
            }
        };
        let remaining_with_sentinel = declared.saturating_sub(accepted).saturating_add(1);
        if remaining_with_sentinel == 0 {
            return;
        }
        let accepted_from_frame = usize::try_from(remaining_with_sentinel)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let mut offset = 0;
        while offset < accepted_from_frame {
            let end = (offset + STREAM_CHUNK_BYTES).min(accepted_from_frame);
            if sender
                .send(UploadMessage::Data(bytes.slice(offset..end)))
                .await
                .is_err()
            {
                return;
            }
            offset = end;
        }
        accepted = accepted.saturating_add(accepted_from_frame as u64);
        if accepted > declared || accepted_from_frame < bytes.len() {
            return;
        }
    }
}

enum UploadMessage {
    Data(Bytes),
    Failed,
}

struct UploadReader {
    receiver: mpsc::Receiver<UploadMessage>,
    current: Bytes,
}

impl UploadReader {
    fn new(receiver: mpsc::Receiver<UploadMessage>) -> Self {
        Self {
            receiver,
            current: Bytes::new(),
        }
    }
}

impl Read for UploadReader {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        while self.current.is_empty() {
            match self.receiver.blocking_recv() {
                Some(UploadMessage::Data(bytes)) => self.current = bytes,
                Some(UploadMessage::Failed) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, UploadBodyError));
                }
                None => return Ok(0),
            }
        }
        let count = destination.len().min(self.current.len());
        destination[..count].copy_from_slice(&self.current[..count]);
        self.current.advance(count);
        Ok(count)
    }
}

trait AdvanceBytes {
    fn advance(&mut self, count: usize);
}

impl AdvanceBytes for Bytes {
    fn advance(&mut self, count: usize) {
        *self = self.slice(count..);
    }
}

#[derive(Debug)]
struct UploadBodyError;

impl fmt::Display for UploadBodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("artifact upload body stream failed")
    }
}

impl Error for UploadBodyError {}

#[derive(Debug)]
struct DownloadBodyError;

impl fmt::Display for DownloadBodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("artifact download body stream failed")
    }
}

impl Error for DownloadBodyError {}

fn file_body(mut file: File) -> Body {
    let (sender, receiver) =
        mpsc::channel::<Result<Bytes, DownloadBodyError>>(STREAM_CHANNEL_CHUNKS);
    let _worker = tokio::task::spawn_blocking(move || {
        loop {
            let mut chunk = vec![0_u8; STREAM_CHUNK_BYTES];
            match file.read(&mut chunk) {
                Ok(0) => return,
                Ok(count) => {
                    chunk.truncate(count);
                    if sender.blocking_send(Ok(Bytes::from(chunk))).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = sender.blocking_send(Err(DownloadBodyError));
                    return;
                }
            }
        }
    });
    let stream = stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    Body::from_stream(stream)
}

fn access_scopes(
    facts: &RequestFacts,
) -> Result<Vec<(StorePurpose, ArtifactScope)>, ControlPlaneError> {
    let mut scopes = Vec::with_capacity(5);
    for purpose in [
        ArtifactPurpose::ClipboardInput,
        ArtifactPurpose::ClipboardOutput,
        ArtifactPurpose::Screenshot,
        ArtifactPurpose::ActionTrace,
        ArtifactPurpose::SupportBundle,
    ] {
        if facts.allowed_purposes.contains(purpose) {
            let purpose = purpose_to_store(purpose);
            scopes.push((
                purpose,
                ArtifactScope::new(
                    facts.owner.clone(),
                    facts.generation,
                    purpose,
                    provenance_for(purpose)?,
                ),
            ));
        }
    }
    Ok(scopes)
}

fn provenance_for(purpose: StorePurpose) -> Result<CapabilityProvenance, ControlPlaneError> {
    let capability = match purpose {
        StorePurpose::ClipboardInput => Grant::ClipboardWrite.as_str(),
        StorePurpose::ClipboardOutput => Grant::ClipboardRead.as_str(),
        StorePurpose::Screenshot => Grant::CaptureRead.as_str(),
        StorePurpose::ActionTrace | StorePurpose::SupportBundle => Grant::ArtifactRead.as_str(),
        _ => return Err(ControlPlaneError::Internal),
    };
    CapabilityProvenance::new(capability, None).map_err(|_| ControlPlaneError::Internal)
}

const fn purpose_to_store(purpose: ArtifactPurpose) -> StorePurpose {
    match purpose {
        ArtifactPurpose::ClipboardInput => StorePurpose::ClipboardInput,
        ArtifactPurpose::ClipboardOutput => StorePurpose::ClipboardOutput,
        ArtifactPurpose::Screenshot => StorePurpose::Screenshot,
        ArtifactPurpose::ActionTrace => StorePurpose::ActionTrace,
        ArtifactPurpose::SupportBundle => StorePurpose::SupportBundle,
    }
}

fn purpose_from_store(purpose: StorePurpose) -> Result<ArtifactPurpose, ControlPlaneError> {
    match purpose {
        StorePurpose::ClipboardInput => Ok(ArtifactPurpose::ClipboardInput),
        StorePurpose::ClipboardOutput => Ok(ArtifactPurpose::ClipboardOutput),
        StorePurpose::Screenshot => Ok(ArtifactPurpose::Screenshot),
        StorePurpose::ActionTrace => Ok(ArtifactPurpose::ActionTrace),
        StorePurpose::SupportBundle => Ok(ArtifactPurpose::SupportBundle),
        _ => Err(ControlPlaneError::Internal),
    }
}

fn metadata_matches(
    metadata: &StoreMetadata,
    facts: &RequestFacts,
    purpose: StorePurpose,
) -> Result<bool, ControlPlaneError> {
    Ok(metadata.owner() == &facts.owner
        && metadata.desktop_generation() == facts.generation
        && metadata.purpose() == purpose
        && metadata.provenance() == &provenance_for(purpose)?)
}

fn metadata_to_ref(
    metadata: &StoreMetadata,
    desktop_id: DesktopId,
) -> Result<ArtifactRef, ControlPlaneError> {
    let created_at = timestamp_to_protocol(metadata.created_at())?;
    let expires_at = timestamp_to_protocol(metadata.expires_at())?;
    let artifact = ArtifactRef {
        artifact_id: ArtifactId::from_uuid(metadata.id().as_uuid()),
        purpose: purpose_from_store(metadata.purpose())?,
        desktop_id,
        desktop_generation: DesktopGeneration::from_uuid(metadata.desktop_generation().as_uuid()),
        content_type: ArtifactContentType::new(metadata.content_type())
            .map_err(|_| ControlPlaneError::Internal)?,
        content_length: metadata.size(),
        sha256: Sha256Digest::new(metadata.sha256().to_string())
            .map_err(|_| ControlPlaneError::Internal)?,
        created_at,
        expires_at,
    };
    artifact
        .validate()
        .map_err(|_| ControlPlaneError::Internal)?;
    Ok(artifact)
}

fn timestamp_to_protocol(value: TimestampMillis) -> Result<Timestamp, ControlPlaneError> {
    let nanos = i128::from(value.as_unix_millis())
        .checked_mul(1_000_000)
        .ok_or(ControlPlaneError::Internal)?;
    Timestamp::from_unix_timestamp_nanos(nanos).map_err(|_| ControlPlaneError::Internal)
}

fn map_upload_store_error(error: StoreError) -> ControlPlaneError {
    match error {
        StoreError::SizeMismatch | StoreError::DigestMismatch | StoreError::EmptyBody => {
            ControlPlaneError::InvalidRequest
        }
        StoreError::Io(ref source)
            if source
                .get_ref()
                .is_some_and(|inner| inner.is::<UploadBodyError>()) =>
        {
            ControlPlaneError::InvalidRequest
        }
        StoreError::TooLarge | StoreError::StoreQuotaExceeded | StoreError::OwnerQuotaExceeded => {
            ControlPlaneError::ResourceExhausted
        }
        _ => ControlPlaneError::Internal,
    }
}

fn map_access_store_error(error: StoreError) -> ControlPlaneError {
    match error {
        StoreError::NotFound | StoreError::Expired => ControlPlaneError::NotFound,
        _ => ControlPlaneError::Internal,
    }
}

fn map_generated_store_error(error: StoreError) -> ControlPlaneError {
    match error {
        StoreError::TooLarge | StoreError::StoreQuotaExceeded | StoreError::OwnerQuotaExceeded => {
            ControlPlaneError::ResourceExhausted
        }
        _ => ControlPlaneError::Internal,
    }
}

#[cfg(test)]
#[path = "artifact_service_tests.rs"]
mod tests;
