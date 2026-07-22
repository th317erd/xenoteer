//! Privilege-separated registered-application broker.
//!
//! The network daemon is an unprivileged IPC client. This crate's broker is
//! separately supervised as root, authenticates the daemon with Unix peer
//! credentials, and drops every application child to the desktop identity.

#![forbid(unsafe_code)]

mod ipc;
#[allow(dead_code)] // Registry policy variants are retained for future compiled image profiles.
mod process_manager;

pub use ipc::{
    BrokerClient, BrokerClientError, BrokerConfig, BrokerErrorCode, BrokerEventReplay,
    BrokerEventStream, BrokerEventSubscription, BrokerLiveEvent, BrokerPidCorrelation,
    BrokerPidCorrelationEvidence, BrokerProcessEvent, BrokerServer, BrokerServerError,
    DEFAULT_BROKER_SOCKET,
};

use std::{collections::BTreeMap, env, path::PathBuf};

use process_manager::{
    ApplicationProfile, ApplicationProfileSpec, ArgumentRule, ArgumentSchema, ProcessManagerError,
    StdinPolicy,
};

/// Fixed desktop user ID in the release-one image.
pub const DESKTOP_UID: u32 = 1_000;
/// Fixed desktop group ID in the release-one image.
pub const DESKTOP_GID: u32 = 1_000;
/// Dedicated daemon user ID accepted by the broker.
pub const DAEMON_UID: u32 = 1_001;
/// Dedicated daemon primary group ID owning the private broker socket.
pub const DAEMON_GID: u32 = 1_001;
/// Maximum PIDs accepted by one non-authoritative process-correlation query.
pub const MAX_PROCESS_CORRELATION_PIDS: usize = 32;

/// Builds the immutable application registry compiled into the image.
fn image_profiles() -> Result<Vec<ApplicationProfile>, ProcessManagerError> {
    let mut base_environment = BTreeMap::new();
    for key in [
        "AT_SPI_BUS_ADDRESS",
        "DBUS_SESSION_BUS_ADDRESS",
        "DISPLAY",
        "GDK_DPI_SCALE",
        "GDK_SCALE",
        "GTK_OVERLAY_SCROLLING",
        "HOME",
        "LANG",
        "LC_ALL",
        "NO_AT_BRIDGE",
        "QT_AUTO_SCREEN_SCALE_FACTOR",
        "QT_FONT_DPI",
        "QT_LINUX_ACCESSIBILITY_ALWAYS_ON",
        "QT_SCALE_FACTOR",
        "QT_STYLE_OVERRIDE",
        "TZ",
        "XAUTHORITY",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CURRENT_DESKTOP",
        "XDG_DATA_DIRS",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "XDG_SESSION_DESKTOP",
        "XDG_SESSION_TYPE",
    ] {
        let value =
            env::var(key).map_err(|_| ProcessManagerError::ProfileEnvironmentMissing(key))?;
        base_environment.insert(key.to_owned(), value);
    }

    let message_arguments = ArgumentSchema::default().with_repeated(
        ArgumentRule::Text {
            maximum_bytes: 1_024,
            allow_empty: false,
            allow_leading_hyphen: false,
        },
        4,
    )?;
    let message = ApplicationProfile::register(ApplicationProfileSpec {
        application_id: "xmessage".to_owned(),
        executable: PathBuf::from("/usr/bin/xmessage"),
        fixed_arguments: vec!["-center".to_owned()],
        argument_schema: message_arguments,
        base_environment,
        allowed_environment: BTreeMap::new(),
        working_directory_roots: vec![PathBuf::from("/workspace")],
        default_working_directory: PathBuf::from("/workspace"),
        stdin: StdinPolicy::Null,
    })?;
    Ok(vec![message])
}

/// Returns the production broker boundary and resource policy.
fn image_broker_config() -> BrokerConfig {
    BrokerConfig::new(
        PathBuf::from(DEFAULT_BROKER_SOCKET),
        DAEMON_UID,
        DAEMON_GID,
        DESKTOP_UID,
        DESKTOP_GID,
    )
}

/// Binds the production socket with the compiled image-owned registry.
pub async fn bind_image_broker() -> Result<BrokerServer, BrokerServerError> {
    BrokerServer::bind(image_broker_config(), image_profiles()?).await
}
