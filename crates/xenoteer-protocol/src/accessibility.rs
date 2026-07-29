//! Backend-independent accessibility identities, snapshots, queries, waits, and events.

#![allow(missing_docs)]

use core::fmt;
use std::collections::{BTreeSet, HashSet};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::geometry::StrictRect;
use crate::window::{StrictWindowRef, deserialize_strict_window_ref};
use crate::{CoordinateSpace, DesktopGeneration, DesktopId, Rect, WindowRef};

fn atspi_screen_coordinate_space_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["atspi_screen"]
    })
}

fn deserialize_strict_rect<'de, D>(deserializer: D) -> Result<Rect, D::Error>
where
    D: Deserializer<'de>,
{
    StrictRect::deserialize(deserializer).map(Into::into)
}

pub const ACCESSIBILITY_READ_GRANT: &str = "accessibility:read";
pub const ACCESSIBILITY_WRITE_GRANT: &str = "accessibility:write";

pub const ACCESSIBILITY_ELEMENT_CREATED_TOPIC: &str = "accessibility.element_created";
pub const ACCESSIBILITY_ELEMENT_CHANGED_TOPIC: &str = "accessibility.element_changed";
pub const ACCESSIBILITY_ELEMENT_REMOVED_TOPIC: &str = "accessibility.element_removed";
pub const ACCESSIBILITY_RESYNC_REQUIRED_TOPIC: &str = "accessibility.resync_required";

pub const MAX_ACCESSIBILITY_BUS_NAME_BYTES: usize = 255;
pub const MAX_ACCESSIBILITY_OBJECT_PATH_BYTES: usize = 4_096;
pub const ACCESSIBILITY_IDENTITY_HASH_BYTES: usize = 64;
pub const MAX_ACCESSIBILITY_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_ACCESSIBILITY_SHORT_TEXT_BYTES: usize = 4_096;
pub const MAX_ACCESSIBILITY_COLLECTION_ITEMS: usize = 256;
pub const MAX_ACCESSIBILITY_ACTIONS: usize = 128;
pub const MAX_ACCESSIBILITY_STATES: usize = 128;
pub const MAX_ACCESSIBILITY_INTERFACES: usize = 64;
pub const MAX_ACCESSIBILITY_WARNINGS: usize = 32;
pub const MAX_ACCESSIBILITY_ATTRIBUTES: usize = 256;
pub const MAX_ACCESSIBILITY_RELATIONS: usize = 128;
pub const MAX_ACCESSIBILITY_RELATION_TARGETS: usize = 128;
pub const MAX_ACCESSIBILITY_SELECTOR_PREDICATES: usize = 64;
pub const MAX_ACCESSIBILITY_REGEX_BYTES: usize = 1_024;
pub const MAX_ACCESSIBILITY_PAGE_LIMIT: u16 = 1_000;
pub const DEFAULT_ACCESSIBILITY_PAGE_LIMIT: u16 = 100;
pub const MAX_ACCESSIBILITY_QUERY_VISITED_NODES: u32 = 25_000;
pub const MAX_ACCESSIBILITY_SELECTOR_DEPTH: u16 = 64;
pub const MAX_ACCESSIBILITY_QUERY_MATCHES: u16 = 1_000;
pub const MAX_ACCESSIBILITY_QUERY_TIMEOUT_MS: u32 = 10_000;
pub const MAX_ACCESSIBILITY_PROXY_TIMEOUT_MS: u32 = 2_000;
pub const MAX_ACCESSIBILITY_SNAPSHOT_NODES: u32 = 10_000;
pub const MAX_ACCESSIBILITY_SNAPSHOT_BYTES: u32 = 16 * 1024 * 1024;
pub const MAX_ACCESSIBILITY_WAIT_TIMEOUT_MS: u32 = 120_000;
pub const MAX_ACCESSIBILITY_CURSOR_BYTES: usize = 1_024;
/// Maximum encoded size of one independently returned element snapshot.
pub const MAX_ACCESSIBILITY_ELEMENT_ENCODED_BYTES: usize = 1024 * 1024;
/// Maximum lifetime of an authenticated server-side accessibility page cursor.
pub const ACCESSIBILITY_CURSOR_TTL_MS: u32 = 30_000;
/// Maximum live accessibility cursors retained for one principal.
pub const MAX_ACCESSIBILITY_CURSORS_PER_PRINCIPAL: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
pub struct AtspiGeneration(u64);

impl AtspiGeneration {
    pub const fn new(value: u64) -> Result<Self, AccessibilityValidationError> {
        if value == 0 {
            return Err(AccessibilityValidationError::Generation);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for AtspiGeneration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        crate::wire_integer::nonzero::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for AtspiGeneration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(crate::wire_integer::nonzero::deserialize(deserializer)?)
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
pub struct AccessibilityRevision(u64);

impl AccessibilityRevision {
    pub const fn new(value: u64) -> Result<Self, AccessibilityValidationError> {
        if value == 0 {
            return Err(AccessibilityValidationError::Revision);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for AccessibilityRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        crate::wire_integer::nonzero::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for AccessibilityRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(crate::wire_integer::nonzero::deserialize(deserializer)?)
            .map_err(de::Error::custom)
    }
}

macro_rules! checked_string {
    ($name:ident, $schema_path:literal, $schema:ident, $max:expr, $validator:expr, $error:expr) => {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
        #[schemars(schema_with = $schema_path)]
        pub struct $name(String);

        fn $schema(_: &mut SchemaGenerator) -> Schema {
            schemars::json_schema!({
                "type": "string",
                "minLength": 1,
                "maxLength": $max
            })
        }

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, AccessibilityValidationError> {
                let value = value.into();
                if value.is_empty() || value.len() > $max || !($validator)(&value) {
                    return Err($error);
                }
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
            }
        }
    };
}

checked_string!(
    AtspiBusName,
    "atspi_bus_name_schema",
    atspi_bus_name_schema,
    MAX_ACCESSIBILITY_BUS_NAME_BYTES,
    valid_atspi_bus_name,
    AccessibilityValidationError::BusName
);

checked_string!(
    AtspiObjectPath,
    "atspi_object_path_schema",
    atspi_object_path_schema,
    MAX_ACCESSIBILITY_OBJECT_PATH_BYTES,
    valid_atspi_object_path,
    AccessibilityValidationError::ObjectPath
);

fn valid_atspi_bus_name(value: &str) -> bool {
    value.strip_prefix(':').is_some_and(|suffix| {
        let mut elements = suffix.split('.');
        let first = elements.next();
        let second = elements.next();
        first.is_some_and(valid_bus_element)
            && second.is_some_and(valid_bus_element)
            && elements.all(valid_bus_element)
    })
}

fn valid_bus_element(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_atspi_object_path(value: &str) -> bool {
    value == "/"
        || value.strip_prefix('/').is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix.split('/').all(|element| {
                    !element.is_empty()
                        && element
                            .bytes()
                            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
                })
        })
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(schema_with = "accessibility_identity_hash_schema")]
pub struct AccessibilityIdentityHash(String);

fn accessibility_identity_hash_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": ACCESSIBILITY_IDENTITY_HASH_BYTES,
        "maxLength": ACCESSIBILITY_IDENTITY_HASH_BYTES,
        "pattern": "^[0-9a-f]{64}$"
    })
}

impl AccessibilityIdentityHash {
    pub fn new(value: impl Into<String>) -> Result<Self, AccessibilityValidationError> {
        let value = value.into();
        if value.len() != ACCESSIBILITY_IDENTITY_HASH_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(AccessibilityValidationError::IdentityHash);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessibilityIdentityHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("AccessibilityIdentityHash")
            .field(&self.0)
            .finish()
    }
}

impl Serialize for AccessibilityIdentityHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AccessibilityIdentityHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ApplicationRef {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub atspi_generation: AtspiGeneration,
    pub unique_bus_name: AtspiBusName,
    pub root_object_path: AtspiObjectPath,
    #[serde(with = "crate::wire_integer::nonzero")]
    #[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
    pub app_instance_generation: u64,
    pub identity_hash: AccessibilityIdentityHash,
}

impl ApplicationRef {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        if self.app_instance_generation == 0 {
            return Err(AccessibilityValidationError::Generation);
        }
        Ok(())
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "ApplicationRef")]
pub(crate) struct StrictApplicationRef {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    atspi_generation: AtspiGeneration,
    unique_bus_name: AtspiBusName,
    root_object_path: AtspiObjectPath,
    #[serde(with = "crate::wire_integer::nonzero")]
    #[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
    app_instance_generation: u64,
    identity_hash: AccessibilityIdentityHash,
}

impl From<StrictApplicationRef> for ApplicationRef {
    fn from(value: StrictApplicationRef) -> Self {
        Self {
            desktop_id: value.desktop_id,
            desktop_generation: value.desktop_generation,
            atspi_generation: value.atspi_generation,
            unique_bus_name: value.unique_bus_name,
            root_object_path: value.root_object_path,
            app_instance_generation: value.app_instance_generation,
            identity_hash: value.identity_hash,
        }
    }
}

pub(crate) fn deserialize_strict_application_ref<'de, D>(
    deserializer: D,
) -> Result<ApplicationRef, D::Error>
where
    D: Deserializer<'de>,
{
    StrictApplicationRef::deserialize(deserializer).map(Into::into)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ElementRef {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub atspi_generation: AtspiGeneration,
    pub application: ApplicationRef,
    pub object_path: AtspiObjectPath,
    pub object_identity_hash: AccessibilityIdentityHash,
    #[serde(with = "crate::wire_integer::nonzero")]
    #[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
    pub cache_sequence: u64,
}

impl ElementRef {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        self.application.validate()?;
        if self.cache_sequence == 0 {
            return Err(AccessibilityValidationError::CacheSequence);
        }
        if self.desktop_id != self.application.desktop_id
            || self.desktop_generation != self.application.desktop_generation
            || self.atspi_generation != self.application.atspi_generation
        {
            return Err(AccessibilityValidationError::ReferenceScope);
        }
        Ok(())
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "ElementRef")]
pub(crate) struct StrictElementRef {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    atspi_generation: AtspiGeneration,
    #[serde(deserialize_with = "deserialize_strict_application_ref")]
    #[schemars(with = "StrictApplicationRef")]
    application: ApplicationRef,
    object_path: AtspiObjectPath,
    object_identity_hash: AccessibilityIdentityHash,
    #[serde(with = "crate::wire_integer::nonzero")]
    #[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
    cache_sequence: u64,
}

impl From<StrictElementRef> for ElementRef {
    fn from(value: StrictElementRef) -> Self {
        Self {
            desktop_id: value.desktop_id,
            desktop_generation: value.desktop_generation,
            atspi_generation: value.atspi_generation,
            application: value.application,
            object_path: value.object_path,
            object_identity_hash: value.object_identity_hash,
            cache_sequence: value.cache_sequence,
        }
    }
}

pub(crate) fn deserialize_strict_element_ref<'de, D>(
    deserializer: D,
) -> Result<ElementRef, D::Error>
where
    D: Deserializer<'de>,
{
    StrictElementRef::deserialize(deserializer).map(Into::into)
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ElementRole {
    Unknown,
    Application,
    Window,
    Dialog,
    Alert,
    Document,
    Section,
    Panel,
    Toolbar,
    Menu,
    MenuItem,
    Tab,
    TabList,
    Button,
    CheckBox,
    RadioButton,
    ToggleButton,
    ComboBox,
    List,
    ListItem,
    Tree,
    TreeItem,
    Table,
    TableCell,
    Label,
    Text,
    Entry,
    PasswordText,
    SpinButton,
    Slider,
    ScrollBar,
    ProgressBar,
    Link,
    Image,
    Canvas,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ElementRoleSnapshot {
    pub role: ElementRole,
    pub raw_name: Option<String>,
    pub raw_numeric: Option<u32>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ElementState {
    Active,
    Busy,
    Checked,
    Defunct,
    Editable,
    Enabled,
    Expandable,
    Expanded,
    Focusable,
    Focused,
    Indeterminate,
    Modal,
    MultiSelectable,
    Opaque,
    Pressed,
    Protected,
    ReadOnly,
    Required,
    Selectable,
    Selected,
    Sensitive,
    Showing,
    SingleLine,
    Stale,
    Transient,
    Visible,
    Visited,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ElementInterface {
    Accessible,
    Action,
    Application,
    Collection,
    Component,
    Document,
    EditableText,
    Hypertext,
    Image,
    Selection,
    Table,
    TableCell,
    Text,
    Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ElementActionSnapshot {
    pub name: String,
    pub description: Option<String>,
    pub key_binding: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementValueSnapshot {
    pub current: f64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub increment: Option<f64>,
    pub text: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct AccessibleTextContent(String);

impl AccessibleTextContent {
    pub fn new(value: impl Into<String>) -> Result<Self, AccessibilityValidationError> {
        let value = value.into();
        if value.len() > MAX_ACCESSIBILITY_TEXT_BYTES {
            return Err(AccessibilityValidationError::Text);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessibleTextContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessibleTextContent(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for AccessibleTextContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ElementTextRange {
    #[schemars(range(min = 0))]
    pub start: i32,
    #[schemars(range(min = 0))]
    pub end: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ElementTextSnapshot {
    #[schemars(range(min = 0))]
    pub character_count: i32,
    #[schemars(range(min = -1))]
    pub caret_offset: i32,
    #[schemars(length(max = MAX_ACCESSIBILITY_COLLECTION_ITEMS))]
    pub selections: Vec<ElementTextRange>,
    pub content: Option<AccessibleTextContent>,
    pub content_truncated: bool,
    pub protected: bool,
}

impl ElementTextSnapshot {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        if self.character_count < 0
            || self.caret_offset < -1
            || (self.caret_offset >= 0 && self.caret_offset > self.character_count)
            || self.selections.len() > MAX_ACCESSIBILITY_COLLECTION_ITEMS
            || self.selections.iter().any(|range| {
                range.start < 0 || range.end < range.start || range.end > self.character_count
            })
        {
            return Err(AccessibilityValidationError::Text);
        }
        if self.protected && self.content.is_some() {
            return Err(AccessibilityValidationError::ProtectedTextExposed);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ElementComponentSnapshot {
    #[schemars(schema_with = "atspi_screen_coordinate_space_schema")]
    pub coordinate_space: CoordinateSpace,
    pub extents: Option<Rect>,
    pub layer: Option<String>,
    pub z_order: Option<i16>,
    pub alpha: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowCorrelationConfidence {
    None,
    Weak,
    Strong,
    ExactProcess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowCorrelationSignal {
    ProcessId,
    ManagedProcess,
    TopLevelExtents,
    Title,
    ToolkitIdentity,
    ClientLeader,
    FocusTransition,
    CreationProximity,
    ExplicitCallerReference,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowCorrelationEvidence {
    pub signal: WindowCorrelationSignal,
    pub matched: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ElementWindowCorrelation {
    pub window: Option<WindowRef>,
    pub confidence: WindowCorrelationConfidence,
    #[schemars(length(max = MAX_ACCESSIBILITY_COLLECTION_ITEMS))]
    pub evidence: Vec<WindowCorrelationEvidence>,
    pub conflicting_evidence: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementCompleteness {
    Complete,
    Partial,
    Truncated,
    Dirty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccessibilityWarning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ElementAttribute {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementRelationType {
    LabelFor,
    LabelledBy,
    ControllerFor,
    ControlledBy,
    MemberOf,
    NodeChildOf,
    FlowsTo,
    FlowsFrom,
    DescriptionFor,
    DescribedBy,
    EmbeddedBy,
    Embeds,
    PopupFor,
    ParentWindowOf,
    ErrorFor,
    ErrorMessage,
    Details,
    DetailsFor,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ElementRelation {
    pub relation: ElementRelationType,
    #[schemars(length(max = MAX_ACCESSIBILITY_RELATION_TARGETS))]
    pub targets: Vec<ElementRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementSnapshot {
    #[serde(rename = "ref")]
    pub element: ElementRef,
    pub parent: Option<ElementRef>,
    pub index_in_parent: Option<u32>,
    pub child_count: Option<u32>,
    pub role: ElementRoleSnapshot,
    pub name: Option<String>,
    pub description: Option<String>,
    pub accessible_id: Option<String>,
    pub locale: Option<String>,
    #[schemars(length(max = MAX_ACCESSIBILITY_STATES))]
    pub states: Vec<ElementState>,
    #[schemars(length(max = MAX_ACCESSIBILITY_INTERFACES))]
    pub interfaces: Vec<ElementInterface>,
    #[schemars(length(max = MAX_ACCESSIBILITY_ACTIONS))]
    pub actions: Vec<ElementActionSnapshot>,
    pub value: Option<ElementValueSnapshot>,
    pub text: Option<ElementTextSnapshot>,
    pub component: Option<ElementComponentSnapshot>,
    #[schemars(length(max = MAX_ACCESSIBILITY_ATTRIBUTES))]
    pub attributes: Vec<ElementAttribute>,
    #[schemars(length(max = MAX_ACCESSIBILITY_RELATIONS))]
    pub relations: Vec<ElementRelation>,
    pub window_correlation: ElementWindowCorrelation,
    pub revision: AccessibilityRevision,
    pub completeness: ElementCompleteness,
    pub truncated: bool,
    #[schemars(length(max = MAX_ACCESSIBILITY_WARNINGS))]
    pub warnings: Vec<AccessibilityWarning>,
}

impl ElementSnapshot {
    /// Whether text/value/attribute fields require protected-data handling.
    /// Password roles are protected even when a toolkit omits its state flag.
    #[must_use]
    pub fn is_protected(&self) -> bool {
        self.role.role == ElementRole::PasswordText
            || self.states.contains(&ElementState::Protected)
            || self.text.as_ref().is_some_and(|text| text.protected)
    }

    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        self.element.validate()?;
        if let Some(parent) = &self.parent {
            parent.validate()?;
            validate_same_scope(&self.element, parent)?;
        }
        validate_short_optional(&self.role.raw_name)?;
        validate_short_optional(&self.name)?;
        validate_short_optional(&self.description)?;
        validate_short_optional(&self.accessible_id)?;
        validate_short_optional(&self.locale)?;
        validate_collection(self.states.len(), MAX_ACCESSIBILITY_STATES)?;
        validate_collection(self.interfaces.len(), MAX_ACCESSIBILITY_INTERFACES)?;
        validate_collection(self.actions.len(), MAX_ACCESSIBILITY_ACTIONS)?;
        validate_collection(self.attributes.len(), MAX_ACCESSIBILITY_ATTRIBUTES)?;
        validate_collection(self.relations.len(), MAX_ACCESSIBILITY_RELATIONS)?;
        validate_collection(self.warnings.len(), MAX_ACCESSIBILITY_WARNINGS)?;
        validate_unique(&self.states)?;
        validate_unique(&self.interfaces)?;
        for action in &self.actions {
            validate_short(&action.name)?;
            validate_optional_bounded(&action.description, true)?;
            validate_optional_bounded(&action.key_binding, true)?;
        }
        if let Some(value) = &self.value {
            if !value.current.is_finite()
                || value.minimum.is_some_and(|value| !value.is_finite())
                || value.maximum.is_some_and(|value| !value.is_finite())
                || value.increment.is_some_and(|value| !value.is_finite())
                || value
                    .minimum
                    .zip(value.maximum)
                    .is_some_and(|(minimum, maximum)| minimum > maximum)
            {
                return Err(AccessibilityValidationError::NumericRange);
            }
            validate_optional_bounded(&value.text, true)?;
        }
        if let Some(text) = &self.text {
            text.validate()?;
        }
        if self.is_protected()
            && (self
                .text
                .as_ref()
                .is_some_and(|text| text.content.is_some())
                || self.value.is_some()
                || !self.attributes.is_empty())
        {
            return Err(AccessibilityValidationError::ProtectedTextExposed);
        }
        if let Some(component) = &self.component {
            if component.coordinate_space != CoordinateSpace::AtspiScreen
                || component
                    .extents
                    .is_some_and(|rect| rect.validate().is_err())
            {
                return Err(AccessibilityValidationError::Geometry);
            }
            validate_optional_bounded(&component.layer, true)?;
        }
        if self.window_correlation.evidence.len() > MAX_ACCESSIBILITY_COLLECTION_ITEMS {
            return Err(AccessibilityValidationError::Collection);
        }
        if (self.window_correlation.confidence == WindowCorrelationConfidence::None)
            != self.window_correlation.window.is_none()
        {
            return Err(AccessibilityValidationError::Correlation);
        }
        if let Some(window) = &self.window_correlation.window {
            window
                .validate_shape()
                .map_err(|_| AccessibilityValidationError::Correlation)?;
            if window.desktop_id != self.element.desktop_id
                || window.desktop_generation != self.element.desktop_generation
            {
                return Err(AccessibilityValidationError::Correlation);
            }
        }
        for evidence in &self.window_correlation.evidence {
            validate_optional_bounded(&evidence.detail, true)?;
        }
        for attribute in &self.attributes {
            validate_short(&attribute.name)?;
            validate_bounded(&attribute.value, true)?;
        }
        for relation in &self.relations {
            validate_collection(relation.targets.len(), MAX_ACCESSIBILITY_RELATION_TARGETS)?;
            for target in &relation.targets {
                target.validate()?;
                if target.desktop_id != self.element.desktop_id
                    || target.desktop_generation != self.element.desktop_generation
                    || target.atspi_generation != self.element.atspi_generation
                {
                    return Err(AccessibilityValidationError::ReferenceScope);
                }
            }
        }
        validate_warnings(&self.warnings)?;
        if self.completeness == ElementCompleteness::Truncated && !self.truncated {
            return Err(AccessibilityValidationError::Completeness);
        }
        validate_aggregate_encoding(self, MAX_ACCESSIBILITY_ELEMENT_ENCODED_BYTES)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementOrder {
    Preorder,
    ReversePreorder,
    NameAscending,
    NameDescending,
    RoleThenName,
    ObjectPathAscending,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementStringMatch {
    Exact {
        value: String,
        case_sensitive: bool,
    },
    Contains {
        value: String,
        case_sensitive: bool,
    },
    Prefix {
        value: String,
        case_sensitive: bool,
    },
    Suffix {
        value: String,
        case_sensitive: bool,
    },
    Regex {
        pattern: String,
        case_sensitive: bool,
    },
}

impl fmt::Debug for ElementStringMatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, case_sensitive) = match self {
            Self::Exact { case_sensitive, .. } => ("exact", case_sensitive),
            Self::Contains { case_sensitive, .. } => ("contains", case_sensitive),
            Self::Prefix { case_sensitive, .. } => ("prefix", case_sensitive),
            Self::Suffix { case_sensitive, .. } => ("suffix", case_sensitive),
            Self::Regex { case_sensitive, .. } => ("regex", case_sensitive),
        };
        formatter
            .debug_struct("ElementStringMatch")
            .field("type", &kind)
            .field("value", &"<redacted>")
            .field("case_sensitive", case_sensitive)
            .finish()
    }
}

impl ElementStringMatch {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        let (value, maximum) = match self {
            Self::Exact { value, .. }
            | Self::Contains { value, .. }
            | Self::Prefix { value, .. }
            | Self::Suffix { value, .. } => (value, MAX_ACCESSIBILITY_SHORT_TEXT_BYTES),
            Self::Regex { pattern, .. } => (pattern, MAX_ACCESSIBILITY_REGEX_BYTES),
        };
        if value.is_empty() || value.len() > maximum || value.contains('\0') {
            return Err(AccessibilityValidationError::SelectorText);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementPredicate {
    Role {
        roles: Vec<ElementRole>,
    },
    Name {
        matcher: ElementStringMatch,
    },
    Description {
        matcher: ElementStringMatch,
    },
    /// Reserved for a future bounded on-demand metadata hydration contract.
    #[serde(skip)]
    #[schemars(skip)]
    AccessibleId {
        matcher: ElementStringMatch,
    },
    State {
        state: ElementState,
        value: bool,
    },
    Interface {
        interface: ElementInterface,
    },
    /// Reserved for a future bounded on-demand metadata hydration contract.
    #[serde(skip)]
    #[schemars(skip)]
    Attribute {
        name: String,
        matcher: ElementStringMatch,
    },
    /// Reserved for a future bounded on-demand metadata hydration contract.
    #[serde(skip)]
    #[schemars(skip)]
    Action {
        matcher: ElementStringMatch,
    },
    ValueRange {
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    IndexInParent {
        index: u32,
    },
    ChildCount {
        minimum: Option<u32>,
        maximum: Option<u32>,
    },
    /// Reserved for a future bounded relation-target hydration contract.
    #[serde(skip)]
    #[schemars(skip)]
    Relation {
        relation: ElementRelationType,
        #[serde(deserialize_with = "deserialize_strict_element_ref")]
        #[schemars(with = "StrictElementRef")]
        target: ElementRef,
    },
    ComponentIntersects {
        #[schemars(schema_with = "atspi_screen_coordinate_space_schema")]
        coordinate_space: CoordinateSpace,
        #[serde(deserialize_with = "deserialize_strict_rect")]
        #[schemars(with = "StrictRect")]
        rect: Rect,
    },
}

impl ElementPredicate {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        match self {
            Self::Role { roles }
                if roles.is_empty() || roles.len() > MAX_ACCESSIBILITY_COLLECTION_ITEMS =>
            {
                Err(AccessibilityValidationError::Collection)
            }
            Self::Name { matcher } | Self::Description { matcher } => matcher.validate(),
            Self::AccessibleId { .. }
            | Self::Attribute { .. }
            | Self::Action { .. }
            | Self::Relation { .. } => Err(AccessibilityValidationError::UnsupportedFeature),
            Self::ValueRange { minimum, maximum }
                if minimum.is_some_and(|value| !value.is_finite())
                    || maximum.is_some_and(|value| !value.is_finite())
                    || minimum
                        .zip(*maximum)
                        .is_some_and(|(minimum, maximum)| minimum > maximum) =>
            {
                Err(AccessibilityValidationError::NumericRange)
            }
            Self::ChildCount { minimum, maximum }
                if minimum
                    .zip(*maximum)
                    .is_some_and(|(minimum, maximum)| minimum > maximum) =>
            {
                Err(AccessibilityValidationError::NumericRange)
            }
            Self::ComponentIntersects {
                coordinate_space,
                rect,
            } => {
                if *coordinate_space != CoordinateSpace::AtspiScreen {
                    return Err(AccessibilityValidationError::Geometry);
                }
                rect.validate()
                    .map_err(|_| AccessibilityValidationError::Geometry)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementScope {
    Desktop,
    Application {
        #[serde(deserialize_with = "deserialize_strict_application_ref")]
        #[schemars(with = "StrictApplicationRef")]
        application: ApplicationRef,
    },
    Window {
        #[serde(deserialize_with = "deserialize_strict_window_ref")]
        #[schemars(with = "StrictWindowRef")]
        window: WindowRef,
    },
    Subtree {
        #[serde(deserialize_with = "deserialize_strict_element_ref")]
        #[schemars(with = "StrictElementRef")]
        root: ElementRef,
        include_root: bool,
    },
    Children {
        #[serde(deserialize_with = "deserialize_strict_element_ref")]
        #[schemars(with = "StrictElementRef")]
        parent: ElementRef,
    },
}

impl ElementScope {
    pub fn validate_for(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), AccessibilityValidationError> {
        match self {
            Self::Desktop => Ok(()),
            Self::Application { application } => {
                application.validate()?;
                validate_application_scope(desktop_id, desktop_generation, application)
            }
            Self::Window { window } => {
                window
                    .validate_shape()
                    .map_err(|_| AccessibilityValidationError::ReferenceScope)?;
                if window.desktop_id != desktop_id
                    || window.desktop_generation != desktop_generation
                {
                    return Err(AccessibilityValidationError::ReferenceScope);
                }
                Ok(())
            }
            Self::Subtree { root, .. } | Self::Children { parent: root } => {
                root.validate()?;
                validate_element_scope(desktop_id, desktop_generation, root)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementSelector {
    pub scope: ElementScope,
    #[schemars(length(max = MAX_ACCESSIBILITY_SELECTOR_PREDICATES))]
    pub predicates: Vec<ElementPredicate>,
    pub order: ElementOrder,
    pub result_index: Option<u32>,
}

impl ElementSelector {
    pub fn validate_for(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), AccessibilityValidationError> {
        self.scope.validate_for(desktop_id, desktop_generation)?;
        if self.predicates.len() > MAX_ACCESSIBILITY_SELECTOR_PREDICATES {
            return Err(AccessibilityValidationError::SelectorPredicates);
        }
        for predicate in &self.predicates {
            predicate.validate()?;
            if let ElementPredicate::Relation { target, .. } = predicate {
                validate_element_scope(desktop_id, desktop_generation, target)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccessibilityQueryLimits {
    #[schemars(range(min = 1, max = MAX_ACCESSIBILITY_QUERY_VISITED_NODES))]
    pub max_visited_nodes: u32,
    #[schemars(range(min = 1, max = MAX_ACCESSIBILITY_SELECTOR_DEPTH))]
    pub max_depth: u16,
    #[schemars(range(min = 1, max = MAX_ACCESSIBILITY_QUERY_MATCHES))]
    pub max_matches: u16,
    #[schemars(range(min = 1, max = MAX_ACCESSIBILITY_QUERY_TIMEOUT_MS))]
    pub timeout_ms: u32,
}

impl Default for AccessibilityQueryLimits {
    fn default() -> Self {
        Self {
            max_visited_nodes: MAX_ACCESSIBILITY_QUERY_VISITED_NODES,
            max_depth: MAX_ACCESSIBILITY_SELECTOR_DEPTH,
            max_matches: MAX_ACCESSIBILITY_QUERY_MATCHES,
            timeout_ms: MAX_ACCESSIBILITY_QUERY_TIMEOUT_MS,
        }
    }
}

impl AccessibilityQueryLimits {
    pub fn validate(self) -> Result<(), AccessibilityValidationError> {
        if self.max_visited_nodes == 0
            || self.max_visited_nodes > MAX_ACCESSIBILITY_QUERY_VISITED_NODES
            || self.max_depth == 0
            || self.max_depth > MAX_ACCESSIBILITY_SELECTOR_DEPTH
            || self.max_matches == 0
            || self.max_matches > MAX_ACCESSIBILITY_QUERY_MATCHES
            || self.timeout_ms == 0
            || self.timeout_ms > MAX_ACCESSIBILITY_QUERY_TIMEOUT_MS
        {
            return Err(AccessibilityValidationError::QueryLimits);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementSnapshotExpansion {
    /// Reserved until bounded action hydration is available.
    #[serde(skip)]
    #[schemars(skip)]
    pub actions: bool,
    pub value: bool,
    pub text_metadata: bool,
    /// Reserved until authorized, bounded text reads are available.
    #[serde(skip)]
    #[schemars(skip)]
    pub text_content: bool,
    /// Reserved until bounded attribute hydration is available.
    #[serde(skip)]
    #[schemars(skip)]
    pub attributes: bool,
    /// Reserved until bounded relation hydration is available.
    #[serde(skip)]
    #[schemars(skip)]
    pub relations: bool,
    pub component: bool,
}

impl Default for ElementSnapshotExpansion {
    fn default() -> Self {
        Self {
            actions: false,
            value: false,
            text_metadata: false,
            text_content: false,
            attributes: false,
            relations: false,
            component: true,
        }
    }
}

impl ElementSnapshotExpansion {
    pub fn validate(self) -> Result<(), AccessibilityValidationError> {
        if self.actions || self.text_content || self.attributes || self.relations {
            return Err(AccessibilityValidationError::UnsupportedFeature);
        }
        Ok(())
    }
}

/// Opaque authenticated cursor into immutable actor-owned query state.
///
/// The server must bind each token to principal, desktop/AT-SPI generations,
/// snapshot revision, selector/scope, traversal order, and expansion. It must
/// expire state after [`ACCESSIBILITY_CURSOR_TTL_MS`] and enforce
/// [`MAX_ACCESSIBILITY_CURSORS_PER_PRINCIPAL`]; clients cannot construct or
/// extend cursor state from the token contents.
#[derive(Clone, PartialEq, Eq, JsonSchema)]
#[schemars(schema_with = "accessibility_page_cursor_schema")]
pub struct AccessibilityPageCursor(String);

fn accessibility_page_cursor_schema(_: &mut SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 16,
        "maxLength": MAX_ACCESSIBILITY_CURSOR_BYTES,
        "pattern": "^[A-Za-z0-9_-]+$"
    })
}

impl AccessibilityPageCursor {
    pub fn new(value: impl Into<String>) -> Result<Self, AccessibilityValidationError> {
        let value = value.into();
        if value.len() < 16
            || value.len() > MAX_ACCESSIBILITY_CURSOR_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(AccessibilityValidationError::Cursor);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessibilityPageCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessibilityPageCursor(<opaque>)")
    }
}

impl Serialize for AccessibilityPageCursor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AccessibilityPageCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

const fn default_page_limit() -> u16 {
    DEFAULT_ACCESSIBILITY_PAGE_LIMIT
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementListRequest {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub scope: ElementScope,
    pub order: ElementOrder,
    #[serde(default = "default_page_limit")]
    #[schemars(range(min = 1, max = MAX_ACCESSIBILITY_PAGE_LIMIT))]
    pub limit: u16,
    pub cursor: Option<AccessibilityPageCursor>,
    #[serde(default)]
    pub expansion: ElementSnapshotExpansion,
    #[serde(default)]
    pub limits: AccessibilityQueryLimits,
}

impl ElementListRequest {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        self.scope
            .validate_for(self.desktop_id, self.desktop_generation)?;
        validate_page_limit(self.limit)?;
        self.limits.validate()?;
        self.expansion.validate()?;
        if self.limit > self.limits.max_matches {
            return Err(AccessibilityValidationError::PageLimit);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementQueryRequest {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub selector: ElementSelector,
    #[serde(default = "default_page_limit")]
    #[schemars(range(min = 1, max = MAX_ACCESSIBILITY_PAGE_LIMIT))]
    pub limit: u16,
    pub cursor: Option<AccessibilityPageCursor>,
    #[serde(default)]
    pub expansion: ElementSnapshotExpansion,
    #[serde(default)]
    pub limits: AccessibilityQueryLimits,
}

impl ElementQueryRequest {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        self.selector
            .validate_for(self.desktop_id, self.desktop_generation)?;
        validate_page_limit(self.limit)?;
        self.limits.validate()?;
        self.expansion.validate()?;
        if self.limit > self.limits.max_matches
            || self
                .selector
                .result_index
                .is_some_and(|index| index >= u32::from(self.limits.max_matches))
        {
            return Err(AccessibilityValidationError::PageLimit);
        }
        Ok(())
    }
}

/// Resolves a selector only when the complete bounded evaluation has exactly
/// one match. `result_index` is forbidden because indexing would hide
/// ambiguity instead of proving exact-one resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementResolveRequest {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub selector: ElementSelector,
    #[serde(default)]
    pub expansion: ElementSnapshotExpansion,
    #[serde(default)]
    pub limits: AccessibilityQueryLimits,
}

impl ElementResolveRequest {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        self.selector
            .validate_for(self.desktop_id, self.desktop_generation)?;
        self.expansion.validate()?;
        self.limits.validate()?;
        if self.selector.result_index.is_some() || self.limits.max_matches < 2 {
            return Err(AccessibilityValidationError::ExactOnePolicy);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementSnapshotRequest {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    #[serde(deserialize_with = "deserialize_strict_element_ref")]
    #[schemars(with = "StrictElementRef")]
    pub element: ElementRef,
    #[serde(default)]
    pub expansion: ElementSnapshotExpansion,
}

impl ElementSnapshotRequest {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        self.element.validate()?;
        validate_element_scope(self.desktop_id, self.desktop_generation, &self.element)?;
        self.expansion.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementSnapshotEntry {
    pub snapshot: ElementSnapshot,
}

impl ElementSnapshotEntry {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        self.snapshot.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementListPage {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub atspi_generation: AtspiGeneration,
    pub snapshot_revision: AccessibilityRevision,
    pub order: ElementOrder,
    #[schemars(length(max = MAX_ACCESSIBILITY_PAGE_LIMIT))]
    pub elements: Vec<ElementSnapshotEntry>,
    pub next_cursor: Option<AccessibilityPageCursor>,
    pub visited_nodes: u32,
    pub truncated: bool,
    #[schemars(length(max = MAX_ACCESSIBILITY_WARNINGS))]
    pub warnings: Vec<AccessibilityWarning>,
}

impl ElementListPage {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        validate_collection(
            self.elements.len(),
            usize::from(MAX_ACCESSIBILITY_PAGE_LIMIT),
        )?;
        validate_collection(self.warnings.len(), MAX_ACCESSIBILITY_WARNINGS)?;
        if self.visited_nodes > MAX_ACCESSIBILITY_QUERY_VISITED_NODES {
            return Err(AccessibilityValidationError::QueryLimits);
        }
        validate_warnings(&self.warnings)?;
        let mut seen = HashSet::with_capacity(self.elements.len());
        for entry in &self.elements {
            entry.validate()?;
            let element = &entry.snapshot.element;
            validate_element_scope(self.desktop_id, self.desktop_generation, element)?;
            if element.atspi_generation != self.atspi_generation
                || entry.snapshot.revision > self.snapshot_revision
                || !seen.insert(element.clone())
            {
                return Err(AccessibilityValidationError::ResultShape);
            }
        }
        validate_aggregate_encoding(self, MAX_ACCESSIBILITY_SNAPSHOT_BYTES as usize)
    }
}

pub type ElementQueryPage = ElementListPage;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementSnapshotResult {
    pub snapshot_revision: AccessibilityRevision,
    pub element: ElementSnapshotEntry,
}

impl ElementSnapshotResult {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        self.element.validate()?;
        if self.element.snapshot.revision > self.snapshot_revision {
            return Err(AccessibilityValidationError::ResultShape);
        }
        Ok(())
    }
}

/// Atomic successful exact-one resolution at one actor-owned revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementResolveResult {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub atspi_generation: AtspiGeneration,
    pub snapshot_revision: AccessibilityRevision,
    pub element: ElementSnapshotEntry,
}

impl ElementResolveResult {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        self.element.validate()?;
        let element = &self.element.snapshot.element;
        validate_element_scope(self.desktop_id, self.desktop_generation, element)?;
        if element.atspi_generation != self.atspi_generation
            || self.element.snapshot.revision != self.snapshot_revision
        {
            return Err(AccessibilityValidationError::ResultShape);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementWaitQuantifier {
    Any,
    All,
    ExactlyOne,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementWaitTarget {
    Reference {
        #[serde(deserialize_with = "deserialize_strict_element_ref")]
        #[schemars(with = "StrictElementRef")]
        element: ElementRef,
    },
    Selector {
        selector: ElementSelector,
        quantifier: ElementWaitQuantifier,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementWaitPredicate {
    Exists,
    Gone,
    State {
        state: ElementState,
        value: bool,
    },
    Name {
        matcher: ElementStringMatch,
    },
    Value {
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    /// Reserved until authorized, bounded text reads are available.
    #[serde(skip)]
    #[schemars(skip)]
    Text {
        matcher: ElementStringMatch,
    },
    ChildCount {
        minimum: Option<u32>,
        maximum: Option<u32>,
    },
    Geometry {
        #[schemars(schema_with = "atspi_screen_coordinate_space_schema")]
        coordinate_space: CoordinateSpace,
        #[serde(deserialize_with = "deserialize_strict_rect")]
        #[schemars(with = "StrictRect")]
        intersects: Rect,
    },
    SelectorCount {
        minimum: u32,
        maximum: Option<u32>,
    },
}

impl ElementWaitPredicate {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        match self {
            Self::Name { matcher } => matcher.validate(),
            Self::Text { .. } => Err(AccessibilityValidationError::UnsupportedFeature),
            Self::Value { minimum, maximum }
                if minimum.is_some_and(|value| !value.is_finite())
                    || maximum.is_some_and(|value| !value.is_finite())
                    || minimum
                        .zip(*maximum)
                        .is_some_and(|(minimum, maximum)| minimum > maximum) =>
            {
                Err(AccessibilityValidationError::NumericRange)
            }
            Self::ChildCount { minimum, maximum }
                if minimum
                    .zip(*maximum)
                    .is_some_and(|(minimum, maximum)| minimum > maximum) =>
            {
                Err(AccessibilityValidationError::NumericRange)
            }
            Self::Geometry {
                coordinate_space,
                intersects,
            } => {
                if *coordinate_space != CoordinateSpace::AtspiScreen {
                    return Err(AccessibilityValidationError::Geometry);
                }
                intersects
                    .validate()
                    .map_err(|_| AccessibilityValidationError::Geometry)
            }
            Self::SelectorCount { minimum, maximum }
                if maximum.is_some_and(|maximum| *minimum > maximum) =>
            {
                Err(AccessibilityValidationError::NumericRange)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementWaitRequest {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub target: ElementWaitTarget,
    pub predicate: ElementWaitPredicate,
    pub after_revision: Option<AccessibilityRevision>,
    #[schemars(range(min = 1, max = MAX_ACCESSIBILITY_WAIT_TIMEOUT_MS))]
    pub timeout_ms: u32,
    pub allow_poll_fallback: bool,
    #[serde(default)]
    pub expansion: ElementSnapshotExpansion,
    #[serde(default)]
    pub limits: AccessibilityQueryLimits,
}

impl ElementWaitRequest {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        if self.timeout_ms == 0 || self.timeout_ms > MAX_ACCESSIBILITY_WAIT_TIMEOUT_MS {
            return Err(AccessibilityValidationError::WaitTimeout);
        }
        match &self.target {
            ElementWaitTarget::Reference { element } => {
                element.validate()?;
                validate_element_scope(self.desktop_id, self.desktop_generation, element)?;
            }
            ElementWaitTarget::Selector { selector, .. } => {
                selector.validate_for(self.desktop_id, self.desktop_generation)?;
            }
        }
        self.predicate.validate()?;
        self.limits.validate()?;
        self.expansion.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementWaitStatus {
    Matched,
    TimedOut,
    ResyncRequired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ElementWaitResult {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub atspi_generation: AtspiGeneration,
    pub status: ElementWaitStatus,
    pub evaluated_revision: AccessibilityRevision,
    pub predicate_satisfied: bool,
    pub matched_count: u32,
    #[schemars(length(max = MAX_ACCESSIBILITY_PAGE_LIMIT))]
    pub elements: Vec<ElementSnapshotEntry>,
    pub poll_fallback_used: bool,
    pub truncated: bool,
    #[schemars(length(max = MAX_ACCESSIBILITY_WARNINGS))]
    pub warnings: Vec<AccessibilityWarning>,
}

impl ElementWaitResult {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        validate_collection(
            self.elements.len(),
            usize::from(MAX_ACCESSIBILITY_PAGE_LIMIT),
        )?;
        validate_collection(self.warnings.len(), MAX_ACCESSIBILITY_WARNINGS)?;
        validate_warnings(&self.warnings)?;
        let status_matches = match self.status {
            ElementWaitStatus::Matched => self.predicate_satisfied,
            ElementWaitStatus::TimedOut | ElementWaitStatus::ResyncRequired => {
                !self.predicate_satisfied
            }
        };
        if !status_matches || self.matched_count < self.elements.len() as u32 {
            return Err(AccessibilityValidationError::ResultShape);
        }
        let mut seen = HashSet::with_capacity(self.elements.len());
        for entry in &self.elements {
            entry.validate()?;
            let element = &entry.snapshot.element;
            validate_element_scope(self.desktop_id, self.desktop_generation, element)?;
            if element.atspi_generation != self.atspi_generation
                || entry.snapshot.revision > self.evaluated_revision
                || !seen.insert(element.clone())
            {
                return Err(AccessibilityValidationError::ResultShape);
            }
        }
        validate_aggregate_encoding(self, MAX_ACCESSIBILITY_SNAPSHOT_BYTES as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccessibilityEventKind {
    ElementCreated,
    ElementRemoved,
    StateChanged,
    PropertyChanged,
    ChildrenChanged,
    ActiveDescendantChanged,
    FocusChanged,
    WindowActivated,
    WindowDeactivated,
    WindowCreated,
    WindowDestroyed,
    TextInserted,
    TextDeleted,
    TextCaretMoved,
    TextSelectionChanged,
    ValueChanged,
    SelectionChanged,
    BoundsChanged,
    VisibleDataChanged,
    CacheAdded,
    CacheRemoved,
    ResyncRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccessibilityTextEventDetail {
    #[schemars(range(min = 0))]
    pub start: u32,
    pub length: u32,
    pub content: Option<AccessibleTextContent>,
    pub redacted: bool,
}

impl AccessibilityTextEventDetail {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        if self.redacted && self.content.is_some() {
            return Err(AccessibilityValidationError::ProtectedTextExposed);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AccessibilityEventDetail {
    pub property: Option<String>,
    pub state: Option<ElementState>,
    pub enabled: Option<bool>,
    pub child: Option<ElementRef>,
    pub text: Option<AccessibilityTextEventDetail>,
    pub value: Option<f64>,
    pub bounds: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccessibilityResyncReason {
    ActorSignal,
    GenerationChanged,
    EventGap,
    EventQueueOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AccessibilityRawSource {
    pub bus_name: AtspiBusName,
    pub object_path: AtspiObjectPath,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AccessibilityEvent {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub atspi_generation: AtspiGeneration,
    pub source: Option<ElementRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_source: Option<AccessibilityRawSource>,
    pub kind: AccessibilityEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resync_reason: Option<AccessibilityResyncReason>,
    pub detail: AccessibilityEventDetail,
    pub revision: AccessibilityRevision,
    #[serde(with = "crate::wire_integer::nonzero")]
    #[schemars(schema_with = "crate::wire_integer::nonzero_schema")]
    pub cache_sequence: u64,
    pub source_stale: bool,
}

impl AccessibilityEvent {
    pub fn validate(&self) -> Result<(), AccessibilityValidationError> {
        validate_desktop_scope(self.desktop_id, self.desktop_generation)?;
        if self.cache_sequence == 0 {
            return Err(AccessibilityValidationError::CacheSequence);
        }
        if let Some(source) = &self.source {
            source.validate()?;
            validate_element_scope(self.desktop_id, self.desktop_generation, source)?;
            if source.atspi_generation != self.atspi_generation {
                return Err(AccessibilityValidationError::ReferenceScope);
            }
            let Some(raw_source) = &self.raw_source else {
                return Err(AccessibilityValidationError::EventSource);
            };
            if source.application.unique_bus_name != raw_source.bus_name
                || source.object_path != raw_source.object_path
            {
                return Err(AccessibilityValidationError::EventSource);
            }
        }
        if self.raw_source.is_none()
            && (self.source.is_some() || self.kind != AccessibilityEventKind::ResyncRequired)
        {
            return Err(AccessibilityValidationError::EventSource);
        }
        if (self.kind == AccessibilityEventKind::ResyncRequired) != self.resync_reason.is_some() {
            return Err(AccessibilityValidationError::EventSource);
        }
        let source_resolution_failed = self.source.is_none() && self.raw_source.is_some();
        if self.source_stale != source_resolution_failed {
            return Err(AccessibilityValidationError::EventSource);
        }
        if let Some(text) = &self.detail.text {
            text.validate()?;
        }
        validate_optional_bounded(&self.detail.property, true)?;
        if self.detail.value.is_some_and(|value| !value.is_finite()) {
            return Err(AccessibilityValidationError::NumericRange);
        }
        if self
            .detail
            .bounds
            .is_some_and(|bounds| bounds.validate().is_err())
        {
            return Err(AccessibilityValidationError::Geometry);
        }
        if let Some(child) = &self.detail.child {
            child.validate()?;
            validate_element_scope(self.desktop_id, self.desktop_generation, child)?;
            if child.atspi_generation != self.atspi_generation {
                return Err(AccessibilityValidationError::ReferenceScope);
            }
        }
        validate_aggregate_encoding(self, MAX_ACCESSIBILITY_ELEMENT_ENCODED_BYTES)
    }
}

fn validate_desktop_scope(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<(), AccessibilityValidationError> {
    if desktop_id.as_uuid().is_nil() || desktop_generation.as_uuid().is_nil() {
        return Err(AccessibilityValidationError::NilIdentifier);
    }
    Ok(())
}

fn validate_application_scope(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    application: &ApplicationRef,
) -> Result<(), AccessibilityValidationError> {
    if application.desktop_id != desktop_id || application.desktop_generation != desktop_generation
    {
        return Err(AccessibilityValidationError::ReferenceScope);
    }
    Ok(())
}

pub(crate) fn validate_element_scope(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    element: &ElementRef,
) -> Result<(), AccessibilityValidationError> {
    if element.desktop_id != desktop_id || element.desktop_generation != desktop_generation {
        return Err(AccessibilityValidationError::ReferenceScope);
    }
    Ok(())
}

fn validate_same_scope(
    left: &ElementRef,
    right: &ElementRef,
) -> Result<(), AccessibilityValidationError> {
    if left.desktop_id != right.desktop_id
        || left.desktop_generation != right.desktop_generation
        || left.atspi_generation != right.atspi_generation
        || left.application != right.application
    {
        return Err(AccessibilityValidationError::ReferenceScope);
    }
    Ok(())
}

fn validate_short(value: &str) -> Result<(), AccessibilityValidationError> {
    if value.is_empty() || value.len() > MAX_ACCESSIBILITY_SHORT_TEXT_BYTES || value.contains('\0')
    {
        return Err(AccessibilityValidationError::Text);
    }
    Ok(())
}

fn validate_short_optional(value: &Option<String>) -> Result<(), AccessibilityValidationError> {
    if let Some(value) = value {
        validate_short(value)?;
    }
    Ok(())
}

fn validate_bounded(value: &str, allow_empty: bool) -> Result<(), AccessibilityValidationError> {
    if (!allow_empty && value.is_empty())
        || value.len() > MAX_ACCESSIBILITY_SHORT_TEXT_BYTES
        || value.contains('\0')
    {
        return Err(AccessibilityValidationError::Text);
    }
    Ok(())
}

fn validate_optional_bounded(
    value: &Option<String>,
    allow_empty: bool,
) -> Result<(), AccessibilityValidationError> {
    if let Some(value) = value {
        validate_bounded(value, allow_empty)?;
    }
    Ok(())
}

fn validate_warnings(
    warnings: &[AccessibilityWarning],
) -> Result<(), AccessibilityValidationError> {
    for warning in warnings {
        if warning.code.is_empty()
            || warning.code.len() > 64
            || !warning.code.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
        {
            return Err(AccessibilityValidationError::Warning);
        }
        validate_bounded(&warning.message, false)?;
    }
    Ok(())
}

fn validate_aggregate_encoding<T: Serialize>(
    value: &T,
    maximum: usize,
) -> Result<(), AccessibilityValidationError> {
    let encoded =
        serde_json::to_vec(value).map_err(|_| AccessibilityValidationError::ElementEncoding)?;
    if encoded.len() > maximum {
        return Err(AccessibilityValidationError::ElementEncoding);
    }
    Ok(())
}

fn validate_unique<T: Ord + Copy>(values: &[T]) -> Result<(), AccessibilityValidationError> {
    let mut seen = BTreeSet::new();
    if values.iter().any(|value| !seen.insert(*value)) {
        return Err(AccessibilityValidationError::Collection);
    }
    Ok(())
}

fn validate_collection(length: usize, maximum: usize) -> Result<(), AccessibilityValidationError> {
    if length > maximum {
        return Err(AccessibilityValidationError::Collection);
    }
    Ok(())
}

fn validate_page_limit(limit: u16) -> Result<(), AccessibilityValidationError> {
    if limit == 0 || limit > MAX_ACCESSIBILITY_PAGE_LIMIT {
        return Err(AccessibilityValidationError::PageLimit);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AccessibilityValidationError {
    #[error("desktop identifiers must be non-nil")]
    NilIdentifier,
    #[error("accessibility generation must be non-zero")]
    Generation,
    #[error("accessibility revision must be non-zero")]
    Revision,
    #[error("AT-SPI unique bus name is invalid")]
    BusName,
    #[error("AT-SPI object path is invalid")]
    ObjectPath,
    #[error("accessibility identity hash is invalid")]
    IdentityHash,
    #[error("accessibility cache sequence must be non-zero")]
    CacheSequence,
    #[error("accessibility reference belongs to another scope")]
    ReferenceScope,
    #[error("bounded accessibility collection is invalid")]
    Collection,
    #[error("accessibility text is invalid")]
    Text,
    #[error("protected accessibility text must be redacted")]
    ProtectedTextExposed,
    #[error("accessibility geometry is invalid")]
    Geometry,
    #[error("window correlation shape is invalid")]
    Correlation,
    #[error("selector text is invalid")]
    SelectorText,
    #[error("selector has too many predicates")]
    SelectorPredicates,
    #[error("numeric range is invalid")]
    NumericRange,
    #[error("query limits are outside protocol ceilings")]
    QueryLimits,
    #[error("page limit is outside protocol ceilings")]
    PageLimit,
    #[error("page cursor is invalid")]
    Cursor,
    #[error("wait timeout is outside protocol ceilings")]
    WaitTimeout,
    #[error("accessibility event source resolution is inconsistent")]
    EventSource,
    #[error("accessibility warning is invalid")]
    Warning,
    #[error("accessibility completeness flags are inconsistent")]
    Completeness,
    #[error("encoded element snapshot exceeds its protocol ceiling")]
    ElementEncoding,
    #[error("snapshot expansion is internally inconsistent")]
    Expansion,
    #[error("accessibility feature is reserved and not part of this protocol version")]
    UnsupportedFeature,
    #[error("exact-one resolution policy cannot prove uniqueness")]
    ExactOnePolicy,
    #[error("accessibility result shape is inconsistent")]
    ResultShape,
}
