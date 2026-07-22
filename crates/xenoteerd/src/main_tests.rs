use super::*;

use std::path::{Path, PathBuf};

use uuid::Uuid;

const REQUIRED_NO_VNC_ASSETS: &[&str] = &[
    "core/base64.js",
    "core/crypto/aes.js",
    "core/crypto/bigint.js",
    "core/crypto/crypto.js",
    "core/crypto/des.js",
    "core/crypto/dh.js",
    "core/crypto/md5.js",
    "core/crypto/rsa.js",
    "core/decoders/copyrect.js",
    "core/decoders/h264.js",
    "core/decoders/hextile.js",
    "core/decoders/jpeg.js",
    "core/decoders/raw.js",
    "core/decoders/rre.js",
    "core/decoders/tight.js",
    "core/decoders/tightpng.js",
    "core/decoders/zlib.js",
    "core/decoders/zrle.js",
    "core/deflator.js",
    "core/display.js",
    "core/encodings.js",
    "core/inflator.js",
    "core/input/domkeytable.js",
    "core/input/fixedkeys.js",
    "core/input/gesturehandler.js",
    "core/input/keyboard.js",
    "core/input/keysym.js",
    "core/input/keysymdef.js",
    "core/input/util.js",
    "core/input/vkeys.js",
    "core/input/xtscancodes.js",
    "core/ra2.js",
    "core/rfb.js",
    "core/util/browser.js",
    "core/util/cursor.js",
    "core/util/element.js",
    "core/util/events.js",
    "core/util/eventtarget.js",
    "core/util/int.js",
    "core/util/logging.js",
    "core/util/strings.js",
    "core/websock.js",
    "vendor/pako/lib/utils/common.js",
    "vendor/pako/lib/zlib/adler32.js",
    "vendor/pako/lib/zlib/crc32.js",
    "vendor/pako/lib/zlib/deflate.js",
    "vendor/pako/lib/zlib/inffast.js",
    "vendor/pako/lib/zlib/inflate.js",
    "vendor/pako/lib/zlib/inftrees.js",
    "vendor/pako/lib/zlib/messages.js",
    "vendor/pako/lib/zlib/trees.js",
    "vendor/pako/lib/zlib/zstream.js",
];

struct TestViewerAssets(PathBuf);

impl TestViewerAssets {
    fn new() -> Result<Self, std::io::Error> {
        let root = std::env::temp_dir().join(format!("xenoteer-viewer-test-{}", Uuid::new_v4()));
        for relative in REQUIRED_NO_VNC_ASSETS {
            let path = root.join(relative);
            std::fs::create_dir_all(
                path.parent()
                    .ok_or_else(|| std::io::Error::other("viewer fixture asset has no parent"))?,
            )?;
            std::fs::write(path, b"export default true;\n")?;
        }
        Ok(Self(root))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestViewerAssets {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn viewer_composition_is_disabled_without_touching_assets() -> Result<(), Box<dyn std::error::Error>>
{
    let config = Config::load(
        Some("[viewer]\nno_vnc_root = '/definitely/absent/xenoteer-novnc'"),
        std::iter::empty::<(&str, &str)>(),
        &ConfigOverrides::default(),
    )?;
    assert!(matches!(
        configured_viewer(config.viewer())?,
        ConfiguredViewer::Disabled
    ));
    Ok(())
}

#[test]
fn enabled_viewer_composes_exact_origins_shared_tickets_and_gateway()
-> Result<(), Box<dyn std::error::Error>> {
    let assets = TestViewerAssets::new()?;
    let document = format!(
        "[viewer]\nenabled = true\nallowed_origins = ['https://viewer.example']\nno_vnc_root = '{}'",
        assets.path().display()
    );
    let config = Config::load(
        Some(&document),
        std::iter::empty::<(&str, &str)>(),
        &ConfigOverrides::default(),
    )?;
    let ConfiguredViewer::Enabled {
        origins,
        tickets,
        gateway,
    } = configured_viewer(config.viewer())?
    else {
        return Err(std::io::Error::other("enabled viewer was not composed").into());
    };
    assert_eq!(
        origins,
        AllowedOrigins::exact(["https://viewer.example".to_owned()])?
    );
    assert_eq!(Arc::strong_count(&tickets), 1);
    assert!(format!("{gateway:?}").contains("asset_count"));
    Ok(())
}

#[test]
fn enabled_viewer_fails_startup_when_pinned_assets_are_missing()
-> Result<(), Box<dyn std::error::Error>> {
    let missing = std::env::temp_dir().join(format!("xenoteer-missing-novnc-{}", Uuid::new_v4()));
    let document = format!(
        "[viewer]\nenabled = true\nallowed_origins = ['https://viewer.example']\nno_vnc_root = '{}'",
        missing.display()
    );
    let config = Config::load(
        Some(&document),
        std::iter::empty::<(&str, &str)>(),
        &ConfigOverrides::default(),
    )?;
    assert!(matches!(
        configured_viewer(config.viewer()),
        Err(DaemonError::ViewerGateway(
            ViewerGatewayConfigurationError::Assets
        ))
    ));
    Ok(())
}

#[test]
fn launcher_config_key_is_not_forwarded_to_typed_configuration() -> Result<(), ConfigLoadError> {
    let config = Config::load(
        None,
        configuration_environment([
            (
                CONFIG_PATH_ENV.to_owned(),
                "LAUNCHER_CONFIG_VALUE_CANARY".to_owned(),
            ),
            ("XENOTEER__LOGGING__FILTER".to_owned(), "warn".to_owned()),
        ]),
        &ConfigOverrides::default(),
    )?;
    assert_eq!(config.logging().filter(), "warn");
    Ok(())
}

#[test]
fn configured_principal_honors_least_privilege_grants() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(
        Some("[auth]\ngrants = ['desktop:status']"),
        std::iter::empty::<(&str, &str)>(),
        &ConfigOverrides::default(),
    )?;
    let principal = configured_principal(&config)?;
    assert!(principal.has_grant(Grant::DesktopStatus));
    assert!(!principal.has_grant(Grant::DesktopObserve));
    assert!(!principal.has_grant(Grant::InputControl));
    Ok(())
}

#[test]
fn phase_four_config_maps_to_checked_runtime_types() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(
        Some(
            "[artifacts]\nroot_directory = '/tmp/xenoteer-artifacts-test'\nupload_total_timeout_ms = 200\nupload_idle_timeout_ms = 50\n[observation]\nrequest_capacity = 7\nmax_waiters = 6",
        ),
        std::iter::empty::<(&str, &str)>(),
        &ConfigOverrides::default(),
    )?;
    let artifact_limits = configured_artifact_limits(&config)?;
    assert_eq!(
        artifact_limits.max_object_bytes(),
        config.artifacts().max_object_bytes()
    );
    assert_eq!(
        configured_window_model_limits(&config).max_live_windows,
        config.observation().max_live_windows()
    );
    assert!(configured_observation_settings(&config).is_ok());
    assert_eq!(
        ArtifactUploadTimeoutPolicy::new(
            Duration::from_millis(config.artifacts().upload_total_timeout_ms()),
            Duration::from_millis(config.artifacts().upload_idle_timeout_ms()),
        )?
        .idle(),
        Duration::from_millis(50)
    );
    Ok(())
}

#[test]
fn unknown_xenoteer_environment_is_redacted_through_daemon_error()
-> Result<(), Box<dyn std::error::Error>> {
    const KEY_CANARY: &str = "XENOTEER_BAD_KEY_SECRET_CANARY";
    const VALUE_CANARY: &str = "UNKNOWN_ENV_VALUE_SECRET_CANARY";
    let result = Config::load(
        None,
        configuration_environment([(KEY_CANARY.to_owned(), VALUE_CANARY.to_owned())]),
        &ConfigOverrides::default(),
    );
    let error = match result {
        Err(error) => DaemonError::Config(error),
        Ok(_) => {
            return Err(std::io::Error::other(
                "unknown Xenoteer environment key unexpectedly loaded",
            )
            .into());
        }
    };
    assert_error_chain_redacted(&error, KEY_CANARY);
    assert_error_chain_redacted(&error, VALUE_CANARY);
    Ok(())
}

#[tokio::test]
async fn detached_join_monitor_reports_immediate_completion()
-> Result<(), Box<dyn std::error::Error>> {
    let monitor = spawn_detached_join_monitor("xenoteer-test-immediate-join", || 17_u8)?;
    let first = first_join_attempt(Ok(monitor), Duration::from_secs(1)).await;
    let result = finish_join_attempt(first, Duration::from_secs(1)).await;

    assert!(matches!(result, Ok(17)));
    Ok(())
}

#[tokio::test]
async fn detached_join_monitor_preserves_second_grace_window()
-> Result<(), Box<dyn std::error::Error>> {
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let monitor = spawn_detached_join_monitor("xenoteer-test-two-phase-join", move || {
        let _released = release_rx.recv_timeout(Duration::from_secs(2));
        23_u8
    })?;

    let first = first_join_attempt(Ok(monitor), Duration::from_millis(10)).await;
    assert!(matches!(first, FirstJoinAttempt::TimedOut(_)));
    release_tx.send(())?;
    let result = finish_join_attempt(first, Duration::from_secs(1)).await;

    assert!(matches!(result, Ok(23)));
    Ok(())
}

#[tokio::test]
async fn detached_join_monitor_propagates_initial_monitor_error() {
    let first = first_join_attempt::<u8>(
        Err(DetachedJoinMonitorError::Closed),
        Duration::from_secs(1),
    )
    .await;
    let result = finish_join_attempt(first, Duration::from_secs(1)).await;

    assert!(matches!(
        result,
        Err(ActorJoinWaitError::Monitor(
            DetachedJoinMonitorError::Closed
        ))
    ));
}

#[tokio::test]
async fn detached_join_monitor_propagates_panic_during_second_grace_window()
-> Result<(), Box<dyn std::error::Error>> {
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let monitor =
        spawn_detached_join_monitor("xenoteer-test-second-window-panic", move || -> u8 {
            let _released = release_rx.recv_timeout(Duration::from_secs(2));
            std::panic::resume_unwind(Box::new("second grace window panic test canary"))
        })?;

    let first = first_join_attempt(Ok(monitor), Duration::from_millis(10)).await;
    assert!(matches!(first, FirstJoinAttempt::TimedOut(_)));
    release_tx.send(())?;
    let result = finish_join_attempt(first, Duration::from_secs(1)).await;

    assert!(matches!(
        result,
        Err(ActorJoinWaitError::Monitor(
            DetachedJoinMonitorError::Closed
        ))
    ));
    Ok(())
}

#[test]
fn detached_join_monitor_hard_timeout_does_not_pin_tokio_teardown()
-> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let monitor = runtime.block_on(async {
        spawn_detached_join_monitor("xenoteer-test-hard-timeout", move || {
            let _started = started_tx.send(());
            let _released = release_rx.recv_timeout(Duration::from_secs(5));
            29_u8
        })
    })?;
    started_rx.recv_timeout(Duration::from_secs(1))?;
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        let _released = release_tx.send(());
    });

    let started = std::time::Instant::now();
    let result = runtime.block_on(async {
        let first = first_join_attempt(Ok(monitor), Duration::from_millis(10)).await;
        finish_join_attempt(first, Duration::from_millis(10)).await
    });
    drop(runtime);
    let teardown_elapsed = started.elapsed();

    assert!(matches!(result, Err(ActorJoinWaitError::TimedOut)));
    assert!(teardown_elapsed < Duration::from_secs(1));
    release
        .join()
        .map_err(|_| std::io::Error::other("test release thread panicked"))?;
    Ok(())
}

#[tokio::test]
async fn detached_join_monitor_reports_panicked_worker_as_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let mut monitor = spawn_detached_join_monitor("xenoteer-test-panicked-join", || -> u8 {
        std::panic::resume_unwind(Box::new("join monitor panic test canary"))
    })?;

    let result = tokio::time::timeout(Duration::from_secs(1), monitor.wait()).await?;

    assert!(matches!(result, Err(DetachedJoinMonitorError::Closed)));
    Ok(())
}

#[test]
fn detached_join_owner_drop_returns_promptly_while_cleanup_is_blocked()
-> Result<(), Box<dyn std::error::Error>> {
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let owner = DetachedJoinOwner::new("xenoteer-test-startup-cleanup", move || {
        let _started = started_tx.send(());
        let _released = release_rx.recv_timeout(Duration::from_secs(2));
        31_u8
    });

    let started = std::time::Instant::now();
    drop(owner);
    let drop_elapsed = started.elapsed();

    assert!(drop_elapsed < Duration::from_millis(500));
    started_rx.recv_timeout(Duration::from_secs(1))?;
    release_tx.send(())?;
    Ok(())
}

fn assert_error_chain_redacted(error: &DaemonError, canary: &str) {
    assert!(!format!("{error}").contains(canary));
    assert!(!format!("{error:?}").contains(canary));
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        assert!(!format!("{current}").contains(canary));
        assert!(!format!("{current:?}").contains(canary));
        source = current.source();
    }
}
