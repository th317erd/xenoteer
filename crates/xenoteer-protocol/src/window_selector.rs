//! Bounded window listing, selector/query, pagination, and wait payloads.

#![allow(missing_docs)]

use core::fmt;

use regex_automata::{nfa::thompson::NFA, util::syntax};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

use crate::geometry::{StrictPoint, deserialize_strict_point};
use crate::process::{StrictProcessRef, deserialize_strict_process_ref};
use crate::{DesktopGeneration, DesktopId, Point, ProcessRef};

use crate::window::{
    StrictWindowRect, StrictWindowRef, WindowAtomName, WindowMapState, WindowModelRevision,
    WindowRect, WindowRef, WindowSnapshot, WindowValidationError, deserialize_strict_window_rect,
    deserialize_strict_window_ref,
};

/// Default maximum returned by a window list/query when the client omits policy.
pub const DEFAULT_WINDOW_PAGE_LIMIT: u16 = 50;
/// Maximum snapshots in one window list/query/wait result.
pub const MAX_WINDOW_PAGE_LIMIT: u16 = 200;
/// Maximum UTF-8 bytes in a selector literal.
pub const MAX_WINDOW_SELECTOR_TEXT_BYTES: usize = 4_096;
/// Tighter ceiling for one untrusted regular-expression pattern.
pub const MAX_WINDOW_REGEX_BYTES: usize = 1_024;
/// Maximum approximate heap bytes in one admitted compiled Thompson NFA.
pub const MAX_WINDOW_REGEX_NFA_BYTES: usize = 256 * 1_024;
/// Maximum recursive selector nesting, counting the root as depth one.
pub const MAX_WINDOW_SELECTOR_DEPTH: usize = 8;
/// Maximum leaf predicates in one selector.
pub const MAX_WINDOW_SELECTOR_PREDICATES: usize = 64;
/// Maximum independently compiled regular expressions in one selector.
pub const MAX_WINDOW_SELECTOR_REGEXES: usize = 8;
/// Maximum total selector nodes, including composition nodes.
pub const MAX_WINDOW_SELECTOR_NODES: usize = 128;
/// Maximum URL-safe bytes in a server-authenticated cursor/reference token.
pub const MAX_WINDOW_OPAQUE_TOKEN_BYTES: usize = 1_024;
/// Minimum entropy-bearing token representation accepted from clients.
pub const MIN_WINDOW_OPAQUE_TOKEN_BYTES: usize = 16;
/// Protocol ceiling for a server-side observation wait.
pub const MAX_WINDOW_WAIT_TIMEOUT_MS: u32 = 300_000;
/// Maximum count usable in a bounded selector-count predicate.
pub const MAX_WINDOW_WAIT_COUNT: u32 = 10_000;

macro_rules! opaque_window_token {
    ($name:ident, $schema:ident, $schema_path:literal, $docs:literal) => {
        #[doc = $docs]
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
        #[schemars(schema_with = $schema_path)]
        pub struct $name(String);

        fn $schema(_: &mut SchemaGenerator) -> Schema {
            schemars::json_schema!({
                "type": "string",
                "minLength": MIN_WINDOW_OPAQUE_TOKEN_BYTES,
                "maxLength": MAX_WINDOW_OPAQUE_TOKEN_BYTES,
                "pattern": "^[A-Za-z0-9_-]+$"
            })
        }

        impl $name {
            /// Creates a bounded URL-safe opaque value.
            pub fn new(value: impl Into<String>) -> Result<Self, WindowQueryValidationError> {
                let value = value.into();
                if value.len() < MIN_WINDOW_OPAQUE_TOKEN_BYTES
                    || value.len() > MAX_WINDOW_OPAQUE_TOKEN_BYTES
                    || !value
                        .as_bytes()
                        .iter()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                {
                    return Err(WindowQueryValidationError::OpaqueToken);
                }
                Ok(Self(value))
            }

            /// Returns the opaque URL-safe representation.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&"<opaque>")
                    .finish()
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

opaque_window_token!(
    WindowPageCursor,
    window_page_cursor_schema,
    "window_page_cursor_schema",
    "Opaque, short-lived cursor bound to principal, generation, revision, query, and order."
);
opaque_window_token!(
    WindowReferenceToken,
    window_reference_token_schema,
    "window_reference_token_schema",
    "Opaque URL-safe transport form of a generation-bound window reference."
);

/// Stable ordering applied before limit/cursor projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowOrder {
    /// First-observed revision, then exact window-birth identity.
    CreationAscending,
    /// Reverse first-observed revision, then exact window-birth identity.
    CreationDescending,
    /// Observed stacking index with unlisted windows last, then birth identity.
    StackingBottomToTop,
    /// Reverse observed stacking index with unlisted windows last, then birth identity.
    StackingTopToBottom,
    /// Unicode scalar-value title order; missing titles last, then birth identity.
    TitleAscending,
    /// Reverse Unicode scalar-value title order; missing titles last, then birth identity.
    TitleDescending,
    /// Numeric XID, then observed generation and identity hash.
    XidAscending,
}

const fn default_window_page_limit() -> u16 {
    DEFAULT_WINDOW_PAGE_LIMIT
}

/// Bounded list request authorized by `desktop:observe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowListRequest {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    #[serde(default = "default_window_page_limit")]
    #[schemars(range(min = 1, max = MAX_WINDOW_PAGE_LIMIT))]
    pub limit: u16,
    pub order: WindowOrder,
    pub cursor: Option<WindowPageCursor>,
}

impl WindowListRequest {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        validate_scope(self.desktop_id, self.desktop_generation)?;
        validate_limit(self.limit)
    }
}

/// One observed window paired with its server-issued lookup authority.
///
/// The token is opaque and must be authenticated by the server against the
/// principal, desktop generation, exact window birth, and issuance policy.
/// Embedding it beside the snapshot makes every discovery result directly
/// usable with the token-based window resource without parallel arrays or
/// client-side token construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowSnapshotEntry {
    /// Complete bounded observation of one exact XID birth.
    pub snapshot: WindowSnapshot,
    /// Server-authenticated URL-safe token for subsequent exact lookup.
    pub reference_token: WindowReferenceToken,
}

impl WindowSnapshotEntry {
    /// Revalidates the public snapshot shape.
    ///
    /// Token authenticity and claim binding remain server responsibilities;
    /// the token wrapper itself validates syntax during construction/decode.
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        self.snapshot.validate().map_err(Into::into)
    }
}

/// One consistent page from an authoritative window model revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowListPage {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub snapshot_revision: WindowModelRevision,
    #[schemars(length(max = MAX_WINDOW_PAGE_LIMIT))]
    pub windows: Vec<WindowSnapshotEntry>,
    pub next_cursor: Option<WindowPageCursor>,
}

impl WindowListPage {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        validate_scope(self.desktop_id, self.desktop_generation)?;
        validate_snapshot_page(
            self.desktop_id,
            self.desktop_generation,
            self.snapshot_revision,
            &self.windows,
        )
    }
}

/// Full-reference or URL-token lookup target for one window snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WindowSnapshotTarget {
    Reference {
        #[serde(deserialize_with = "deserialize_strict_window_ref")]
        #[schemars(with = "StrictWindowRef")]
        window: WindowRef,
    },
    Token {
        token: WindowReferenceToken,
    },
}

/// Read-only exact-window lookup authorized by `desktop:observe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowSnapshotRequest {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub target: WindowSnapshotTarget,
}

impl WindowSnapshotRequest {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        validate_scope(self.desktop_id, self.desktop_generation)?;
        if let WindowSnapshotTarget::Reference { window } = &self.target {
            window.validate_shape()?;
            validate_reference_scope(self.desktop_id, self.desktop_generation, window)?;
        }
        Ok(())
    }
}

/// Exact snapshot plus the atomic model revision at which it was resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowSnapshotResult {
    pub snapshot_revision: WindowModelRevision,
    pub window: WindowSnapshotEntry,
}

impl WindowSnapshotResult {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        self.window.validate()?;
        if self.window.snapshot.model_revision != self.snapshot_revision {
            return Err(WindowQueryValidationError::SnapshotRevision);
        }
        Ok(())
    }
}

/// Observed text field selected by a string matcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowTextField {
    Title,
    VisibleTitle,
    IconTitle,
    ClassInstance,
    Class,
    ClientMachine,
}

/// Bounded locale-independent match expression.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WindowStringMatch {
    Exact {
        #[schemars(length(min = 1, max = MAX_WINDOW_SELECTOR_TEXT_BYTES))]
        value: String,
        case_sensitive: bool,
    },
    Contains {
        #[schemars(length(min = 1, max = MAX_WINDOW_SELECTOR_TEXT_BYTES))]
        value: String,
        case_sensitive: bool,
    },
    Prefix {
        #[schemars(length(min = 1, max = MAX_WINDOW_SELECTOR_TEXT_BYTES))]
        value: String,
        case_sensitive: bool,
    },
    Suffix {
        #[schemars(length(min = 1, max = MAX_WINDOW_SELECTOR_TEXT_BYTES))]
        value: String,
        case_sensitive: bool,
    },
    /// Pattern compiled only by the bounded linear-time selector backend.
    Regex {
        #[schemars(length(min = 1, max = MAX_WINDOW_REGEX_BYTES))]
        pattern: String,
        case_sensitive: bool,
    },
}

impl fmt::Debug for WindowStringMatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, case_sensitive) = match self {
            Self::Exact { case_sensitive, .. } => ("Exact", case_sensitive),
            Self::Contains { case_sensitive, .. } => ("Contains", case_sensitive),
            Self::Prefix { case_sensitive, .. } => ("Prefix", case_sensitive),
            Self::Suffix { case_sensitive, .. } => ("Suffix", case_sensitive),
            Self::Regex { case_sensitive, .. } => ("Regex", case_sensitive),
        };
        formatter
            .debug_struct(kind)
            .field("value", &"<redacted>")
            .field("case_sensitive", case_sensitive)
            .finish()
    }
}

impl WindowStringMatch {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        let (value, maximum) = match self {
            Self::Exact { value, .. }
            | Self::Contains { value, .. }
            | Self::Prefix { value, .. }
            | Self::Suffix { value, .. } => (value, MAX_WINDOW_SELECTOR_TEXT_BYTES),
            Self::Regex { pattern, .. } => (pattern, MAX_WINDOW_REGEX_BYTES),
        };
        if value.is_empty() || value.len() > maximum || value.contains('\0') {
            return Err(WindowQueryValidationError::StringMatcher);
        }
        if let Self::Regex {
            pattern,
            case_sensitive,
        } = self
        {
            let mut compiler = NFA::compiler();
            compiler
                .configure(NFA::config().nfa_size_limit(Some(MAX_WINDOW_REGEX_NFA_BYTES)))
                .syntax(
                    syntax::Config::new()
                        .utf8(true)
                        .unicode(true)
                        .case_insensitive(!case_sensitive),
                );
            compiler
                .build(pattern)
                .map_err(|_| WindowQueryValidationError::RegexSyntaxOrComplexity)?;
        }
        Ok(())
    }
}

/// One safe leaf predicate over the bounded window model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WindowPredicate {
    Text {
        field: WindowTextField,
        matcher: WindowStringMatch,
    },
    ManagedProcess {
        #[serde(deserialize_with = "deserialize_strict_process_ref")]
        #[schemars(with = "StrictProcessRef")]
        process: ProcessRef,
    },
    ReportedPid {
        #[schemars(range(min = 1))]
        pid: u32,
    },
    WindowType {
        value: WindowAtomName,
    },
    State {
        value: WindowAtomName,
        present: bool,
    },
    MapState {
        value: WindowMapState,
    },
    Workspace {
        workspace: u32,
    },
    Active {
        value: bool,
    },
    Focused {
        value: bool,
    },
    TransientFor {
        #[serde(deserialize_with = "deserialize_strict_window_ref")]
        #[schemars(with = "StrictWindowRef")]
        window: WindowRef,
    },
    GroupLeader {
        #[serde(deserialize_with = "deserialize_strict_window_ref")]
        #[schemars(with = "StrictWindowRef")]
        window: WindowRef,
    },
    HasAccessibilityApplication {
        value: bool,
    },
    CreatedAfter {
        model_revision: WindowModelRevision,
    },
}

impl WindowPredicate {
    fn validate_for_scope(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), WindowQueryValidationError> {
        match self {
            Self::Text { matcher, .. } => matcher.validate(),
            Self::ManagedProcess { process } => {
                process
                    .validate()
                    .map_err(|_| WindowQueryValidationError::ProcessReference)?;
                if process.desktop_generation != desktop_generation {
                    return Err(WindowQueryValidationError::ReferenceScope);
                }
                Ok(())
            }
            Self::ReportedPid { pid: 0 } => Err(WindowQueryValidationError::ReportedPid),
            Self::TransientFor { window } | Self::GroupLeader { window } => {
                window.validate_shape()?;
                validate_reference_scope(desktop_id, desktop_generation, window)
            }
            Self::ReportedPid { .. }
            | Self::WindowType { .. }
            | Self::State { .. }
            | Self::MapState { .. }
            | Self::Workspace { .. }
            | Self::Active { .. }
            | Self::Focused { .. }
            | Self::HasAccessibilityApplication { .. }
            | Self::CreatedAfter { .. } => Ok(()),
        }
    }
}

/// Recursively composed selector with explicit depth/node/predicate budgets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WindowSelector {
    Predicate {
        predicate: WindowPredicate,
    },
    All {
        #[schemars(length(min = 1, max = MAX_WINDOW_SELECTOR_NODES))]
        selectors: Vec<WindowSelector>,
    },
    Any {
        #[schemars(length(min = 1, max = MAX_WINDOW_SELECTOR_NODES))]
        selectors: Vec<WindowSelector>,
    },
    Not {
        selector: Box<WindowSelector>,
    },
}

impl WindowSelector {
    /// Validates recursive budgets and every scope-bearing predicate.
    pub fn validate_for_scope(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), WindowQueryValidationError> {
        validate_scope(desktop_id, desktop_generation)?;
        let mut budget = SelectorBudget::default();
        self.validate_node(desktop_id, desktop_generation, 1, &mut budget)
    }

    fn validate_node(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        depth: usize,
        budget: &mut SelectorBudget,
    ) -> Result<(), WindowQueryValidationError> {
        if depth > MAX_WINDOW_SELECTOR_DEPTH {
            return Err(WindowQueryValidationError::SelectorDepth);
        }
        budget.nodes += 1;
        if budget.nodes > MAX_WINDOW_SELECTOR_NODES {
            return Err(WindowQueryValidationError::SelectorNodes);
        }
        match self {
            Self::Predicate { predicate } => {
                budget.predicates += 1;
                if budget.predicates > MAX_WINDOW_SELECTOR_PREDICATES {
                    return Err(WindowQueryValidationError::SelectorPredicates);
                }
                if matches!(
                    predicate,
                    WindowPredicate::Text {
                        matcher: WindowStringMatch::Regex { .. },
                        ..
                    }
                ) {
                    budget.regexes += 1;
                    if budget.regexes > MAX_WINDOW_SELECTOR_REGEXES {
                        return Err(WindowQueryValidationError::SelectorRegexes);
                    }
                }
                predicate.validate_for_scope(desktop_id, desktop_generation)
            }
            Self::All { selectors } | Self::Any { selectors } => {
                if selectors.is_empty() {
                    return Err(WindowQueryValidationError::EmptyComposition);
                }
                for selector in selectors {
                    selector.validate_node(desktop_id, desktop_generation, depth + 1, budget)?;
                }
                Ok(())
            }
            Self::Not { selector } => {
                selector.validate_node(desktop_id, desktop_generation, depth + 1, budget)
            }
        }
    }
}

#[derive(Default)]
struct SelectorBudget {
    nodes: usize,
    predicates: usize,
    regexes: usize,
}

/// Read-only selector query authorized by `desktop:observe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowQueryRequest {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub selector: WindowSelector,
    pub order: WindowOrder,
    #[serde(default = "default_window_page_limit")]
    #[schemars(range(min = 1, max = MAX_WINDOW_PAGE_LIMIT))]
    pub limit: u16,
    pub cursor: Option<WindowPageCursor>,
}

impl WindowQueryRequest {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        validate_scope(self.desktop_id, self.desktop_generation)?;
        validate_limit(self.limit)?;
        self.selector
            .validate_for_scope(self.desktop_id, self.desktop_generation)
    }
}

/// One consistent page from a bounded selector query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowQueryPage {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub snapshot_revision: WindowModelRevision,
    #[schemars(length(max = MAX_WINDOW_PAGE_LIMIT))]
    pub windows: Vec<WindowSnapshotEntry>,
    pub next_cursor: Option<WindowPageCursor>,
}

impl WindowQueryPage {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        validate_scope(self.desktop_id, self.desktop_generation)?;
        validate_snapshot_page(
            self.desktop_id,
            self.desktop_generation,
            self.snapshot_revision,
            &self.windows,
        )
    }
}

/// Explicit single-target selector policy; ambiguity never silently picks a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowSingleMatchPolicy {
    ExactlyOne,
    /// Deterministic first result under the request's explicit `order`.
    First,
}

/// Safely resolves a selector to one stable reference before a later command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowResolveRequest {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub selector: WindowSelector,
    pub order: WindowOrder,
    pub match_policy: WindowSingleMatchPolicy,
}

impl WindowResolveRequest {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        self.selector
            .validate_for_scope(self.desktop_id, self.desktop_generation)
    }
}

/// Atomic successful resolution of a selector to one exact window birth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowResolveResult {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub snapshot_revision: WindowModelRevision,
    pub window: WindowSnapshotEntry,
}

impl WindowResolveResult {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        validate_scope(self.desktop_id, self.desktop_generation)?;
        self.window.validate()?;
        validate_reference_scope(
            self.desktop_id,
            self.desktop_generation,
            &self.window.snapshot.window,
        )?;
        if self.window.snapshot.model_revision != self.snapshot_revision {
            return Err(WindowQueryValidationError::SnapshotRevision);
        }
        Ok(())
    }
}

/// Stable geometry projection selected by a wait predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowGeometryArea {
    Client,
    Frame,
    Content,
}

/// Bounded geometry relation evaluated against root-physical model data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WindowGeometryPredicate {
    Equals {
        area: WindowGeometryArea,
        #[serde(deserialize_with = "deserialize_strict_window_rect")]
        #[schemars(with = "StrictWindowRect")]
        expected: WindowRect,
    },
    Intersects {
        area: WindowGeometryArea,
        #[serde(deserialize_with = "deserialize_strict_window_rect")]
        #[schemars(with = "StrictWindowRect")]
        region: WindowRect,
    },
    ContainedBy {
        area: WindowGeometryArea,
        #[serde(deserialize_with = "deserialize_strict_window_rect")]
        #[schemars(with = "StrictWindowRect")]
        bounds: WindowRect,
    },
    ContainsPoint {
        area: WindowGeometryArea,
        #[serde(deserialize_with = "deserialize_strict_point")]
        #[schemars(with = "StrictPoint")]
        point: Point,
    },
}

impl WindowGeometryPredicate {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        let rect = match self {
            Self::Equals { expected, .. } => Some(expected),
            Self::Intersects { region, .. } => Some(region),
            Self::ContainedBy { bounds, .. } => Some(bounds),
            Self::ContainsPoint { .. } => None,
        };
        if let Some(rect) = rect {
            rect.validate()?;
            if rect.coordinate_space != crate::CoordinateSpace::RootPhysical {
                return Err(WindowQueryValidationError::GeometryCoordinateSpace);
            }
        }
        Ok(())
    }
}

/// Closed comparison operation for selector-count waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowCountComparison {
    Exactly,
    AtLeast,
    AtMost,
}

/// Bounded selector-count predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowCountPredicate {
    pub comparison: WindowCountComparison,
    #[schemars(range(max = MAX_WINDOW_WAIT_COUNT))]
    pub count: u32,
}

impl WindowCountPredicate {
    pub fn validate(self) -> Result<(), WindowQueryValidationError> {
        if self.count > MAX_WINDOW_WAIT_COUNT {
            return Err(WindowQueryValidationError::WaitCount);
        }
        Ok(())
    }
}

/// Reference or selector whose state is evaluated atomically inside the actor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WindowWaitTarget {
    Reference {
        #[serde(deserialize_with = "deserialize_strict_window_ref")]
        #[schemars(with = "StrictWindowRef")]
        window: WindowRef,
    },
    Selector {
        selector: WindowSelector,
        /// Quantifier for state predicates. `all` is false for an empty set.
        quantifier: WindowWaitSelectorQuantifier,
    },
}

/// Explicit semantics for applying one state predicate to selector matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowWaitSelectorQuantifier {
    /// Satisfied when at least one selected window satisfies the predicate.
    Any,
    /// Satisfied when the selected set is non-empty and every window satisfies it.
    All,
    /// Satisfied only when the selector resolves to one window and it satisfies it.
    ExactlyOne,
}

/// Race-free state predicate evaluated from a captured model revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WindowWaitPredicate {
    Exists,
    Closed,
    MapState { state: WindowMapState },
    Active { desired: bool },
    Focused { desired: bool },
    Geometry { predicate: WindowGeometryPredicate },
    Count { predicate: WindowCountPredicate },
}

/// Observation-actor wait registration authorized by `desktop:observe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowWaitRequest {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub target: WindowWaitTarget,
    pub predicate: WindowWaitPredicate,
    /// Optional client-observed lower boundary for transition-oriented waits.
    ///
    /// The actor must still capture its current revision, evaluate, register
    /// the waiter, and re-evaluate atomically. This field never permits a
    /// check-then-subscribe implementation.
    pub after_revision: Option<WindowModelRevision>,
    #[schemars(range(min = 1, max = MAX_WINDOW_WAIT_TIMEOUT_MS))]
    pub timeout_ms: u32,
}

impl WindowWaitRequest {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        validate_scope(self.desktop_id, self.desktop_generation)?;
        if self.timeout_ms == 0 || self.timeout_ms > MAX_WINDOW_WAIT_TIMEOUT_MS {
            return Err(WindowQueryValidationError::WaitTimeout);
        }
        match &self.target {
            WindowWaitTarget::Reference { window } => {
                window.validate_shape()?;
                validate_reference_scope(self.desktop_id, self.desktop_generation, window)?;
                if matches!(&self.predicate, WindowWaitPredicate::Count { .. }) {
                    return Err(WindowQueryValidationError::WaitTargetPredicate);
                }
            }
            WindowWaitTarget::Selector {
                selector,
                quantifier,
            } => {
                selector.validate_for_scope(self.desktop_id, self.desktop_generation)?;
                if matches!(
                    &self.predicate,
                    WindowWaitPredicate::Closed | WindowWaitPredicate::Geometry { .. }
                ) {
                    return Err(WindowQueryValidationError::WaitTargetPredicate);
                }
                if matches!(
                    &self.predicate,
                    WindowWaitPredicate::Exists | WindowWaitPredicate::Count { .. }
                ) && *quantifier != WindowWaitSelectorQuantifier::Any
                {
                    return Err(WindowQueryValidationError::WaitTargetPredicate);
                }
            }
        }
        match &self.predicate {
            WindowWaitPredicate::Geometry { predicate } => predicate.validate()?,
            WindowWaitPredicate::Count { predicate } => predicate.validate()?,
            WindowWaitPredicate::Exists
            | WindowWaitPredicate::Closed
            | WindowWaitPredicate::MapState { .. }
            | WindowWaitPredicate::Active { .. }
            | WindowWaitPredicate::Focused { .. } => {}
        }
        Ok(())
    }
}

/// Terminal disposition of one observation wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowWaitStatus {
    Matched,
    TimedOut,
    TargetVanished,
    ResyncRequired,
}

/// Bounded wait result carrying only actor-local model revision evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowWaitResult {
    pub desktop_id: DesktopId,
    pub desktop_generation: DesktopGeneration,
    pub status: WindowWaitStatus,
    pub evaluated_revision: WindowModelRevision,
    pub predicate_satisfied: bool,
    pub matched_count: u32,
    #[schemars(length(max = MAX_WINDOW_PAGE_LIMIT))]
    pub windows: Vec<WindowSnapshotEntry>,
}

impl WindowWaitResult {
    pub fn validate(&self) -> Result<(), WindowQueryValidationError> {
        validate_scope(self.desktop_id, self.desktop_generation)?;
        if self.windows.len() > usize::from(MAX_WINDOW_PAGE_LIMIT)
            || self.matched_count > MAX_WINDOW_WAIT_COUNT
            || self.matched_count < u32::try_from(self.windows.len()).unwrap_or(u32::MAX)
            || (self.status == WindowWaitStatus::Matched) != self.predicate_satisfied
        {
            return Err(WindowQueryValidationError::WaitResult);
        }
        for (index, window) in self.windows.iter().enumerate() {
            window.validate()?;
            validate_reference_scope(
                self.desktop_id,
                self.desktop_generation,
                &window.snapshot.window,
            )?;
            if window.snapshot.model_revision != self.evaluated_revision {
                return Err(WindowQueryValidationError::SnapshotRevision);
            }
            if self.windows[..index]
                .iter()
                .any(|prior| prior.snapshot.window == window.snapshot.window)
            {
                return Err(WindowQueryValidationError::DuplicateWindow);
            }
        }
        Ok(())
    }
}

fn validate_scope(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<(), WindowQueryValidationError> {
    if desktop_id.as_uuid().is_nil() || desktop_generation.as_uuid().is_nil() {
        return Err(WindowQueryValidationError::NilIdentifier);
    }
    Ok(())
}

fn validate_limit(limit: u16) -> Result<(), WindowQueryValidationError> {
    if limit == 0 || limit > MAX_WINDOW_PAGE_LIMIT {
        return Err(WindowQueryValidationError::PageLimit);
    }
    Ok(())
}

fn validate_reference_scope(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    window: &WindowRef,
) -> Result<(), WindowQueryValidationError> {
    if window.desktop_id != desktop_id || window.desktop_generation != desktop_generation {
        return Err(WindowQueryValidationError::ReferenceScope);
    }
    Ok(())
}

fn validate_snapshot_page(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    snapshot_revision: WindowModelRevision,
    windows: &[WindowSnapshotEntry],
) -> Result<(), WindowQueryValidationError> {
    if windows.len() > usize::from(MAX_WINDOW_PAGE_LIMIT) {
        return Err(WindowQueryValidationError::PageLimit);
    }
    for (index, window) in windows.iter().enumerate() {
        window.validate()?;
        validate_reference_scope(desktop_id, desktop_generation, &window.snapshot.window)?;
        if window.snapshot.model_revision != snapshot_revision {
            return Err(WindowQueryValidationError::SnapshotRevision);
        }
        if windows[..index]
            .iter()
            .any(|prior| prior.snapshot.window == window.snapshot.window)
        {
            return Err(WindowQueryValidationError::DuplicateWindow);
        }
    }
    Ok(())
}

/// Window read/query/wait protocol validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WindowQueryValidationError {
    #[error("window request contains a nil desktop identifier")]
    NilIdentifier,
    #[error("window page limit is outside the supported range")]
    PageLimit,
    #[error("window cursor or reference token is malformed")]
    OpaqueToken,
    #[error("window selector exceeds the maximum nesting depth")]
    SelectorDepth,
    #[error("window selector exceeds the total node budget")]
    SelectorNodes,
    #[error("window selector exceeds the leaf predicate budget")]
    SelectorPredicates,
    #[error("window selector exceeds the compiled regular-expression budget")]
    SelectorRegexes,
    #[error("window selector composition cannot be empty")]
    EmptyComposition,
    #[error("window selector string or regex is empty, oversized, or contains NUL")]
    StringMatcher,
    #[error("window regex is invalid or exceeds the compiled NFA size limit")]
    RegexSyntaxOrComplexity,
    #[error("window selector reported PID must be non-zero")]
    ReportedPid,
    #[error("window selector process reference is invalid")]
    ProcessReference,
    #[error("window reference belongs to another desktop lifetime")]
    ReferenceScope,
    #[error("window snapshots do not share the declared atomic model revision")]
    SnapshotRevision,
    #[error("window result contains the same exact window birth more than once")]
    DuplicateWindow,
    #[error("window geometry predicate must use root-physical coordinates")]
    GeometryCoordinateSpace,
    #[error("window wait timeout is outside the supported range")]
    WaitTimeout,
    #[error("window count predicate exceeds the bounded model ceiling")]
    WaitCount,
    #[error("window wait target and predicate are incompatible")]
    WaitTargetPredicate,
    #[error("window wait result is internally inconsistent")]
    WaitResult,
    #[error(transparent)]
    Window(#[from] WindowValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::{
        WINDOW_IDENTITY_HASH_BYTES, WindowIdentityHash, WindowMetadata, WindowObservedState,
        WindowProcessConfidence, WindowProcessCorrelation,
    };

    fn reference() -> Result<WindowRef, WindowValidationError> {
        Ok(WindowRef {
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            xid: 42,
            observed_generation: 1,
            identity_hash: WindowIdentityHash::new("e".repeat(WINDOW_IDENTITY_HASH_BYTES))?,
        })
    }

    fn leaf() -> WindowSelector {
        WindowSelector::Predicate {
            predicate: WindowPredicate::Active { value: true },
        }
    }

    fn snapshot(
        window: WindowRef,
        model_revision: WindowModelRevision,
    ) -> Result<WindowSnapshot, WindowValidationError> {
        Ok(WindowSnapshot {
            xid_hex: window.xid_hex(),
            window,
            model_revision,
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

    fn entry(snapshot: WindowSnapshot) -> Result<WindowSnapshotEntry, WindowQueryValidationError> {
        Ok(WindowSnapshotEntry {
            snapshot,
            reference_token: WindowReferenceToken::new("A_window_reference_1")?,
        })
    }

    #[test]
    fn opaque_tokens_validate_during_decode_and_redact_debug()
    -> Result<(), WindowQueryValidationError> {
        let token = WindowPageCursor::new("A_secure_cursor_1")?;
        assert_eq!(format!("{token:?}"), "WindowPageCursor(\"<opaque>\")");
        assert!(serde_json::from_str::<WindowPageCursor>("\"bad token\"").is_err());
        Ok(())
    }

    #[test]
    fn selector_rejects_excessive_depth() -> Result<(), WindowValidationError> {
        let reference = reference()?;
        let mut selector = leaf();
        for _ in 0..MAX_WINDOW_SELECTOR_DEPTH {
            selector = WindowSelector::Not {
                selector: Box::new(selector),
            };
        }
        assert_eq!(
            selector.validate_for_scope(reference.desktop_id, reference.desktop_generation),
            Err(WindowQueryValidationError::SelectorDepth)
        );
        Ok(())
    }

    #[test]
    fn selector_rejects_cross_generation_relationships() -> Result<(), WindowValidationError> {
        let request_scope = reference()?;
        let selector = WindowSelector::Predicate {
            predicate: WindowPredicate::TransientFor {
                window: reference()?,
            },
        };
        assert_eq!(
            selector.validate_for_scope(request_scope.desktop_id, request_scope.desktop_generation),
            Err(WindowQueryValidationError::ReferenceScope)
        );
        Ok(())
    }

    #[test]
    fn regex_and_literal_matchers_have_independent_byte_bounds()
    -> Result<(), WindowQueryValidationError> {
        assert_eq!(
            WindowStringMatch::Regex {
                pattern: "x".repeat(MAX_WINDOW_REGEX_BYTES + 1),
                case_sensitive: true,
            }
            .validate(),
            Err(WindowQueryValidationError::StringMatcher)
        );
        WindowStringMatch::Exact {
            value: "x".repeat(MAX_WINDOW_REGEX_BYTES + 1),
            case_sensitive: true,
        }
        .validate()?;
        Ok(())
    }

    #[test]
    fn regex_admission_rejects_invalid_or_oversized_programs() {
        for pattern in ["(", r"\w{20}"] {
            assert_eq!(
                WindowStringMatch::Regex {
                    pattern: pattern.to_owned(),
                    case_sensitive: true,
                }
                .validate(),
                Err(WindowQueryValidationError::RegexSyntaxOrComplexity)
            );
        }
    }

    #[test]
    fn selector_enforces_an_aggregate_regex_budget() -> Result<(), WindowValidationError> {
        let scope = reference()?;
        let selector = WindowSelector::Any {
            selectors: (0..=MAX_WINDOW_SELECTOR_REGEXES)
                .map(|index| WindowSelector::Predicate {
                    predicate: WindowPredicate::Text {
                        field: WindowTextField::Title,
                        matcher: WindowStringMatch::Regex {
                            pattern: format!("window-{index}"),
                            case_sensitive: true,
                        },
                    },
                })
                .collect(),
        };
        assert_eq!(
            selector.validate_for_scope(scope.desktop_id, scope.desktop_generation),
            Err(WindowQueryValidationError::SelectorRegexes)
        );
        Ok(())
    }

    #[test]
    fn selector_match_debug_redacts_literals_and_patterns() {
        for matcher in [
            WindowStringMatch::Exact {
                value: "canary-literal-secret".to_owned(),
                case_sensitive: true,
            },
            WindowStringMatch::Regex {
                pattern: "canary-regex-secret.*".to_owned(),
                case_sensitive: false,
            },
        ] {
            let debug = format!("{matcher:?}");
            assert!(debug.contains("<redacted>"));
            assert!(!debug.contains("canary"));
        }
    }

    #[test]
    fn omitted_page_limit_uses_the_documented_default() -> Result<(), Box<dyn std::error::Error>> {
        let decoded: WindowListRequest = serde_json::from_value(serde_json::json!({
            "desktop_id": DesktopId::new(),
            "desktop_generation": DesktopGeneration::new(),
            "order": "xid_ascending",
            "cursor": null
        }))?;
        assert_eq!(decoded.limit, DEFAULT_WINDOW_PAGE_LIMIT);
        decoded.validate()?;
        Ok(())
    }

    #[test]
    fn pages_reject_duplicate_exact_window_births() -> Result<(), Box<dyn std::error::Error>> {
        let window = reference()?;
        let revision = WindowModelRevision::new(7)?;
        let observed = snapshot(window.clone(), revision)?;
        let page = WindowListPage {
            desktop_id: window.desktop_id,
            desktop_generation: window.desktop_generation,
            snapshot_revision: revision,
            windows: vec![entry(observed.clone())?, entry(observed)?],
            next_cursor: None,
        };
        assert_eq!(
            page.validate(),
            Err(WindowQueryValidationError::DuplicateWindow)
        );
        Ok(())
    }

    #[test]
    fn exists_wait_requires_any_selector_quantifier() -> Result<(), WindowValidationError> {
        let scope = reference()?;
        let request = WindowWaitRequest {
            desktop_id: scope.desktop_id,
            desktop_generation: scope.desktop_generation,
            target: WindowWaitTarget::Selector {
                selector: leaf(),
                quantifier: WindowWaitSelectorQuantifier::All,
            },
            predicate: WindowWaitPredicate::Exists,
            after_revision: None,
            timeout_ms: 1_000,
        };
        assert_eq!(
            request.validate(),
            Err(WindowQueryValidationError::WaitTargetPredicate)
        );
        Ok(())
    }

    #[test]
    fn wait_results_reject_cross_generation_windows() -> Result<(), Box<dyn std::error::Error>> {
        let window = reference()?;
        let revision = WindowModelRevision::new(9)?;
        let result = WindowWaitResult {
            desktop_id: window.desktop_id,
            desktop_generation: DesktopGeneration::new(),
            status: WindowWaitStatus::Matched,
            evaluated_revision: revision,
            predicate_satisfied: true,
            matched_count: 1,
            windows: vec![entry(snapshot(window, revision)?)?],
        };
        assert_eq!(
            result.validate(),
            Err(WindowQueryValidationError::ReferenceScope)
        );
        Ok(())
    }

    #[test]
    fn wait_results_reject_duplicate_exact_window_births() -> Result<(), Box<dyn std::error::Error>>
    {
        let window = reference()?;
        let revision = WindowModelRevision::new(10)?;
        let observed = snapshot(window.clone(), revision)?;
        let result = WindowWaitResult {
            desktop_id: window.desktop_id,
            desktop_generation: window.desktop_generation,
            status: WindowWaitStatus::Matched,
            evaluated_revision: revision,
            predicate_satisfied: true,
            matched_count: 2,
            windows: vec![entry(observed.clone())?, entry(observed)?],
        };
        assert_eq!(
            result.validate(),
            Err(WindowQueryValidationError::DuplicateWindow)
        );
        Ok(())
    }

    #[test]
    fn wait_predicates_are_compatible_with_their_target_kind() -> Result<(), WindowValidationError>
    {
        let window = reference()?;
        let invalid = WindowWaitRequest {
            desktop_id: window.desktop_id,
            desktop_generation: window.desktop_generation,
            target: WindowWaitTarget::Reference { window },
            predicate: WindowWaitPredicate::Count {
                predicate: WindowCountPredicate {
                    comparison: WindowCountComparison::AtLeast,
                    count: 1,
                },
            },
            after_revision: None,
            timeout_ms: 1_000,
        };
        assert_eq!(
            invalid.validate(),
            Err(WindowQueryValidationError::WaitTargetPredicate)
        );
        Ok(())
    }

    #[test]
    fn list_requests_require_nonzero_bounded_pages() -> Result<(), WindowValidationError> {
        let window = reference()?;
        let request = WindowListRequest {
            desktop_id: window.desktop_id,
            desktop_generation: window.desktop_generation,
            limit: 0,
            order: WindowOrder::CreationAscending,
            cursor: None,
        };
        assert_eq!(
            request.validate(),
            Err(WindowQueryValidationError::PageLimit)
        );
        Ok(())
    }
}
