use std::{
    error::Error,
    fs,
    io::{self, Cursor, Read},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::*;

type TestResult = Result<(), Box<dyn Error>>;

const CONCURRENCY_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn probe_rejects_a_rebound_store_root_without_mutating_it() -> TestResult {
    let directory = TestDirectory::new();
    let root = directory.path().to_path_buf();
    let store = ArtifactStore::with_clock(&root, limits(8, 16, 2, 16)?, TestClock::new(1_000))?;
    assert!(store.probe().is_ok());

    let displaced = root.with_extension("displaced");
    fs::rename(&root, &displaced)?;
    fs::create_dir(&root)?;
    assert!(matches!(store.probe(), Err(StoreError::RootChanged)));
    assert!(fs::read_dir(&root)?.next().is_none());

    fs::remove_dir(&root)?;
    fs::rename(displaced, &root)?;
    assert!(store.probe().is_ok());
    Ok(())
}

struct BlockingReader {
    started: Option<mpsc::SyncSender<()>>,
    release: mpsc::Receiver<()>,
    body: Cursor<Vec<u8>>,
}

impl Read for BlockingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(started) = self.started.take() {
            started
                .send(())
                .map_err(|_| io::Error::other("blocking reader start receiver closed"))?;
            self.release
                .recv()
                .map_err(|_| io::Error::other("blocking reader release sender closed"))?;
        }
        self.body.read(buffer)
    }
}

fn blocking_reader(body: &[u8]) -> (BlockingReader, mpsc::Receiver<()>, mpsc::SyncSender<()>) {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    (
        BlockingReader {
            started: Some(started_tx),
            release: release_rx,
            body: Cursor::new(body.to_vec()),
        },
        started_rx,
        release_tx,
    )
}

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
        let path = std::env::temp_dir().join(format!("xenoteer-artifacts-test-{}", Uuid::new_v4()));
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn owner(value: &str) -> Result<ArtifactOwner, ValidationError> {
    ArtifactOwner::new(value)
}

fn provenance() -> Result<CapabilityProvenance, ValidationError> {
    CapabilityProvenance::new("capture:read", Some("test-policy-v1".to_owned()))
}

fn scope(owner_value: &str) -> Result<ArtifactScope, ValidationError> {
    Ok(ArtifactScope::new(
        owner(owner_value)?,
        generation(),
        ArtifactPurpose::Screenshot,
        provenance()?,
    ))
}

fn generation() -> DesktopGeneration {
    DesktopGeneration::from_uuid(Uuid::from_u128(42))
}

fn request(
    owner: ArtifactOwner,
    size: u64,
    expires_at: u64,
) -> Result<ArtifactCreate, ValidationError> {
    ArtifactCreate::new(
        owner,
        ArtifactPurpose::Screenshot,
        generation(),
        provenance()?,
        "image/png",
        size,
        TimestampMillis::from_unix_millis(expires_at),
    )
}

fn limits(
    max_object: u64,
    total: u64,
    objects: u64,
    owner_bytes: u64,
) -> Result<ArtifactLimits, StoreError> {
    ArtifactLimits::new(max_object, total, objects, owner_bytes, objects, 10_000)
}

#[test]
fn creates_reads_and_deletes_private_immutable_artifact() -> TestResult {
    let directory = TestDirectory::new();
    let clock = TestClock::new(1_000);
    let store = ArtifactStore::with_clock(directory.path(), limits(64, 256, 8, 128)?, clock)?;
    assert_eq!(store.current_time().as_unix_millis(), 1_000);
    let bytes = b"bounded screenshot";
    let digest = Sha256Digest::from_bytes(Sha256::digest(bytes).into());
    let metadata = store.create(
        request(owner("principal-a")?, bytes.len() as u64, 2_000)?.with_expected_sha256(digest),
        Cursor::new(bytes),
    )?;

    assert_eq!(metadata.sha256(), digest);
    assert_eq!(metadata.size(), bytes.len() as u64);
    assert_eq!(metadata.owner().as_str(), "principal-a");
    assert_eq!(metadata.provenance().capability(), "capture:read");

    let scope = scope("principal-a")?;
    let mut opened = store.open_body(metadata.id(), &scope)?;
    let mut actual = Vec::new();
    opened.read_to_end(&mut actual)?;
    assert_eq!(actual, bytes);

    let record = directory.path().join(metadata.id().to_string());
    assert_eq!(
        fs::metadata(directory.path())?.permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(record.join("body"))?.permissions().mode() & 0o777,
        0o600
    );

    let deleted = store.delete_with_metadata(metadata.id(), &scope)?;
    assert_eq!(deleted, metadata);
    assert!(matches!(
        store.open_body(metadata.id(), &scope),
        Err(StoreError::NotFound)
    ));
    Ok(())
}

#[test]
fn wrong_owner_or_generation_never_reveals_artifact() -> TestResult {
    let directory = TestDirectory::new();
    let store = ArtifactStore::with_clock(
        directory.path(),
        limits(64, 256, 8, 128)?,
        TestClock::new(1_000),
    )?;
    let metadata = store.create(request(owner("principal-a")?, 1, 2_000)?, Cursor::new(b"x"))?;

    let wrong_owner = ArtifactScope::new(
        owner("principal-b")?,
        generation(),
        ArtifactPurpose::Screenshot,
        provenance()?,
    );
    let wrong_generation = ArtifactScope::new(
        owner("principal-a")?,
        DesktopGeneration::from_uuid(Uuid::from_u128(99)),
        ArtifactPurpose::Screenshot,
        provenance()?,
    );
    assert!(matches!(
        store.open_body(metadata.id(), &wrong_owner),
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.delete(metadata.id(), &wrong_generation),
        Err(StoreError::NotFound)
    ));
    Ok(())
}

#[test]
fn failed_size_and_digest_release_quota_and_leave_no_temporary_entry() -> TestResult {
    let directory = TestDirectory::new();
    let store =
        ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000))?;
    let wrong_digest = Sha256Digest::from_bytes([7; 32]);
    let error = store
        .create(
            request(owner("principal-a")?, 8, 2_000)?.with_expected_sha256(wrong_digest),
            Cursor::new(b"12345678"),
        )
        .err();
    assert!(matches!(error, Some(StoreError::DigestMismatch)));

    let error = store
        .create(
            request(owner("principal-a")?, 8, 2_000)?,
            Cursor::new(b"123456789"),
        )
        .err();
    assert!(matches!(error, Some(StoreError::SizeMismatch)));

    store.create(
        request(owner("principal-a")?, 8, 2_000)?,
        Cursor::new(b"12345678"),
    )?;
    let object_directories = fs::read_dir(directory.path())?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .collect::<Vec<_>>();
    assert_eq!(object_directories.len(), 1);
    assert!(
        !object_directories[0]
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp-")
    );
    Ok(())
}

#[test]
fn deterministic_expiry_cleanup_reclaims_quota() -> TestResult {
    let directory = TestDirectory::new();
    let clock = TestClock::new(1_000);
    let store = ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, clock.clone())?;
    let first = store.create(
        request(owner("principal-a")?, 8, 2_000)?,
        Cursor::new(b"12345678"),
    )?;
    clock.set(2_000);

    let scope = scope("principal-a")?;
    assert!(matches!(
        store.open_body(first.id(), &scope),
        Err(StoreError::Expired)
    ));
    store.create(
        request(owner("principal-a")?, 8, 3_000)?,
        Cursor::new(b"abcdefgh"),
    )?;
    Ok(())
}

#[test]
fn startup_removes_temporary_symlink_without_following_it() -> TestResult {
    let directory = TestDirectory::new();
    let initialized =
        ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000))?;
    drop(initialized);
    let outside = directory.path().with_extension("outside");
    fs::write(&outside, b"sentinel")?;
    let temporary = format!(".tmp-{}", Uuid::new_v4().hyphenated());
    symlink(&outside, directory.path().join(&temporary))?;

    let store =
        ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000))?;
    drop(store);
    assert_eq!(fs::read(&outside)?, b"sentinel");
    assert!(!directory.path().join(temporary).exists());
    fs::remove_file(outside)?;
    Ok(())
}

#[test]
fn unmarked_non_store_root_preserves_temporary_sentinel() -> TestResult {
    let directory = TestDirectory::new();
    fs::create_dir(directory.path())?;
    let temporary = directory
        .path()
        .join(format!(".tmp-{}", Uuid::new_v4().hyphenated()));
    fs::create_dir(&temporary)?;
    let sentinel = temporary.join("sentinel");
    fs::write(&sentinel, b"do not delete")?;

    let result =
        ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000));
    assert!(matches!(result, Err(StoreError::RootNotInitialized)));
    assert_eq!(fs::read(sentinel)?, b"do not delete");
    Ok(())
}

#[test]
fn relative_store_roots_are_rejected_before_filesystem_access() -> TestResult {
    let result = ArtifactStore::with_clock(
        Path::new("relative-artifact-root"),
        limits(8, 8, 1, 8)?,
        TestClock::new(1_000),
    );
    assert!(matches!(result, Err(StoreError::RootNotAbsolute)));
    Ok(())
}

#[test]
fn initialized_store_does_not_delete_noncanonical_temporary_names() -> TestResult {
    let directory = TestDirectory::new();
    let initialized =
        ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000))?;
    drop(initialized);
    let temporary = directory.path().join(".tmp-not-an-artifact");
    fs::create_dir(&temporary)?;
    let sentinel = temporary.join("sentinel");
    fs::write(&sentinel, b"do not delete")?;

    let result =
        ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000));
    assert!(matches!(result, Err(StoreError::UnexpectedEntry)));
    assert_eq!(fs::read(sentinel)?, b"do not delete");
    Ok(())
}

#[test]
fn wrong_purpose_or_provenance_never_authorizes_access() -> TestResult {
    let directory = TestDirectory::new();
    let store = ArtifactStore::with_clock(
        directory.path(),
        limits(64, 256, 8, 128)?,
        TestClock::new(1_000),
    )?;
    let metadata = store.create(request(owner("principal-a")?, 1, 2_000)?, Cursor::new(b"x"))?;

    let wrong_purpose = ArtifactScope::new(
        owner("principal-a")?,
        generation(),
        ArtifactPurpose::ActionTrace,
        provenance()?,
    );
    let wrong_provenance = ArtifactScope::new(
        owner("principal-a")?,
        generation(),
        ArtifactPurpose::Screenshot,
        CapabilityProvenance::new("capture:read", Some("other-policy".to_owned()))?,
    );
    assert!(matches!(
        store.open_body(metadata.id(), &wrong_purpose),
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.delete(metadata.id(), &wrong_provenance),
        Err(StoreError::NotFound)
    ));
    store.open_body(metadata.id(), &scope("principal-a")?)?;
    Ok(())
}

#[test]
fn same_size_body_tampering_is_rejected_before_exposure() -> TestResult {
    let directory = TestDirectory::new();
    let store = ArtifactStore::with_clock(
        directory.path(),
        limits(64, 256, 8, 128)?,
        TestClock::new(1_000),
    )?;
    let bytes = b"bounded screenshot";
    let metadata = store.create(
        request(owner("principal-a")?, bytes.len() as u64, 2_000)?,
        Cursor::new(bytes),
    )?;
    let body_path = directory
        .path()
        .join(metadata.id().to_string())
        .join("body");
    fs::write(body_path, vec![b'z'; bytes.len()])?;

    assert!(matches!(
        store.open_body(metadata.id(), &scope("principal-a")?),
        Err(StoreError::CorruptArtifact {
            kind: ArtifactCorruption::BodyDigest,
            ..
        })
    ));
    assert!(matches!(
        store.delete_with_metadata(metadata.id(), &scope("principal-a")?),
        Err(StoreError::CorruptArtifact {
            kind: ArtifactCorruption::BodyDigest,
            ..
        })
    ));
    Ok(())
}

#[test]
fn malformed_metadata_errors_never_echo_persisted_content() -> TestResult {
    let directory = TestDirectory::new();
    let store = ArtifactStore::with_clock(
        directory.path(),
        limits(64, 256, 8, 128)?,
        TestClock::new(1_000),
    )?;
    let metadata = store.create(request(owner("principal-a")?, 1, 2_000)?, Cursor::new(b"x"))?;
    let metadata_path = directory
        .path()
        .join(metadata.id().to_string())
        .join("metadata.json");
    fs::write(&metadata_path, b"SECRET-CANARY invalid json")?;

    let error = store
        .open_body(metadata.id(), &scope("principal-a")?)
        .err()
        .ok_or("corrupt metadata unexpectedly opened")?;
    assert!(matches!(
        &error,
        StoreError::CorruptArtifact {
            kind: ArtifactCorruption::MetadataInvalid,
            ..
        }
    ));
    assert!(!error.to_string().contains("SECRET-CANARY"));
    Ok(())
}

#[test]
fn persisted_records_with_broadened_permissions_are_rejected_not_repaired() -> TestResult {
    let directory = TestDirectory::new();
    let store = ArtifactStore::with_clock(
        directory.path(),
        limits(64, 256, 8, 128)?,
        TestClock::new(1_000),
    )?;
    let metadata = store.create(request(owner("principal-a")?, 1, 2_000)?, Cursor::new(b"x"))?;
    let body_path = directory
        .path()
        .join(metadata.id().to_string())
        .join("body");
    fs::set_permissions(&body_path, fs::Permissions::from_mode(0o644))?;

    assert!(matches!(
        store.open_body(metadata.id(), &scope("principal-a")?),
        Err(StoreError::CorruptArtifact {
            kind: ArtifactCorruption::FileType,
            ..
        })
    ));
    assert_eq!(fs::metadata(body_path)?.permissions().mode() & 0o777, 0o644);
    Ok(())
}

#[test]
fn one_live_store_exclusively_owns_its_root() -> TestResult {
    let directory = TestDirectory::new();
    let first =
        ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000))?;
    let second =
        ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000));
    assert!(matches!(second, Err(StoreError::RootAlreadyOpen)));
    drop(first);
    ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000))?;
    Ok(())
}

#[test]
fn reopening_reconciles_existing_usage_before_accepting_writes() -> TestResult {
    let directory = TestDirectory::new();
    let first =
        ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000))?;
    first.create(
        request(owner("principal-a")?, 8, 2_000)?,
        Cursor::new(b"12345678"),
    )?;
    drop(first);

    let reopened =
        ArtifactStore::with_clock(directory.path(), limits(8, 8, 1, 8)?, TestClock::new(1_000))?;
    assert!(matches!(
        reopened.create(request(owner("principal-a")?, 1, 2_000)?, Cursor::new(b"x")),
        Err(StoreError::StoreQuotaExceeded)
    ));
    Ok(())
}

#[test]
fn blocked_stream_does_not_block_open_delete_or_generated_publication() -> TestResult {
    let directory = TestDirectory::new();
    let store = Arc::new(ArtifactStore::with_clock(
        directory.path(),
        limits(64, 512, 16, 256)?,
        TestClock::new(1_000),
    )?);
    let readable = store.create(
        request(owner("reader-owner")?, 8, 2_000)?,
        Cursor::new(b"readable"),
    )?;
    let deletable = store.create(
        request(owner("delete-owner")?, 8, 2_000)?,
        Cursor::new(b"delete!!"),
    )?;
    let readable_id = readable.id();
    let deletable_id = deletable.id();
    let readable_scope = scope("reader-owner")?;
    let deletable_scope = scope("delete-owner")?;

    let (reader, started_rx, release_tx) = blocking_reader(b"slow-body");
    let slow_store = Arc::clone(&store);
    let slow =
        thread::spawn(move || slow_store.create(request(owner("slow-owner")?, 9, 2_000)?, reader));
    if let Err(error) = started_rx.recv_timeout(CONCURRENCY_TIMEOUT) {
        let _ignored = release_tx.send(());
        let _ignored = slow.join();
        return Err(error.into());
    }

    let (open_tx, open_rx) = mpsc::sync_channel(1);
    let open_store = Arc::clone(&store);
    let open = thread::spawn(move || {
        let result = open_store
            .open_body(readable_id, &readable_scope)
            .map(|opened| opened.metadata().id());
        let _ignored = open_tx.send(result);
    });
    let (delete_tx, delete_rx) = mpsc::sync_channel(1);
    let delete_store = Arc::clone(&store);
    let delete = thread::spawn(move || {
        let result = delete_store
            .delete_with_metadata(deletable_id, &deletable_scope)
            .map(|metadata| metadata.id());
        let _ignored = delete_tx.send(result);
    });
    let (create_tx, create_rx) = mpsc::sync_channel(1);
    let create_store = Arc::clone(&store);
    let generated = thread::spawn(move || {
        let result = (|| -> Result<ArtifactMetadata, StoreError> {
            let request = request(owner("generated-owner")?, 9, 2_000)?;
            create_store.create(request, Cursor::new(b"generated"))
        })()
        .map(|metadata| metadata.id());
        let _ignored = create_tx.send(result);
    });

    let opened_before_release = open_rx.recv_timeout(CONCURRENCY_TIMEOUT);
    let deleted_before_release = delete_rx.recv_timeout(CONCURRENCY_TIMEOUT);
    let generated_before_release = create_rx.recv_timeout(CONCURRENCY_TIMEOUT);
    release_tx.send(())?;

    let slow_metadata = slow
        .join()
        .map_err(|_| io::Error::other("slow artifact thread panicked"))??;
    open.join()
        .map_err(|_| io::Error::other("artifact open thread panicked"))?;
    delete
        .join()
        .map_err(|_| io::Error::other("artifact delete thread panicked"))?;
    generated
        .join()
        .map_err(|_| io::Error::other("generated artifact thread panicked"))?;

    assert_eq!(opened_before_release??, readable_id);
    assert_eq!(deleted_before_release??, deletable_id);
    let generated_id = generated_before_release??;
    assert_ne!(generated_id, slow_metadata.id());
    store.open_body(slow_metadata.id(), &scope("slow-owner")?)?;
    Ok(())
}

#[test]
fn cleanup_preserves_active_temporary_and_its_quota_reservation() -> TestResult {
    let directory = TestDirectory::new();
    let clock = TestClock::new(1_000);
    let store = Arc::new(ArtifactStore::with_clock(
        directory.path(),
        limits(8, 16, 2, 16)?,
        clock.clone(),
    )?);
    store.create(
        request(owner("expired-owner")?, 8, 2_000)?,
        Cursor::new(b"expired!"),
    )?;
    let abandoned = directory
        .path()
        .join(format!(".tmp-{}", Uuid::from_u128(9_999).hyphenated()));
    fs::create_dir(&abandoned)?;
    clock.set(2_000);

    let (reader, started_rx, release_tx) = blocking_reader(b"reserved");
    let slow_store = Arc::clone(&store);
    let slow =
        thread::spawn(move || slow_store.create(request(owner("slow-owner")?, 8, 3_000)?, reader));
    if let Err(error) = started_rx.recv_timeout(CONCURRENCY_TIMEOUT) {
        let _ignored = release_tx.send(());
        let _ignored = slow.join();
        return Err(error.into());
    }

    let (cleanup_tx, cleanup_rx) = mpsc::sync_channel(1);
    let cleanup_store = Arc::clone(&store);
    let cleanup = thread::spawn(move || {
        let _ignored = cleanup_tx.send(cleanup_store.cleanup_expired());
    });
    let cleanup_before_release = cleanup_rx.recv_timeout(CONCURRENCY_TIMEOUT);

    let (create_tx, create_rx) = mpsc::sync_channel(1);
    let create_store = Arc::clone(&store);
    let generated = thread::spawn(move || {
        let result = (|| -> Result<ArtifactMetadata, StoreError> {
            let request = request(owner("generated-owner")?, 8, 3_000)?;
            create_store.create(request, Cursor::new(b"generate"))
        })();
        let _ignored = create_tx.send(result);
    });
    let generated_before_release = create_rx.recv_timeout(CONCURRENCY_TIMEOUT);
    release_tx.send(())?;

    let slow_metadata = slow
        .join()
        .map_err(|_| io::Error::other("slow artifact thread panicked"))??;
    cleanup
        .join()
        .map_err(|_| io::Error::other("artifact cleanup thread panicked"))?;
    generated
        .join()
        .map_err(|_| io::Error::other("generated artifact thread panicked"))?;

    let report = cleanup_before_release??;
    assert_eq!(report.removed_objects(), 1);
    assert_eq!(report.removed_bytes(), 8);
    assert_eq!(report.removed_temporary_entries(), 1);
    assert!(!abandoned.exists());
    let generated_metadata = generated_before_release??;
    store.open_body(slow_metadata.id(), &scope("slow-owner")?)?;
    store.open_body(generated_metadata.id(), &scope("generated-owner")?)?;
    assert!(matches!(
        store.create(
            request(owner("over-quota-owner")?, 1, 3_000)?,
            Cursor::new(b"x")
        ),
        Err(StoreError::StoreQuotaExceeded)
    ));
    Ok(())
}

#[test]
fn runtime_cleanup_rejects_replaced_active_temporary_without_following_it() -> TestResult {
    let directory = TestDirectory::new();
    let store = Arc::new(ArtifactStore::with_clock(
        directory.path(),
        limits(8, 16, 2, 16)?,
        TestClock::new(1_000),
    )?);
    let (reader, started_rx, release_tx) = blocking_reader(b"reserved");
    let slow_store = Arc::clone(&store);
    let slow =
        thread::spawn(move || slow_store.create(request(owner("slow-owner")?, 8, 2_000)?, reader));
    if let Err(error) = started_rx.recv_timeout(CONCURRENCY_TIMEOUT) {
        let _ignored = release_tx.send(());
        let _ignored = slow.join();
        return Err(error.into());
    }

    let temporary_entries = fs::read_dir(directory.path())?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".tmp-"))
        })
        .collect::<Vec<_>>();
    assert_eq!(temporary_entries.len(), 1);
    assert!(temporary_entries[0].file_type()?.is_dir());
    let temporary = temporary_entries[0].path();
    fs::remove_dir_all(&temporary)?;
    let outside = directory.path().with_extension("active-outside");
    fs::create_dir(&outside)?;
    let sentinel = outside.join("sentinel");
    fs::write(&sentinel, b"do not follow")?;
    symlink(&outside, &temporary)?;

    let (cleanup_tx, cleanup_rx) = mpsc::sync_channel(1);
    let cleanup_store = Arc::clone(&store);
    let cleanup = thread::spawn(move || {
        let _ignored = cleanup_tx.send(cleanup_store.cleanup_expired());
    });
    let cleanup_before_release = cleanup_rx.recv_timeout(CONCURRENCY_TIMEOUT);
    release_tx.send(())?;
    let result = slow
        .join()
        .map_err(|_| io::Error::other("slow artifact thread panicked"))?;
    cleanup
        .join()
        .map_err(|_| io::Error::other("artifact cleanup thread panicked"))?;
    assert!(matches!(
        cleanup_before_release?,
        Err(StoreError::UnexpectedEntry)
    ));
    assert!(result.is_err());
    assert_eq!(fs::read(&sentinel)?, b"do not follow");
    assert!(!outside.join("metadata.json").exists());
    assert!(!temporary.exists());

    store.create(
        request(owner("recovered-owner")?, 8, 2_000)?,
        Cursor::new(b"recovery"),
    )?;
    fs::remove_dir_all(outside)?;
    Ok(())
}
