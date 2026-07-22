//! Backend-independent Xenoteer domain logic.

#![forbid(unsafe_code)]

pub mod config;
pub mod coordinator;
pub mod domain;
pub mod input;

pub use config::{
    AuthConfig, Config, ConfigDiagnostic, ConfigDiagnosticKind, ConfigLoadError, ConfigOverrides,
    DesktopConfig, InputConfig, LimitsConfig, LoggingConfig, MAX_ACCEPTED_COMMANDS_PER_DAEMON,
    MAX_ACCEPTED_COMMANDS_PER_PRINCIPAL, MAX_DEFAULT_ACTION_TIMEOUT_MS, MAX_INPUT_QUEUE_CAPACITY,
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
