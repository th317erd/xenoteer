use std::{
    collections::HashMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    ArtifactCreate, ArtifactId, ArtifactMetadata, ArtifactOwner, ArtifactScope, Sha256Digest,
    TimestampMillis, ValidationError,
};

const BODY_FILE: &str = "body";
const METADATA_FILE: &str = "metadata.json";
const STORE_MARKER_FILE: &str = ".xenoteer-artifact-store";
const STORE_MARKER_CONTENT: &[u8] = b"xenoteer-artifact-store\nformat=1\n";
const TEMP_PREFIX: &str = ".tmp-";
const MAX_METADATA_BYTES: u64 = 64 * 1024;
const METADATA_FORMAT_VERSION: u32 = 1;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

/// Clock abstraction used for deterministic expiry and retention decisions.
pub trait Clock: Send + Sync {
    /// Returns the current time in Unix-epoch milliseconds.
    fn now(&self) -> TimestampMillis;
}

/// Production wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> TimestampMillis {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0_u128, |duration| duration.as_millis());
        TimestampMillis::from_unix_millis(u64::try_from(millis).unwrap_or(u64::MAX))
    }
}

/// Store-wide, per-owner, per-object, and retention ceilings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactLimits {
    max_object_bytes: u64,
    max_total_bytes: u64,
    max_objects: u64,
    max_owner_bytes: u64,
    max_owner_objects: u64,
    max_retention_millis: u64,
}

impl ArtifactLimits {
    /// Creates a checked set of store limits.
    pub fn new(
        max_object_bytes: u64,
        max_total_bytes: u64,
        max_objects: u64,
        max_owner_bytes: u64,
        max_owner_objects: u64,
        max_retention_millis: u64,
    ) -> Result<Self, StoreError> {
        let limits = Self {
            max_object_bytes,
            max_total_bytes,
            max_objects,
            max_owner_bytes,
            max_owner_objects,
            max_retention_millis,
        };
        if [
            max_object_bytes,
            max_total_bytes,
            max_objects,
            max_owner_bytes,
            max_owner_objects,
            max_retention_millis,
        ]
        .contains(&0)
            || max_object_bytes > max_total_bytes
            || max_object_bytes > max_owner_bytes
            || max_owner_bytes > max_total_bytes
            || max_owner_objects > max_objects
        {
            return Err(StoreError::InvalidLimits);
        }
        Ok(limits)
    }

    /// Maximum body size for one artifact.
    pub const fn max_object_bytes(self) -> u64 {
        self.max_object_bytes
    }

    /// Maximum durable plus reserved bytes across the store.
    pub const fn max_total_bytes(self) -> u64 {
        self.max_total_bytes
    }

    /// Maximum durable plus in-flight artifact count across the store.
    pub const fn max_objects(self) -> u64 {
        self.max_objects
    }

    /// Maximum durable plus reserved bytes for one owner.
    pub const fn max_owner_bytes(self) -> u64 {
        self.max_owner_bytes
    }

    /// Maximum durable plus in-flight artifact count for one owner.
    pub const fn max_owner_objects(self) -> u64 {
        self.max_owner_objects
    }

    /// Maximum creation-to-expiry interval.
    pub const fn max_retention_millis(self) -> u64 {
        self.max_retention_millis
    }
}

impl Default for ArtifactLimits {
    fn default() -> Self {
        Self {
            max_object_bytes: 32 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            max_objects: 1024,
            max_owner_bytes: 256 * 1024 * 1024,
            max_owner_objects: 256,
            max_retention_millis: 60 * 60 * 1000,
        }
    }
}

/// Aggregate result of an expiry cleanup pass; artifact identities are omitted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CleanupReport {
    removed_objects: u64,
    removed_bytes: u64,
    removed_temporary_entries: u64,
}

impl CleanupReport {
    /// Number of expired artifacts removed.
    pub const fn removed_objects(self) -> u64 {
        self.removed_objects
    }

    /// Number of expired body bytes removed.
    pub const fn removed_bytes(self) -> u64 {
        self.removed_bytes
    }

    /// Number of abandoned temporary entries removed.
    pub const fn removed_temporary_entries(self) -> u64 {
        self.removed_temporary_entries
    }
}

/// A securely opened immutable artifact body and its checked metadata.
#[derive(Debug)]
pub struct OpenedArtifact {
    metadata: ArtifactMetadata,
    body: File,
}

impl OpenedArtifact {
    /// Returns the authorization-checked metadata.
    pub fn metadata(&self) -> &ArtifactMetadata {
        &self.metadata
    }

    /// Consumes the wrapper and returns the immutable body file.
    pub fn into_file(self) -> File {
        self.body
    }
}

impl Read for OpenedArtifact {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.body.read(buffer)
    }
}

/// Filesystem artifact-store failures.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Store limits were zero or internally inconsistent.
    #[error("artifact store limits are invalid")]
    InvalidLimits,
    /// Input metadata was invalid.
    #[error(transparent)]
    Validation(#[from] ValidationError),
    /// Artifact is not visible within the supplied owner/generation scope.
    #[error("artifact was not found")]
    NotFound,
    /// Artifact exists in scope but has expired.
    #[error("artifact has expired")]
    Expired,
    /// Requested body exceeds its per-object or quota reservation.
    #[error("artifact is too large")]
    TooLarge,
    /// Empty artifact bodies are not published.
    #[error("artifact body is empty")]
    EmptyBody,
    /// Store-wide byte or count quota is exhausted.
    #[error("artifact store quota is exhausted")]
    StoreQuotaExceeded,
    /// Per-owner byte or count quota is exhausted.
    #[error("artifact owner quota is exhausted")]
    OwnerQuotaExceeded,
    /// Stream ended before or continued beyond its declared size.
    #[error("artifact body size did not match its declared size")]
    SizeMismatch,
    /// Stream digest did not match the caller-provided SHA-256.
    #[error("artifact SHA-256 did not match")]
    DigestMismatch,
    /// Configured store root was relative.
    #[error("artifact store root must be an absolute path")]
    RootNotAbsolute,
    /// Existing nonempty directory lacked the durable store-format marker.
    #[error("artifact store root is not an initialized dedicated store")]
    RootNotInitialized,
    /// Store root or its marker has unsafe ownership, mode, identity, or type.
    #[error("artifact store root failed its ownership or identity checks")]
    UnsafeRoot,
    /// Another live store instance already owns this root.
    #[error("artifact store root is already owned by another process or instance")]
    RootAlreadyOpen,
    /// The configured root path no longer resolves to the locked directory.
    #[error("artifact store root identity changed after initialization")]
    RootChanged,
    /// Root path or a durable object contained an unexpected entry or type.
    #[error("artifact store contains an unsafe or unexpected entry")]
    UnexpectedEntry,
    /// Durable metadata or body did not satisfy store invariants.
    #[error("artifact {artifact_id} is corrupt ({kind})")]
    CorruptArtifact {
        /// Artifact whose private record failed validation.
        artifact_id: ArtifactId,
        /// Fixed non-content-bearing corruption category.
        kind: ArtifactCorruption,
    },
    /// Published directory could not be confirmed durable or removed. The ID is
    /// returned so the caller can perform a scoped delete/reconciliation.
    #[error("artifact {artifact_id} was published but directory durability is uncertain")]
    DurabilityUncertain {
        /// Potentially published artifact ID.
        artifact_id: ArtifactId,
        /// Root-directory synchronization error.
        #[source]
        source: io::Error,
    },
    /// Internal mutex was poisoned by a panic in another caller.
    #[error("artifact store state is unavailable")]
    StateUnavailable,
    /// Filesystem or stream I/O failed.
    #[error("artifact store I/O failed")]
    Io(#[from] io::Error),
    /// Durable metadata could not be encoded or decoded.
    #[error("artifact metadata encoding failed")]
    Metadata(#[from] serde_json::Error),
}

/// Fixed, log-safe persisted-record corruption category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ArtifactCorruption {
    /// Artifact path was not a private directory.
    RecordType,
    /// Artifact directory contained a name outside the closed record format.
    UnexpectedRecordEntry,
    /// Artifact directory lacked its body or metadata file.
    IncompleteRecord,
    /// Persisted metadata exceeded its fixed byte ceiling.
    MetadataTooLarge,
    /// Persisted metadata could not be strictly decoded.
    MetadataInvalid,
    /// Persisted metadata used an unsupported format version.
    MetadataFormat,
    /// Decoded metadata violated a semantic invariant.
    MetadataInvariant,
    /// Body length differed from the immutable metadata.
    BodySize,
    /// Body content differed from the immutable SHA-256 digest.
    BodyDigest,
    /// Body or metadata was not a private, singly linked regular file.
    FileType,
}

impl std::fmt::Display for ArtifactCorruption {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::RecordType => "record_type",
            Self::UnexpectedRecordEntry => "unexpected_record_entry",
            Self::IncompleteRecord => "incomplete_record",
            Self::MetadataTooLarge => "metadata_too_large",
            Self::MetadataInvalid => "metadata_invalid",
            Self::MetadataFormat => "metadata_format",
            Self::MetadataInvariant => "metadata_invariant",
            Self::BodySize => "body_size",
            Self::BodyDigest => "body_digest",
            Self::FileType => "file_type",
        })
    }
}

/// Private filesystem-backed artifact store.
///
/// One instance must own a configured root per process. Root access is forced to
/// `0700`; object directories are `0700`, body and metadata files are `0600`.
pub struct ArtifactStore<C = SystemClock> {
    root: PathBuf,
    root_directory: File,
    limits: ArtifactLimits,
    clock: C,
    state: Arc<Mutex<StoreState>>,
    filesystem_operation: Mutex<()>,
}

impl ArtifactStore<SystemClock> {
    /// Opens a production store and performs startup cleanup/reconciliation.
    pub fn open(root: impl Into<PathBuf>, limits: ArtifactLimits) -> Result<Self, StoreError> {
        Self::with_clock(root, limits, SystemClock)
    }
}

impl<C: Clock> ArtifactStore<C> {
    /// Opens a store with an injected deterministic clock.
    pub fn with_clock(
        root: impl Into<PathBuf>,
        limits: ArtifactLimits,
        clock: C,
    ) -> Result<Self, StoreError> {
        let root = root.into();
        let (root, root_directory) = prepare_root(&root)?;
        let active_temporary = HashMap::new();
        let (usage, _) = reconcile_root(&root, clock.now(), &active_temporary)?;
        Ok(Self {
            root,
            root_directory,
            limits,
            clock,
            state: Arc::new(Mutex::new(StoreState {
                usage,
                active_temporary,
            })),
            filesystem_operation: Mutex::new(()),
        })
    }

    /// Returns the store clock used for publication and expiry decisions.
    ///
    /// Adapters use this value when deriving a bounded `expires_at`; keeping
    /// the calculation on the store's clock avoids disagreements with an
    /// independently sampled wall clock.
    pub fn current_time(&self) -> TimestampMillis {
        self.clock.now()
    }

    /// Verifies that the private root still names the directory opened by this
    /// store without mutating artifacts or running reconciliation.
    ///
    /// Capability monitors use this cheap check to avoid advertising a store
    /// after its configured root has been replaced, rebound, or made
    /// inaccessible. Ordinary operations repeat the same binding check before
    /// touching caller data.
    pub fn probe(&self) -> Result<(), StoreError> {
        let _operation = self.lock_operations()?;
        self.validate_root_binding()
    }

    /// Streams, hashes, fsyncs, and atomically publishes one immutable body.
    ///
    /// The declared size is reserved before reading. A partial, oversized, or
    /// hash-mismatched body is discarded without consuming durable quota.
    pub fn create<R: Read>(
        &self,
        request: ArtifactCreate,
        mut body: R,
    ) -> Result<ArtifactMetadata, StoreError> {
        request.validate()?;
        self.validate_time_and_size(&request, self.clock.now())?;

        // Admission, path creation, and registration are one short filesystem
        // transaction. The potentially slow caller-controlled body stream is
        // deliberately copied after releasing the store-wide operation lock.
        let (mut staged, mut destination) = {
            let _operation = self.lock_operations()?;
            self.validate_root_binding()?;
            let id = ArtifactId::random();
            let temp_path = self.root.join(format!("{TEMP_PREFIX}{id}"));
            create_private_directory(&temp_path)?;
            let temporary = TemporaryDirectory::new(temp_path)?;
            let body_path = temporary.path().join(BODY_FILE);
            let destination = create_private_file(&body_path)?;
            let reservation = self.reserve(&request, id, temporary.identity())?;
            (
                StagedArtifact {
                    temporary,
                    reservation,
                },
                destination,
            )
        };

        let digest = copy_bounded(
            &mut body,
            &mut destination,
            request.expected_size,
            self.limits.max_object_bytes,
        )?;
        if request
            .expected_sha256
            .is_some_and(|expected| expected != digest)
        {
            return Err(StoreError::DigestMismatch);
        }
        destination.sync_all()?;
        drop(destination);

        let published_at = self.clock.now();
        self.validate_time_and_size(&request, published_at)?;
        let metadata = ArtifactMetadata {
            id: staged.id(),
            owner: request.owner,
            purpose: request.purpose,
            desktop_generation: request.desktop_generation,
            provenance: request.provenance,
            content_type: request.content_type,
            created_at: published_at,
            expires_at: request.expires_at,
            size: request.expected_size,
            sha256: digest,
            redaction: request.redaction,
        };
        staged.validate_temporary_binding()?;
        write_metadata(staged.path(), &metadata)?;
        sync_directory(staged.path())?;

        // Revalidate the locked root and hold the global lock only across the
        // atomic namespace publication plus root-directory durability commit.
        let _operation = self.lock_operations()?;
        self.validate_root_binding()?;
        staged.validate_temporary_binding()?;
        let final_path = self.artifact_path(staged.id());
        fs::rename(staged.path(), &final_path)?;
        staged.disarm_temporary();

        if let Err(source) = sync_directory(&self.root) {
            if remove_entry_no_follow(&final_path).is_err() {
                staged.commit();
                return Err(StoreError::DurabilityUncertain {
                    artifact_id: staged.id(),
                    source,
                });
            }
            if let Err(cleanup_source) = sync_directory(&self.root) {
                staged.commit();
                return Err(StoreError::DurabilityUncertain {
                    artifact_id: staged.id(),
                    source: cleanup_source,
                });
            }
            return Err(StoreError::Io(source));
        }

        staged.commit();
        Ok(metadata)
    }

    /// Opens an artifact only when owner and generation both match. This does
    /// not replace purpose-specific authorization in the server.
    pub fn open_body(
        &self,
        id: ArtifactId,
        scope: &ArtifactScope,
    ) -> Result<OpenedArtifact, StoreError> {
        let _operation = self.lock_operations()?;
        self.validate_root_binding()?;
        let path = self.artifact_path(id);
        let (metadata, mut body) = match load_checked_artifact(&path, id) {
            Ok(artifact) => artifact,
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound);
            }
            Err(error) => return Err(error),
        };
        if !scope.matches(&metadata) {
            return Err(StoreError::NotFound);
        }
        if metadata.expires_at() <= self.clock.now() {
            self.remove_known(&path, &metadata)?;
            return Err(StoreError::Expired);
        }

        verify_body_digest(&mut body, &metadata)?;
        Ok(OpenedArtifact { metadata, body })
    }

    /// Deletes an artifact only when owner and generation both match.
    pub fn delete(&self, id: ArtifactId, scope: &ArtifactScope) -> Result<(), StoreError> {
        self.delete_with_metadata(id, scope).map(|_| ())
    }

    /// Deletes an artifact only when its complete scope matches and returns
    /// the former checked metadata.
    ///
    /// Returning metadata from the same locked operation lets async adapters
    /// construct the deletion response without an authorize/open/delete race.
    pub fn delete_with_metadata(
        &self,
        id: ArtifactId,
        scope: &ArtifactScope,
    ) -> Result<ArtifactMetadata, StoreError> {
        let _operation = self.lock_operations()?;
        self.validate_root_binding()?;
        let path = self.artifact_path(id);
        let (metadata, mut body) = match load_checked_artifact(&path, id) {
            Ok(artifact) => artifact,
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound);
            }
            Err(error) => return Err(error),
        };
        if !scope.matches(&metadata) {
            return Err(StoreError::NotFound);
        }
        let expired = metadata.expires_at() <= self.clock.now();
        if !expired {
            verify_body_digest(&mut body, &metadata)?;
        }
        drop(body);
        self.remove_known(&path, &metadata)?;
        if expired {
            return Err(StoreError::Expired);
        }
        Ok(metadata)
    }

    /// Removes expired artifacts and abandoned temporary entries, returning
    /// aggregate counts only.
    pub fn cleanup_expired(&self) -> Result<CleanupReport, StoreError> {
        let _operation = self.lock_operations()?;
        self.validate_root_binding()?;
        let mut state = self.lock_state()?;
        let (mut reconciled, report) =
            reconcile_root(&self.root, self.clock.now(), &state.active_temporary)?;
        for active in state.active_temporary.values() {
            reconciled.add_existing(&active.owner, active.bytes);
        }
        state.usage = reconciled;
        Ok(report)
    }

    fn validate_time_and_size(
        &self,
        request: &ArtifactCreate,
        now: TimestampMillis,
    ) -> Result<(), StoreError> {
        if request.expected_size == 0 {
            return Err(StoreError::EmptyBody);
        }
        if request.expected_size > self.limits.max_object_bytes
            || request.expected_size > request.purpose.maximum_bytes()
        {
            return Err(StoreError::TooLarge);
        }
        let latest_expiry = now
            .checked_add(self.limits.max_retention_millis)
            .ok_or(ValidationError::Expiry)?;
        if request.expires_at <= now || request.expires_at > latest_expiry {
            return Err(ValidationError::Expiry.into());
        }
        Ok(())
    }

    fn reserve(
        &self,
        request: &ArtifactCreate,
        id: ArtifactId,
        identity: TemporaryIdentity,
    ) -> Result<Reservation, StoreError> {
        let mut state = self.lock_state()?;
        if state.active_temporary.contains_key(&id) {
            return Err(StoreError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "artifact staging identifier collision",
            )));
        }
        state
            .usage
            .reserve(&request.owner, request.expected_size, self.limits)?;
        let replaced = state.active_temporary.insert(
            id,
            StagingReservation {
                owner: request.owner.clone(),
                bytes: request.expected_size,
                identity,
            },
        );
        debug_assert!(replaced.is_none());
        Ok(Reservation {
            state: Arc::clone(&self.state),
            id,
            committed: false,
        })
    }

    fn remove_known(&self, path: &Path, metadata: &ArtifactMetadata) -> Result<(), StoreError> {
        remove_entry_no_follow(path)?;
        sync_directory(&self.root)?;
        self.lock_state()?
            .usage
            .remove(&metadata.owner, metadata.size);
        Ok(())
    }

    fn artifact_path(&self, id: ArtifactId) -> PathBuf {
        self.root.join(id.to_string())
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, StoreState>, StoreError> {
        self.state.lock().map_err(|_| StoreError::StateUnavailable)
    }

    fn lock_operations(&self) -> Result<MutexGuard<'_, ()>, StoreError> {
        self.filesystem_operation
            .lock()
            .map_err(|_| StoreError::StateUnavailable)
    }

    fn validate_root_binding(&self) -> Result<(), StoreError> {
        let locked = self.root_directory.metadata()?;
        let current = fs::symlink_metadata(&self.root)?;
        if current.file_type().is_symlink()
            || !current.is_dir()
            || current.dev() != locked.dev()
            || current.ino() != locked.ino()
        {
            return Err(StoreError::RootChanged);
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct StoreState {
    usage: Usage,
    active_temporary: HashMap<ArtifactId, StagingReservation>,
}

#[derive(Clone, Debug)]
struct StagingReservation {
    owner: ArtifactOwner,
    bytes: u64,
    identity: TemporaryIdentity,
}

#[derive(Debug, Default)]
struct Usage {
    total_bytes: u64,
    total_objects: u64,
    owners: HashMap<ArtifactOwner, OwnerUsage>,
}

impl Usage {
    fn reserve(
        &mut self,
        owner: &ArtifactOwner,
        bytes: u64,
        limits: ArtifactLimits,
    ) -> Result<(), StoreError> {
        if self.total_bytes.saturating_add(bytes) > limits.max_total_bytes
            || self.total_objects.saturating_add(1) > limits.max_objects
        {
            return Err(StoreError::StoreQuotaExceeded);
        }
        let owner_usage = self.owners.entry(owner.clone()).or_default();
        if owner_usage.bytes.saturating_add(bytes) > limits.max_owner_bytes
            || owner_usage.objects.saturating_add(1) > limits.max_owner_objects
        {
            if owner_usage.bytes == 0 && owner_usage.objects == 0 {
                self.owners.remove(owner);
            }
            return Err(StoreError::OwnerQuotaExceeded);
        }
        self.total_bytes += bytes;
        self.total_objects += 1;
        owner_usage.bytes += bytes;
        owner_usage.objects += 1;
        Ok(())
    }

    fn add_existing(&mut self, owner: &ArtifactOwner, bytes: u64) {
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.total_objects = self.total_objects.saturating_add(1);
        let owner_usage = self.owners.entry(owner.clone()).or_default();
        owner_usage.bytes = owner_usage.bytes.saturating_add(bytes);
        owner_usage.objects = owner_usage.objects.saturating_add(1);
    }

    fn remove(&mut self, owner: &ArtifactOwner, bytes: u64) {
        self.total_bytes = self.total_bytes.saturating_sub(bytes);
        self.total_objects = self.total_objects.saturating_sub(1);
        if let Some(owner_usage) = self.owners.get_mut(owner) {
            owner_usage.bytes = owner_usage.bytes.saturating_sub(bytes);
            owner_usage.objects = owner_usage.objects.saturating_sub(1);
            if owner_usage.bytes == 0 && owner_usage.objects == 0 {
                self.owners.remove(owner);
            }
        }
    }
}

#[derive(Debug, Default)]
struct OwnerUsage {
    bytes: u64,
    objects: u64,
}

struct Reservation {
    state: Arc<Mutex<StoreState>>,
    id: ArtifactId,
    committed: bool,
}

impl Reservation {
    fn commit(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = state.active_temporary.remove(&self.id);
        debug_assert!(removed.is_some());
        self.committed = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = state.active_temporary.remove(&self.id) {
            state.usage.remove(&active.owner, active.bytes);
        }
    }
}

struct StagedArtifact {
    // Field order is intentional: cleanup of the filesystem entry runs before
    // the reservation unregisters the active staging identifier.
    temporary: TemporaryDirectory,
    reservation: Reservation,
}

impl StagedArtifact {
    fn id(&self) -> ArtifactId {
        self.reservation.id
    }

    fn path(&self) -> &Path {
        self.temporary.path()
    }

    fn disarm_temporary(&mut self) {
        self.temporary.disarm();
    }

    fn validate_temporary_binding(&self) -> Result<(), StoreError> {
        self.temporary.validate_binding()
    }

    fn commit(&mut self) {
        self.reservation.commit();
    }
}

struct TemporaryDirectory {
    path: PathBuf,
    identity: TemporaryIdentity,
    armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TemporaryIdentity {
    device: u64,
    inode: u64,
}

impl TemporaryDirectory {
    fn new(path: PathBuf) -> Result<Self, StoreError> {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ignored = remove_entry_no_follow(&path);
                return Err(error.into());
            }
        };
        if let Err(error) = validate_private_directory_metadata(&metadata) {
            let _ignored = remove_entry_no_follow(&path);
            return Err(error);
        }
        Ok(Self {
            path,
            identity: TemporaryIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            armed: true,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn identity(&self) -> TemporaryIdentity {
        self.identity
    }

    fn validate_binding(&self) -> Result<(), StoreError> {
        let metadata = fs::symlink_metadata(&self.path)?;
        validate_private_directory_metadata(&metadata)?;
        if (TemporaryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }) != self.identity
        {
            return Err(StoreError::UnexpectedEntry);
        }
        Ok(())
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if self.armed {
            let _ = remove_entry_no_follow(&self.path);
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredMetadata {
    format_version: u32,
    artifact: ArtifactMetadata,
}

fn prepare_root(configured_root: &Path) -> Result<(PathBuf, File), StoreError> {
    if !configured_root.is_absolute() {
        return Err(StoreError::RootNotAbsolute);
    }
    match fs::symlink_metadata(configured_root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(StoreError::UnsafeRoot);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory(configured_root)?;
        }
        Err(error) => return Err(error.into()),
    }

    let root = fs::canonicalize(configured_root)?;
    let root_directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&root)?;
    root_directory
        .try_lock()
        .map_err(|_| StoreError::RootAlreadyOpen)?;

    let root_metadata = root_directory.metadata()?;
    if !root_metadata.is_dir() || root_metadata.uid() != nix::unistd::Uid::effective().as_raw() {
        return Err(StoreError::UnsafeRoot);
    }

    let marker_path = root.join(STORE_MARKER_FILE);
    match fs::symlink_metadata(&marker_path) {
        Ok(_) => {
            if root_metadata.mode() & 0o777 != 0o700 {
                return Err(StoreError::UnsafeRoot);
            }
            validate_store_marker(&marker_path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if fs::read_dir(&root)?.next().is_some() {
                return Err(StoreError::RootNotInitialized);
            }
            root_directory.set_permissions(fs::Permissions::from_mode(0o700))?;
            let mut marker = create_private_file(&marker_path)?;
            marker.write_all(STORE_MARKER_CONTENT)?;
            marker.sync_all()?;
            sync_directory(&root)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok((root, root_directory))
}

fn validate_store_marker(path: &Path) -> Result<(), StoreError> {
    let marker = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = marker.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() != STORE_MARKER_CONTENT.len() as u64
    {
        return Err(StoreError::UnsafeRoot);
    }
    let mut contents = Vec::with_capacity(STORE_MARKER_CONTENT.len());
    marker
        .take(STORE_MARKER_CONTENT.len() as u64 + 1)
        .read_to_end(&mut contents)?;
    if contents != STORE_MARKER_CONTENT {
        return Err(StoreError::UnsafeRoot);
    }
    Ok(())
}

fn reconcile_root(
    root: &Path,
    now: TimestampMillis,
    active_temporary: &HashMap<ArtifactId, StagingReservation>,
) -> Result<(Usage, CleanupReport), StoreError> {
    let mut usage = Usage::default();
    let mut report = CleanupReport::default();
    let mut changed = false;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| StoreError::UnexpectedEntry)?;
        let path = entry.path();
        if name == STORE_MARKER_FILE {
            continue;
        }
        if name.starts_with(TEMP_PREFIX) {
            let Some(id) = parse_temporary_id(&name) else {
                return Err(StoreError::UnexpectedEntry);
            };
            if let Some(active) = active_temporary.get(&id) {
                validate_active_temporary_directory(&path, active.identity)?;
                continue;
            }
            remove_entry_no_follow(&path)?;
            report.removed_temporary_entries += 1;
            changed = true;
            continue;
        }
        let id = match name.parse::<ArtifactId>() {
            Ok(id) => id,
            Err(_) => return Err(StoreError::UnexpectedEntry),
        };
        let entry_type = entry.file_type()?;
        if entry_type.is_symlink() || !entry_type.is_dir() {
            return Err(StoreError::UnexpectedEntry);
        }
        let metadata = load_checked_metadata(&path, id)?;
        if metadata.expires_at() <= now {
            remove_entry_no_follow(&path)?;
            report.removed_objects += 1;
            report.removed_bytes = report.removed_bytes.saturating_add(metadata.size());
            changed = true;
        } else {
            usage.add_existing(metadata.owner(), metadata.size());
        }
    }
    if changed {
        sync_directory(root)?;
    }
    Ok((usage, report))
}

fn validate_active_temporary_directory(
    path: &Path,
    expected_identity: TemporaryIdentity,
) -> Result<(), StoreError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    validate_private_directory_metadata(&metadata)?;
    if (TemporaryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }) != expected_identity
    {
        return Err(StoreError::UnexpectedEntry);
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        match entry.file_name().to_str() {
            Some(BODY_FILE | METADATA_FILE) => {}
            _ => return Err(StoreError::UnexpectedEntry),
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if file_type.is_symlink() || !file_type.is_file() {
            return Err(StoreError::UnexpectedEntry);
        }
        match open_private_regular_file(&entry.path()) {
            Ok(file) => drop(file),
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn validate_private_directory_metadata(metadata: &fs::Metadata) -> Result<(), StoreError> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(StoreError::UnexpectedEntry);
    }
    Ok(())
}

fn parse_temporary_id(name: &str) -> Option<ArtifactId> {
    let value = name.strip_prefix(TEMP_PREFIX)?;
    value.parse().ok()
}

fn load_checked_metadata(path: &Path, id: ArtifactId) -> Result<ArtifactMetadata, StoreError> {
    let (metadata, _) = load_checked_artifact(path, id)?;
    Ok(metadata)
}

fn load_checked_artifact(
    path: &Path,
    id: ArtifactId,
) -> Result<(ArtifactMetadata, File), StoreError> {
    validate_artifact_directory(path, id)?;
    let metadata_path = path.join(METADATA_FILE);
    let mut file = open_artifact_file(&metadata_path, id)?;
    if file.metadata()?.len() > MAX_METADATA_BYTES {
        return Err(corrupt(id, ArtifactCorruption::MetadataTooLarge));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(corrupt(id, ArtifactCorruption::MetadataTooLarge));
    }
    let stored: StoredMetadata = serde_json::from_slice(&bytes)
        .map_err(|_| corrupt(id, ArtifactCorruption::MetadataInvalid))?;
    if stored.format_version != METADATA_FORMAT_VERSION {
        return Err(corrupt(id, ArtifactCorruption::MetadataFormat));
    }
    stored
        .artifact
        .validate(id)
        .map_err(|_| corrupt(id, ArtifactCorruption::MetadataInvariant))?;
    let body = open_artifact_file(&path.join(BODY_FILE), id)?;
    if body.metadata()?.len() != stored.artifact.size() {
        return Err(corrupt(id, ArtifactCorruption::BodySize));
    }
    Ok((stored.artifact, body))
}

fn validate_artifact_directory(path: &Path, id: ArtifactId) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(corrupt(id, ArtifactCorruption::RecordType));
    }
    let mut found_body = false;
    let mut found_metadata = false;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        match entry.file_name().to_str() {
            Some(BODY_FILE) => found_body = true,
            Some(METADATA_FILE) => found_metadata = true,
            _ => return Err(corrupt(id, ArtifactCorruption::UnexpectedRecordEntry)),
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_file() {
            return Err(corrupt(id, ArtifactCorruption::FileType));
        }
    }
    if !found_body || !found_metadata {
        return Err(corrupt(id, ArtifactCorruption::IncompleteRecord));
    }
    Ok(())
}

fn write_metadata(path: &Path, metadata: &ArtifactMetadata) -> Result<(), StoreError> {
    let stored = StoredMetadata {
        format_version: METADATA_FORMAT_VERSION,
        artifact: metadata.clone(),
    };
    let metadata_path = path.join(METADATA_FILE);
    let mut file = create_private_file(&metadata_path)?;
    serde_json::to_writer(&mut file, &stored)?;
    file.write_all(b"\n")?;
    if file.metadata()?.len() > MAX_METADATA_BYTES {
        return Err(corrupt(metadata.id(), ArtifactCorruption::MetadataTooLarge));
    }
    file.sync_all()?;
    Ok(())
}

fn copy_bounded<R: Read, W: Write>(
    source: &mut R,
    destination: &mut W,
    expected_size: u64,
    maximum_size: u64,
) -> Result<Sha256Digest, StoreError> {
    if expected_size > maximum_size {
        return Err(StoreError::TooLarge);
    }
    let mut hasher = Sha256::new();
    let mut written = 0_u64;
    let mut buffer = [0_u8; STREAM_BUFFER_BYTES];
    loop {
        let remaining = expected_size.saturating_sub(written);
        let read_limit = usize::try_from(remaining.saturating_add(1))
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let count = source.read(&mut buffer[..read_limit])?;
        if count == 0 {
            break;
        }
        let count_u64 = count as u64;
        if count_u64 > remaining {
            return Err(StoreError::SizeMismatch);
        }
        destination.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        written += count_u64;
    }
    if written != expected_size {
        return Err(StoreError::SizeMismatch);
    }
    Ok(Sha256Digest::from_bytes(hasher.finalize().into()))
}

fn verify_body_digest(body: &mut File, metadata: &ArtifactMetadata) -> Result<(), StoreError> {
    body.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut read = 0_u64;
    let mut buffer = [0_u8; STREAM_BUFFER_BYTES];
    loop {
        let remaining = metadata.size().saturating_sub(read);
        let read_limit = usize::try_from(remaining.saturating_add(1))
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let count = body.read(&mut buffer[..read_limit])?;
        if count == 0 {
            break;
        }
        let count_u64 = count as u64;
        if count_u64 > remaining {
            return Err(corrupt(metadata.id(), ArtifactCorruption::BodySize));
        }
        hasher.update(&buffer[..count]);
        read += count_u64;
    }
    if read != metadata.size() {
        return Err(corrupt(metadata.id(), ArtifactCorruption::BodySize));
    }
    let actual = Sha256Digest::from_bytes(hasher.finalize().into());
    if actual != metadata.sha256() {
        return Err(corrupt(metadata.id(), ArtifactCorruption::BodyDigest));
    }
    body.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), StoreError> {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn create_private_file(path: &Path) -> Result<File, StoreError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn open_private_regular_file(path: &Path) -> Result<File, StoreError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(StoreError::UnexpectedEntry);
    }
    Ok(file)
}

fn open_artifact_file(path: &Path, id: ArtifactId) -> Result<File, StoreError> {
    match open_private_regular_file(path) {
        Ok(file) => Ok(file),
        Err(StoreError::UnexpectedEntry) => Err(corrupt(id, ArtifactCorruption::FileType)),
        Err(error) => Err(error),
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?
        .sync_all()?;
    Ok(())
}

fn remove_entry_no_follow(path: &Path) -> Result<(), StoreError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        fs::remove_file(path)?;
    } else {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn corrupt(id: ArtifactId, kind: ArtifactCorruption) -> StoreError {
    StoreError::CorruptArtifact {
        artifact_id: id,
        kind,
    }
}
