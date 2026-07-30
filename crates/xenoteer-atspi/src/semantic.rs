//! Protocol-independent semantic operation requests and redacted results.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::{BackendFailure, ObjectAddress};

/// Maximum UTF-8 bytes admitted in one semantic text write.
pub const MAX_SEMANTIC_TEXT_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 bytes admitted in an action selector or action metadata field.
pub const MAX_ACTION_FIELD_BYTES: usize = 64 * 1024;
/// Maximum actions returned as content-free invocation evidence.
pub const MAX_ACTIONS: usize = 256;
/// Maximum aggregate UTF-8 bytes in action evidence.
pub const MAX_ACTION_EVIDENCE_BYTES: usize = 1024 * 1024;
/// Maximum selection ranges returned as text readback evidence.
pub const MAX_SELECTION_RANGES: usize = 256;

/// Adapter-owned classification used to keep password verification content-free.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextProtection {
    /// The stable AT-SPI password-text role requires length-only verification.
    Protected,
    /// A recognized non-password role permits the same content-free editable-text call.
    Unprotected,
    /// A future or otherwise unrecognized role cannot prove a safe verification policy.
    Unknown,
}

/// Secret-safe SHA-256 identity evidence derived from stable cache fields.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct IdentityFingerprint([u8; 32]);

impl IdentityFingerprint {
    pub(crate) fn from_parts(
        object: &ObjectAddress,
        application: &ObjectAddress,
        parent: Option<&ObjectAddress>,
        index_in_parent: Option<usize>,
        name: &str,
        description: &str,
    ) -> Self {
        let mut digest = Sha256::new();
        for field in [
            object.bus_name().as_bytes(),
            object.object_path().as_bytes(),
            application.bus_name().as_bytes(),
            application.object_path().as_bytes(),
            parent.map_or(&[][..], |value| value.bus_name().as_bytes()),
            parent.map_or(&[][..], |value| value.object_path().as_bytes()),
            name.as_bytes(),
            description.as_bytes(),
        ] {
            digest.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes());
            digest.update(field);
        }
        digest.update(
            index_in_parent
                .and_then(|value| u64::try_from(value).ok())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        Self(digest.finalize().into())
    }
}

impl fmt::Debug for IdentityFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IdentityFingerprint(<redacted-sha256>)")
    }
}

/// Secret-bearing text whose debug output never contains the supplied content.
#[derive(Eq, PartialEq)]
pub struct RedactedText(String);

impl RedactedText {
    /// Validate and own a bounded D-Bus string.
    pub fn new(value: impl Into<String>) -> Result<Self, SemanticError> {
        let value = value.into();
        if value.len() > MAX_SEMANTIC_TEXT_BYTES {
            return Err(SemanticError::InvalidRequest(
                "editable text exceeds the adapter byte limit",
            ));
        }
        if value.contains('\0') {
            return Err(SemanticError::InvalidRequest(
                "editable text contains a D-Bus NUL byte",
            ));
        }
        if value.chars().count() > i32::MAX as usize {
            return Err(SemanticError::InvalidRequest(
                "editable text character count exceeds AT-SPI",
            ));
        }
        Ok(Self(value))
    }

    /// UTF-8 byte length, safe to expose in logs and evidence.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the supplied content is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Unicode scalar count expected by AT-SPI text offsets.
    #[must_use]
    pub fn character_count(&self) -> u32 {
        u32::try_from(self.0.chars().count()).unwrap_or(u32::MAX)
    }

    /// Character length expected by AT-SPI's editable-text methods.
    #[cfg(feature = "live-atspi")]
    pub(crate) fn character_len(&self) -> i32 {
        i32::try_from(self.character_count()).unwrap_or(i32::MAX)
    }

    #[cfg(feature = "live-atspi")]
    pub(crate) fn expose_to_backend(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RedactedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedactedText")
            .field("utf8_bytes", &self.0.len())
            .field("characters", &self.0.chars().count())
            .finish_non_exhaustive()
    }
}

/// Exact actor-owned identity and freshness evidence required for a call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticTarget {
    /// Target object on the central accessibility bus.
    pub object: ObjectAddress,
    /// Application root that owned the cached target.
    pub application: ObjectAddress,
    /// Expected actor accessibility connection generation.
    pub accessibility_generation: u64,
    /// Expected unique application-owner generation.
    pub application_generation: u64,
    /// Expected whole-cache revision.
    pub cache_revision: u64,
    /// Expected last mutation revision of this exact node.
    pub node_revision: u64,
    /// Cache-reported parent index, absent for roots and legacy cache entries.
    pub index_in_parent: Option<usize>,
    /// Secret-safe fingerprint of object/application/parent/index/name/description.
    pub identity_fingerprint: IdentityFingerprint,
    /// Raw expected role, retained for forward-compatible identity checking.
    pub role: u32,
    /// Exact raw two-word AT-SPI state-set evidence from the cache.
    pub states: Vec<u32>,
}

/// Cache-coordinate proof used to ask the actor to mint a semantic target.
///
/// Callers cannot construct [`IdentityFingerprint`]; the actor copies it from
/// the exact cache node only after every generation and revision fence passes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticTargetRequest {
    /// Exact central-bus object address.
    pub object: ObjectAddress,
    /// Exact application root address mirrored with the object.
    pub application: ObjectAddress,
    /// Expected actor accessibility connection generation.
    pub accessibility_generation: u64,
    /// Expected unique application-owner generation.
    pub application_generation: u64,
    /// Expected actor cache revision.
    pub cache_revision: u64,
    /// Expected last mutation revision of this exact node.
    pub node_revision: u64,
}

/// Completion evidence for one actor-owned targeted cache reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticReconcileResult {
    /// Accessibility generation that owned the refresh.
    pub accessibility_generation: u64,
    /// Application generation that owned the object.
    pub application_generation: u64,
    /// Cache revision before applying the fresh read.
    pub previous_cache_revision: u64,
    /// Cache revision after applying the fresh read.
    pub cache_revision: u64,
    /// Current exact node revision after reconciliation.
    pub node_revision: u64,
    /// Whether fresh evidence changed actor-owned cache state.
    pub changed: bool,
}

/// Select an accessible action without relying on localized descriptions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionSelector {
    /// Resolve one unique conventional default action by normalized live name.
    Default,
    /// Zero-based action index.
    Index(u32),
    /// Exact machine-readable action name.
    Name(String),
}

/// Selection mutation supported by the AT-SPI Selection interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionOperation {
    /// Clear every selected child.
    Clear,
    /// Select one zero-based child.
    SelectChild(u32),
    /// Deselect one zero-based child.
    DeselectChild(u32),
    /// Select every child supported by the target.
    SelectAll,
}

/// Stable adapter representation of AT-SPI scroll placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollPlacement {
    /// Top-left corner.
    TopLeft,
    /// Bottom-right corner.
    BottomRight,
    /// Top edge.
    TopEdge,
    /// Bottom edge.
    BottomEdge,
    /// Left edge.
    LeftEdge,
    /// Right edge.
    RightEdge,
    /// Toolkit-selected visible placement.
    Anywhere,
}

/// Position source for an editable-text insertion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextInsertPosition {
    /// Exact zero-based character offset.
    Offset(u32),
    /// Fresh caret offset read immediately before dispatch.
    LiveCaret,
}

/// Explicit post-write caret and selection behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextSelectionPolicy {
    /// Restore pre-write caret and selection offsets, clamped to the new length.
    Preserve,
    /// Clear selections and collapse the caret before the inserted/replaced range.
    CollapseBefore,
    /// Clear selections and collapse the caret after the inserted/replaced range.
    CollapseAfter,
    /// Select the inserted/replacement range and place the caret after it.
    SelectInserted,
}

/// Content-verification policy for one semantic text write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextVerificationMode {
    /// Verify only public character-count, caret, and selection evidence.
    LengthOnly,
    /// Privately compare bounded AT-SPI text content and expose only the boolean result.
    Exact,
}

/// Semantic operation accepted by the protocol-independent actor seam.
#[derive(Debug, PartialEq)]
pub enum SemanticOperation {
    /// Invoke an action by exact index or machine name.
    Invoke(ActionSelector),
    /// Request keyboard focus through the Component interface.
    Focus,
    /// Set a finite numeric Value property.
    SetValue(f64),
    /// Mutate child selection.
    Selection(SelectionOperation),
    /// Replace editable text. Content is always redacted from debug output.
    SetText {
        /// Secret-bearing replacement content.
        text: RedactedText,
        /// Explicit post-write caret and selection behavior.
        selection: TextSelectionPolicy,
        /// Content verification permitted for this target.
        verification: TextVerificationMode,
    },
    /// Insert editable text at a nonnegative character position.
    InsertText {
        /// Zero-based character position.
        position: TextInsertPosition,
        /// Secret-bearing text to insert.
        text: RedactedText,
        /// Explicit post-write caret and selection behavior.
        selection: TextSelectionPolicy,
        /// Content verification permitted for this target.
        verification: TextVerificationMode,
    },
    /// Scroll the target to a toolkit-independent placement.
    Scroll(ScrollPlacement),
    /// Scroll the target to an absolute screen point.
    ScrollToPoint {
        /// Screen x coordinate.
        x: i32,
        /// Screen y coordinate.
        y: i32,
    },
}

impl SemanticOperation {
    pub(crate) fn required_interfaces(&self) -> &'static [&'static str] {
        match self {
            Self::Invoke(_) => &["org.a11y.atspi.Action"],
            Self::Focus | Self::Scroll(_) | Self::ScrollToPoint { .. } => {
                &["org.a11y.atspi.Component"]
            }
            Self::SetValue(_) => &["org.a11y.atspi.Value"],
            Self::Selection(_) => &["org.a11y.atspi.Selection"],
            Self::SetText { .. } | Self::InsertText { .. } => {
                &["org.a11y.atspi.EditableText", "org.a11y.atspi.Text"]
            }
        }
    }

    pub(crate) fn is_text_write(&self) -> bool {
        self.text_verification().is_some()
    }

    pub(crate) const fn text_verification(&self) -> Option<TextVerificationMode> {
        match self {
            Self::SetText { verification, .. } | Self::InsertText { verification, .. } => {
                Some(*verification)
            }
            _ => None,
        }
    }

    /// Validate bounded indices, finite values, and selector strings.
    pub fn validate(&self) -> Result<(), SemanticError> {
        match self {
            Self::Invoke(ActionSelector::Name(name))
                if name.is_empty()
                    || name.contains('\0')
                    || name.len() > MAX_ACTION_FIELD_BYTES =>
            {
                Err(SemanticError::InvalidRequest(
                    "action name is empty or exceeds the adapter byte limit",
                ))
            }
            Self::Invoke(ActionSelector::Index(index))
            | Self::Selection(SelectionOperation::SelectChild(index))
            | Self::Selection(SelectionOperation::DeselectChild(index))
                if i32::try_from(*index).is_err() =>
            {
                Err(SemanticError::InvalidRequest(
                    "semantic index exceeds AT-SPI's signed range",
                ))
            }
            Self::SetValue(value) if !value.is_finite() => Err(SemanticError::InvalidRequest(
                "value operation requires a finite number",
            )),
            Self::InsertText {
                position: TextInsertPosition::Offset(position),
                ..
            } if i32::try_from(*position).is_err() => Err(SemanticError::InvalidRequest(
                "text position exceeds AT-SPI's signed range",
            )),
            _ => Ok(()),
        }
    }
}

/// One serialized semantic call with an absolute terminal deadline.
#[derive(Debug, PartialEq)]
pub struct SemanticRequest {
    /// Exact target evidence to revalidate.
    pub target: SemanticTarget,
    /// Operation to dispatch without a physical-input fallback.
    pub operation: SemanticOperation,
    /// Caller-owned terminal deadline.
    pub deadline: Instant,
}

/// One serialized, read-only exact observation with an absolute deadline.
#[derive(Debug, PartialEq)]
pub struct SemanticObservationRequest {
    /// Cache identity and generation evidence to revalidate before the live read.
    pub target: SemanticTarget,
    /// Caller-owned terminal deadline.
    pub deadline: Instant,
}

/// Bounded action metadata returned without reading accessible text content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionEvidence {
    /// Zero-based action index.
    pub index: u32,
    /// Machine/localized name returned by `GetActions`.
    pub name: String,
    /// Human-readable action description.
    pub description: String,
    /// Toolkit key binding notation.
    pub keybinding: String,
}

/// Screen-coordinate component rectangle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticRect {
    /// Screen x coordinate.
    pub x: i32,
    /// Screen y coordinate.
    pub y: i32,
    /// Width reported by the toolkit.
    pub width: i32,
    /// Height reported by the toolkit.
    pub height: i32,
}

/// Text selection offsets; it intentionally contains no text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionRangeEvidence {
    /// Inclusive start offset.
    pub start: u32,
    /// Exclusive end offset.
    pub end: u32,
}

/// Content-free text state captured around a write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextReadbackEvidence {
    /// Current character count.
    pub character_count: u32,
    /// Current caret offset, or `-1` when this Text object exposes no caret.
    pub caret_offset: i32,
    /// Bounded selection offsets.
    pub selections: Vec<SelectionRangeEvidence>,
}

/// Content-free readback evidence collected after a semantic call.
#[derive(Clone, Debug, PartialEq)]
pub enum SemanticEvidence {
    /// Invocation result and bounded action metadata.
    Action {
        /// Whether the toolkit accepted the invocation.
        accepted: bool,
        /// Selected zero-based action index.
        invoked_index: u32,
        /// Current bounded action list.
        actions: Vec<ActionEvidence>,
    },
    /// Focus result and fresh focused-state observation.
    Focus {
        /// Whether the toolkit accepted the focus request.
        accepted: bool,
        /// Whether fresh state words contained the focused bit.
        focused: bool,
    },
    /// Finite Value properties read after setting the value.
    Value {
        /// Current value.
        current: f64,
        /// Minimum value.
        minimum: f64,
        /// Maximum value.
        maximum: f64,
        /// Minimum increment.
        minimum_increment: f64,
    },
    /// Child-selection readback without names or child content.
    Selection {
        /// Whether the toolkit accepted the mutation.
        accepted: bool,
        /// Number of selected children.
        selected_children: u32,
        /// Selected state for the addressed child when applicable.
        addressed_child_selected: Option<bool>,
    },
    /// Editable-text size, caret, and ranges without text content.
    Text {
        /// Whether the toolkit accepted the write.
        accepted: bool,
        /// Content-free state immediately before dispatch.
        before: TextReadbackEvidence,
        /// Content-free state after the write and selection policy.
        after: TextReadbackEvidence,
        /// Sanitized exact-comparison result, absent for length-only verification.
        exact_match: Option<bool>,
    },
    /// Geometry before and after a scroll request.
    Scroll {
        /// Whether the toolkit accepted the scroll request.
        accepted: bool,
        /// Screen-coordinate bounds before dispatch.
        before: SemanticRect,
        /// Screen-coordinate bounds after dispatch.
        after: SemanticRect,
    },
}

/// Successful, fully fenced semantic operation result.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticResult {
    /// Accessibility generation that owned the dispatch.
    pub accessibility_generation: u64,
    /// Application generation that owned the target.
    pub application_generation: u64,
    /// Cache revision revalidated immediately before dispatch.
    pub cache_revision: u64,
    /// Content-free post-operation evidence.
    pub evidence: SemanticEvidence,
}

/// Fresh, content-free accessible evidence returned by the live backend.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticObservationEvidence {
    /// Fresh secret-safe identity fingerprint.
    pub identity_fingerprint: IdentityFingerprint,
    /// Fresh parent address, absent only for the application root.
    pub parent: Option<ObjectAddress>,
    /// Fresh nonnegative index when the object has a parent.
    pub index_in_parent: Option<usize>,
    /// Fresh raw role number.
    pub role: u32,
    /// Fresh bounded raw state words.
    pub states: Vec<u32>,
    /// Fresh bounded interface names.
    pub interfaces: Vec<String>,
    /// Fresh screen-coordinate component bounds when Component is available.
    pub bounds: Option<SemanticRect>,
    /// Highest accessible below the application root, when one can be proven.
    pub top_level: Option<ObjectAddress>,
    /// Unix process ID owning the unique application bus name, when available.
    pub application_pid: Option<u32>,
    /// Fresh finite Value properties when Value is available.
    pub value: Option<SemanticValueEvidence>,
    /// Fresh content-free Text state when Text is available.
    pub text: Option<TextReadbackEvidence>,
    /// Fresh selected-child count when Selection is available.
    pub selected_children: Option<u32>,
}

/// Fresh finite Value properties returned by an exact observation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SemanticValueEvidence {
    /// Current value.
    pub current: f64,
    /// Minimum value.
    pub minimum: f64,
    /// Maximum value.
    pub maximum: f64,
    /// Minimum increment.
    pub minimum_increment: f64,
}

/// Successful actor-fenced exact observation.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticObservationResult {
    /// Accessibility generation that owned the read.
    pub accessibility_generation: u64,
    /// Application generation that owned the target.
    pub application_generation: u64,
    /// Current actor cache revision at the admitted read.
    ///
    /// This can be newer than the target's minting revision when unrelated
    /// cache entries advanced while the exact target-node fence stayed valid.
    pub cache_revision: u64,
    /// Actor-lifetime monotonic successful-read epoch.
    pub read_epoch: u64,
    /// Exact object address observed.
    pub object: ObjectAddress,
    /// Exact application root observed.
    pub application: ObjectAddress,
    /// Fresh bounded evidence.
    pub evidence: SemanticObservationEvidence,
}

/// Failure from target revalidation, cancellation, deadline, or live dispatch.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SemanticError {
    /// Public actor request queue was full.
    #[error("AT-SPI semantic actor request queue is full")]
    QueueFull,
    /// Actor stopped before it could return a result.
    #[error("AT-SPI semantic actor stopped")]
    Stopped,
    /// A request was admitted, but its actor reply was lost; effect is unknown.
    #[error("AT-SPI semantic actor reply was lost after admission; effect is unknown")]
    ReplyLostAfterAdmission,
    /// The request itself violates a bounded adapter invariant.
    #[error("invalid AT-SPI semantic request: {0}")]
    InvalidRequest(&'static str),
    /// Actor generation changed before dispatch.
    #[error("stale AT-SPI accessibility generation: expected {expected}, current {current}")]
    StaleAccessibilityGeneration {
        /// Request generation.
        expected: u64,
        /// Actor generation.
        current: u64,
    },
    /// Application owner generation changed before dispatch.
    #[error("stale AT-SPI application generation: expected {expected}, current {current}")]
    StaleApplicationGeneration {
        /// Request generation.
        expected: u64,
        /// Actor-owned generation.
        current: u64,
    },
    /// Whole-cache revision changed before dispatch.
    #[error("stale AT-SPI semantic cache revision: expected {expected}, current {current}")]
    StaleCacheRevision {
        /// Request revision.
        expected: u64,
        /// Actor-owned revision.
        current: u64,
    },
    /// Target disappeared or its application, role, state, or node revision changed.
    #[error("AT-SPI semantic target identity changed")]
    StaleIdentity,
    /// Required semantic interface was absent from the exact cached node.
    #[error("AT-SPI semantic target lacks interface {0}")]
    InterfaceUnavailable(&'static str),
    /// Requested/default action was absent from fresh live action metadata.
    #[error("AT-SPI semantic action was not found")]
    ActionNotFound,
    /// Requested/default action was ambiguous in fresh live action metadata.
    #[error("AT-SPI semantic action was ambiguous")]
    AmbiguousAction,
    /// An unknown future role cannot prove whether length-only verification is safe.
    #[error("AT-SPI unclassified text write denied")]
    UnclassifiedTextDenied,
    /// Cancellation was observed before a backend marked the operation dispatched.
    #[error("AT-SPI semantic operation cancelled before dispatch")]
    CancelledBeforeDispatch,
    /// Cancellation raced with an already dispatched call; effect is unknown.
    #[error("AT-SPI semantic operation cancelled after dispatch; effect is unknown")]
    CancelledAfterDispatch,
    /// Deadline elapsed before a backend marked the operation dispatched.
    #[error("AT-SPI semantic operation deadline elapsed before dispatch")]
    DeadlineBeforeDispatch,
    /// Deadline elapsed after dispatch; effect is unknown.
    #[error("AT-SPI semantic operation deadline elapsed after dispatch; effect is unknown")]
    DeadlineAfterDispatch,
    /// Actor is not healthy enough to execute semantic operations.
    #[error("AT-SPI semantic operation is unavailable")]
    Unavailable,
    /// Live backend call failed with bounded, secret-free diagnostics.
    #[error(transparent)]
    Backend(BackendFailure),
    /// Backend failed after dispatch, so the external effect may have occurred.
    #[error("AT-SPI semantic backend failed after dispatch; effect is unknown: {0}")]
    BackendAfterDispatch(BackendFailure),
    /// The actor-lifetime exact-read epoch cannot be advanced safely.
    #[error("AT-SPI semantic observation read epoch exhausted")]
    ReadEpochExhausted,
}

impl SemanticError {
    /// Whether the actor can prove that no semantic effect was dispatched.
    ///
    /// A daemon may reacquire cache evidence and retry only when this returns
    /// `true`. `CancelledAfterDispatch`, `DeadlineAfterDispatch`, and
    /// `BackendAfterDispatch` are intentionally terminal unknown-effect results.
    #[must_use]
    pub fn effect_definitely_not_dispatched(&self) -> bool {
        !matches!(
            self,
            Self::CancelledAfterDispatch
                | Self::DeadlineAfterDispatch
                | Self::BackendAfterDispatch(_)
                | Self::ReplyLostAfterAdmission
        )
    }
}

/// Actor-validated request passed to its exclusively owned backend.
#[doc(hidden)]
#[derive(Debug)]
pub struct BackendSemanticRequest {
    /// Exact central-bus address.
    pub object: ObjectAddress,
    /// Expected application root address.
    pub application: ObjectAddress,
    /// Expected secret-safe cache identity fingerprint.
    pub expected_identity: IdentityFingerprint,
    /// Cache-reported index included only when the wire cache exposed it.
    pub expected_index_in_parent: Option<usize>,
    /// Expected fresh raw role.
    pub expected_role: u32,
    /// Expected fresh raw states.
    pub expected_states: Vec<u32>,
    /// Cache-reported child count used by exact selection readback when available.
    pub expected_child_count: Option<usize>,
    /// Already validated operation.
    pub operation: SemanticOperation,
    /// Absolute caller-owned terminal deadline for dispatch and settling.
    pub deadline: Instant,
    /// Per-proxy-call deadline ceiling.
    pub proxy_call_timeout: Duration,
    /// Admission limits for all variable-size live replies.
    pub cache_limits: crate::CacheLimits,
    /// Actor-captured ingress epoch checked immediately before dispatch.
    pub dispatch_permit: SemanticDispatchPermit,
}

/// Actor-validated exact observation request passed only to its owned backend.
#[doc(hidden)]
#[derive(Debug)]
pub struct BackendObservationRequest {
    /// Exact central-bus address.
    pub object: ObjectAddress,
    /// Expected application root address.
    pub application: ObjectAddress,
    /// Expected secret-safe identity fingerprint.
    pub expected_identity: IdentityFingerprint,
    /// Cache-reported index included only when available.
    pub expected_index_in_parent: Option<usize>,
    /// Expected stable raw role.
    pub expected_role: u32,
    /// Per-proxy-call deadline ceiling.
    pub proxy_call_timeout: Duration,
    /// Admission limits for all returned variable-size evidence.
    pub cache_limits: crate::CacheLimits,
    /// Actor-captured ingress epoch checked immediately before returning evidence.
    pub read_permit: SemanticDispatchPermit,
}

/// Cheap generation permit proving no backend event changed during preflight.
#[doc(hidden)]
#[derive(Clone)]
pub struct SemanticDispatchPermit {
    epoch: Arc<AtomicU64>,
    expected: u64,
}

impl SemanticDispatchPermit {
    pub(crate) fn new(epoch: Arc<AtomicU64>, expected: u64) -> Self {
        Self { epoch, expected }
    }

    /// Fail closed unless the captured even epoch remains current.
    pub fn ensure_current(&self) -> Result<(), BackendFailure> {
        let current = self.epoch.load(Ordering::SeqCst);
        if self.expected & 1 == 0 && current == self.expected {
            Ok(())
        } else {
            Err(BackendFailure::new(
                crate::BackendFailureKind::Protocol,
                "backend ingress changed during semantic preflight",
            ))
        }
    }
}

impl fmt::Debug for SemanticDispatchPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SemanticDispatchPermit")
            .field("expected_epoch", &self.expected)
            .finish_non_exhaustive()
    }
}

/// Conservative dispatch marker shared across cancellation boundaries.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct SemanticDispatchMarker(Arc<AtomicBool>);

impl SemanticDispatchMarker {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Mark the point immediately before the mutating D-Bus method is called.
    pub fn mark_dispatched(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub(crate) fn was_dispatched(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_fingerprint_detects_same_path_rebirth_without_debug_disclosure()
    -> Result<(), crate::CacheError> {
        let object = ObjectAddress::new(":1.200", "/test/reused")?;
        let application = ObjectAddress::new(":1.200", "/test/app")?;
        let parent = ObjectAddress::new(":1.200", "/test/parent")?;
        let before = IdentityFingerprint::from_parts(
            &object,
            &application,
            Some(&parent),
            Some(2),
            "old secret-ish label",
            "description",
        );
        let reborn = IdentityFingerprint::from_parts(
            &object,
            &application,
            Some(&parent),
            Some(2),
            "new secret-ish label",
            "description",
        );
        assert_ne!(before, reborn);
        let debug = format!("{before:?}");
        assert!(!debug.contains("old secret-ish label"));
        assert!(!debug.contains(":1.200"));
        Ok(())
    }
}
