use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use axum::body::to_bytes;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use xenoteer_artifacts::ArtifactLimits;

use super::*;

type TestResult = Result<(), Box<dyn Error>>;

#[derive(Clone)]
struct TestClock(Arc<AtomicU64>);

impl TestClock {
    fn new(now: u64) -> Self {
        Self(Arc::new(AtomicU64::new(now)))
    }

    fn set(&self, now: u64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now(&self) -> TimestampMillis {
        TimestampMillis::from_unix_millis(self.0.load(Ordering::SeqCst))
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "xenoteerd-artifact-adapter-test-{}",
            Uuid::new_v4()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn object_directories(&self) -> usize {
        fs::read_dir(&self.0)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .count()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn test_store(
    directory: &TestDirectory,
    clock: TestClock,
) -> Result<Arc<ArtifactStore<TestClock>>, StoreError> {
    let limits = ArtifactLimits::new(
        1024 * 1024,
        4 * 1024 * 1024,
        32,
        2 * 1024 * 1024,
        16,
        10_000,
    )?;
    Ok(Arc::new(ArtifactStore::with_clock(
        directory.path(),
        limits,
        clock,
    )?))
}

fn facts(
    owner: &str,
    generation: DesktopGeneration,
    allowed_purposes: ArtifactPurposeSet,
) -> Result<RequestFacts, xenoteer_artifacts::ValidationError> {
    Ok(RequestFacts {
        owner: ArtifactOwner::new(owner)?,
        desktop_id: DesktopId::from_uuid(Uuid::from_u128(7)),
        generation: StoreGeneration::from_uuid(generation.as_uuid()),
        allowed_purposes,
    })
}

fn upload_request(bytes: &[u8]) -> Result<ArtifactUploadRequest, Box<dyn Error>> {
    Ok(ArtifactUploadRequest {
        purpose: ArtifactPurpose::ClipboardInput,
        content_type: ArtifactContentType::new("application/octet-stream")?,
        content_length: bytes.len() as u64,
        expected_sha256: Some(protocol_digest(bytes)?),
    })
}

fn protocol_digest(bytes: &[u8]) -> Result<Sha256Digest, Box<dyn Error>> {
    let digest = StoreDigest::from_bytes(Sha256::digest(bytes).into());
    Ok(Sha256Digest::new(digest.to_string())?)
}

fn adapter_result<T>(result: Result<T, ControlPlaneError>) -> Result<T, io::Error> {
    result.map_err(|_| io::Error::other("artifact adapter operation failed"))
}

fn seed(
    store: &ArtifactStore<TestClock>,
    facts: &RequestFacts,
    purpose: StorePurpose,
    bytes: &[u8],
    expires_at: u64,
) -> Result<StoreMetadata, Box<dyn Error>> {
    let request = StoreCreate::new(
        facts.owner.clone(),
        purpose,
        facts.generation,
        provenance_for(purpose).map_err(|_| "invalid provenance")?,
        match purpose {
            StorePurpose::Screenshot => "image/png",
            _ => "application/octet-stream",
        },
        bytes.len() as u64,
        TimestampMillis::from_unix_millis(expires_at),
    )?;
    Ok(store.create(request, Cursor::new(bytes))?)
}

fn access_request(metadata: &StoreMetadata) -> ArtifactAccessRequest {
    ArtifactAccessRequest {
        artifact_id: ArtifactId::from_uuid(metadata.id().as_uuid()),
    }
}

fn service(
    store: Arc<ArtifactStore<TestClock>>,
) -> Result<StoreArtifactService<TestClock>, Box<dyn Error>> {
    Ok(StoreArtifactService::new(
        store,
        ArtifactRetentionPolicy::new(Duration::from_secs(1))?
            .with_generated_retention(Duration::from_secs(1))?,
        ArtifactUploadTimeoutPolicy::new(Duration::from_secs(1), Duration::from_millis(100))?,
    ))
}

fn internal_context(
    owner: &str,
    generation: DesktopGeneration,
) -> Result<InternalArtifactContext, ControlPlaneError> {
    InternalArtifactContext::new(owner, DesktopId::from_uuid(Uuid::from_u128(7)), generation)
}

#[tokio::test]
async fn uploads_short_and_multichunk_bodies_without_collecting_them() -> TestResult {
    let directory = TestDirectory::new();
    let store = test_store(&directory, TestClock::new(1_000))?;
    let service = service(Arc::clone(&store))?;
    let generation = DesktopGeneration::from_uuid(Uuid::from_u128(8));
    let facts = facts(
        "owner-a",
        generation,
        ArtifactPurposeSet::only(ArtifactPurpose::ClipboardInput),
    )?;

    for bytes in [vec![b'x'], vec![b'y'; STREAM_CHUNK_BYTES * 3 + 17]] {
        let artifact = adapter_result(
            service
                .upload_authorized(
                    facts.clone(),
                    upload_request(&bytes)?,
                    Body::from(bytes.clone()),
                )
                .await,
        )?;
        assert_eq!(artifact.content_length, bytes.len() as u64);
        assert_eq!(artifact.sha256, protocol_digest(&bytes)?);
        assert_eq!(artifact.desktop_generation, generation);
        assert_eq!(artifact.purpose, ArtifactPurpose::ClipboardInput);
    }
    assert_eq!(directory.object_directories(), 2);
    Ok(())
}

#[tokio::test]
async fn short_long_stream_error_and_digest_mismatch_leave_no_partial() -> TestResult {
    let directory = TestDirectory::new();
    let store = test_store(&directory, TestClock::new(1_000))?;
    let service = service(store)?;
    let facts = facts(
        "owner-a",
        DesktopGeneration::from_uuid(Uuid::from_u128(8)),
        ArtifactPurposeSet::only(ArtifactPurpose::ClipboardInput),
    )?;
    let declared = b"four";

    let short = service
        .upload_authorized(facts.clone(), upload_request(declared)?, Body::from("abc"))
        .await;
    assert_eq!(short.err(), Some(ControlPlaneError::InvalidRequest));

    let long = service
        .upload_authorized(
            facts.clone(),
            upload_request(declared)?,
            Body::from("abcde"),
        )
        .await;
    assert_eq!(long.err(), Some(ControlPlaneError::InvalidRequest));

    let polled = Arc::new(AtomicU64::new(0));
    let stream_polled = Arc::clone(&polled);
    let byte_stream = stream::unfold(0_u8, move |index| {
        let stream_polled = Arc::clone(&stream_polled);
        async move {
            if index == 10 {
                None
            } else {
                stream_polled.fetch_add(1, Ordering::SeqCst);
                Some((Ok::<Bytes, io::Error>(Bytes::from_static(b"x")), index + 1))
            }
        }
    });
    let bounded = service
        .upload_authorized(
            facts.clone(),
            upload_request(declared)?,
            Body::from_stream(byte_stream),
        )
        .await;
    assert_eq!(bounded.err(), Some(ControlPlaneError::InvalidRequest));
    assert_eq!(polled.load(Ordering::SeqCst), declared.len() as u64 + 1);

    let error_stream = stream::iter([
        Ok::<Bytes, io::Error>(Bytes::from_static(b"fo")),
        Err(io::Error::other("SECRET-CANARY")),
    ]);
    let failed = service
        .upload_authorized(
            facts.clone(),
            upload_request(declared)?,
            Body::from_stream(error_stream),
        )
        .await;
    assert_eq!(
        failed.as_ref().err(),
        Some(&ControlPlaneError::InvalidRequest)
    );
    assert!(!format!("{failed:?}").contains("SECRET-CANARY"));

    let mut mismatch = upload_request(declared)?;
    mismatch.expected_sha256 = Some(protocol_digest(b"nope")?);
    let failed = service
        .upload_authorized(facts, mismatch, Body::from(declared.as_slice()))
        .await;
    assert_eq!(failed.err(), Some(ControlPlaneError::InvalidRequest));
    assert_eq!(directory.object_directories(), 0);
    Ok(())
}

#[tokio::test]
async fn cancellation_removes_inflight_or_unacknowledged_publication() -> TestResult {
    let directory = TestDirectory::new();
    let store = test_store(&directory, TestClock::new(1_000))?;
    let service = Arc::new(service(store)?);
    let facts = facts(
        "owner-a",
        DesktopGeneration::from_uuid(Uuid::from_u128(8)),
        ArtifactPurposeSet::only(ArtifactPurpose::ClipboardInput),
    )?;
    let body = Body::from_stream(
        stream::once(async { Ok::<Bytes, io::Error>(Bytes::from_static(b"four")) })
            .chain(stream::pending()),
    );
    let request = upload_request(b"four")?;
    let task_service = Arc::clone(&service);
    let task = tokio::spawn(async move {
        task_service
            .upload_authorized(facts, request, body)
            .await
            .map_err(|error| format!("{error:?}"))
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    task.abort();
    let _ = task.await;

    for _ in 0..50 {
        if directory.object_directories() == 0 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("cancelled upload left an artifact directory".into())
}

#[tokio::test]
async fn idle_and_total_upload_timeouts_fail_closed_and_remove_partial_files() -> TestResult {
    for (total, idle, body) in [
        (
            Duration::from_millis(200),
            Duration::from_millis(20),
            Body::from_stream(
                stream::once(async { Ok::<Bytes, io::Error>(Bytes::from_static(b"f")) })
                    .chain(stream::pending()),
            ),
        ),
        (
            Duration::from_millis(45),
            Duration::from_millis(30),
            Body::from_stream(stream::unfold(0_u8, |index| async move {
                tokio::time::sleep(Duration::from_millis(15)).await;
                Some((
                    Ok::<Bytes, io::Error>(Bytes::from_static(b"x")),
                    index.wrapping_add(1),
                ))
            })),
        ),
    ] {
        let directory = TestDirectory::new();
        let store = test_store(&directory, TestClock::new(1_000))?;
        let service = StoreArtifactService::new(
            store,
            ArtifactRetentionPolicy::new(Duration::from_secs(1))?,
            ArtifactUploadTimeoutPolicy::new(total, idle)?,
        );
        let facts = facts(
            "owner-a",
            DesktopGeneration::from_uuid(Uuid::from_u128(8)),
            ArtifactPurposeSet::only(ArtifactPurpose::ClipboardInput),
        )?;
        let started = std::time::Instant::now();
        let result = service
            .upload_authorized(facts, upload_request(b"four")?, body)
            .await;
        assert_eq!(result.err(), Some(ControlPlaneError::InvalidRequest));
        assert!(started.elapsed() < Duration::from_millis(500));

        for _ in 0..50 {
            if directory.object_directories() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(directory.object_directories(), 0);
    }
    Ok(())
}

#[test]
fn upload_timeout_policy_rejects_zero_deadlines() {
    assert!(ArtifactUploadTimeoutPolicy::new(Duration::ZERO, Duration::from_secs(1)).is_err());
    assert!(ArtifactUploadTimeoutPolicy::new(Duration::from_secs(1), Duration::ZERO).is_err());
}

#[tokio::test]
async fn bounded_channel_applies_backpressure_and_consumer_failure_stops_upload() -> TestResult {
    let (sender, _receiver) = mpsc::channel(STREAM_CHANNEL_CHUNKS);
    for _ in 0..STREAM_CHANNEL_CHUNKS {
        sender.try_send(UploadMessage::Data(Bytes::from_static(b"x")))?;
    }
    assert!(matches!(
        sender.try_send(UploadMessage::Data(Bytes::from_static(b"x"))),
        Err(mpsc::error::TrySendError::Full(_))
    ));

    let directory = TestDirectory::new();
    let limits = ArtifactLimits::new(4, 4, 1, 4, 1, 10_000)?;
    let clock = TestClock::new(1_000);
    let store = Arc::new(ArtifactStore::with_clock(directory.path(), limits, clock)?);
    let generation = DesktopGeneration::from_uuid(Uuid::from_u128(8));
    let facts = facts(
        "owner-a",
        generation,
        ArtifactPurposeSet::only(ArtifactPurpose::ClipboardInput),
    )?;
    seed(&store, &facts, StorePurpose::ClipboardInput, b"full", 2_000)?;
    let result = service(store)?
        .upload_authorized(facts, upload_request(b"x")?, Body::from("x"))
        .await;
    assert_eq!(result.err(), Some(ControlPlaneError::ResourceExhausted));
    assert_eq!(directory.object_directories(), 1);
    Ok(())
}

#[tokio::test]
async fn download_is_scope_exact_and_returns_header_complete_verified_metadata() -> TestResult {
    let directory = TestDirectory::new();
    let store = test_store(&directory, TestClock::new(1_000))?;
    let service = service(Arc::clone(&store))?;
    let generation = DesktopGeneration::from_uuid(Uuid::from_u128(8));
    let allowed = ArtifactPurposeSet::only(ArtifactPurpose::Screenshot);
    let authorized = facts("owner-a", generation, allowed)?;
    let bytes = vec![b'z'; STREAM_CHUNK_BYTES * 2 + 9];
    let metadata = seed(&store, &authorized, StorePurpose::Screenshot, &bytes, 2_000)?;

    let download = adapter_result(
        service
            .download_authorized(authorized.clone(), access_request(&metadata))
            .await,
    )?;
    assert_eq!(download.artifact.content_type.as_str(), "image/png");
    assert_eq!(download.artifact.content_length, bytes.len() as u64);
    assert_eq!(download.artifact.sha256, protocol_digest(&bytes)?);
    assert_eq!(
        download.artifact.created_at.as_str(),
        "1970-01-01T00:00:01Z"
    );
    assert_eq!(
        download.artifact.expires_at.as_str(),
        "1970-01-01T00:00:02Z"
    );
    assert_eq!(
        to_bytes(download.body, bytes.len() + 1).await?.as_ref(),
        bytes
    );

    let wrong_owner = facts("owner-b", generation, allowed)?;
    let wrong_generation = facts(
        "owner-a",
        DesktopGeneration::from_uuid(Uuid::from_u128(9)),
        allowed,
    )?;
    let wrong_purpose = facts(
        "owner-a",
        generation,
        ArtifactPurposeSet::only(ArtifactPurpose::ActionTrace),
    )?;
    for invalid in [wrong_owner, wrong_generation, wrong_purpose] {
        let result = service
            .download_authorized(invalid, access_request(&metadata))
            .await;
        assert_eq!(result.err(), Some(ControlPlaneError::NotFound));
    }
    Ok(())
}

#[tokio::test]
async fn expiry_and_delete_remove_bytes_without_weakening_scope() -> TestResult {
    let directory = TestDirectory::new();
    let clock = TestClock::new(1_000);
    let store = test_store(&directory, clock.clone())?;
    let service = service(Arc::clone(&store))?;
    let generation = DesktopGeneration::from_uuid(Uuid::from_u128(8));
    let facts = facts(
        "owner-a",
        generation,
        ArtifactPurposeSet::only(ArtifactPurpose::Screenshot),
    )?;
    let first = seed(&store, &facts, StorePurpose::Screenshot, b"expired", 1_100)?;
    clock.set(1_100);
    let expired = service
        .download_authorized(facts.clone(), access_request(&first))
        .await;
    assert_eq!(expired.err(), Some(ControlPlaneError::NotFound));

    clock.set(1_200);
    let second = seed(
        &store,
        &facts,
        StorePurpose::Screenshot,
        b"delete-me",
        2_000,
    )?;
    let wrong_owner = RequestFacts {
        owner: ArtifactOwner::new("owner-b")?,
        ..facts.clone()
    };
    let denied = service
        .delete_authorized(wrong_owner, access_request(&second))
        .await;
    assert_eq!(denied.err(), Some(ControlPlaneError::NotFound));
    let deleted = adapter_result(
        service
            .delete_authorized(facts.clone(), access_request(&second))
            .await,
    )?;
    assert_eq!(deleted.artifact_id.as_uuid(), second.id().as_uuid());
    assert_eq!(deleted.purpose, ArtifactPurpose::Screenshot);
    let absent = service
        .delete_authorized(facts, access_request(&second))
        .await;
    assert_eq!(absent.err(), Some(ControlPlaneError::NotFound));
    assert_eq!(directory.object_directories(), 0);
    Ok(())
}

#[tokio::test]
async fn generated_publication_is_purpose_bound_private_and_redacted() -> TestResult {
    let directory = TestDirectory::new();
    let store = test_store(&directory, TestClock::new(1_000))?;
    let service = service(store)?;
    let generation = DesktopGeneration::from_uuid(Uuid::from_u128(8));
    let context = internal_context("owner-a", generation)
        .map_err(|_| io::Error::other("invalid internal context"))?;

    for (purpose, media_type, body) in [
        (
            ArtifactPurpose::ClipboardOutput,
            "text/plain;charset=utf-8",
            b"SECRET-CLIPBOARD".as_slice(),
        ),
        (
            ArtifactPurpose::Screenshot,
            "image/png",
            b"PNG-BYTES".as_slice(),
        ),
    ] {
        let request = GeneratedArtifactRequest::new(
            purpose,
            ArtifactContentType::new(media_type)?,
            body.to_vec(),
        )
        .map_err(|_| io::Error::other("invalid generated request"))?;
        let debug = format!("{request:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("SECRET-CLIPBOARD"));
        let artifact = adapter_result(service.publish_generated(context.clone(), request).await)?;
        assert_eq!(artifact.purpose, purpose);
        assert_eq!(artifact.desktop_generation, generation);
        assert_eq!(artifact.content_type.as_str(), media_type);
        assert_eq!(artifact.content_length, body.len() as u64);
        assert_eq!(artifact.sha256, protocol_digest(body)?);
        assert_eq!(artifact.created_at.as_str(), "1970-01-01T00:00:01Z");
        assert_eq!(artifact.expires_at.as_str(), "1970-01-01T00:00:02Z");
    }
    assert_eq!(directory.object_directories(), 2);

    for purpose in [
        ArtifactPurpose::ClipboardInput,
        ArtifactPurpose::ActionTrace,
        ArtifactPurpose::SupportBundle,
    ] {
        assert_eq!(
            GeneratedArtifactRequest::new(
                purpose,
                ArtifactContentType::new("application/octet-stream")?,
                vec![1],
            )
            .err(),
            Some(ControlPlaneError::InvalidRequest)
        );
    }
    assert_eq!(
        GeneratedArtifactRequest::new(
            ArtifactPurpose::Screenshot,
            ArtifactContentType::new("image/png")?,
            Vec::new(),
        )
        .err(),
        Some(ControlPlaneError::InvalidRequest)
    );
    Ok(())
}

#[tokio::test]
async fn clipboard_input_consumption_rechecks_complete_reference_and_scope() -> TestResult {
    let directory = TestDirectory::new();
    let clock = TestClock::new(1_000);
    let store = test_store(&directory, clock.clone())?;
    let service = service(store)?;
    let generation = DesktopGeneration::from_uuid(Uuid::from_u128(8));
    let authorized = facts(
        "owner-a",
        generation,
        ArtifactPurposeSet::only(ArtifactPurpose::ClipboardInput),
    )?;
    let body = b"private command input";
    let artifact = adapter_result(
        service
            .upload_authorized(
                authorized,
                upload_request(body)?,
                Body::from(body.as_slice()),
            )
            .await,
    )?;
    let context = internal_context("owner-a", generation)
        .map_err(|_| io::Error::other("invalid internal context"))?;
    assert_eq!(
        adapter_result(
            service
                .read_clipboard_input(&context, &artifact, body.len() as u64)
                .await,
        )?,
        body
    );

    let wrong_owner = internal_context("owner-b", generation)
        .map_err(|_| io::Error::other("invalid internal context"))?;
    assert_eq!(
        service
            .read_clipboard_input(&wrong_owner, &artifact, body.len() as u64)
            .await
            .err(),
        Some(ControlPlaneError::NotFound)
    );
    let mut tampered = artifact.clone();
    tampered.sha256 = protocol_digest(b"different")?;
    assert_eq!(
        service
            .read_clipboard_input(&context, &tampered, body.len() as u64)
            .await
            .err(),
        Some(ControlPlaneError::NotFound)
    );
    assert_eq!(
        service
            .read_clipboard_input(&context, &artifact, (body.len() - 1) as u64)
            .await
            .err(),
        Some(ControlPlaneError::InvalidRequest)
    );

    clock.set(2_000);
    assert_eq!(
        service
            .read_clipboard_input(&context, &artifact, body.len() as u64)
            .await
            .err(),
        Some(ControlPlaneError::NotFound)
    );
    Ok(())
}

#[test]
fn internal_artifact_context_and_retention_reject_invalid_shapes() {
    assert!(
        InternalArtifactContext::new(
            "",
            DesktopId::from_uuid(Uuid::from_u128(7)),
            DesktopGeneration::from_uuid(Uuid::from_u128(8)),
        )
        .is_err()
    );
    assert!(
        ArtifactRetentionPolicy::new(Duration::from_secs(1))
            .and_then(|policy| policy.with_generated_retention(Duration::ZERO))
            .is_err()
    );
}
