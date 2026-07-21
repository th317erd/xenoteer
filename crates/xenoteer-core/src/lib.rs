//! Backend-independent Xenoteer domain logic.

#![forbid(unsafe_code)]

pub mod config;
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
