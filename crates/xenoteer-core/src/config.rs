//! Typed, precedence-aware, fail-closed configuration.

use std::{
    collections::BTreeSet,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;
use toml::{Table, Value};
use xenoteer_protocol::{
    ACCESSIBILITY_CURSOR_TTL_MS, MAX_ACCESSIBILITY_PAGE_LIMIT, MAX_ACCESSIBILITY_QUERY_TIMEOUT_MS,
    MAX_ACCESSIBILITY_QUERY_VISITED_NODES, MAX_VIEWER_TICKET_TTL_SECONDS, MAX_WINDOW_PAGE_LIMIT,
    MAX_XTEST_DELAY_MS, ViewerOrigin,
};

/// Environment-variable prefix. Nested fields use a double underscore.
pub const ENV_PREFIX: &str = "XENOTEER__";
/// Hard ceiling for an ordinary HTTP body or reassembled WebSocket message.
pub const MAX_REQUEST_BODY_LIMIT_BYTES: u64 = 1_048_576;
/// Hard ceiling for the bounded input actor queue.
pub const MAX_INPUT_QUEUE_CAPACITY: usize = 256;
/// Hard ceiling for accepted/running commands tracked for one principal.
pub const MAX_ACCEPTED_COMMANDS_PER_PRINCIPAL: usize = 32;
/// Hard ceiling for accepted/running commands tracked by one daemon.
pub const MAX_ACCEPTED_COMMANDS_PER_DAEMON: usize = 128;
/// Hard ceiling for in-memory result-ledger entries.
pub const MAX_RESULT_LEDGER_ENTRIES: usize = 10_000;
/// Hard ceiling for the default action timer, covering a five-minute key sequence.
pub const MAX_DEFAULT_ACTION_TIMEOUT_MS: u64 = 305_000;
/// Hard ceiling for retaining recent in-memory command results.
pub const MAX_RESULT_LEDGER_TTL_SECONDS: u64 = 900;
/// Hard ceiling for artifact upload wall-clock duration.
pub const MAX_ARTIFACT_UPLOAD_TOTAL_TIMEOUT_MS: u64 = 300_000;
/// Hard ceiling for artifact upload silence between body chunks.
pub const MAX_ARTIFACT_UPLOAD_IDLE_TIMEOUT_MS: u64 = 60_000;
/// Hard ceiling for daemon observation request and waiter lanes.
pub const MAX_OBSERVATION_QUEUE_CAPACITY: usize = 4_096;
/// Hard ceiling for opaque cursor/reference claims retained in memory.
pub const MAX_OBSERVATION_TOKEN_CAPACITY: usize = 1_048_576;
/// Hard ceiling for accessibility actor request and waiter lanes.
pub const MAX_ACCESSIBILITY_QUEUE_CAPACITY: usize = 4_096;
/// Hard ceiling for normalized accessibility events waiting for the model owner.
pub const MAX_ACCESSIBILITY_EVENT_CAPACITY: usize = 65_536;
/// Hard ceiling for normalized accessible objects retained per desktop.
pub const MAX_ACCESSIBILITY_CACHED_NODES: usize = 100_000;
/// Hard ceiling for destroyed accessible identities retained against retargeting.
pub const MAX_ACCESSIBILITY_TOMBSTONES: usize = 1_000_000;
/// Hard ceiling for opaque semantic cursor claims retained in memory.
pub const MAX_ACCESSIBILITY_TOKEN_CAPACITY: usize = 1_048_576;
/// Hard ceiling for accessible objects visited by one selector evaluation.
pub const MAX_ACCESSIBILITY_QUERY_NODES: usize = MAX_ACCESSIBILITY_QUERY_VISITED_NODES as usize;
/// Hard ceiling for one public semantic snapshot's encoded size.
pub const MAX_ACCESSIBILITY_SNAPSHOT_BYTES: usize = 16 * 1_024 * 1_024;
/// Hard ceiling for simultaneous unexpired one-time viewer tickets.
pub const MAX_VIEWER_TICKET_CAPACITY: usize = 16_384;
/// Hard ceiling for viewer backend connection establishment.
pub const MAX_VIEWER_CONNECT_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling for one viewer backend or browser write.
pub const MAX_VIEWER_WRITE_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling for viewer session inactivity.
pub const MAX_VIEWER_IDLE_TIMEOUT_MS: u64 = 60 * 60 * 1_000;
/// Hard ceiling for one viewer session lifetime.
pub const MAX_VIEWER_SESSION_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;
/// Hard ceiling for one viewer WebSocket frame or reassembled message.
pub const MAX_VIEWER_FRAME_BYTES: usize = 8 * 1_024 * 1_024;
/// Hard ceiling for simultaneous viewer sessions.
pub const MAX_VIEWER_SESSIONS: usize = 64;
/// Closed release-five authorization grant vocabulary.
pub const AUTHORIZATION_GRANTS: [&str; 14] = [
    "desktop:status",
    "desktop:observe",
    "input:control",
    "application:launch",
    "application:terminate",
    "window:control",
    "clipboard:read",
    "clipboard:write",
    "capture:read",
    "artifact:read",
    "artifact:delete",
    "viewer:read",
    "accessibility:read",
    "accessibility:write",
];

/// Complete immutable daemon configuration.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    server: ServerConfig,
    auth: AuthConfig,
    desktop: DesktopConfig,
    input: InputConfig,
    limits: LimitsConfig,
    artifacts: ArtifactConfig,
    observation: ObservationConfig,
    accessibility: AccessibilityConfig,
    viewer: ViewerConfig,
    logging: LoggingConfig,
}

impl Config {
    /// Loads configuration with `defaults < TOML < environment < CLI` precedence.
    ///
    /// Environment keys have the form `XENOTEER__SERVER__LISTEN`. Values are
    /// parsed as TOML scalars/arrays when possible and otherwise treated as
    /// strings. Unknown fields fail during the single final typed decode.
    pub fn load<I, K, V>(
        file_toml: Option<&str>,
        environment: I,
        overrides: &ConfigOverrides,
    ) -> Result<Self, ConfigLoadError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut merged = match file_toml {
            Some(contents) => toml::from_str::<Value>(contents)
                .map_err(|error| ConfigLoadError::toml(error, contents))?,
            None => Value::Table(Table::new()),
        };
        if !merged.is_table() {
            return Err(ConfigLoadError::RootNotTable);
        }

        for (key, raw_value) in environment {
            let key = key.as_ref();
            let Some(path) = key.strip_prefix(ENV_PREFIX) else {
                if key.starts_with("XENOTEER_") {
                    return Err(ConfigLoadError::InvalidEnvironmentPath);
                }
                continue;
            };
            let segments = normalized_env_path(path)?;
            insert_value(&mut merged, &segments, parse_env_value(raw_value.as_ref()))?;
        }
        overrides.apply(&mut merged)?;

        let config: Self = merged.try_into().map_err(ConfigLoadError::decode)?;
        config.validate().map_err(ConfigLoadError::Validation)?;
        Ok(config)
    }

    /// Validates cross-field constraints and returns every discovered issue.
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut issues = Vec::new();

        if self.server.insecure_disable_auth && !self.server.listen.ip().is_loopback() {
            issues.push(ValidationIssue::new(
                "server.insecure_disable_auth",
                "authentication may be disabled only on a loopback listener",
            ));
        }
        if self.auth.grants.len() > AUTHORIZATION_GRANTS.len()
            || self
                .auth
                .grants
                .iter()
                .any(|grant| !AUTHORIZATION_GRANTS.contains(&grant.as_str()))
            || {
                let mut unique = self.auth.grants.clone();
                unique.sort();
                unique.dedup();
                unique.len() != self.auth.grants.len()
            }
        {
            issues.push(ValidationIssue::new(
                "auth.grants",
                "authorization grants must be unique members of the closed release-five vocabulary",
            ));
        }
        if self.server.request_body_limit_bytes == 0
            || self.server.request_body_limit_bytes > MAX_REQUEST_BODY_LIMIT_BYTES
        {
            issues.push(ValidationIssue::new(
                "server.request_body_limit_bytes",
                "request body limit must be between 1 and 1048576 bytes",
            ));
        }
        if self
            .server
            .metrics_listen
            .is_some_and(|address| address == self.server.listen)
        {
            issues.push(ValidationIssue::new(
                "server.metrics_listen",
                "metrics and API listeners must not use the same address",
            ));
        }
        if self.desktop.display_width != 1_920 {
            issues.push(ValidationIssue::new(
                "desktop.display_width",
                "release-one display width must be exactly 1920 physical pixels",
            ));
        }
        if self.desktop.display_height != 1_080 {
            issues.push(ValidationIssue::new(
                "desktop.display_height",
                "release-one display height must be exactly 1080 physical pixels",
            ));
        }
        if self.desktop.depth != 24 {
            issues.push(ValidationIssue::new(
                "desktop.depth",
                "release-one image profile requires 24-bit X depth",
            ));
        }
        if self.desktop.dpi != 96 {
            issues.push(ValidationIssue::new(
                "desktop.dpi",
                "release-one display DPI must be exactly 96",
            ));
        }
        if self.input.queue_capacity == 0 || self.input.queue_capacity > MAX_INPUT_QUEUE_CAPACITY {
            issues.push(ValidationIssue::new(
                "input.queue_capacity",
                "input queue capacity must be between 1 and 256",
            ));
        }
        if self.input.pointer_sample_rate_hz == 0 || self.input.pointer_sample_rate_hz > 240 {
            issues.push(ValidationIssue::new(
                "input.pointer_sample_rate_hz",
                "pointer sample rate must be between 1 and 240 Hz",
            ));
        }
        if self.input.pointer_nominal_speed_px_s == 0 {
            issues.push(ValidationIssue::new(
                "input.pointer_nominal_speed_px_s",
                "nominal pointer speed must be greater than zero",
            ));
        }
        if self.input.pointer_move_max_ms > MAX_XTEST_DELAY_MS {
            issues.push(ValidationIssue::new(
                "input.pointer_move_max_ms",
                "maximum pointer movement duration must not exceed 10000 ms",
            ));
        }
        if self.input.pointer_move_min_ms > self.input.pointer_move_max_ms {
            issues.push(ValidationIssue::new(
                "input.pointer_move_min_ms",
                "minimum pointer duration must not exceed maximum",
            ));
        }
        for (path, value, description) in [
            (
                "input.pre_click_dwell_ms",
                self.input.pre_click_dwell_ms,
                "pre-click dwell",
            ),
            (
                "input.pointer_press_ms",
                self.input.pointer_press_ms,
                "pointer press",
            ),
            (
                "input.keyboard_hold_ms",
                self.input.keyboard_hold_ms,
                "keyboard hold",
            ),
        ] {
            if value > MAX_XTEST_DELAY_MS {
                issues.push(ValidationIssue::new(
                    path,
                    match description {
                        "pre-click dwell" => "pre-click dwell must not exceed 10000 ms",
                        "pointer press" => "pointer press must not exceed 10000 ms",
                        "keyboard hold" => "keyboard hold must not exceed 10000 ms",
                        _ => "XTEST delay must not exceed 10000 ms",
                    },
                ));
            }
        }

        let pointer_compound_ms = self
            .input
            .pointer_move_max_ms
            .checked_add(self.input.pre_click_dwell_ms)
            .and_then(|duration| duration.checked_add(self.input.pointer_press_ms));
        if pointer_compound_ms.is_none() {
            issues.push(ValidationIssue::new(
                "input.pointer_compound_duration",
                "configured pointer movement, dwell, and press duration overflow",
            ));
        }
        if let Some(pointer_compound_ms) = pointer_compound_ms {
            // A keyboard key/chord primitive has one configured hold interval;
            // sequences have their separate command-level 305-second timeout.
            let longest_primitive_ms =
                u64::from(pointer_compound_ms.max(self.input.keyboard_hold_ms));
            if self.limits.default_action_timeout_ms < longest_primitive_ms {
                issues.push(ValidationIssue::new(
                    "limits.default_action_timeout_ms",
                    "default action timeout must cover the longest pointer-click or keyboard-hold primitive",
                ));
            }
        }
        if self.limits.default_action_timeout_ms > MAX_DEFAULT_ACTION_TIMEOUT_MS {
            issues.push(ValidationIssue::new(
                "limits.default_action_timeout_ms",
                "default action timeout must not exceed 305000 ms",
            ));
        }
        if self.limits.accepted_commands_per_daemon == 0
            || self.limits.accepted_commands_per_daemon > MAX_ACCEPTED_COMMANDS_PER_DAEMON
        {
            issues.push(ValidationIssue::new(
                "limits.accepted_commands_per_daemon",
                "daemon command limit must be between 1 and 128",
            ));
        }
        if self.limits.accepted_commands_per_principal == 0
            || self.limits.accepted_commands_per_principal > MAX_ACCEPTED_COMMANDS_PER_PRINCIPAL
            || self.limits.accepted_commands_per_principal
                > self.limits.accepted_commands_per_daemon
        {
            issues.push(ValidationIssue::new(
                "limits.accepted_commands_per_principal",
                "per-principal command limit must be between 1 and 32 and no larger than daemon limit",
            ));
        }
        if self.limits.result_ledger_entries == 0
            || self.limits.result_ledger_entries > MAX_RESULT_LEDGER_ENTRIES
        {
            issues.push(ValidationIssue::new(
                "limits.result_ledger_entries",
                "result ledger entry limit must be between 1 and 10000",
            ));
        }
        if self.limits.result_ledger_ttl_seconds == 0
            || self.limits.result_ledger_ttl_seconds > MAX_RESULT_LEDGER_TTL_SECONDS
        {
            issues.push(ValidationIssue::new(
                "limits.result_ledger_ttl_seconds",
                "result ledger TTL must be between 1 and 900 seconds",
            ));
        }
        if self.artifacts.root_directory.as_os_str().is_empty()
            || !self.artifacts.root_directory.is_absolute()
            || self.artifacts.root_directory == Path::new("/")
        {
            issues.push(ValidationIssue::new(
                "artifacts.root_directory",
                "artifact root must be a non-root absolute path",
            ));
        }
        if self.artifacts.max_object_bytes == 0
            || self.artifacts.max_object_bytes > self.artifacts.max_total_bytes
            || self.artifacts.max_object_bytes > self.artifacts.max_owner_bytes
        {
            issues.push(ValidationIssue::new(
                "artifacts.max_object_bytes",
                "artifact object bytes must be non-zero and fit total and owner quotas",
            ));
        }
        if self.artifacts.max_total_bytes == 0
            || self.artifacts.max_owner_bytes == 0
            || self.artifacts.max_owner_bytes > self.artifacts.max_total_bytes
        {
            issues.push(ValidationIssue::new(
                "artifacts.max_total_bytes",
                "artifact total and owner byte quotas must be non-zero and ordered",
            ));
        }
        if self.artifacts.max_objects == 0
            || self.artifacts.max_owner_objects == 0
            || self.artifacts.max_owner_objects > self.artifacts.max_objects
        {
            issues.push(ValidationIssue::new(
                "artifacts.max_objects",
                "artifact total and owner object quotas must be non-zero and ordered",
            ));
        }
        if self.artifacts.max_retention_ms == 0
            || self.artifacts.clipboard_input_retention_ms == 0
            || self.artifacts.clipboard_input_retention_ms > self.artifacts.max_retention_ms
        {
            issues.push(ValidationIssue::new(
                "artifacts.clipboard_input_retention_ms",
                "clipboard retention must be non-zero and fit store retention",
            ));
        }
        if self.artifacts.upload_total_timeout_ms == 0
            || self.artifacts.upload_total_timeout_ms > MAX_ARTIFACT_UPLOAD_TOTAL_TIMEOUT_MS
        {
            issues.push(ValidationIssue::new(
                "artifacts.upload_total_timeout_ms",
                "artifact upload total timeout must be between 1 and 300000 ms",
            ));
        }
        if self.artifacts.upload_idle_timeout_ms == 0
            || self.artifacts.upload_idle_timeout_ms > MAX_ARTIFACT_UPLOAD_IDLE_TIMEOUT_MS
            || self.artifacts.upload_idle_timeout_ms > self.artifacts.upload_total_timeout_ms
        {
            issues.push(ValidationIssue::new(
                "artifacts.upload_idle_timeout_ms",
                "artifact upload idle timeout must be non-zero, at most 60000 ms, and no greater than total timeout",
            ));
        }
        if self.observation.request_capacity == 0
            || self.observation.request_capacity > MAX_OBSERVATION_QUEUE_CAPACITY
        {
            issues.push(ValidationIssue::new(
                "observation.request_capacity",
                "observation request capacity must be between 1 and 4096",
            ));
        }
        if self.observation.max_waiters == 0
            || self.observation.max_waiters > MAX_OBSERVATION_QUEUE_CAPACITY
        {
            issues.push(ValidationIssue::new(
                "observation.max_waiters",
                "observation waiter capacity must be between 1 and 4096",
            ));
        }
        if self.observation.token_capacity < usize::from(MAX_WINDOW_PAGE_LIMIT)
            || self.observation.token_capacity > MAX_OBSERVATION_TOKEN_CAPACITY
        {
            issues.push(ValidationIssue::new(
                "observation.token_capacity",
                "observation token capacity must cover one maximum page and not exceed 1048576",
            ));
        }
        for (path, value, maximum) in [
            (
                "observation.cursor_ttl_ms",
                self.observation.cursor_ttl_ms,
                86_400_000,
            ),
            (
                "observation.reference_ttl_ms",
                self.observation.reference_ttl_ms,
                86_400_000,
            ),
            (
                "observation.raw_request_timeout_ms",
                self.observation.raw_request_timeout_ms,
                60_000,
            ),
            (
                "observation.startup_timeout_ms",
                self.observation.startup_timeout_ms,
                60_000,
            ),
            (
                "observation.idle_poll_interval_ms",
                self.observation.idle_poll_interval_ms,
                1_000,
            ),
            (
                "observation.tombstone_ttl_ms",
                self.observation.tombstone_ttl_ms,
                86_400_000,
            ),
        ] {
            if value == 0 || value > maximum {
                issues.push(ValidationIssue::new(
                    path,
                    "observation duration must be non-zero and within its hard ceiling",
                ));
            }
        }
        if self.observation.max_live_windows == 0 || self.observation.max_live_windows > 4_096 {
            issues.push(ValidationIssue::new(
                "observation.max_live_windows",
                "live window capacity must be between 1 and 4096",
            ));
        }
        if self.observation.max_tombstones == 0 || self.observation.max_tombstones > 1_048_576 {
            issues.push(ValidationIssue::new(
                "observation.max_tombstones",
                "window tombstone capacity must be between 1 and 1048576",
            ));
        }
        if self.accessibility.request_capacity == 0
            || self.accessibility.request_capacity > MAX_ACCESSIBILITY_QUEUE_CAPACITY
        {
            issues.push(ValidationIssue::new(
                "accessibility.request_capacity",
                "accessibility request capacity must be between 1 and 4096",
            ));
        }
        if self.accessibility.event_capacity == 0
            || self.accessibility.event_capacity > MAX_ACCESSIBILITY_EVENT_CAPACITY
        {
            issues.push(ValidationIssue::new(
                "accessibility.event_capacity",
                "accessibility event capacity must be between 1 and 65536",
            ));
        }
        if self.accessibility.max_waiters == 0
            || self.accessibility.max_waiters > MAX_ACCESSIBILITY_QUEUE_CAPACITY
        {
            issues.push(ValidationIssue::new(
                "accessibility.max_waiters",
                "accessibility waiter capacity must be between 1 and 4096",
            ));
        }
        if self.accessibility.token_capacity < usize::from(MAX_ACCESSIBILITY_PAGE_LIMIT)
            || self.accessibility.token_capacity > MAX_ACCESSIBILITY_TOKEN_CAPACITY
        {
            issues.push(ValidationIssue::new(
                "accessibility.token_capacity",
                "accessibility token capacity must cover one maximum page and not exceed 1048576",
            ));
        }
        if self.accessibility.max_cached_nodes == 0
            || self.accessibility.max_cached_nodes > MAX_ACCESSIBILITY_CACHED_NODES
        {
            issues.push(ValidationIssue::new(
                "accessibility.max_cached_nodes",
                "accessibility cache capacity must be between 1 and 100000 nodes",
            ));
        }
        if self.accessibility.max_tombstones == 0
            || self.accessibility.max_tombstones > MAX_ACCESSIBILITY_TOMBSTONES
        {
            issues.push(ValidationIssue::new(
                "accessibility.max_tombstones",
                "accessibility tombstone capacity must be between 1 and 1000000",
            ));
        }
        if self.accessibility.max_nodes_per_query == 0
            || self.accessibility.max_nodes_per_query > MAX_ACCESSIBILITY_QUERY_NODES
        {
            issues.push(ValidationIssue::new(
                "accessibility.max_nodes_per_query",
                "accessibility query budget must be between 1 and 25000 nodes",
            ));
        }
        if self.accessibility.max_selector_depth == 0 || self.accessibility.max_selector_depth > 64
        {
            issues.push(ValidationIssue::new(
                "accessibility.max_selector_depth",
                "accessibility selector depth must be between 1 and 64",
            ));
        }
        if self.accessibility.max_query_matches == 0 || self.accessibility.max_query_matches > 1_000
        {
            issues.push(ValidationIssue::new(
                "accessibility.max_query_matches",
                "accessibility query result capacity must be between 1 and 1000",
            ));
        }
        if self.accessibility.max_snapshot_nodes == 0
            || self.accessibility.max_snapshot_nodes > 10_000
            || self.accessibility.max_snapshot_nodes > self.accessibility.max_nodes_per_query
        {
            issues.push(ValidationIssue::new(
                "accessibility.max_snapshot_nodes",
                "accessibility snapshot nodes must be between 1 and 10000 and fit the query budget",
            ));
        }
        if self.accessibility.max_snapshot_bytes == 0
            || self.accessibility.max_snapshot_bytes > MAX_ACCESSIBILITY_SNAPSHOT_BYTES
        {
            issues.push(ValidationIssue::new(
                "accessibility.max_snapshot_bytes",
                "accessibility snapshot bytes must be between 1 and 16777216",
            ));
        }
        for (path, value) in [
            (
                "accessibility.proxy_timeout_ms",
                self.accessibility.proxy_timeout_ms,
            ),
            (
                "accessibility.reconnect_initial_backoff_ms",
                self.accessibility.reconnect_initial_backoff_ms,
            ),
            (
                "accessibility.reconnect_max_backoff_ms",
                self.accessibility.reconnect_max_backoff_ms,
            ),
        ] {
            if value == 0 || value > 60_000 {
                issues.push(ValidationIssue::new(
                    path,
                    "accessibility duration must be non-zero and not exceed 60000 ms",
                ));
            }
        }
        if self.accessibility.query_timeout_ms == 0
            || self.accessibility.query_timeout_ms > u64::from(MAX_ACCESSIBILITY_QUERY_TIMEOUT_MS)
        {
            issues.push(ValidationIssue::new(
                "accessibility.query_timeout_ms",
                "accessibility query timeout must be between 1 and 10000 ms",
            ));
        }
        if self.accessibility.cursor_ttl_ms == 0
            || self.accessibility.cursor_ttl_ms > u64::from(ACCESSIBILITY_CURSOR_TTL_MS)
        {
            issues.push(ValidationIssue::new(
                "accessibility.cursor_ttl_ms",
                "accessibility cursor lifetime must be non-zero and not exceed the protocol maximum",
            ));
        }
        if self.accessibility.query_timeout_ms < self.accessibility.proxy_timeout_ms {
            issues.push(ValidationIssue::new(
                "accessibility.query_timeout_ms",
                "accessibility query timeout must cover one proxy timeout",
            ));
        }
        if self.accessibility.reconnect_max_backoff_ms
            < self.accessibility.reconnect_initial_backoff_ms
        {
            issues.push(ValidationIssue::new(
                "accessibility.reconnect_max_backoff_ms",
                "accessibility reconnect maximum must cover the initial backoff",
            ));
        }
        if !self.viewer.view_only {
            issues.push(ValidationIssue::new(
                "viewer.view_only",
                "release one requires server-side view-only viewer input",
            ));
        }
        let canonical_origins = self
            .viewer
            .allowed_origins
            .iter()
            .filter_map(|origin| ViewerOrigin::new(origin.clone()).ok())
            .map(|origin| origin.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        if (self.viewer.enabled && self.viewer.allowed_origins.is_empty())
            || canonical_origins.len() != self.viewer.allowed_origins.len()
        {
            issues.push(ValidationIssue::new(
                "viewer.allowed_origins",
                "enabled viewer origins must be non-empty, unique, canonical exact HTTP(S) origins",
            ));
        }
        if !self.viewer.backend_address.ip().is_loopback()
            || self.viewer.backend_address.port() == 0
        {
            issues.push(ValidationIssue::new(
                "viewer.backend_address",
                "viewer backend must be an explicit nonzero loopback socket",
            ));
        }
        if self.viewer.no_vnc_root.as_os_str().is_empty()
            || !self.viewer.no_vnc_root.is_absolute()
            || self.viewer.no_vnc_root == Path::new("/")
        {
            issues.push(ValidationIssue::new(
                "viewer.no_vnc_root",
                "noVNC asset root must be a non-root absolute path",
            ));
        }
        if self.viewer.ticket_capacity == 0
            || self.viewer.ticket_capacity > MAX_VIEWER_TICKET_CAPACITY
        {
            issues.push(ValidationIssue::new(
                "viewer.ticket_capacity",
                "viewer ticket capacity must be between 1 and 16384",
            ));
        }
        if self.viewer.ticket_ttl_seconds == 0
            || i128::from(self.viewer.ticket_ttl_seconds) > MAX_VIEWER_TICKET_TTL_SECONDS
        {
            issues.push(ValidationIssue::new(
                "viewer.ticket_ttl_seconds",
                "viewer ticket TTL must be between 1 and 60 seconds",
            ));
        }
        if self.viewer.connect_timeout_ms == 0
            || self.viewer.connect_timeout_ms > MAX_VIEWER_CONNECT_TIMEOUT_MS
        {
            issues.push(ValidationIssue::new(
                "viewer.connect_timeout_ms",
                "viewer connect timeout must be between 1 and 30000 ms",
            ));
        }
        if self.viewer.write_timeout_ms == 0
            || self.viewer.write_timeout_ms > MAX_VIEWER_WRITE_TIMEOUT_MS
        {
            issues.push(ValidationIssue::new(
                "viewer.write_timeout_ms",
                "viewer write timeout must be between 1 and 30000 ms",
            ));
        }
        if self.viewer.idle_timeout_ms == 0
            || self.viewer.idle_timeout_ms > MAX_VIEWER_IDLE_TIMEOUT_MS
        {
            issues.push(ValidationIssue::new(
                "viewer.idle_timeout_ms",
                "viewer idle timeout must be between 1 and 3600000 ms",
            ));
        }
        if self.viewer.session_timeout_ms < self.viewer.idle_timeout_ms
            || self.viewer.session_timeout_ms > MAX_VIEWER_SESSION_TIMEOUT_MS
        {
            issues.push(ValidationIssue::new(
                "viewer.session_timeout_ms",
                "viewer session timeout must cover idle timeout and not exceed 86400000 ms",
            ));
        }
        if self.viewer.maximum_frame_bytes == 0
            || self.viewer.maximum_frame_bytes > MAX_VIEWER_FRAME_BYTES
        {
            issues.push(ValidationIssue::new(
                "viewer.maximum_frame_bytes",
                "viewer frame limit must be between 1 and 8388608 bytes",
            ));
        }
        if self.viewer.maximum_sessions == 0 || self.viewer.maximum_sessions > MAX_VIEWER_SESSIONS {
            issues.push(ValidationIssue::new(
                "viewer.maximum_sessions",
                "viewer session capacity must be between 1 and 64",
            ));
        }

        issues.sort();
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(issues))
        }
    }

    /// Returns a deliberately small diagnostic summary with no secret paths.
    #[must_use]
    pub fn redacted_summary(&self) -> RedactedConfigSummary {
        RedactedConfigSummary {
            listen: self.server.listen,
            metrics_listen: self.server.metrics_listen,
            auth_disabled: self.server.insecure_disable_auth,
            token_source: "<redacted>",
            display: format!(
                "{}x{}x{}@{}dpi",
                self.desktop.display_width,
                self.desktop.display_height,
                self.desktop.depth,
                self.desktop.dpi
            ),
            viewer_view_only: self.viewer.view_only,
            log_filter: self.logging.filter.clone(),
        }
    }

    /// Returns server configuration.
    #[must_use]
    pub const fn server(&self) -> &ServerConfig {
        &self.server
    }

    /// Returns authentication configuration.
    #[must_use]
    pub const fn auth(&self) -> &AuthConfig {
        &self.auth
    }

    /// Returns desktop configuration.
    #[must_use]
    pub const fn desktop(&self) -> &DesktopConfig {
        &self.desktop
    }

    /// Returns input configuration.
    #[must_use]
    pub const fn input(&self) -> &InputConfig {
        &self.input
    }

    /// Returns limits configuration.
    #[must_use]
    pub const fn limits(&self) -> &LimitsConfig {
        &self.limits
    }

    /// Returns private artifact-store configuration.
    #[must_use]
    pub const fn artifacts(&self) -> &ArtifactConfig {
        &self.artifacts
    }

    /// Returns daemon observation model/actor configuration.
    #[must_use]
    pub const fn observation(&self) -> &ObservationConfig {
        &self.observation
    }

    /// Returns the independently degrading accessibility actor/cache settings.
    #[must_use]
    pub const fn accessibility(&self) -> &AccessibilityConfig {
        &self.accessibility
    }

    /// Returns viewer configuration.
    #[must_use]
    pub const fn viewer(&self) -> &ViewerConfig {
        &self.viewer
    }

    /// Returns logging configuration.
    #[must_use]
    pub const fn logging(&self) -> &LoggingConfig {
        &self.logging
    }
}

/// HTTP listener and extraction limits.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    listen: SocketAddr,
    metrics_listen: Option<SocketAddr>,
    request_body_limit_bytes: u64,
    insecure_disable_auth: bool,
}

impl ServerConfig {
    /// Returns the API bind address.
    #[must_use]
    pub const fn listen(&self) -> SocketAddr {
        self.listen
    }

    /// Returns the optional separate metrics bind address.
    #[must_use]
    pub const fn metrics_listen(&self) -> Option<SocketAddr> {
        self.metrics_listen
    }

    /// Returns the ordinary JSON/WS message byte limit.
    #[must_use]
    pub const fn request_body_limit_bytes(&self) -> u64 {
        self.request_body_limit_bytes
    }

    /// Returns whether the explicit loopback-only development auth bypass is on.
    #[must_use]
    pub const fn insecure_disable_auth(&self) -> bool {
        self.insecure_disable_auth
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            metrics_listen: None,
            request_body_limit_bytes: 1_048_576,
            insecure_disable_auth: false,
        }
    }
}

/// Static-token authentication inputs.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    token_file: SecretFile,
    grants: Vec<String>,
}

impl AuthConfig {
    /// Returns the secret-bearing token-file wrapper.
    #[must_use]
    pub const fn token_file(&self) -> &SecretFile {
        &self.token_file
    }

    /// Returns the configured least-privilege grant set in declared order.
    #[must_use]
    pub fn grants(&self) -> &[String] {
        &self.grants
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            token_file: SecretFile(PathBuf::from("/run/xenoteer/api-token")),
            grants: AUTHORIZATION_GRANTS.map(str::to_owned).to_vec(),
        }
    }
}

/// Fixed Xvfb geometry profile.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopConfig {
    display_width: u32,
    display_height: u32,
    depth: u8,
    dpi: u16,
}

impl DesktopConfig {
    /// Returns root-window width in physical pixels.
    #[must_use]
    pub const fn display_width(&self) -> u32 {
        self.display_width
    }

    /// Returns root-window height in physical pixels.
    #[must_use]
    pub const fn display_height(&self) -> u32 {
        self.display_height
    }

    /// Returns X screen depth.
    #[must_use]
    pub const fn depth(&self) -> u8 {
        self.depth
    }

    /// Returns fixed display DPI.
    #[must_use]
    pub const fn dpi(&self) -> u16 {
        self.dpi
    }
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            display_width: 1920,
            display_height: 1080,
            depth: 24,
            dpi: 96,
        }
    }
}

/// Input timing and queue defaults.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    queue_capacity: usize,
    pointer_nominal_speed_px_s: u32,
    pointer_move_min_ms: u32,
    pointer_move_max_ms: u32,
    pointer_sample_rate_hz: u16,
    pre_click_dwell_ms: u32,
    pointer_press_ms: u32,
    keyboard_hold_ms: u32,
}

impl InputConfig {
    /// Returns public input queue capacity.
    #[must_use]
    pub const fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }

    /// Returns nominal automatic movement speed.
    #[must_use]
    pub const fn pointer_nominal_speed_px_s(&self) -> u32 {
        self.pointer_nominal_speed_px_s
    }

    /// Returns minimum automatic movement duration.
    #[must_use]
    pub const fn pointer_move_min_ms(&self) -> u32 {
        self.pointer_move_min_ms
    }

    /// Returns maximum automatic movement duration.
    #[must_use]
    pub const fn pointer_move_max_ms(&self) -> u32 {
        self.pointer_move_max_ms
    }

    /// Returns pointer interpolation sample rate.
    #[must_use]
    pub const fn pointer_sample_rate_hz(&self) -> u16 {
        self.pointer_sample_rate_hz
    }

    /// Returns the pre-click dwell duration.
    #[must_use]
    pub const fn pre_click_dwell_ms(&self) -> u32 {
        self.pre_click_dwell_ms
    }

    /// Returns default pointer press duration.
    #[must_use]
    pub const fn pointer_press_ms(&self) -> u32 {
        self.pointer_press_ms
    }

    /// Returns default keyboard hold duration.
    #[must_use]
    pub const fn keyboard_hold_ms(&self) -> u32 {
        self.keyboard_hold_ms
    }
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 256,
            pointer_nominal_speed_px_s: 1_200,
            pointer_move_min_ms: 80,
            pointer_move_max_ms: 650,
            pointer_sample_rate_hz: 60,
            pre_click_dwell_ms: 30,
            pointer_press_ms: 50,
            keyboard_hold_ms: 35,
        }
    }
}

/// Global command and result-ledger ceilings used by the skeleton.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    default_action_timeout_ms: u64,
    accepted_commands_per_principal: usize,
    accepted_commands_per_daemon: usize,
    result_ledger_entries: usize,
    result_ledger_ttl_seconds: u64,
}

impl LimitsConfig {
    /// Returns default action timeout.
    #[must_use]
    pub const fn default_action_timeout_ms(&self) -> u64 {
        self.default_action_timeout_ms
    }

    /// Returns concurrent accepted/running commands per principal.
    #[must_use]
    pub const fn accepted_commands_per_principal(&self) -> usize {
        self.accepted_commands_per_principal
    }

    /// Returns concurrent accepted/running commands for the daemon.
    #[must_use]
    pub const fn accepted_commands_per_daemon(&self) -> usize {
        self.accepted_commands_per_daemon
    }

    /// Returns result-ledger entry count.
    #[must_use]
    pub const fn result_ledger_entries(&self) -> usize {
        self.result_ledger_entries
    }

    /// Returns result-ledger TTL in seconds.
    #[must_use]
    pub const fn result_ledger_ttl_seconds(&self) -> u64 {
        self.result_ledger_ttl_seconds
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            default_action_timeout_ms: 30_000,
            accepted_commands_per_principal: 32,
            accepted_commands_per_daemon: 128,
            result_ledger_entries: 10_000,
            result_ledger_ttl_seconds: 900,
        }
    }
}

/// Private artifact-store quotas, retention, and streaming deadlines.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArtifactConfig {
    root_directory: PathBuf,
    max_object_bytes: u64,
    max_total_bytes: u64,
    max_objects: u64,
    max_owner_bytes: u64,
    max_owner_objects: u64,
    max_retention_ms: u64,
    clipboard_input_retention_ms: u64,
    upload_total_timeout_ms: u64,
    upload_idle_timeout_ms: u64,
}

impl ArtifactConfig {
    /// Returns the private artifact-store root directory.
    #[must_use]
    pub fn root_directory(&self) -> &Path {
        &self.root_directory
    }

    /// Returns the maximum size of one artifact in bytes.
    #[must_use]
    pub const fn max_object_bytes(&self) -> u64 {
        self.max_object_bytes
    }

    /// Returns the aggregate artifact-store byte quota.
    #[must_use]
    pub const fn max_total_bytes(&self) -> u64 {
        self.max_total_bytes
    }

    /// Returns the aggregate artifact-store object-count quota.
    #[must_use]
    pub const fn max_objects(&self) -> u64 {
        self.max_objects
    }

    /// Returns the per-owner artifact byte quota.
    #[must_use]
    pub const fn max_owner_bytes(&self) -> u64 {
        self.max_owner_bytes
    }

    /// Returns the per-owner artifact object-count quota.
    #[must_use]
    pub const fn max_owner_objects(&self) -> u64 {
        self.max_owner_objects
    }

    /// Returns the maximum artifact retention duration in milliseconds.
    #[must_use]
    pub const fn max_retention_ms(&self) -> u64 {
        self.max_retention_ms
    }

    /// Returns clipboard-input artifact retention in milliseconds.
    #[must_use]
    pub const fn clipboard_input_retention_ms(&self) -> u64 {
        self.clipboard_input_retention_ms
    }

    /// Returns the total upload deadline in milliseconds.
    #[must_use]
    pub const fn upload_total_timeout_ms(&self) -> u64 {
        self.upload_total_timeout_ms
    }

    /// Returns the between-chunk upload deadline in milliseconds.
    #[must_use]
    pub const fn upload_idle_timeout_ms(&self) -> u64 {
        self.upload_idle_timeout_ms
    }
}

impl Default for ArtifactConfig {
    fn default() -> Self {
        Self {
            root_directory: PathBuf::from("/run/xenoteer/artifacts"),
            max_object_bytes: 32 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            max_objects: 1_024,
            max_owner_bytes: 256 * 1024 * 1024,
            max_owner_objects: 256,
            max_retention_ms: 60 * 60 * 1_000,
            clipboard_input_retention_ms: 15 * 60 * 1_000,
            upload_total_timeout_ms: 60_000,
            upload_idle_timeout_ms: 10_000,
        }
    }
}

/// Bounded daemon-owned observation actor and window-model settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObservationConfig {
    request_capacity: usize,
    max_waiters: usize,
    token_capacity: usize,
    cursor_ttl_ms: u64,
    reference_ttl_ms: u64,
    raw_request_timeout_ms: u64,
    startup_timeout_ms: u64,
    idle_poll_interval_ms: u64,
    max_live_windows: usize,
    max_tombstones: usize,
    tombstone_ttl_ms: u64,
}

impl ObservationConfig {
    /// Returns the bounded model-request queue capacity.
    #[must_use]
    pub const fn request_capacity(&self) -> usize {
        self.request_capacity
    }

    /// Returns the maximum number of simultaneous waits.
    #[must_use]
    pub const fn max_waiters(&self) -> usize {
        self.max_waiters
    }

    /// Returns the aggregate cursor and reference token capacity.
    #[must_use]
    pub const fn token_capacity(&self) -> usize {
        self.token_capacity
    }

    /// Returns cursor-token lifetime in milliseconds.
    #[must_use]
    pub const fn cursor_ttl_ms(&self) -> u64 {
        self.cursor_ttl_ms
    }

    /// Returns reference-token lifetime in milliseconds.
    #[must_use]
    pub const fn reference_ttl_ms(&self) -> u64 {
        self.reference_ttl_ms
    }

    /// Returns the per-request raw-X11 deadline in milliseconds.
    #[must_use]
    pub const fn raw_request_timeout_ms(&self) -> u64 {
        self.raw_request_timeout_ms
    }

    /// Returns the initial observation reconciliation deadline in milliseconds.
    #[must_use]
    pub const fn startup_timeout_ms(&self) -> u64 {
        self.startup_timeout_ms
    }

    /// Returns the idle actor polling interval in milliseconds.
    #[must_use]
    pub const fn idle_poll_interval_ms(&self) -> u64 {
        self.idle_poll_interval_ms
    }

    /// Returns the maximum number of simultaneously live windows.
    #[must_use]
    pub const fn max_live_windows(&self) -> usize {
        self.max_live_windows
    }

    /// Returns the maximum number of retained window tombstones.
    #[must_use]
    pub const fn max_tombstones(&self) -> usize {
        self.max_tombstones
    }

    /// Returns window-tombstone lifetime in milliseconds.
    #[must_use]
    pub const fn tombstone_ttl_ms(&self) -> u64 {
        self.tombstone_ttl_ms
    }
}

impl Default for ObservationConfig {
    fn default() -> Self {
        Self {
            request_capacity: 128,
            max_waiters: 128,
            token_capacity: 8_192,
            cursor_ttl_ms: 60_000,
            reference_ttl_ms: 15 * 60 * 1_000,
            raw_request_timeout_ms: 2_000,
            startup_timeout_ms: 10_000,
            idle_poll_interval_ms: 5,
            max_live_windows: 4_096,
            max_tombstones: 8_192,
            tombstone_ttl_ms: 15 * 60 * 1_000,
        }
    }
}

/// Bounded AT-SPI actor, cache, query, and reconnect settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessibilityConfig {
    enabled: bool,
    request_capacity: usize,
    event_capacity: usize,
    max_waiters: usize,
    token_capacity: usize,
    cursor_ttl_ms: u64,
    max_cached_nodes: usize,
    max_tombstones: usize,
    max_nodes_per_query: usize,
    max_selector_depth: usize,
    max_query_matches: usize,
    max_snapshot_nodes: usize,
    max_snapshot_bytes: usize,
    proxy_timeout_ms: u64,
    query_timeout_ms: u64,
    reconnect_initial_backoff_ms: u64,
    reconnect_max_backoff_ms: u64,
}

impl AccessibilityConfig {
    /// Returns whether semantic automation should attempt to connect.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the bounded actor request capacity.
    #[must_use]
    pub const fn request_capacity(&self) -> usize {
        self.request_capacity
    }

    /// Returns the bounded normalized event capacity.
    #[must_use]
    pub const fn event_capacity(&self) -> usize {
        self.event_capacity
    }

    /// Returns the maximum simultaneous semantic waits.
    #[must_use]
    pub const fn max_waiters(&self) -> usize {
        self.max_waiters
    }

    /// Returns the aggregate opaque page-cursor capacity.
    #[must_use]
    pub const fn token_capacity(&self) -> usize {
        self.token_capacity
    }

    /// Returns semantic page-cursor lifetime in milliseconds.
    #[must_use]
    pub const fn cursor_ttl_ms(&self) -> u64 {
        self.cursor_ttl_ms
    }

    /// Returns the maximum normalized nodes retained per desktop.
    #[must_use]
    pub const fn max_cached_nodes(&self) -> usize {
        self.max_cached_nodes
    }

    /// Returns the maximum destroyed identities retained against retargeting.
    #[must_use]
    pub const fn max_tombstones(&self) -> usize {
        self.max_tombstones
    }

    /// Returns the per-query traversal budget.
    #[must_use]
    pub const fn max_nodes_per_query(&self) -> usize {
        self.max_nodes_per_query
    }

    /// Returns the maximum selector nesting/traversal depth.
    #[must_use]
    pub const fn max_selector_depth(&self) -> usize {
        self.max_selector_depth
    }

    /// Returns the maximum number of public query matches.
    #[must_use]
    pub const fn max_query_matches(&self) -> usize {
        self.max_query_matches
    }

    /// Returns the maximum nodes admitted into one semantic snapshot.
    #[must_use]
    pub const fn max_snapshot_nodes(&self) -> usize {
        self.max_snapshot_nodes
    }

    /// Returns the maximum encoded semantic snapshot bytes.
    #[must_use]
    pub const fn max_snapshot_bytes(&self) -> usize {
        self.max_snapshot_bytes
    }

    /// Returns the deadline for one AT-SPI proxy operation.
    #[must_use]
    pub const fn proxy_timeout_ms(&self) -> u64 {
        self.proxy_timeout_ms
    }

    /// Returns the default whole-query deadline.
    #[must_use]
    pub const fn query_timeout_ms(&self) -> u64 {
        self.query_timeout_ms
    }

    /// Returns the first reconnect backoff.
    #[must_use]
    pub const fn reconnect_initial_backoff_ms(&self) -> u64 {
        self.reconnect_initial_backoff_ms
    }

    /// Returns the capped reconnect backoff.
    #[must_use]
    pub const fn reconnect_max_backoff_ms(&self) -> u64 {
        self.reconnect_max_backoff_ms
    }
}

impl Default for AccessibilityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            request_capacity: 128,
            event_capacity: 4_096,
            max_waiters: 128,
            token_capacity: 65_536,
            cursor_ttl_ms: u64::from(ACCESSIBILITY_CURSOR_TTL_MS),
            max_cached_nodes: 100_000,
            max_tombstones: 100_000,
            max_nodes_per_query: 25_000,
            max_selector_depth: 64,
            max_query_matches: 1_000,
            max_snapshot_nodes: 10_000,
            max_snapshot_bytes: 16 * 1_024 * 1_024,
            proxy_timeout_ms: 2_000,
            query_timeout_ms: 10_000,
            reconnect_initial_backoff_ms: 100,
            reconnect_max_backoff_ms: 5_000,
        }
    }
}

/// Viewer safety policy.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ViewerConfig {
    enabled: bool,
    allowed_origins: Vec<String>,
    backend_address: SocketAddr,
    no_vnc_root: PathBuf,
    ticket_capacity: usize,
    ticket_ttl_seconds: u64,
    connect_timeout_ms: u64,
    write_timeout_ms: u64,
    idle_timeout_ms: u64,
    session_timeout_ms: u64,
    maximum_frame_bytes: usize,
    maximum_sessions: usize,
    view_only: bool,
}

impl ViewerConfig {
    /// Returns whether viewer ticket issuance and gateway routes are enabled.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the unique canonical exact browser-origin allowlist.
    #[must_use]
    pub fn allowed_origins(&self) -> &[String] {
        &self.allowed_origins
    }

    /// Returns the fixed loopback websockify endpoint.
    #[must_use]
    pub const fn backend_address(&self) -> SocketAddr {
        self.backend_address
    }

    /// Returns the pinned noVNC module root.
    #[must_use]
    pub fn no_vnc_root(&self) -> &Path {
        &self.no_vnc_root
    }

    /// Returns the maximum number of unexpired one-time viewer tickets.
    #[must_use]
    pub const fn ticket_capacity(&self) -> usize {
        self.ticket_capacity
    }

    /// Returns the one-time viewer ticket lifetime in seconds.
    #[must_use]
    pub const fn ticket_ttl_seconds(&self) -> u64 {
        self.ticket_ttl_seconds
    }

    /// Returns the viewer backend connection deadline in milliseconds.
    #[must_use]
    pub const fn connect_timeout_ms(&self) -> u64 {
        self.connect_timeout_ms
    }

    /// Returns the per-write viewer deadline in milliseconds.
    #[must_use]
    pub const fn write_timeout_ms(&self) -> u64 {
        self.write_timeout_ms
    }

    /// Returns the viewer inactivity deadline in milliseconds.
    #[must_use]
    pub const fn idle_timeout_ms(&self) -> u64 {
        self.idle_timeout_ms
    }

    /// Returns the absolute viewer session deadline in milliseconds.
    #[must_use]
    pub const fn session_timeout_ms(&self) -> u64 {
        self.session_timeout_ms
    }

    /// Returns the maximum WebSocket frame or reassembled message size.
    #[must_use]
    pub const fn maximum_frame_bytes(&self) -> usize {
        self.maximum_frame_bytes
    }

    /// Returns the maximum simultaneous viewer sessions.
    #[must_use]
    pub const fn maximum_sessions(&self) -> usize {
        self.maximum_sessions
    }

    /// Returns whether server-side viewer input is disabled.
    #[must_use]
    pub const fn view_only(&self) -> bool {
        self.view_only
    }
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_origins: Vec::new(),
            backend_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6_080),
            no_vnc_root: PathBuf::from("/usr/share/novnc"),
            ticket_capacity: 1_024,
            ticket_ttl_seconds: 60,
            connect_timeout_ms: 3_000,
            write_timeout_ms: 5_000,
            idle_timeout_ms: 2 * 60 * 1_000,
            session_timeout_ms: 8 * 60 * 60 * 1_000,
            maximum_frame_bytes: MAX_VIEWER_FRAME_BYTES,
            maximum_sessions: 16,
            view_only: true,
        }
    }
}

/// Structured logging configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    filter: String,
}

impl LoggingConfig {
    /// Returns the tracing filter expression.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            filter: "info,xenoteer=debug".to_owned(),
        }
    }
}

/// A path from which secret material is read.
///
/// Both `Debug` and display output are unconditionally redacted. Callers must
/// explicitly request the path to perform the bounded startup read.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct SecretFile(PathBuf);

impl SecretFile {
    /// Creates a secret-file reference.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Returns the path for the startup secret loader.
    #[must_use]
    pub fn expose_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Debug for SecretFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretFile(<redacted>)")
    }
}

impl Default for SecretFile {
    fn default() -> Self {
        AuthConfig::default().token_file
    }
}

/// CLI's optional final-precedence overrides.
#[derive(Clone, Debug, Default)]
pub struct ConfigOverrides {
    listen: Option<SocketAddr>,
    insecure_disable_auth: Option<bool>,
    log_filter: Option<String>,
}

impl ConfigOverrides {
    /// Overrides the API listener.
    #[must_use]
    pub fn with_listen(mut self, listen: SocketAddr) -> Self {
        self.listen = Some(listen);
        self
    }

    /// Overrides the development authentication bypass.
    #[must_use]
    pub fn with_insecure_disable_auth(mut self, disabled: bool) -> Self {
        self.insecure_disable_auth = Some(disabled);
        self
    }

    /// Overrides the tracing filter.
    #[must_use]
    pub fn with_log_filter(mut self, filter: impl Into<String>) -> Self {
        self.log_filter = Some(filter.into());
        self
    }

    fn apply(&self, root: &mut Value) -> Result<(), ConfigLoadError> {
        if let Some(listen) = self.listen {
            insert_value(
                root,
                &["server", "listen"],
                Value::String(listen.to_string()),
            )?;
        }
        if let Some(value) = self.insecure_disable_auth {
            insert_value(
                root,
                &["server", "insecure_disable_auth"],
                Value::Boolean(value),
            )?;
        }
        if let Some(filter) = &self.log_filter {
            insert_value(root, &["logging", "filter"], Value::String(filter.clone()))?;
        }
        Ok(())
    }
}

/// Safe diagnostic projection of effective configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedactedConfigSummary {
    /// API listener.
    pub listen: SocketAddr,
    /// Optional metrics listener.
    pub metrics_listen: Option<SocketAddr>,
    /// Explicit development auth-bypass state.
    pub auth_disabled: bool,
    /// Always the literal `<redacted>`.
    pub token_source: &'static str,
    /// Fixed display profile summary.
    pub display: String,
    /// Server-side view-only policy.
    pub viewer_view_only: bool,
    /// Non-secret tracing filter.
    pub log_filter: String,
}

/// One cross-field validation issue.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ValidationIssue {
    /// Stable dotted field path.
    pub path: &'static str,
    /// Safe diagnostic message.
    pub message: &'static str,
}

impl ValidationIssue {
    const fn new(path: &'static str, message: &'static str) -> Self {
        Self { path, message }
    }
}

/// Sorted collection of cross-field validation issues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationErrors(Vec<ValidationIssue>);

impl ValidationErrors {
    /// Returns validation issues sorted by field path and message.
    #[must_use]
    pub fn issues(&self) -> &[ValidationIssue] {
        &self.0
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, issue) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{}: {}", issue.path, issue.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

/// Configuration parsing, merging, decoding, or validation failure.
#[derive(Debug, Error)]
pub enum ConfigLoadError {
    /// The file is not valid TOML.
    #[error("configuration TOML syntax is invalid: {0}")]
    Toml(#[source] ConfigDiagnostic),
    /// The decoded merged configuration is invalid, including unknown fields.
    #[error("configuration shape is invalid: {0}")]
    Decode(#[source] ConfigDiagnostic),
    /// Configuration root was not a TOML table.
    #[error("configuration root must be a TOML table")]
    RootNotTable,
    /// An environment key had an invalid nested path.
    ///
    /// The attacker-controlled key and value are deliberately not retained.
    #[error("invalid Xenoteer environment configuration key")]
    InvalidEnvironmentPath,
    /// One layer tried to nest beneath an existing scalar.
    #[error("configuration path conflicts with scalar at {0}")]
    PathConflict(String),
    /// Typed values violate one or more cross-field invariants.
    #[error("configuration validation failed: {0}")]
    Validation(ValidationErrors),
}

impl ConfigLoadError {
    fn toml(error: toml::de::Error, input: &str) -> Self {
        Self::Toml(ConfigDiagnostic::from_toml(
            &error,
            Some(input),
            ConfigDiagnosticKind::Syntax,
        ))
    }

    fn decode(error: toml::de::Error) -> Self {
        Self::Decode(ConfigDiagnostic::from_toml(
            &error,
            None,
            ConfigDiagnosticKind::Shape,
        ))
    }
}

/// Secret-safe metadata retained from a TOML parser or typed-decoder error.
///
/// This type intentionally does not retain the source document, offending
/// value, or upstream error because all three may contain token material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    kind: ConfigDiagnosticKind,
    line: Option<usize>,
    column: Option<usize>,
}

impl ConfigDiagnostic {
    fn from_toml(
        error: &toml::de::Error,
        input: Option<&str>,
        fallback: ConfigDiagnosticKind,
    ) -> Self {
        let message = error.message().to_ascii_lowercase();
        let kind = if message.contains("unknown field") {
            ConfigDiagnosticKind::UnknownField
        } else if message.contains("invalid type") {
            ConfigDiagnosticKind::InvalidType
        } else if message.contains("missing field") {
            ConfigDiagnosticKind::MissingField
        } else if message.contains("invalid value") {
            ConfigDiagnosticKind::InvalidValue
        } else {
            fallback
        };
        let (line, column) = input
            .zip(error.span())
            .map(|(input, span)| line_column(input, span.start))
            .map_or((None, None), |(line, column)| (Some(line), Some(column)));
        Self { kind, line, column }
    }

    /// Returns the safe diagnostic category.
    #[must_use]
    pub const fn kind(&self) -> ConfigDiagnosticKind {
        self.kind
    }

    /// Returns a one-based source line when the parser provided a span.
    #[must_use]
    pub const fn line(&self) -> Option<usize> {
        self.line
    }

    /// Returns a one-based source byte column when the parser provided a span.
    #[must_use]
    pub const fn column(&self) -> Option<usize> {
        self.column
    }
}

impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(formatter)?;
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(formatter, " at line {line}, column {column}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigDiagnostic {}

/// Secret-safe category for configuration decode diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigDiagnosticKind {
    /// TOML syntax could not be parsed.
    Syntax,
    /// A typed configuration field was not recognized.
    UnknownField,
    /// A field had an incompatible TOML type.
    InvalidType,
    /// A required typed field was absent.
    MissingField,
    /// A field value could not be decoded.
    InvalidValue,
    /// Typed decoding failed for another shape reason.
    Shape,
}

impl fmt::Display for ConfigDiagnosticKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Syntax => "syntax error",
            Self::UnknownField => "unknown field",
            Self::InvalidType => "incompatible value type",
            Self::MissingField => "missing field",
            Self::InvalidValue => "invalid field value",
            Self::Shape => "invalid typed shape",
        })
    }
}

fn line_column(input: &str, index: usize) -> (usize, usize) {
    let prefix = &input.as_bytes()[..index.min(input.len())];
    let line = prefix.iter().filter(|byte| **byte == b'\n').count() + 1;
    let column = prefix
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(prefix.len() + 1, |position| prefix.len() - position);
    (line, column)
}

fn normalized_env_path(path: &str) -> Result<Vec<&str>, ConfigLoadError> {
    let segments: Vec<_> = path.split("__").collect();
    if segments.len() != 2 || segments.iter().any(|segment| !valid_env_segment(segment)) {
        return Err(ConfigLoadError::InvalidEnvironmentPath);
    }
    Ok(segments)
}

fn valid_env_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.split('_').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        })
}

fn parse_env_value(raw: &str) -> Value {
    let document = format!("value = {raw}");
    toml::from_str::<Value>(&document)
        .ok()
        .and_then(|mut value| value.as_table_mut()?.remove("value"))
        .unwrap_or_else(|| Value::String(raw.to_owned()))
}

fn insert_value(root: &mut Value, segments: &[&str], value: Value) -> Result<(), ConfigLoadError> {
    let table = root.as_table_mut().ok_or(ConfigLoadError::RootNotTable)?;
    insert_table_value(table, segments, value, String::new())
}

fn insert_table_value(
    table: &mut Table,
    segments: &[&str],
    value: Value,
    mut prefix: String,
) -> Result<(), ConfigLoadError> {
    let Some((head, tail)) = segments.split_first() else {
        return Err(ConfigLoadError::InvalidEnvironmentPath);
    };
    let normalized = head.to_ascii_lowercase();
    if !prefix.is_empty() {
        prefix.push('.');
    }
    prefix.push_str(&normalized);
    if tail.is_empty() {
        table.insert(normalized, value);
        return Ok(());
    }

    let child = table
        .entry(normalized)
        .or_insert_with(|| Value::Table(Table::new()));
    let child_table = child
        .as_table_mut()
        .ok_or_else(|| ConfigLoadError::PathConflict(prefix.clone()))?;
    insert_table_value(child_table, tail, value, prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_file_env_and_cli_precedence() -> Result<(), ConfigLoadError> {
        let file = r#"
            [server]
            listen = "127.0.0.1:8100"
            [logging]
            filter = "warn"
        "#;
        let environment = [
            ("XENOTEER__SERVER__LISTEN", "127.0.0.1:8200"),
            ("XENOTEER__LOGGING__FILTER", "debug"),
        ];
        let overrides = ConfigOverrides::default()
            .with_listen(
                "127.0.0.1:8300"
                    .parse()
                    .map_err(|_| ConfigLoadError::RootNotTable)?,
            )
            .with_log_filter("trace");
        let config = Config::load(Some(file), environment, &overrides)?;
        assert_eq!(config.server().listen().port(), 8300);
        assert_eq!(config.logging().filter(), "trace");
        Ok(())
    }

    #[test]
    fn valid_nested_environment_is_applied_and_unrelated_environment_is_ignored()
    -> Result<(), ConfigLoadError> {
        let config = Config::load(
            None,
            [
                (
                    "UNRELATED_SECRET_CANARY",
                    "not valid TOML and not inspected",
                ),
                ("XENOTEER__LOGGING__FILTER", "warn"),
            ],
            &ConfigOverrides::default(),
        )?;
        assert_eq!(config.logging().filter(), "warn");
        Ok(())
    }

    #[test]
    fn malformed_xenoteer_environment_keys_fail_without_echoing_input()
    -> Result<(), Box<dyn std::error::Error>> {
        const VALUE_CANARY: &str = "ENV_VALUE_SECRET_CANARY";
        for key in [
            "XENOTEER_BAD_SECRET_KEY_CANARY",
            "XENOTEER___SECRET_KEY_CANARY__LISTEN",
            "XENOTEER__SERVER___SECRET_KEY_CANARY",
            "XENOTEER__SERVER____SECRET_KEY_CANARY",
            "XENOTEER__SERVER__SECRET_KEY_CANARY__EXTRA",
            "XENOTEER__SERVER",
            "XENOTEER__SERVER__",
        ] {
            let result = Config::load(None, [(key, VALUE_CANARY)], &ConfigOverrides::default());
            let error = match result {
                Err(error) => error,
                Ok(_) => {
                    return Err(std::io::Error::other(
                        "malformed Xenoteer environment key unexpectedly loaded",
                    )
                    .into());
                }
            };
            assert!(matches!(&error, ConfigLoadError::InvalidEnvironmentPath));
            assert_error_chain_redacted(&error, key);
            assert_error_chain_redacted(&error, VALUE_CANARY);
        }
        Ok(())
    }

    #[test]
    fn unknown_fields_fail_closed() {
        let error = Config::load(
            Some("[server]\nlistne = '127.0.0.1:9000'"),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        );
        assert!(matches!(error, Err(ConfigLoadError::Decode(_))));
    }

    #[test]
    fn cross_validation_reports_sorted_multiple_issues() -> Result<(), Box<dyn std::error::Error>> {
        let error = Config::load(
            Some(
                r#"
                [server]
                listen = "0.0.0.0:8080"
                insecure_disable_auth = true
                [viewer]
                view_only = false
                [input]
                queue_capacity = 0
                "#,
            ),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        );
        let errors = match error {
            Err(ConfigLoadError::Validation(errors)) => errors,
            _ => return Err(std::io::Error::other("expected validation errors").into()),
        };
        let paths: Vec<_> = errors.issues().iter().map(|issue| issue.path).collect();
        assert_eq!(
            paths,
            [
                "input.queue_capacity",
                "server.insecure_disable_auth",
                "viewer.view_only"
            ]
        );
        Ok(())
    }

    #[test]
    fn secret_file_debug_is_redacted() {
        let secret = SecretFile::new("/run/secrets/do-not-log-this");
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "SecretFile(<redacted>)");
        assert!(!rendered.contains("do-not-log-this"));
    }

    #[test]
    fn complete_config_debug_does_not_reveal_secret_path() -> Result<(), ConfigLoadError> {
        let config = Config::load(
            Some("[auth]\ntoken_file = '/run/secrets/config-debug-must-hide'"),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        )?;
        let rendered = format!("{config:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("config-debug-must-hide"));
        Ok(())
    }

    #[test]
    fn public_parse_error_does_not_echo_secret_bearing_input()
    -> Result<(), Box<dyn std::error::Error>> {
        const CANARY: &str = "PARSE_ERROR_SECRET_CANARY";
        let error = Config::load(
            Some("[auth\ntoken_file = 'PARSE_ERROR_SECRET_CANARY'"),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        );
        let error = match error {
            Err(error) => error,
            Ok(_) => return Err(std::io::Error::other("malformed TOML unexpectedly loaded").into()),
        };
        assert_error_chain_redacted(&error, CANARY);
        assert!(error.to_string().contains("line 1"));
        Ok(())
    }

    #[test]
    fn public_decode_error_does_not_retain_secret_bearing_input()
    -> Result<(), Box<dyn std::error::Error>> {
        const CANARY: &str = "DECODE_ERROR_SECRET_CANARY";
        let error = Config::load(
            Some("[auth]\ntoken_file = { secret = 'DECODE_ERROR_SECRET_CANARY' }"),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        );
        let error = match error {
            Err(error) => error,
            Ok(_) => {
                return Err(std::io::Error::other(
                    "invalid typed configuration unexpectedly loaded",
                )
                .into());
            }
        };
        assert_error_chain_redacted(&error, CANARY);
        assert!(matches!(error, ConfigLoadError::Decode(_)));
        Ok(())
    }

    #[test]
    fn defaults_match_product_contract() -> Result<(), ConfigLoadError> {
        let config = Config::load(
            None,
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        )?;
        assert_eq!(config.desktop().display_width(), 1920);
        assert_eq!(config.desktop().display_height(), 1080);
        assert_eq!(config.input().queue_capacity(), 256);
        assert_eq!(
            config.artifacts().root_directory(),
            Path::new("/run/xenoteer/artifacts")
        );
        assert_eq!(config.artifacts().upload_total_timeout_ms(), 60_000);
        assert_eq!(config.artifacts().upload_idle_timeout_ms(), 10_000);
        assert_eq!(config.observation().max_live_windows(), 4_096);
        assert_eq!(config.observation().token_capacity(), 8_192);
        assert!(config.accessibility().enabled());
        assert_eq!(config.accessibility().request_capacity(), 128);
        assert_eq!(config.accessibility().event_capacity(), 4_096);
        assert_eq!(config.accessibility().max_waiters(), 128);
        assert_eq!(config.accessibility().token_capacity(), 65_536);
        assert_eq!(config.accessibility().cursor_ttl_ms(), 30_000);
        assert_eq!(config.accessibility().max_cached_nodes(), 100_000);
        assert_eq!(config.accessibility().max_tombstones(), 100_000);
        assert_eq!(config.accessibility().max_nodes_per_query(), 25_000);
        assert_eq!(config.accessibility().max_selector_depth(), 64);
        assert_eq!(config.accessibility().max_query_matches(), 1_000);
        assert_eq!(config.accessibility().max_snapshot_nodes(), 10_000);
        assert_eq!(config.accessibility().max_snapshot_bytes(), 16_777_216);
        assert_eq!(config.accessibility().proxy_timeout_ms(), 2_000);
        assert_eq!(config.accessibility().query_timeout_ms(), 10_000);
        assert_eq!(config.accessibility().reconnect_initial_backoff_ms(), 100);
        assert_eq!(config.accessibility().reconnect_max_backoff_ms(), 5_000);
        assert!(!config.viewer().enabled());
        assert!(config.viewer().allowed_origins().is_empty());
        assert_eq!(
            config.viewer().backend_address(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6_080)
        );
        assert_eq!(config.viewer().no_vnc_root(), Path::new("/usr/share/novnc"));
        assert_eq!(config.viewer().ticket_capacity(), 1_024);
        assert_eq!(config.viewer().ticket_ttl_seconds(), 60);
        assert_eq!(config.viewer().connect_timeout_ms(), 3_000);
        assert_eq!(config.viewer().write_timeout_ms(), 5_000);
        assert_eq!(config.viewer().idle_timeout_ms(), 120_000);
        assert_eq!(config.viewer().session_timeout_ms(), 28_800_000);
        assert_eq!(config.viewer().maximum_frame_bytes(), 8_388_608);
        assert_eq!(config.viewer().maximum_sessions(), 16);
        assert_eq!(
            config.auth().grants(),
            AUTHORIZATION_GRANTS.map(str::to_owned)
        );
        assert!(config.viewer().view_only());
        Ok(())
    }

    #[test]
    fn authorization_grants_are_closed_unique_and_configurable()
    -> Result<(), Box<dyn std::error::Error>> {
        let restricted = Config::load(
            Some("[auth]\ngrants = ['desktop:status']"),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        )?;
        assert_eq!(restricted.auth().grants(), ["desktop:status"]);

        for document in [
            "[auth]\ngrants = ['desktop:status', 'desktop:status']",
            "[auth]\ngrants = ['desktop:administrator']",
        ] {
            assert_validation_path(document, "auth.grants")?;
        }
        Ok(())
    }

    #[test]
    fn release_one_desktop_profile_is_exact() -> Result<(), Box<dyn std::error::Error>> {
        for (document, expected_path) in [
            ("[desktop]\ndisplay_width = 1921", "desktop.display_width"),
            ("[desktop]\ndisplay_height = 1079", "desktop.display_height"),
            ("[desktop]\ndepth = 16", "desktop.depth"),
            ("[desktop]\ndpi = 97", "desktop.dpi"),
        ] {
            assert_validation_path(document, expected_path)?;
        }
        Ok(())
    }

    #[test]
    fn artifact_observation_and_accessibility_config_fail_closed_at_boundaries()
    -> Result<(), Box<dyn std::error::Error>> {
        for (document, expected_path) in [
            (
                "[artifacts]\nroot_directory = 'relative/path'",
                "artifacts.root_directory",
            ),
            (
                "[artifacts]\nupload_total_timeout_ms = 0",
                "artifacts.upload_total_timeout_ms",
            ),
            (
                "[artifacts]\nupload_total_timeout_ms = 10\nupload_idle_timeout_ms = 11",
                "artifacts.upload_idle_timeout_ms",
            ),
            (
                "[artifacts]\nmax_object_bytes = 1025\nmax_total_bytes = 1024",
                "artifacts.max_object_bytes",
            ),
            (
                "[artifacts]\nclipboard_input_retention_ms = 3600001",
                "artifacts.clipboard_input_retention_ms",
            ),
            (
                "[observation]\nrequest_capacity = 0",
                "observation.request_capacity",
            ),
            (
                "[observation]\ntoken_capacity = 1",
                "observation.token_capacity",
            ),
            (
                "[observation]\nraw_request_timeout_ms = 0",
                "observation.raw_request_timeout_ms",
            ),
            (
                "[observation]\nmax_live_windows = 4097",
                "observation.max_live_windows",
            ),
            (
                "[accessibility]\nrequest_capacity = 0",
                "accessibility.request_capacity",
            ),
            (
                "[accessibility]\nevent_capacity = 65537",
                "accessibility.event_capacity",
            ),
            (
                "[accessibility]\ntoken_capacity = 999",
                "accessibility.token_capacity",
            ),
            (
                "[accessibility]\ncursor_ttl_ms = 0",
                "accessibility.cursor_ttl_ms",
            ),
            (
                "[accessibility]\ncursor_ttl_ms = 30001",
                "accessibility.cursor_ttl_ms",
            ),
            (
                "[accessibility]\nmax_cached_nodes = 100001",
                "accessibility.max_cached_nodes",
            ),
            (
                "[accessibility]\nmax_nodes_per_query = 0",
                "accessibility.max_nodes_per_query",
            ),
            (
                "[accessibility]\nmax_nodes_per_query = 25001",
                "accessibility.max_nodes_per_query",
            ),
            (
                "[accessibility]\nmax_selector_depth = 65",
                "accessibility.max_selector_depth",
            ),
            (
                "[accessibility]\nmax_snapshot_nodes = 10001",
                "accessibility.max_snapshot_nodes",
            ),
            (
                "[accessibility]\nmax_snapshot_bytes = 16777217",
                "accessibility.max_snapshot_bytes",
            ),
            (
                "[accessibility]\nproxy_timeout_ms = 2001\nquery_timeout_ms = 2000",
                "accessibility.query_timeout_ms",
            ),
            (
                "[accessibility]\nquery_timeout_ms = 10001",
                "accessibility.query_timeout_ms",
            ),
            (
                "[accessibility]\nreconnect_initial_backoff_ms = 101\nreconnect_max_backoff_ms = 100",
                "accessibility.reconnect_max_backoff_ms",
            ),
        ] {
            assert_validation_path(document, expected_path)?;
        }

        let unknown = Config::load(
            Some("[artifacts]\nsecret_backdoor = true"),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        );
        assert!(matches!(unknown, Err(ConfigLoadError::Decode(_))));
        let unknown = Config::load(
            Some("[accessibility]\nsecret_backdoor = true"),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        );
        assert!(matches!(unknown, Err(ConfigLoadError::Decode(_))));
        Ok(())
    }

    #[test]
    fn viewer_config_requires_canonical_origins_and_server_hard_limits()
    -> Result<(), Box<dyn std::error::Error>> {
        let enabled = Config::load(
            Some(
                r#"
                [viewer]
                enabled = true
                allowed_origins = ["https://viewer.example", "http://127.0.0.1:8080"]
                backend_address = "127.0.0.1:6080"
                no_vnc_root = "/usr/share/novnc"
                ticket_capacity = 1024
                ticket_ttl_seconds = 60
                connect_timeout_ms = 3000
                write_timeout_ms = 5000
                idle_timeout_ms = 120000
                session_timeout_ms = 28800000
                maximum_frame_bytes = 8388608
                maximum_sessions = 16
                view_only = true
                "#,
            ),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        );
        assert!(enabled.is_ok());
        let exact_ceilings = Config::load(
            Some(
                r#"
                [viewer]
                enabled = true
                allowed_origins = ["https://viewer.example"]
                ticket_capacity = 16384
                ticket_ttl_seconds = 60
                connect_timeout_ms = 30000
                write_timeout_ms = 30000
                idle_timeout_ms = 3600000
                session_timeout_ms = 86400000
                maximum_frame_bytes = 8388608
                maximum_sessions = 64
                "#,
            ),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        );
        assert!(exact_ceilings.is_ok());

        for (document, expected_path) in [
            ("[viewer]\nenabled = true", "viewer.allowed_origins"),
            (
                "[viewer]\nenabled = true\nallowed_origins = ['https://viewer.example', 'https://viewer.example']",
                "viewer.allowed_origins",
            ),
            (
                "[viewer]\nallowed_origins = ['ftp://viewer.example']",
                "viewer.allowed_origins",
            ),
            (
                "[viewer]\nbackend_address = '0.0.0.0:6080'",
                "viewer.backend_address",
            ),
            (
                "[viewer]\nbackend_address = '127.0.0.1:0'",
                "viewer.backend_address",
            ),
            (
                "[viewer]\nno_vnc_root = 'relative/novnc'",
                "viewer.no_vnc_root",
            ),
            ("[viewer]\nno_vnc_root = '/'", "viewer.no_vnc_root"),
            ("[viewer]\nticket_capacity = 0", "viewer.ticket_capacity"),
            (
                "[viewer]\nticket_capacity = 16385",
                "viewer.ticket_capacity",
            ),
            (
                "[viewer]\nticket_ttl_seconds = 0",
                "viewer.ticket_ttl_seconds",
            ),
            (
                "[viewer]\nticket_ttl_seconds = 61",
                "viewer.ticket_ttl_seconds",
            ),
            (
                "[viewer]\nconnect_timeout_ms = 0",
                "viewer.connect_timeout_ms",
            ),
            (
                "[viewer]\nconnect_timeout_ms = 30001",
                "viewer.connect_timeout_ms",
            ),
            ("[viewer]\nwrite_timeout_ms = 0", "viewer.write_timeout_ms"),
            (
                "[viewer]\nwrite_timeout_ms = 30001",
                "viewer.write_timeout_ms",
            ),
            ("[viewer]\nidle_timeout_ms = 0", "viewer.idle_timeout_ms"),
            (
                "[viewer]\nidle_timeout_ms = 3600001",
                "viewer.idle_timeout_ms",
            ),
            (
                "[viewer]\nidle_timeout_ms = 100\nsession_timeout_ms = 99",
                "viewer.session_timeout_ms",
            ),
            (
                "[viewer]\nsession_timeout_ms = 86400001",
                "viewer.session_timeout_ms",
            ),
            (
                "[viewer]\nmaximum_frame_bytes = 0",
                "viewer.maximum_frame_bytes",
            ),
            (
                "[viewer]\nmaximum_frame_bytes = 8388609",
                "viewer.maximum_frame_bytes",
            ),
            ("[viewer]\nmaximum_sessions = 0", "viewer.maximum_sessions"),
            ("[viewer]\nmaximum_sessions = 65", "viewer.maximum_sessions"),
        ] {
            assert_validation_path(document, expected_path)?;
        }
        Ok(())
    }

    #[test]
    fn checked_in_example_is_complete_and_valid() -> Result<(), ConfigLoadError> {
        let config = Config::load(
            Some(include_str!("../../../xenoteer.example.toml")),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        )?;
        assert_eq!(config.server().request_body_limit_bytes(), 1_048_576);
        assert_eq!(config.input().pointer_sample_rate_hz(), 60);
        Ok(())
    }

    #[test]
    fn rejects_out_of_range_input_and_ledger_limits() -> Result<(), Box<dyn std::error::Error>> {
        for (document, expected_path) in [
            (
                "[input]\npointer_sample_rate_hz = 241",
                "input.pointer_sample_rate_hz",
            ),
            (
                "[input]\npointer_nominal_speed_px_s = 0",
                "input.pointer_nominal_speed_px_s",
            ),
            (
                "[input]\npointer_move_max_ms = 10001",
                "input.pointer_move_max_ms",
            ),
            (
                "[input]\npre_click_dwell_ms = 10001",
                "input.pre_click_dwell_ms",
            ),
            (
                "[input]\npointer_press_ms = 10001",
                "input.pointer_press_ms",
            ),
            (
                "[input]\nkeyboard_hold_ms = 10001",
                "input.keyboard_hold_ms",
            ),
            (
                "[limits]\nresult_ledger_entries = 0",
                "limits.result_ledger_entries",
            ),
            (
                "[limits]\nresult_ledger_ttl_seconds = 0",
                "limits.result_ledger_ttl_seconds",
            ),
            (
                "[server]\nrequest_body_limit_bytes = 1048577",
                "server.request_body_limit_bytes",
            ),
            ("[input]\nqueue_capacity = 257", "input.queue_capacity"),
            (
                "[limits]\naccepted_commands_per_daemon = 129",
                "limits.accepted_commands_per_daemon",
            ),
            (
                "[limits]\naccepted_commands_per_principal = 33",
                "limits.accepted_commands_per_principal",
            ),
            (
                "[limits]\nresult_ledger_entries = 10001",
                "limits.result_ledger_entries",
            ),
            (
                "[limits]\ndefault_action_timeout_ms = 9223372036854775807",
                "limits.default_action_timeout_ms",
            ),
            (
                "[limits]\nresult_ledger_ttl_seconds = 9223372036854775807",
                "limits.result_ledger_ttl_seconds",
            ),
        ] {
            assert_validation_path(document, expected_path)?;
        }
        Ok(())
    }

    #[test]
    fn programmatic_duration_maxima_fail_closed() {
        let mut config = Config::default();
        config.limits.default_action_timeout_ms = u64::MAX;
        config.limits.result_ledger_ttl_seconds = u64::MAX;
        let errors = config
            .validate()
            .err()
            .map_or_else(Vec::new, |errors| errors.0);
        assert!(
            errors
                .iter()
                .any(|issue| issue.path == "limits.default_action_timeout_ms")
        );
        assert!(
            errors
                .iter()
                .any(|issue| issue.path == "limits.result_ledger_ttl_seconds")
        );
    }

    #[test]
    fn action_timeout_covers_compound_pointer_primitive() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_validation_path(
            "[input]\npointer_move_max_ms = 1000\npre_click_dwell_ms = 200\npointer_press_ms = 300\n[limits]\ndefault_action_timeout_ms = 1499",
            "limits.default_action_timeout_ms",
        )
    }

    #[test]
    fn action_timeout_covers_keyboard_hold_primitive() -> Result<(), Box<dyn std::error::Error>> {
        assert_validation_path(
            "[input]\nkeyboard_hold_ms = 1000\n[limits]\ndefault_action_timeout_ms = 999",
            "limits.default_action_timeout_ms",
        )
    }

    #[test]
    fn compound_pointer_duration_uses_checked_arithmetic() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_validation_path(
            "[input]\npointer_move_max_ms = 4294967295\npre_click_dwell_ms = 4294967295\npointer_press_ms = 4294967295",
            "input.pointer_compound_duration",
        )
    }

    fn assert_validation_path(
        document: &str,
        expected_path: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let result = Config::load(
            Some(document),
            std::iter::empty::<(&str, &str)>(),
            &ConfigOverrides::default(),
        );
        let errors = match result {
            Err(ConfigLoadError::Validation(errors)) => errors,
            Err(other) => {
                return Err(std::io::Error::other(format!(
                    "expected validation error at {expected_path}, received {other}"
                ))
                .into());
            }
            Ok(_) => {
                return Err(std::io::Error::other(format!(
                    "configuration unexpectedly accepted invalid field {expected_path}"
                ))
                .into());
            }
        };
        assert!(
            errors
                .issues()
                .iter()
                .any(|issue| issue.path == expected_path),
            "missing validation path {expected_path} in {:?}",
            errors.issues()
        );
        Ok(())
    }

    fn assert_error_chain_redacted(error: &ConfigLoadError, canary: &str) {
        assert!(!format!("{error}").contains(canary));
        assert!(!format!("{error:?}").contains(canary));
        let mut source = std::error::Error::source(error);
        while let Some(current) = source {
            assert!(!format!("{current}").contains(canary));
            assert!(!format!("{current:?}").contains(canary));
            source = current.source();
        }
    }
}
