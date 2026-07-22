#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};

use xenoteer_protocol::{
    DesktopGeneration, DesktopId, ScreenshotScale, WindowCaptureSpace, WindowIdentityHash,
};

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FakeMode {
    Healthy,
    Malformed,
    Unavailable,
    Panic,
}

struct CaptureGate {
    entered: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}

impl CaptureGate {
    fn new() -> Self {
        Self {
            entered: AtomicBool::new(false),
            released: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn wait_until_entered(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.entered.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(self.entered.load(Ordering::Acquire));
    }

    fn release(&self) {
        *lock_mutex(&self.released) = true;
        self.changed.notify_all();
    }

    fn block(&self) {
        self.entered.store(true, Ordering::Release);
        let guard = lock_mutex(&self.released);
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(2), |released| !*released)
            .unwrap_or_else(|error| error.into_inner());
        assert!(!timeout.timed_out());
    }
}

struct FakeBackend {
    mode: FakeMode,
    gate: Option<Arc<CaptureGate>>,
    calls: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    owner_thread: Option<thread::ThreadId>,
}

impl FakeBackend {
    fn new(mode: FakeMode) -> (Self, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        (
            Self {
                mode,
                gate: None,
                calls: Arc::clone(&calls),
                shutdown: Arc::clone(&shutdown),
                owner_thread: None,
            },
            calls,
            shutdown,
        )
    }
}

impl CaptureBackend for FakeBackend {
    fn capture(
        &mut self,
        request: &ScreenshotRequest,
        revalidate: Option<Revalidator>,
    ) -> std::result::Result<CapturedFrame, CaptureBackendError> {
        let current = thread::current().id();
        if let Some(owner) = self.owner_thread {
            assert_eq!(owner, current);
        } else {
            self.owner_thread = Some(current);
        }
        self.calls.fetch_add(1, Ordering::AcqRel);
        if let Some(gate) = self.gate.take() {
            gate.block();
        }
        match self.mode {
            FakeMode::Unavailable => return Err(CaptureBackendError::Unavailable),
            FakeMode::Panic => panic!("contained capture backend panic"),
            FakeMode::Healthy | FakeMode::Malformed => {}
        }
        match &request.target {
            ScreenshotTarget::Root => assert!(revalidate.is_none()),
            ScreenshotTarget::WindowVisible { window, .. }
            | ScreenshotTarget::WindowDrawable { window } => {
                revalidate.ok_or(CaptureBackendError::Operation(
                    CaptureActorFailureKind::StaleReference,
                ))?(window)
                .map_err(|error| {
                    CaptureBackendError::Operation(match error {
                        RawCaptureRevalidationError::StaleReference => {
                            CaptureActorFailureKind::StaleReference
                        }
                        RawCaptureRevalidationError::TargetVanished => {
                            CaptureActorFailureKind::TargetVanished
                        }
                        RawCaptureRevalidationError::Unavailable => {
                            CaptureActorFailureKind::BackendUnavailable
                        }
                    })
                })?;
            }
        }
        let limitation = match request.target {
            ScreenshotTarget::Root => RawCaptureLimitation::RootVisibleFramebuffer,
            ScreenshotTarget::WindowVisible { .. } => {
                RawCaptureLimitation::WindowVisibleIncludesOccluders
            }
            ScreenshotTarget::WindowDrawable { .. } => {
                RawCaptureLimitation::WindowDrawableObscuredUndefined
            }
        };
        let mut frame = CapturedFrame {
            target: request.target.clone(),
            source_region: Rect::new(0, 0, 2, 1).unwrap(),
            width: 2,
            height: 1,
            bgra: vec![0, 0, 255, 255, 255, 0, 0, 255],
            cursor: CursorCaptureEvidence {
                requested: request.include_cursor,
                composited: false,
                serial_before: request.include_cursor.then_some(1),
                serial_after: request.include_cursor.then_some(1),
                moved_during_capture: false,
            },
            limitation,
            transport: RawCaptureTransport::CoreGetImage,
            shm_fallback: false,
        };
        if self.mode == FakeMode::Malformed {
            frame.bgra.pop();
        }
        Ok(frame)
    }

    fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn spawn_fake(backend: FakeBackend) -> (CaptureActorHandle, CaptureActorJoin) {
    spawn_with_backend(|| Ok(backend)).unwrap()
}

fn reference() -> WindowRef {
    WindowRef {
        desktop_id: DesktopId::new(),
        desktop_generation: DesktopGeneration::new(),
        xid: 17,
        observed_generation: 1,
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
        max_bytes: None,
    }
}

fn window_request() -> ScreenshotRequest {
    ScreenshotRequest {
        target: ScreenshotTarget::WindowVisible {
            window: reference(),
            coordinate_space: WindowCaptureSpace::Client,
        },
        region: None,
        format: ScreenshotFormat::RawBgra,
        include_cursor: false,
        scale: None,
        max_bytes: None,
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

#[test]
fn raw_and_png_outputs_are_bounded_redacted_and_processed_off_actor()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (backend, calls, shutdown) = FakeBackend::new(FakeMode::Healthy);
    let (handle, join) = spawn_fake(backend);
    let raw = handle
        .try_capture_root(
            root_request(ScreenshotFormat::RawBgra),
            deadline(),
            CancellationToken::new(),
        )?
        .recv_timeout(Duration::from_secs(1))??;
    assert_eq!(raw.bytes.expose_secret(), &[0, 0, 255, 255, 255, 0, 0, 255]);
    assert_eq!(raw.raw_stride_bytes, Some(8));
    assert!(!format!("{raw:?}").contains("255, 255"));

    let mut png_request = root_request(ScreenshotFormat::Png);
    png_request.scale = Some(ScreenshotScale {
        width: Some(4),
        height: Some(2),
        filter: ScreenshotResizeFilter::Nearest,
    });
    let png = handle
        .try_capture_root(png_request, deadline(), CancellationToken::new())?
        .recv_timeout(Duration::from_secs(1))??;
    assert_eq!(&png.bytes.expose_secret()[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(png.output_size, Size::new(4, 2)?);
    assert_eq!(calls.load(Ordering::Acquire), 2);
    let health = handle.health();
    assert_eq!(health.captured_commands, 2);
    assert_eq!(health.core_captures, 2);
    assert_eq!(health.shm_captures, 0);
    assert_eq!(health.shm_fallbacks, 0);
    assert_eq!(join.join(), CaptureActorExit::Stopped);
    assert!(shutdown.load(Ordering::Acquire));
    Ok(())
}

#[test]
fn exact_reference_revalidator_runs_once_on_owner_thread_before_capture()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (backend, _calls, _shutdown) = FakeBackend::new(FakeMode::Healthy);
    let (handle, join) = spawn_fake(backend);
    let revalidations = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&revalidations);
    let actor_name = Arc::new(Mutex::new(None));
    let actor_name_out = Arc::clone(&actor_name);
    let result = handle
        .try_capture_window(
            window_request(),
            deadline(),
            CancellationToken::new(),
            move |reference| {
                assert_eq!(reference.xid, 17);
                observed.fetch_add(1, Ordering::AcqRel);
                *lock_mutex(&actor_name_out) = thread::current().name().map(str::to_owned);
                Ok(())
            },
        )?
        .recv_timeout(Duration::from_secs(1))??;
    assert_eq!(revalidations.load(Ordering::Acquire), 1);
    assert_eq!(
        lock_mutex(&actor_name).as_deref(),
        Some("xenoteer-capture-actor")
    );
    assert_eq!(
        result.limitation,
        RawCaptureLimitation::WindowVisibleIncludesOccluders
    );
    assert_eq!(join.join(), CaptureActorExit::Stopped);
    Ok(())
}

#[test]
fn cancellation_and_deadline_after_capture_discard_output()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (backend, _calls, _shutdown) = FakeBackend::new(FakeMode::Healthy);
    let (handle, join) = spawn_fake(backend);
    let cancellation = CancellationToken::new();
    let cancel_from_revalidation = cancellation.clone();
    let cancelled = handle
        .try_capture_window(window_request(), deadline(), cancellation, move |_| {
            cancel_from_revalidation.cancel();
            Ok(())
        })?
        .recv_timeout(Duration::from_secs(1))?
        .unwrap_err();
    assert_eq!(cancelled.kind, CaptureActorFailureKind::Cancelled);

    let late = handle
        .try_capture_window(
            window_request(),
            Instant::now() + Duration::from_millis(5),
            CancellationToken::new(),
            |_| {
                thread::sleep(Duration::from_millis(10));
                Ok(())
            },
        )?
        .recv_timeout(Duration::from_secs(1))?
        .unwrap_err();
    assert_eq!(late.kind, CaptureActorFailureKind::DeadlineExceeded);
    assert_eq!(join.join(), CaptureActorExit::Stopped);
    Ok(())
}

#[test]
fn stale_revalidation_is_typed_and_prevents_result()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (backend, _calls, _shutdown) = FakeBackend::new(FakeMode::Healthy);
    let (handle, join) = spawn_fake(backend);
    let failure = handle
        .try_capture_window(
            window_request(),
            deadline(),
            CancellationToken::new(),
            |_| Err(RawCaptureRevalidationError::StaleReference),
        )?
        .recv_timeout(Duration::from_secs(1))?
        .unwrap_err();
    assert_eq!(failure.kind, CaptureActorFailureKind::StaleReference);
    assert_eq!(join.join(), CaptureActorExit::Stopped);
    Ok(())
}

#[test]
fn full_ordinary_queue_cannot_block_coalesced_shutdown() {
    let gate = Arc::new(CaptureGate::new());
    let (mut backend, _calls, shutdown) = FakeBackend::new(FakeMode::Healthy);
    backend.gate = Some(Arc::clone(&gate));
    let (handle, join) = spawn_fake(backend);
    let first = handle
        .try_capture_root(
            root_request(ScreenshotFormat::RawBgra),
            deadline(),
            CancellationToken::new(),
        )
        .unwrap();
    gate.wait_until_entered();
    let mut queued = Vec::new();
    for _ in 0..CAPTURE_REQUEST_CAPACITY {
        queued.push(
            handle
                .try_capture_root(
                    root_request(ScreenshotFormat::RawBgra),
                    deadline(),
                    CancellationToken::new(),
                )
                .unwrap(),
        );
    }
    assert_eq!(
        handle
            .try_capture_root(
                root_request(ScreenshotFormat::RawBgra),
                deadline(),
                CancellationToken::new(),
            )
            .unwrap_err(),
        CaptureSubmitError::QueueFull
    );
    let stop_one = handle.shutdown();
    let stop_two = handle.shutdown();
    gate.release();
    let _first_result = first.recv_timeout(Duration::from_secs(1));
    for reply in queued {
        assert_eq!(
            reply
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err()
                .kind,
            CaptureActorFailureKind::ActorStopped
        );
    }
    assert!(
        stop_one
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok()
    );
    assert!(
        stop_two
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_ok()
    );
    assert_eq!(join.join(), CaptureActorExit::Stopped);
    assert!(shutdown.load(Ordering::Acquire));
}

#[test]
fn backend_unavailability_poisons_and_backend_panic_is_contained() {
    let (backend, _calls, shutdown) = FakeBackend::new(FakeMode::Unavailable);
    let (handle, join) = spawn_fake(backend);
    let failure = handle
        .try_capture_root(
            root_request(ScreenshotFormat::RawBgra),
            deadline(),
            CancellationToken::new(),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap_err();
    assert_eq!(failure.kind, CaptureActorFailureKind::BackendUnavailable);
    assert_eq!(join.join(), CaptureActorExit::Poisoned);
    assert!(shutdown.load(Ordering::Acquire));

    let (backend, _calls, shutdown) = FakeBackend::new(FakeMode::Panic);
    let (handle, join) = spawn_fake(backend);
    let reply = handle
        .try_capture_root(
            root_request(ScreenshotFormat::RawBgra),
            deadline(),
            CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(
        reply
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap_err()
            .kind,
        CaptureActorFailureKind::ActorPanicked
    );
    assert_eq!(join.join(), CaptureActorExit::Panicked);
    assert!(shutdown.load(Ordering::Acquire));
}

#[test]
fn malformed_backend_frame_is_rejected_before_copy_or_encode() {
    let (backend, _calls, shutdown) = FakeBackend::new(FakeMode::Malformed);
    let (handle, join) = spawn_fake(backend);
    let failure = handle
        .try_capture_root(
            root_request(ScreenshotFormat::RawBgra),
            deadline(),
            CancellationToken::new(),
        )
        .unwrap()
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap_err();
    assert_eq!(failure.kind, CaptureActorFailureKind::EncodeFailed);
    assert_eq!(join.join(), CaptureActorExit::Stopped);
    assert!(shutdown.load(Ordering::Acquire));
}

#[test]
fn submission_methods_shape_deadline_and_output_limits_fail_closed()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (backend, calls, _shutdown) = FakeBackend::new(FakeMode::Healthy);
    let (handle, join) = spawn_fake(backend);
    assert_eq!(
        handle
            .try_capture_root(window_request(), deadline(), CancellationToken::new())
            .unwrap_err(),
        CaptureSubmitError::InvalidTarget
    );
    assert_eq!(
        handle
            .try_capture_window(
                root_request(ScreenshotFormat::RawBgra),
                deadline(),
                CancellationToken::new(),
                |_| Ok(()),
            )
            .unwrap_err(),
        CaptureSubmitError::InvalidTarget
    );
    assert_eq!(
        handle
            .try_capture_root(
                root_request(ScreenshotFormat::RawBgra),
                Instant::now(),
                CancellationToken::new(),
            )
            .unwrap_err(),
        CaptureSubmitError::InvalidDeadline
    );
    let mut limited = root_request(ScreenshotFormat::RawBgra);
    limited.max_bytes = Some(4);
    let failure = handle
        .try_capture_root(limited, deadline(), CancellationToken::new())?
        .recv_timeout(Duration::from_secs(1))?
        .unwrap_err();
    assert_eq!(failure.kind, CaptureActorFailureKind::OutputTooLarge);
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(join.join(), CaptureActorExit::Stopped);
    Ok(())
}
