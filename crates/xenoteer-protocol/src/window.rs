//! Stable window references, bounded snapshots, and observation event payloads.

#![allow(missing_docs)]

use core::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::geometry::StrictRect;
use crate::{CoordinateSpace, DesktopGeneration, DesktopId, GeometryError, ProcessRef, Rect};

/// Existing read authorization retained for window snapshots, queries, waits, and events.
pub const WINDOW_READ_GRANT: &str = "desktop:observe";
/// Independent authorization required by every window-manager mutation.
pub const WINDOW_CONTROL_GRANT: &str = "window:control";

/// Event topic for the first observation of one XID birth.
pub const WINDOW_CREATED_TOPIC: &str = "window.created";
/// Event topic for bounded metadata, focus, state, or geometry changes.
pub const WINDOW_CHANGED_TOPIC: &str = "window.changed";
/// Event topic for the terminal observation of one XID birth.
pub const WINDOW_DESTROYED_TOPIC: &str = "window.destroyed";
/// Event topic proving that the authoritative window model was reconciled.
pub const WINDOW_MODEL_REBUILT_TOPIC: &str = "window.model_rebuilt";

/// Lowercase hexadecimal characters in a SHA-256 window identity projection.
pub const WINDOW_IDENTITY_HASH_BYTES: usize = 64;
/// Maximum UTF-8 bytes in one observed title or ICCCM text component.
pub const MAX_WINDOW_TEXT_BYTES: usize = 4_096;
/// Maximum UTF-8 bytes in one exposed, escaped X11 atom name.
pub const MAX_WINDOW_ATOM_NAME_BYTES: usize = 255;
/// Maximum atoms retained in any one bounded window metadata set.
pub const MAX_WINDOW_ATOMS: usize = 256;
/// Maximum safe diagnostic warnings retained with one window snapshot.
pub const MAX_WINDOW_WARNINGS: usize = 16;
/// Largest X11 window dimension representable by the release-one model.
pub const MAX_WINDOW_DIMENSION: u32 = u16::MAX as u32;
/// Maximum changed-field names in one coalesced metadata event.
pub const MAX_WINDOW_CHANGED_FIELDS: usize = 32;
/// Maximum independent process-correlation evidence entries.
pub const MAX_WINDOW_PROCESS_EVIDENCE: usize = 8;

/// One non-zero actor-local model revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
pub struct WindowModelRevision(u64);

impl WindowModelRevision {
    /// Creates a non-zero model revision.
    pub const fn new(value: u64) -> Result<Self, WindowValidationError> {
        if value == 0 {
            return Err(WindowValidationError::ModelRevision);
        }
        Ok(Self(value))
    }

    /// Returns the actor-local revision number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for WindowModelRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        crate::wire_integer::nonzero::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for WindowModelRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(crate::wire_integer::nonzero::deserialize(deserializer)?)
            .map_err(de::Error::custom)
    }
}

/// Fixed lowercase SHA-256 projection binding an XID birth to initial evidence.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "window_identity_hash_schema")]
pub struct WindowIdentityHash(String);

fn window_identity_hash_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": WINDOW_IDENTITY_HASH_BYTES,
        "maxLength": WINDOW_IDENTITY_HASH_BYTES,
        "pattern": "^[0-9a-f]{64}$"
    })
}

impl WindowIdentityHash {
    /// Creates a checked lowercase identity hash.
    pub fn new(value: impl Into<String>) -> Result<Self, WindowValidationError> {
        let value = value.into();
        if value.len() != WINDOW_IDENTITY_HASH_BYTES
            || !value
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(WindowValidationError::IdentityHash);
        }
        Ok(Self(value))
    }

    /// Returns the lowercase fixed-width hash.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WindowIdentityHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WindowIdentityHash")
            .field(&self.0)
            .finish()
    }
}

impl Serialize for WindowIdentityHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WindowIdentityHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Stable reference to exactly one observed XID birth in one desktop lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct WindowRef {
    /// Desktop resource that owns the window.
    pub desktop_id: DesktopId,
    /// Exact X server/session lifetime that owns the XID.
    pub desktop_generation: DesktopGeneration,
    /// Non-zero core X11 window identifier.
    #[schemars(range(min = 1))]
    pub xid: u32,
    /// Per-XID birth marker incremented whenever an absent XID reappears.
    #[serde(with = "crate::wire_integer::nonzero")]
    #[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
    pub observed_generation: u64,
    /// Server-generated binding to first-observed identity evidence.
    pub identity_hash: WindowIdentityHash,
}

/// Closed request-direction representation of [`WindowRef`].
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "WindowRef")]
pub(crate) struct StrictWindowRef {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    #[schemars(range(min = 1))]
    xid: u32,
    #[serde(with = "crate::wire_integer::nonzero")]
    #[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
    observed_generation: u64,
    identity_hash: WindowIdentityHash,
}

impl From<StrictWindowRef> for WindowRef {
    fn from(value: StrictWindowRef) -> Self {
        Self {
            desktop_id: value.desktop_id,
            desktop_generation: value.desktop_generation,
            xid: value.xid,
            observed_generation: value.observed_generation,
            identity_hash: value.identity_hash,
        }
    }
}

pub(crate) fn deserialize_strict_window_ref<'de, D>(deserializer: D) -> Result<WindowRef, D::Error>
where
    D: Deserializer<'de>,
{
    StrictWindowRef::deserialize(deserializer).map(Into::into)
}

pub(crate) fn deserialize_optional_strict_window_ref<'de, D>(
    deserializer: D,
) -> Result<Option<WindowRef>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<StrictWindowRef>::deserialize(deserializer).map(|value| value.map(Into::into))
}

impl WindowRef {
    /// Validates only the serialized shape of this reference.
    ///
    /// This does not establish that the XID is live or that its birth marker
    /// and identity hash still match the authoritative window model. Every
    /// reference-taking operation must perform that actor-owned lookup again
    /// immediately before an effect.
    pub fn validate_shape(&self) -> Result<(), WindowValidationError> {
        if self.desktop_id.as_uuid().is_nil() || self.desktop_generation.as_uuid().is_nil() {
            return Err(WindowValidationError::NilIdentifier);
        }
        if self.xid == 0 || self.observed_generation == 0 {
            return Err(WindowValidationError::WindowReference);
        }
        WindowIdentityHash::new(self.identity_hash.as_str())?;
        Ok(())
    }

    /// Compatibility alias for shape-only validation.
    ///
    /// Callers must not treat success as proof of liveness or identity. Use
    /// the observation actor's exact-reference resolver before an effect.
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        self.validate_shape()
    }

    /// Returns the canonical diagnostic XID representation.
    #[must_use]
    pub fn xid_hex(&self) -> String {
        format!("0x{:08x}", self.xid)
    }

    /// Returns whether another reference belongs to the same desktop lifetime.
    #[must_use]
    pub fn shares_desktop_scope(&self, other: &Self) -> bool {
        self.desktop_id == other.desktop_id && self.desktop_generation == other.desktop_generation
    }
}

/// Bounded observed text with explicit lossy-decoding evidence.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowText {
    /// UTF-8 projection exposed to clients.
    #[schemars(length(max = MAX_WINDOW_TEXT_BYTES))]
    pub value: String,
    /// Whether invalid source bytes required replacement or fallback decoding.
    pub lossy: bool,
}

impl fmt::Debug for WindowText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowText")
            .field("value", &"<redacted>")
            .field("lossy", &self.lossy)
            .finish()
    }
}

impl WindowText {
    /// Creates a bounded observed text projection.
    pub fn new(value: impl Into<String>, lossy: bool) -> Result<Self, WindowValidationError> {
        let value = Self {
            value: value.into(),
            lossy,
        };
        value.validate()?;
        Ok(value)
    }

    /// Revalidates the UTF-8 byte ceiling.
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        if self.value.len() > MAX_WINDOW_TEXT_BYTES {
            return Err(WindowValidationError::Text);
        }
        Ok(())
    }
}

/// Separately decoded ICCCM `WM_CLASS` components.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowClass {
    /// Resource instance component, if it could be recovered.
    pub instance: Option<WindowText>,
    /// Resource class component, if it could be recovered.
    pub class: Option<WindowText>,
}

impl WindowClass {
    /// Validates each independently recoverable class component.
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        if self.instance.is_none() && self.class.is_none() {
            return Err(WindowValidationError::WindowClass);
        }
        if let Some(instance) = &self.instance {
            instance.validate()?;
        }
        if let Some(class) = &self.class {
            class.validate()?;
        }
        Ok(())
    }
}

/// Bounded escaped X11 atom name safe for public metadata.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "window_atom_name_schema")]
pub struct WindowAtomName(String);

fn window_atom_name_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_WINDOW_ATOM_NAME_BYTES
    })
}

impl WindowAtomName {
    /// Creates a checked non-control atom-name projection.
    pub fn new(value: impl Into<String>) -> Result<Self, WindowValidationError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_WINDOW_ATOM_NAME_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(WindowValidationError::AtomName);
        }
        Ok(Self(value))
    }

    /// Returns the safe public atom-name projection.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WindowAtomName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WindowAtomName")
            .field(&self.0)
            .finish()
    }
}

impl Serialize for WindowAtomName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WindowAtomName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// One rectangle paired with its explicit coordinate reference frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowRect {
    /// Reference frame used by `rect`.
    pub coordinate_space: CoordinateSpace,
    /// Checked non-empty rectangle.
    pub rect: Rect,
}

/// Closed request-direction coordinate-tagged rectangle.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "WindowRect")]
pub(crate) struct StrictWindowRect {
    coordinate_space: CoordinateSpace,
    #[schemars(with = "StrictRect")]
    rect: StrictRect,
}

impl From<StrictWindowRect> for WindowRect {
    fn from(value: StrictWindowRect) -> Self {
        Self {
            coordinate_space: value.coordinate_space,
            rect: value.rect.into(),
        }
    }
}

pub(crate) fn deserialize_strict_window_rect<'de, D>(
    deserializer: D,
) -> Result<WindowRect, D::Error>
where
    D: Deserializer<'de>,
{
    StrictWindowRect::deserialize(deserializer).map(Into::into)
}

impl WindowRect {
    /// Creates a checked coordinate-tagged rectangle.
    pub fn new(
        coordinate_space: CoordinateSpace,
        rect: Rect,
    ) -> Result<Self, WindowValidationError> {
        let value = Self {
            coordinate_space,
            rect,
        };
        value.validate()?;
        Ok(value)
    }

    /// Validates geometry and the X11 dimension ceiling.
    pub fn validate(self) -> Result<(), WindowValidationError> {
        self.rect.validate()?;
        let size = self.rect.size()?;
        if size.width() > MAX_WINDOW_DIMENSION || size.height() > MAX_WINDOW_DIMENSION {
            return Err(WindowValidationError::GeometryDimension);
        }
        Ok(())
    }
}

/// Advisory window-manager frame borders in root-physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowFrameExtents {
    #[schemars(range(max = MAX_WINDOW_DIMENSION))]
    pub left: u32,
    #[schemars(range(max = MAX_WINDOW_DIMENSION))]
    pub right: u32,
    #[schemars(range(max = MAX_WINDOW_DIMENSION))]
    pub top: u32,
    #[schemars(range(max = MAX_WINDOW_DIMENSION))]
    pub bottom: u32,
}

impl WindowFrameExtents {
    /// Rejects values beyond X11's geometry representation.
    pub fn validate(self) -> Result<(), WindowValidationError> {
        if [self.left, self.right, self.top, self.bottom]
            .into_iter()
            .any(|extent| extent > MAX_WINDOW_DIMENSION)
        {
            return Err(WindowValidationError::FrameExtents);
        }
        Ok(())
    }
}

/// Bounded geometry projection with every rectangle expressed in root coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowGeometry {
    /// Top-level client geometry translated into root-physical coordinates.
    pub client_rect: WindowRect,
    /// Window-manager frame geometry, when a frame can be established.
    pub frame_rect: Option<WindowRect>,
    /// Proven content geometry; initially equal to the client geometry.
    pub content_rect: WindowRect,
    /// Advisory `_NET_FRAME_EXTENTS`, when available.
    pub frame_extents: Option<WindowFrameExtents>,
}

impl WindowGeometry {
    /// Validates all geometry and requires root-physical snapshot coordinates.
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        self.client_rect.validate()?;
        self.content_rect.validate()?;
        if let Some(frame) = self.frame_rect {
            frame.validate()?;
        }
        if [
            Some(self.client_rect),
            self.frame_rect,
            Some(self.content_rect),
        ]
        .into_iter()
        .flatten()
        .any(|value| value.coordinate_space != CoordinateSpace::RootPhysical)
        {
            return Err(WindowValidationError::SnapshotCoordinateSpace);
        }
        if let Some(extents) = self.frame_extents {
            extents.validate()?;
        }
        Ok(())
    }
}

/// X server map/viewability state without guessing minimization semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowMapState {
    /// The client is not mapped.
    Unmapped,
    /// The client is mapped but not currently viewable.
    Unviewable,
    /// The client is mapped and viewable.
    Viewable,
}

/// Observed window-manager and focus state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowObservedState {
    pub map_state: WindowMapState,
    pub minimized: bool,
    pub hidden: bool,
    pub urgent: bool,
    pub modal: bool,
    pub sticky: bool,
    pub active: bool,
    pub focused: bool,
}

/// Confidence attached to advisory process correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowProcessConfidence {
    None,
    Low,
    Medium,
    High,
}

/// Independent evidence used for process/window correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowProcessEvidence {
    NetWmPid,
    ProcStartTime,
    ProcessGroup,
    ClientLeader,
    WmClass,
    WmCommand,
    ClientMachine,
    UniqueCandidate,
}

/// Explicitly advisory process correlation; never authorization evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowProcessCorrelation {
    /// Client-supplied `_NET_WM_PID`, if present and non-zero.
    #[schemars(range(min = 1))]
    pub reported_pid: Option<u32>,
    /// Exact managed process identity when independently correlated.
    pub managed_process: Option<ProcessRef>,
    /// Aggregate confidence reported by the correlation policy.
    pub confidence: WindowProcessConfidence,
    /// Bounded independent evidence used by the policy.
    #[schemars(length(max = MAX_WINDOW_PROCESS_EVIDENCE))]
    pub evidence: Vec<WindowProcessEvidence>,
    /// Whether supplied PID evidence conflicts with stronger evidence.
    pub conflict: bool,
}

impl WindowProcessCorrelation {
    /// Validates positive identifiers, evidence bounds, and confidence consistency.
    pub fn validate(
        &self,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), WindowValidationError> {
        if self.reported_pid == Some(0)
            || self.evidence.len() > MAX_WINDOW_PROCESS_EVIDENCE
            || has_duplicates(&self.evidence)
        {
            return Err(WindowValidationError::ProcessCorrelation);
        }
        if let Some(process) = self.managed_process {
            process
                .validate()
                .map_err(|_| WindowValidationError::ProcessCorrelation)?;
            if process.desktop_generation != desktop_generation {
                return Err(WindowValidationError::ReferenceScope);
            }
        }
        let has_evidence = |needle| self.evidence.contains(&needle);
        let has_identity_anchor = has_evidence(WindowProcessEvidence::ProcStartTime)
            || has_evidence(WindowProcessEvidence::ProcessGroup)
            || has_evidence(WindowProcessEvidence::ClientLeader);

        if has_evidence(WindowProcessEvidence::NetWmPid) && self.reported_pid.is_none() {
            return Err(WindowValidationError::ProcessCorrelation);
        }
        if self.confidence == WindowProcessConfidence::None
            && (!self.evidence.is_empty() || self.managed_process.is_some() || self.conflict)
        {
            return Err(WindowValidationError::ProcessCorrelation);
        }
        if self.confidence != WindowProcessConfidence::None && self.evidence.is_empty() {
            return Err(WindowValidationError::ProcessCorrelation);
        }
        if self.managed_process.is_some() && !has_identity_anchor {
            return Err(WindowValidationError::ProcessCorrelation);
        }
        if self.confidence == WindowProcessConfidence::High
            && (self.managed_process.is_none()
                || self.conflict
                || !has_identity_anchor
                || self.evidence.len() < 2)
        {
            return Err(WindowValidationError::ProcessCorrelation);
        }
        if self.conflict
            && (self.reported_pid.is_none()
                || self.managed_process.is_none()
                || !has_evidence(WindowProcessEvidence::NetWmPid)
                || !has_identity_anchor)
        {
            return Err(WindowValidationError::ProcessCorrelation);
        }
        Ok(())
    }
}

/// Bounded metadata that may originate from untrusted X11 client properties.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowMetadata {
    pub title: Option<WindowText>,
    pub visible_title: Option<WindowText>,
    pub icon_title: Option<WindowText>,
    pub class: Option<WindowClass>,
    pub client_machine: Option<WindowText>,
    #[schemars(length(max = MAX_WINDOW_ATOMS))]
    pub window_types: Vec<WindowAtomName>,
    #[schemars(length(max = MAX_WINDOW_ATOMS))]
    pub states: Vec<WindowAtomName>,
    #[schemars(length(max = MAX_WINDOW_ATOMS))]
    pub allowed_actions: Vec<WindowAtomName>,
    #[schemars(length(max = MAX_WINDOW_ATOMS))]
    pub protocols: Vec<WindowAtomName>,
}

impl WindowMetadata {
    /// Revalidates all untrusted text and bounded atom sets.
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        for text in [
            self.title.as_ref(),
            self.visible_title.as_ref(),
            self.icon_title.as_ref(),
            self.client_machine.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            text.validate()?;
        }
        if let Some(class) = &self.class {
            class.validate()?;
        }
        for values in [
            &self.window_types,
            &self.states,
            &self.allowed_actions,
            &self.protocols,
        ] {
            if values.len() > MAX_WINDOW_ATOMS || has_duplicates(values) {
                return Err(WindowValidationError::AtomSet);
            }
        }
        Ok(())
    }
}

/// Bounded diagnostic attached to an otherwise usable snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum WindowSnapshotWarning {
    MalformedProperty { property: WindowAtomName },
    TruncatedProperty { property: WindowAtomName },
    LossyPropertyText { property: WindowAtomName },
    UnsupportedPropertyEncoding { property: WindowAtomName },
    ProcessEvidenceConflict,
    FrameGeometryUnavailable,
    FrameExtentsUnverified,
}

/// Complete bounded observation of one currently modeled window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowSnapshot {
    /// Stable identity for this exact XID birth.
    #[serde(rename = "ref")]
    pub window: WindowRef,
    /// Canonical lowercase diagnostic XID, redundant by design.
    pub xid_hex: String,
    /// Actor-local model revision that produced this snapshot.
    pub model_revision: WindowModelRevision,
    pub metadata: WindowMetadata,
    pub process: WindowProcessCorrelation,
    pub state: WindowObservedState,
    pub geometry: Option<WindowGeometry>,
    /// Current fixed-workspace identifier, when reported by the WM.
    pub workspace: Option<u32>,
    /// Client leader correlated to a live modeled window, when available.
    pub client_leader: Option<WindowRef>,
    /// Transient parent correlated to a live modeled window, when available.
    pub transient_for: Option<WindowRef>,
    /// Group leader correlated to a live modeled window, when available.
    pub group_leader: Option<WindowRef>,
    /// Zero-based position in the observed stacking order.
    pub stacking_index: Option<u32>,
    /// Whether an AT-SPI application correlation has been established.
    pub has_accessibility_application: bool,
    #[schemars(length(max = MAX_WINDOW_WARNINGS))]
    pub warnings: Vec<WindowSnapshotWarning>,
}

impl WindowSnapshot {
    /// Validates the complete bounded snapshot and every related reference.
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        self.window.validate_shape()?;
        if self.xid_hex != self.window.xid_hex() {
            return Err(WindowValidationError::XidHex);
        }
        self.metadata.validate()?;
        self.process.validate(self.window.desktop_generation)?;
        if let Some(geometry) = &self.geometry {
            geometry.validate()?;
        }
        for related in [
            self.client_leader.as_ref(),
            self.transient_for.as_ref(),
            self.group_leader.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            related.validate_shape()?;
            if !self.window.shares_desktop_scope(related) {
                return Err(WindowValidationError::ReferenceScope);
            }
        }
        if self.warnings.len() > MAX_WINDOW_WARNINGS {
            return Err(WindowValidationError::Warnings);
        }
        if has_duplicates(&self.warnings)
            || self.process.conflict
                != self
                    .warnings
                    .contains(&WindowSnapshotWarning::ProcessEvidenceConflict)
        {
            return Err(WindowValidationError::Warnings);
        }
        Ok(())
    }
}

/// Lifecycle transition for one exact XID birth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowLifecycleKind {
    Created,
    Mapped,
    Unmapped,
    Destroyed,
}

/// Compact lifecycle event before coordinator-global sequence assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowLifecycleEvent {
    pub window: WindowRef,
    /// Actor-local revision only; the event hub assigns the public sequence.
    pub model_revision: WindowModelRevision,
    pub lifecycle: WindowLifecycleKind,
}

impl WindowLifecycleEvent {
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        self.window.validate_shape()
    }
}

/// Metadata fields named by one coalesced `window.changed` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowMetadataField {
    Title,
    VisibleTitle,
    IconTitle,
    Class,
    ClientMachine,
    WindowTypes,
    States,
    AllowedActions,
    Protocols,
    Workspace,
    Relationships,
    ProcessCorrelation,
    AccessibilityCorrelation,
}

/// Bounded current metadata projection after a property/model mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowMetadataEvent {
    pub window: WindowRef,
    /// Actor-local revision only; the event hub assigns the public sequence.
    pub model_revision: WindowModelRevision,
    #[schemars(length(min = 1, max = MAX_WINDOW_CHANGED_FIELDS))]
    pub changed: Vec<WindowMetadataField>,
    pub metadata: WindowMetadata,
}

impl WindowMetadataEvent {
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        self.window.validate_shape()?;
        if self.changed.is_empty()
            || self.changed.len() > MAX_WINDOW_CHANGED_FIELDS
            || has_duplicates(&self.changed)
        {
            return Err(WindowValidationError::ChangedFields);
        }
        self.metadata.validate()
    }
}

/// Root active-window and core-focus transition before global sequencing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowFocusEvent {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    /// Actor-local revision only; the event hub assigns the public sequence.
    pub model_revision: WindowModelRevision,
    pub previous_active: Option<WindowRef>,
    pub active: Option<WindowRef>,
    pub previous_focused: Option<WindowRef>,
    pub focused: Option<WindowRef>,
}

impl WindowFocusEvent {
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        if self.desktop_id.as_uuid().is_nil() || self.desktop_generation.as_uuid().is_nil() {
            return Err(WindowValidationError::NilIdentifier);
        }
        for window in [
            self.previous_active.as_ref(),
            self.active.as_ref(),
            self.previous_focused.as_ref(),
            self.focused.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            window.validate_shape()?;
            if window.desktop_id != self.desktop_id
                || window.desktop_generation != self.desktop_generation
            {
                return Err(WindowValidationError::ReferenceScope);
            }
        }
        Ok(())
    }
}

/// Bounded before/after geometry evidence for one exact window birth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowGeometryEvent {
    pub window: WindowRef,
    /// Actor-local revision only; the event hub assigns the public sequence.
    pub model_revision: WindowModelRevision,
    pub before: Option<WindowGeometry>,
    pub after: WindowGeometry,
}

impl WindowGeometryEvent {
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        self.window.validate_shape()?;
        if let Some(before) = &self.before {
            before.validate()?;
        }
        self.after.validate()
    }
}

/// Why the observation actor rebuilt its authoritative model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowModelRebuildReason {
    Startup,
    ExplicitResync,
    EventOverflow,
    X11Reconnect,
    SuspiciousProperty,
}

/// Metadata-free model reconciliation evidence before global sequencing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowModelRebuiltEvent {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub previous_revision: Option<WindowModelRevision>,
    /// Actor-local revision only; the event hub assigns the public sequence.
    pub model_revision: WindowModelRevision,
    pub window_count: u32,
    pub reason: WindowModelRebuildReason,
}

impl WindowModelRebuiltEvent {
    pub fn validate(&self) -> Result<(), WindowValidationError> {
        if self.desktop_id.as_uuid().is_nil() || self.desktop_generation.as_uuid().is_nil() {
            return Err(WindowValidationError::NilIdentifier);
        }
        if self
            .previous_revision
            .is_some_and(|previous| previous >= self.model_revision)
        {
            return Err(WindowValidationError::ModelRevision);
        }
        Ok(())
    }
}

fn has_duplicates<T: Eq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[..index].contains(value))
}

/// Window protocol validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WindowValidationError {
    #[error("window message contains a nil identifier")]
    NilIdentifier,
    #[error("window reference requires a non-zero XID and birth marker")]
    WindowReference,
    #[error("window identity hash must be 64 lowercase hexadecimal characters")]
    IdentityHash,
    #[error("window model revision must be non-zero and monotonic")]
    ModelRevision,
    #[error("window text exceeds its UTF-8 byte ceiling")]
    Text,
    #[error("window class must retain at least one component")]
    WindowClass,
    #[error("window atom name is empty, oversized, or contains control characters")]
    AtomName,
    #[error("window atom set is oversized or contains duplicates")]
    AtomSet,
    #[error("window geometry exceeds the X11 dimension ceiling")]
    GeometryDimension,
    #[error("window frame extents exceed the X11 dimension ceiling")]
    FrameExtents,
    #[error("window snapshot geometry must use root-physical coordinates")]
    SnapshotCoordinateSpace,
    #[error("window process correlation is inconsistent")]
    ProcessCorrelation,
    #[error("related window or process reference belongs to another desktop lifetime")]
    ReferenceScope,
    #[error("window snapshot XID text does not match its reference")]
    XidHex,
    #[error("window snapshot warnings are oversized, duplicated, or conflict with evidence")]
    Warnings,
    #[error("window metadata change fields are empty, duplicated, or oversized")]
    ChangedFields,
    #[error(transparent)]
    Geometry(#[from] GeometryError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(xid: u32, observed_generation: u64) -> Result<WindowRef, WindowValidationError> {
        Ok(WindowRef {
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            xid,
            observed_generation,
            identity_hash: WindowIdentityHash::new("a".repeat(WINDOW_IDENTITY_HASH_BYTES))?,
        })
    }

    fn snapshot(window: WindowRef) -> Result<WindowSnapshot, WindowValidationError> {
        Ok(WindowSnapshot {
            xid_hex: window.xid_hex(),
            window,
            model_revision: WindowModelRevision::new(1)?,
            metadata: WindowMetadata {
                title: None,
                visible_title: None,
                icon_title: None,
                class: None,
                client_machine: None,
                window_types: Vec::new(),
                states: Vec::new(),
                allowed_actions: Vec::new(),
                protocols: Vec::new(),
            },
            process: WindowProcessCorrelation {
                reported_pid: None,
                managed_process: None,
                confidence: WindowProcessConfidence::None,
                evidence: Vec::new(),
                conflict: false,
            },
            state: WindowObservedState {
                map_state: WindowMapState::Viewable,
                minimized: false,
                hidden: false,
                urgent: false,
                modal: false,
                sticky: false,
                active: false,
                focused: false,
            },
            geometry: None,
            workspace: Some(0),
            client_leader: None,
            transient_for: None,
            group_leader: None,
            stacking_index: Some(0),
            has_accessibility_application: false,
            warnings: Vec::new(),
        })
    }

    #[test]
    fn window_reference_rejects_nil_scope_and_reused_xid_ambiguity()
    -> Result<(), WindowValidationError> {
        let valid = reference(42, 1)?;
        valid.validate_shape()?;

        let mut missing_birth = valid.clone();
        missing_birth.observed_generation = 0;
        assert_eq!(
            missing_birth.validate(),
            Err(WindowValidationError::WindowReference)
        );

        let replacement = WindowRef {
            observed_generation: 2,
            identity_hash: WindowIdentityHash::new("b".repeat(WINDOW_IDENTITY_HASH_BYTES))?,
            ..valid
        };
        assert_ne!(replacement.observed_generation, 1);
        assert_ne!(replacement.identity_hash.as_str(), "a".repeat(64));
        Ok(())
    }

    #[test]
    fn window_text_debug_never_exposes_observed_content() -> Result<(), WindowValidationError> {
        let text = WindowText::new("canary-window-secret", false)?;
        let debug = format!("{text:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("canary-window-secret"));
        Ok(())
    }

    #[test]
    fn high_process_confidence_requires_managed_independent_evidence() {
        let correlation = WindowProcessCorrelation {
            reported_pid: Some(42),
            managed_process: None,
            confidence: WindowProcessConfidence::High,
            evidence: vec![WindowProcessEvidence::NetWmPid],
            conflict: false,
        };
        assert_eq!(
            correlation.validate(DesktopGeneration::new()),
            Err(WindowValidationError::ProcessCorrelation)
        );
    }

    #[test]
    fn process_conflict_requires_reported_pid_and_stronger_managed_identity() {
        let correlation = WindowProcessCorrelation {
            reported_pid: None,
            managed_process: None,
            confidence: WindowProcessConfidence::Low,
            evidence: vec![WindowProcessEvidence::NetWmPid],
            conflict: true,
        };
        assert_eq!(
            correlation.validate(DesktopGeneration::new()),
            Err(WindowValidationError::ProcessCorrelation)
        );
    }

    #[test]
    fn snapshot_conflict_warning_must_match_process_evidence() -> Result<(), WindowValidationError>
    {
        let mut observed = snapshot(reference(43, 1)?)?;
        observed.warnings = vec![WindowSnapshotWarning::ProcessEvidenceConflict];
        assert_eq!(observed.validate(), Err(WindowValidationError::Warnings));
        Ok(())
    }

    #[test]
    fn identity_and_atom_wrappers_validate_during_decode() {
        assert!(
            serde_json::from_str::<WindowIdentityHash>(&format!("\"{}\"", "0".repeat(64))).is_ok()
        );
        assert!(serde_json::from_str::<WindowIdentityHash>("\"ABC\"").is_err());
        assert!(serde_json::from_str::<WindowAtomName>("\"_NET_WM_NAME\"").is_ok());
        assert!(serde_json::from_str::<WindowAtomName>("\"bad\\nname\"").is_err());
        assert!(serde_json::from_str::<WindowModelRevision>("0").is_err());
    }

    #[test]
    fn snapshot_geometry_requires_explicit_root_space() -> Result<(), WindowValidationError> {
        let client = Rect::new(10, 20, 640, 480)?;
        let geometry = WindowGeometry {
            client_rect: WindowRect::new(CoordinateSpace::RootPhysical, client)?,
            frame_rect: None,
            content_rect: WindowRect::new(CoordinateSpace::WindowClient, client)?,
            frame_extents: None,
        };
        assert_eq!(
            geometry.validate(),
            Err(WindowValidationError::SnapshotCoordinateSpace)
        );
        Ok(())
    }

    #[test]
    fn focus_event_rejects_cross_generation_references() -> Result<(), WindowValidationError> {
        let active = reference(42, 1)?;
        let event = WindowFocusEvent {
            desktop_id: active.desktop_id,
            desktop_generation: DesktopGeneration::new(),
            model_revision: WindowModelRevision::new(3)?,
            previous_active: None,
            active: Some(active),
            previous_focused: None,
            focused: None,
        };
        assert_eq!(event.validate(), Err(WindowValidationError::ReferenceScope));
        Ok(())
    }

    #[test]
    fn metadata_change_requires_unique_bounded_field_names() -> Result<(), WindowValidationError> {
        let event = WindowMetadataEvent {
            window: reference(7, 1)?,
            model_revision: WindowModelRevision::new(8)?,
            changed: vec![WindowMetadataField::Title, WindowMetadataField::Title],
            metadata: WindowMetadata {
                title: None,
                visible_title: None,
                icon_title: None,
                class: None,
                client_machine: None,
                window_types: Vec::new(),
                states: Vec::new(),
                allowed_actions: Vec::new(),
                protocols: Vec::new(),
            },
        };
        assert_eq!(event.validate(), Err(WindowValidationError::ChangedFields));
        Ok(())
    }
}
