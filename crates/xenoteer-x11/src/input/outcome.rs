//! Backend-redacted input results and actor health snapshots.

use core::fmt;

use xenoteer_core::domain::RootPoint;
use xenoteer_core::input::{
    ButtonMapping, EffectCertainty, EffectJournal, InputHealth, PhysicalButton, PhysicalKey,
};
use xenoteer_protocol::CommandId;

use crate::keyboard::{KeyboardModelAvailability, NamedKey};

use super::PhysicalTextMode;

/// Redacted keyboard-model identity attached to health and action evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardModelDiagnostics {
    /// Whether native actor-side XKB resolution is compiled and connected.
    pub availability: KeyboardModelAvailability,
    /// Current clean model generation when available.
    pub generation: Option<u64>,
    /// Complete serialized-keymap fingerprint, without typed content.
    pub keymap_fingerprint: Option<u64>,
}

/// Concrete non-text binding evidence retained for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardBindingEvidence {
    /// Physical keycode emitted through XTEST.
    pub key: PhysicalKey,
    /// Concrete configured named key, including chosen modifier side.
    pub concrete_named_key: Option<NamedKey>,
    /// XKB layout used for resolution.
    pub layout: u32,
    /// XKB level used for resolution.
    pub level: u32,
    /// Clean mapping generation captured before emission.
    pub generation: u64,
    /// Whether the concrete key is a modifier provider.
    pub is_modifier: bool,
    /// Physical synthesized modifier providers required by this binding.
    pub required_modifiers: Vec<PhysicalKey>,
}

/// Redacted keyboard evidence for a completed or partially completed action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardOutcomeEvidence {
    /// Model identity/generation evidence.
    pub model: KeyboardModelDiagnostics,
    /// Concrete resolved bindings; scalar text content and keysyms are omitted.
    pub bindings: Vec<KeyboardBindingEvidence>,
    /// Number of exact text scalars requested, without their content.
    pub text_scalar_count: Option<usize>,
    /// Caller-selected text strategy, absent for non-text actions.
    pub requested_text_mode: Option<PhysicalTextMode>,
    /// Scalars emitted from exact bindings already present in the current layout.
    pub current_layout_scalars: usize,
    /// Scalars emitted through a verified temporary mapping.
    pub temporary_mapping_scalars: usize,
    /// Temporary mappings whose installed value was read back exactly.
    pub temporary_mappings_installed: usize,
    /// Temporary mappings whose original value was restored and read back exactly.
    pub temporary_mappings_restored: usize,
    /// Restoration proof when temporary mapping was attempted.
    pub temporary_mapping_restoration_proven: Option<bool>,
}

/// Action effect evidence, structurally redacted when scalar identities could encode text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEffectEvidence {
    /// Full ordered effects for pointer and named/raw-only keyboard actions.
    Journal(EffectJournal),
    /// Aggregate certainty counts with no physical key identities or ordering.
    RedactedKeyboard {
        /// Requests serialized without a complete confirmation proof.
        provisional: usize,
        /// Requests covered by checked cookies and postconditions.
        confirmed: usize,
    },
}

/// Cleanup evidence with scalar-bearing keyboard identities structurally removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputCleanupEvidence {
    /// Full cleanup observations for non-scalar actions and explicit controls.
    Detailed(CleanupReport),
    /// Aggregate cleanup proof for actions whose keycodes could encode text.
    RedactedKeyboard {
        /// Release requests attempted.
        attempted: usize,
        /// Releases covered by successful checks and observations.
        confirmed: usize,
        /// Actor-owned keys still retained, without physical identities.
        residual_owned_key_count: usize,
        /// Actor-owned buttons still retained, without physical identities.
        residual_owned_button_count: usize,
        /// Whether QueryPointer evidence was available.
        pointer_observation_available: bool,
        /// Safe QueryPointer button-mask evidence, without keyboard identities.
        observed_logical_buttons_1_to_5: Option<[bool; 5]>,
        /// Whether QueryKeymap evidence was available.
        key_observation_available: bool,
        /// Whether no actor-owned buttons remained after cleanup.
        owned_buttons_proven_released: bool,
        /// Whether no actor-owned keys remained after cleanup.
        owned_keys_proven_released: bool,
        /// Whether temporary mapping restoration was retried.
        temporary_mapping_restore_attempted: bool,
        /// Whether exact restoration was proved.
        temporary_mapping_restore_proven: bool,
        /// Whether every required cleanup proof succeeded.
        succeeded: bool,
    },
}

impl InputCleanupEvidence {
    /// Whether cleanup completed with every required proof.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        match self {
            Self::Detailed(report) => report.succeeded,
            Self::RedactedKeyboard { succeeded, .. } => *succeeded,
        }
    }

    /// Full report when identities were safe to retain.
    #[must_use]
    pub const fn detailed(&self) -> Option<&CleanupReport> {
        match self {
            Self::Detailed(report) => Some(report),
            Self::RedactedKeyboard { .. } => None,
        }
    }

    /// Release requests attempted.
    #[must_use]
    pub const fn attempted(&self) -> usize {
        match self {
            Self::Detailed(report) => report.attempted,
            Self::RedactedKeyboard { attempted, .. } => *attempted,
        }
    }

    /// Release requests confirmed.
    #[must_use]
    pub const fn confirmed(&self) -> usize {
        match self {
            Self::Detailed(report) => report.confirmed,
            Self::RedactedKeyboard { confirmed, .. } => *confirmed,
        }
    }

    pub(super) fn redact_keyboard(self) -> Self {
        match self {
            Self::Detailed(report) => Self::RedactedKeyboard {
                attempted: report.attempted,
                confirmed: report.confirmed,
                residual_owned_key_count: report.residual_owned_keys.len(),
                residual_owned_button_count: report.residual_owned_buttons.len(),
                pointer_observation_available: report.observed_logical_buttons_1_to_5.is_some(),
                observed_logical_buttons_1_to_5: report.observed_logical_buttons_1_to_5,
                key_observation_available: report.observed_pressed_keys.is_some(),
                owned_buttons_proven_released: report.residual_owned_buttons.is_empty(),
                owned_keys_proven_released: report.residual_owned_keys.is_empty(),
                temporary_mapping_restore_attempted: report.temporary_mapping_restore_attempted,
                temporary_mapping_restore_proven: report.temporary_mapping_restore_proven,
                succeeded: report.succeeded,
            },
            redacted @ Self::RedactedKeyboard { .. } => redacted,
        }
    }

    pub(super) fn from_control_report(report: CleanupReport) -> Self {
        let exposes_key_identity = report
            .observed_pressed_keys
            .as_ref()
            .is_some_and(|keys| !keys.is_empty())
            || !report.residual_owned_keys.is_empty();
        let evidence = Self::Detailed(report);
        if exposes_key_identity {
            evidence.redact_keyboard()
        } else {
            evidence
        }
    }
}

impl From<CleanupReport> for InputCleanupEvidence {
    fn from(report: CleanupReport) -> Self {
        Self::from_control_report(report)
    }
}

impl InputEffectEvidence {
    pub(super) fn from_journal(journal: EffectJournal, redact_keyboard: bool) -> Self {
        if !redact_keyboard {
            return Self::Journal(journal);
        }
        let mut provisional = 0_usize;
        let mut confirmed = 0_usize;
        for record in journal.records() {
            match record.certainty() {
                EffectCertainty::Provisional => provisional = provisional.saturating_add(1),
                EffectCertainty::Confirmed => confirmed = confirmed.saturating_add(1),
            }
        }
        Self::RedactedKeyboard {
            provisional,
            confirmed,
        }
    }
}

impl From<EffectJournal> for InputEffectEvidence {
    fn from(journal: EffectJournal) -> Self {
        Self::Journal(journal)
    }
}

/// Terminal result category for a successfully observed action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcomeKind {
    /// The action completed within its cancellation and deadline contract.
    Completed,
    /// Cancellation arrived after at least one externally visible effect.
    CancelledAfterEffect,
    /// The monotonic deadline elapsed after at least one externally visible effect.
    DeadlineExceededAfterEffect,
}

/// A terminal ordinary-action result with explicit effect evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputOutcome {
    /// Correlated public command.
    pub command_id: CommandId,
    /// Terminal semantic category.
    pub kind: InputOutcomeKind,
    /// Number of successfully serialized XTEST events.
    pub events_emitted: usize,
    /// Complete safe-boundary units, such as clicks or scroll notches.
    pub completed_units: u16,
    /// Requested final pointer endpoint, when the action contains motion.
    pub requested_pointer: Option<RootPoint>,
    /// Last same-connection pointer observation, when available.
    pub observed_pointer: Option<RootPoint>,
    /// Last QueryPointer logical button-mask evidence.
    pub observed_logical_buttons_1_to_5: Option<[bool; 5]>,
    /// The action's mapped button lacked a mask bit or mapping history was ambiguous.
    pub button_observation_partial: bool,
    /// Ordered provisional/confirmed physical-effect evidence.
    pub effects: InputEffectEvidence,
    /// Redacted actor-side keyboard resolution evidence, when applicable.
    pub keyboard: Option<Box<KeyboardOutcomeEvidence>>,
}

/// Stable failure code that never contains a backend error string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFailureKind {
    /// Cancellation was already requested before any effect.
    CancelledBeforeEffect,
    /// The deadline elapsed before any effect.
    DeadlineExceededBeforeEffect,
    /// The actor is not healthy enough to admit ordinary input.
    HealthRejected,
    /// An exact observed target birth failed near-effect revalidation.
    TargetStale,
    /// The exact input target no longer owned focus at the near-effect boundary.
    FocusLost,
    /// Required near-effect target/focus evidence could not be obtained.
    PreconditionUnavailable,
    /// Core ownership validation rejected the transition.
    StateRejected,
    /// This lane has not implemented the otherwise valid operation yet.
    UnsupportedOperation,
    /// The connected backend cannot uniquely realize the requested logical input.
    UnsupportedByBackend,
    /// Active shortcut modifier state conflicts with the requested keyboard semantics.
    ModifierConflict,
    /// Modifier state changed after physical effects began; retry is unsafe.
    ModifierConflictAfterEffect,
    /// A scalar has no exact current-layout physical binding.
    TextNotRepresentable,
    /// Keyboard mapping changed before any effect and the request must be resolved again.
    KeyboardMappingChangedBeforeEffect,
    /// Keyboard mapping changed after an effect; automatic retry is unsafe.
    KeyboardMappingChangedAfterEffect,
    /// Temporary exact-text mapping could not be installed and verified.
    TemporaryMappingInstallFailed,
    /// Exact restoration of a temporary mapping could not be proved.
    TemporaryMappingRestoreFailed,
    /// An X request could not be serialized or the connection was lost.
    BackendUnavailable,
    /// At least one retained checked XTEST cookie failed.
    CheckedRequestFailed,
    /// The same-connection reply-producing observation barrier failed.
    BarrierFailed,
    /// An observed pointer or button state missed a required postcondition.
    PostconditionFailed,
    /// Pointer mapping changed inside a logical-button execution bracket.
    ///
    /// The action may have affected a different logical button and is never
    /// safe for an SDK to retry automatically.
    ButtonMappingChangedAfterEffect,
    /// Conservative reset could not prove owned input was released.
    ResetFailed,
    /// The actor is stopping or has stopped.
    ActorStopped,
    /// The bounded coalesced-control waiter set is full.
    ControlQueueFull,
    /// The actor thread unwound and was caught at its supervision boundary.
    ActorPanicked,
}

/// Terminal input failure with effect and cleanup evidence but no backend text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputFailure {
    /// Correlated command, absent for actor-level controls.
    pub command_id: Option<CommandId>,
    /// Stable failure category.
    pub kind: InputFailureKind,
    /// Ordinary XTEST events serialized before failure; cleanup is excluded.
    pub events_emitted: usize,
    /// Fully confirmed ordinary primitives, clicks, or scroll notches.
    pub completed_units: u16,
    /// Whether event/unit counters are complete rather than panic-boundary placeholders.
    pub progress_known: bool,
    /// Requested final pointer endpoint, when the action contains motion.
    pub requested_pointer: Option<RootPoint>,
    /// Last valid same-connection pointer observation before failure.
    pub last_observed_pointer: Option<RootPoint>,
    /// Last valid QueryPointer logical button-mask evidence before failure.
    pub observed_logical_buttons_1_to_5: Option<[bool; 5]>,
    /// The action's mapped button lacked a mask bit or mapping history was ambiguous.
    pub button_observation_partial: bool,
    /// Ordinary-action evidence accumulated before failure.
    pub effects: Option<Box<InputEffectEvidence>>,
    /// Conservative cleanup result when cleanup was attempted.
    pub cleanup: Option<Box<InputCleanupEvidence>>,
    /// Redacted actor-side keyboard resolution evidence accumulated before failure.
    pub keyboard: Option<Box<KeyboardOutcomeEvidence>>,
}

impl fmt::Display for InputFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "physical input failed: {:?}", self.kind)
    }
}

impl std::error::Error for InputFailure {}

/// Observable state of the actor OS thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorThreadState {
    /// Startup and capability probing are in progress.
    Starting,
    /// The actor loop is running.
    Running,
    /// An orderly shutdown completed.
    Stopped,
    /// A panic was caught at the thread boundary.
    Panicked,
}

/// Latest cheaply cloneable actor health evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputHealthSnapshot {
    /// Core ownership/uncertainty health.
    pub input: InputHealth,
    /// Actor thread lifecycle.
    pub thread: ActorThreadState,
    /// Most recently inverted server pointer mapping.
    pub button_mapping: Option<ButtonMapping>,
    /// Core server minimum keycode advertised at actor startup.
    pub min_keycode: u8,
    /// Core server maximum keycode advertised at actor startup.
    pub max_keycode: u8,
    /// Actor-owned keyboard-model availability and identity.
    pub keyboard_model: KeyboardModelDiagnostics,
}

/// Evidence produced by one explicit conservative release pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    /// Number of owned releases attempted.
    pub attempted: usize,
    /// Number of releases covered by successful cookies and the barrier.
    pub confirmed: usize,
    /// QueryPointer evidence for mapped logical buttons one through five.
    ///
    /// `None` means the barrier or observation failed; it is never fabricated
    /// as an all-released mask.
    pub observed_logical_buttons_1_to_5: Option<[bool; 5]>,
    /// Owned physical buttons whose current logical mapping has no mask bit.
    pub unobservable_buttons: Vec<PhysicalButton>,
    /// Complete QueryKeymap pressed-key evidence, including external keys.
    pub observed_pressed_keys: Option<Vec<PhysicalKey>>,
    /// Xenoteer-owned buttons still retained after the attempt.
    pub residual_owned_buttons: Vec<PhysicalButton>,
    /// Xenoteer-owned keys still retained after the attempt.
    pub residual_owned_keys: Vec<PhysicalKey>,
    /// Whether cleanup retried a persisted temporary keyboard mapping restoration.
    pub temporary_mapping_restore_attempted: bool,
    /// Whether exact mapping readback and model synchronization proved restoration.
    pub temporary_mapping_restore_proven: bool,
    /// Whether all required checked-request, barrier, and observation proofs succeeded.
    pub succeeded: bool,
}

/// Result of a coalesced control-channel operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOutcome {
    /// A liveness/health probe completed.
    Probe(InputHealthSnapshot),
    /// Conservative reset completed.
    Reset(InputCleanupEvidence),
    /// Shutdown cleanup completed and the actor stopped accepting work.
    Shutdown(InputCleanupEvidence),
}
