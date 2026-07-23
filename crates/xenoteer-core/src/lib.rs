//! Backend-independent Xenoteer domain logic.

#![forbid(unsafe_code)]

pub mod accessibility;
pub mod accessibility_click;
pub mod accessibility_correlation;
pub mod config;
pub mod coordinator;
pub mod domain;
pub mod input;
pub mod window;
pub mod window_geometry;
pub mod window_query;

pub use accessibility::{
    AccessibilityCache, AccessibilityContinuationDescriptor, AccessibilityGraphStatus,
    AccessibilityModelError, AccessibilityModelLimits, AccessibilityQueryDeadline,
    AccessibilityQueryError, AccessibilityQueryProjection, AccessibilityResyncBarrier,
    AccessibilityResyncReason, AccessibilitySnapshot, AccessibilityTombstone,
    AccessibilityTraversalOrder, AccessibilityWaitEvaluation, AccessibilityWaitRegistrationError,
    AccessibilityWaitRegistry, AccessibilityWaitToken, DEFAULT_MAX_ACCESSIBILITY_TOMBSTONES,
    DEFAULT_MAX_LIVE_ACCESSIBILITY_NODES, PreparedAccessibilityWait, QueryLimit,
    accessibility_selector_fingerprint,
};
pub use accessibility_click::{
    ElementClickGeometryEvidence, ElementClickObservation, ElementClickOcclusionSnapshot,
    ElementClickPlanError, MAX_ELEMENT_CLICK_OCCLUDERS, PhysicalElementClickPlan,
    RevalidatedElementClick, plan_physical_element_click, revalidate_physical_element_click,
};
pub use accessibility_correlation::{
    AccessibilityCorrelationError, AccessibilityCorrelationLimits, AccessibilityCorrelationSubject,
    AccessibilityWindowCandidate, MAX_ACCESSIBILITY_CORRELATION_CANDIDATES,
    MAX_ACCESSIBILITY_CORRELATION_CREATION_DELTA_MS, MAX_ACCESSIBILITY_CORRELATION_FOCUS_DELTA_MS,
    MAX_ACCESSIBILITY_CORRELATION_OBSERVATION_AGE_MS,
    MAX_ACCESSIBILITY_CORRELATION_RAW_STRING_BYTES, MAX_ACCESSIBILITY_CORRELATION_STRING_BYTES,
    MAX_ACCESSIBILITY_CORRELATION_TOTAL_STRING_BYTES, NormalizedCorrelationText,
    correlate_accessibility_window, correlation_authorizes_physical_effect,
};

#[cfg(test)]
mod window_geometry_tests;

pub use config::{
    AccessibilityConfig, AuthConfig, Config, ConfigDiagnostic, ConfigDiagnosticKind,
    ConfigLoadError, ConfigOverrides, DesktopConfig, InputConfig, LimitsConfig, LoggingConfig,
    MAX_ACCEPTED_COMMANDS_PER_DAEMON, MAX_ACCEPTED_COMMANDS_PER_PRINCIPAL,
    MAX_ACCESSIBILITY_CACHED_NODES, MAX_ACCESSIBILITY_EVENT_CAPACITY,
    MAX_ACCESSIBILITY_QUERY_NODES, MAX_ACCESSIBILITY_QUEUE_CAPACITY,
    MAX_ACCESSIBILITY_SNAPSHOT_BYTES, MAX_ACCESSIBILITY_TOKEN_CAPACITY,
    MAX_ACCESSIBILITY_TOMBSTONES, MAX_DEFAULT_ACTION_TIMEOUT_MS, MAX_INPUT_QUEUE_CAPACITY,
    MAX_REQUEST_BODY_LIMIT_BYTES, MAX_RESULT_LEDGER_ENTRIES, MAX_RESULT_LEDGER_TTL_SECONDS,
    RedactedConfigSummary, SecretFile, ServerConfig, ValidationErrors, ValidationIssue,
    ViewerConfig,
};
pub use coordinator::{
    BoxCoordinatorFuture, CancelCommandOutcome, CanonicalCommandHash, CommandEffect,
    CommandEventMapper, CommandExecutor, CommandLedger, CommandLedgerLimits, CommandRecord,
    CommandRecordState, CommandSubmission, CommandTerminal, CoordinatorError, CoordinatorEvent,
    CoordinatorHandle, CoordinatorSettings, EventHub, EventHubLimits, EventRecord,
    EventSubscription, ExecutionContext, ExecutionOutcome, ExecutionStop, GenerationFence,
    GenerationToken, IdempotencyDecision, LeaseMachine, LeasePhase, LeasePolicy, LeaseRequirement,
    MonotonicMillis, PrincipalId, ReplayResult, ResetOutcome, ResetRequest, ResetRetryOutcome,
    TerminalCause, spawn_coordinator, spawn_coordinator_with_event_mapper,
};
pub use window::{
    DEFAULT_MAX_LIVE_WINDOWS, DEFAULT_MAX_WINDOW_TOMBSTONES, DEFAULT_WINDOW_TOMBSTONE_TTL_MS,
    ResolvedWindow, WindowModel, WindowModelChange, WindowModelError, WindowModelLimits,
    WindowTombstone,
};
pub use window_query::{
    WindowContinuationDescriptor, WindowContinuationQuery, WindowPageProjection, WindowQueryError,
    WindowQueryRecord, WindowQueryView, WindowResolveProjection, WindowSelectorFingerprint,
    WindowWaitEvaluation,
};
