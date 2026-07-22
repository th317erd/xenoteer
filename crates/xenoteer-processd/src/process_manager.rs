//! Registered application launch and process-group lifecycle ownership.
//!
//! The manager is the only component allowed to create or terminate application
//! processes. Callers select an image-owned profile and provide data that is
//! checked against that profile; they never provide an executable, shell
//! fragment, process identifier, or signal.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs,
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use nix::{
    errno::Errno,
    sys::{
        signal::{Signal, killpg},
        wait::{Id, WaitPidFlag, WaitStatus, waitid},
    },
    unistd::Pid,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::{MissedTickBehavior, interval, sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xenoteer_protocol::{DesktopGeneration, MAX_TERMINATION_GRACE_MS};

const MAX_APPLICATION_ID_BYTES: usize = 128;
const MAX_PRINCIPAL_ID_BYTES: usize = 128;
const MAX_ARGUMENT_COUNT: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 4 * 1024;
const MAX_ARGV_BYTES: usize = 64 * 1024;
const MAX_ENVIRONMENT_KEYS: usize = 64;
const MAX_ENVIRONMENT_KEY_BYTES: usize = 128;
const MAX_ENVIRONMENT_VALUE_BYTES: usize = 16 * 1024;
const MAX_ENVIRONMENT_BYTES: usize = 64 * 1024;
const MAX_WORKING_ROOTS: usize = 16;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Credentials applied to every GUI application child before `exec`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChildIdentity {
    uid: u32,
    gid: u32,
}

impl ChildIdentity {
    /// Creates a fixed UID/GID launch boundary.
    pub(crate) const fn new(uid: u32, gid: u32) -> Self {
        Self { uid, gid }
    }
}

/// A validated rule for one caller-supplied argv value.
#[derive(Clone, Debug)]
pub(crate) enum ArgumentRule {
    /// Requires this exact, profile-owned value (normally a fixed flag).
    Literal(String),
    /// Requires membership in a finite, profile-owned value set.
    OneOf(BTreeSet<String>),
    /// Accepts bounded text without applying shell interpretation.
    Text {
        maximum_bytes: usize,
        allow_empty: bool,
        allow_leading_hyphen: bool,
    },
    /// Accepts a base-10 integer within an inclusive interval.
    Integer { minimum: i64, maximum: i64 },
    /// Accepts a lexical relative path with no parent traversal.
    RelativePath { maximum_bytes: usize },
}

impl ArgumentRule {
    fn validate_definition(&self) -> Result<(), ProcessManagerError> {
        match self {
            Self::Literal(value) => validate_argument_storage(value),
            Self::OneOf(values) => {
                if values.is_empty() {
                    return Err(ProcessManagerError::InvalidArgumentSchema);
                }
                for value in values {
                    validate_argument_storage(value)?;
                }
                Ok(())
            }
            Self::Text { maximum_bytes, .. } | Self::RelativePath { maximum_bytes } => {
                if *maximum_bytes == 0 || *maximum_bytes > MAX_ARGUMENT_BYTES {
                    return Err(ProcessManagerError::InvalidArgumentSchema);
                }
                Ok(())
            }
            Self::Integer { minimum, maximum } => {
                if minimum > maximum {
                    return Err(ProcessManagerError::InvalidArgumentSchema);
                }
                Ok(())
            }
        }
    }

    fn validate_value(&self, value: &str, index: usize) -> Result<(), ProcessManagerError> {
        validate_argument_storage(value).map_err(|_| ProcessManagerError::InvalidArgument {
            index,
            reason: "argument_size_or_nul",
        })?;
        match self {
            Self::Literal(expected) if value != expected => {
                Err(ProcessManagerError::InvalidArgument {
                    index,
                    reason: "literal_mismatch",
                })
            }
            Self::OneOf(allowed) if !allowed.contains(value) => {
                Err(ProcessManagerError::InvalidArgument {
                    index,
                    reason: "value_not_allowed",
                })
            }
            Self::Text {
                maximum_bytes,
                allow_empty,
                allow_leading_hyphen,
            } => {
                if value.len() > *maximum_bytes
                    || (!allow_empty && value.is_empty())
                    || (!allow_leading_hyphen && value.starts_with('-'))
                {
                    return Err(ProcessManagerError::InvalidArgument {
                        index,
                        reason: "text_policy",
                    });
                }
                Ok(())
            }
            Self::Integer { minimum, maximum } => {
                let parsed =
                    value
                        .parse::<i64>()
                        .map_err(|_| ProcessManagerError::InvalidArgument {
                            index,
                            reason: "integer_syntax",
                        })?;
                if parsed < *minimum || parsed > *maximum {
                    return Err(ProcessManagerError::InvalidArgument {
                        index,
                        reason: "integer_range",
                    });
                }
                Ok(())
            }
            Self::RelativePath { maximum_bytes } => {
                let path = Path::new(value);
                let components_are_safe = !value.is_empty()
                    && value.len() <= *maximum_bytes
                    && !path.is_absolute()
                    && path.components().all(|component| {
                        matches!(component, Component::Normal(_) | Component::CurDir)
                    });
                if !components_are_safe {
                    return Err(ProcessManagerError::InvalidArgument {
                        index,
                        reason: "relative_path_policy",
                    });
                }
                Ok(())
            }
            Self::Literal(_) | Self::OneOf(_) => Ok(()),
        }
    }
}

/// Positional argv schema for a registered application.
#[derive(Clone, Debug, Default)]
pub(crate) struct ArgumentSchema {
    positional: Vec<ArgumentRule>,
    minimum_count: usize,
    repeated: Option<ArgumentRule>,
    maximum_repeated: usize,
}

impl ArgumentSchema {
    /// Creates an exact positional schema.
    pub(crate) fn exact(positional: Vec<ArgumentRule>) -> Self {
        let minimum_count = positional.len();
        Self {
            positional,
            minimum_count,
            repeated: None,
            maximum_repeated: 0,
        }
    }

    /// Creates a positional schema with optional trailing positions.
    pub(crate) fn positional(
        positional: Vec<ArgumentRule>,
        minimum_count: usize,
    ) -> Result<Self, ProcessManagerError> {
        let schema = Self {
            positional,
            minimum_count,
            repeated: None,
            maximum_repeated: 0,
        };
        schema.validate_definition()?;
        Ok(schema)
    }

    /// Permits at most `maximum_repeated` trailing values under one rule.
    pub(crate) fn with_repeated(
        mut self,
        rule: ArgumentRule,
        maximum_repeated: usize,
    ) -> Result<Self, ProcessManagerError> {
        self.repeated = Some(rule);
        self.maximum_repeated = maximum_repeated;
        self.validate_definition()?;
        Ok(self)
    }

    fn maximum_count(&self) -> usize {
        self.positional.len().saturating_add(self.maximum_repeated)
    }

    fn validate_definition(&self) -> Result<(), ProcessManagerError> {
        if self.minimum_count > self.positional.len()
            || self.maximum_count() > MAX_ARGUMENT_COUNT
            || (self.repeated.is_none() && self.maximum_repeated != 0)
            || (self.repeated.is_some() && self.maximum_repeated == 0)
        {
            return Err(ProcessManagerError::InvalidArgumentSchema);
        }
        for rule in &self.positional {
            rule.validate_definition()?;
        }
        if let Some(rule) = &self.repeated {
            rule.validate_definition()?;
        }
        Ok(())
    }

    fn validate(&self, values: &[String]) -> Result<(), ProcessManagerError> {
        if values.len() < self.minimum_count || values.len() > self.maximum_count() {
            return Err(ProcessManagerError::InvalidArgumentCount {
                minimum: self.minimum_count,
                maximum: self.maximum_count(),
                actual: values.len(),
            });
        }
        for (index, value) in values.iter().enumerate() {
            let rule = self
                .positional
                .get(index)
                .or(self.repeated.as_ref())
                .ok_or(ProcessManagerError::InvalidArgumentCount {
                    minimum: self.minimum_count,
                    maximum: self.maximum_count(),
                    actual: values.len(),
                })?;
            rule.validate_value(value, index)?;
        }
        Ok(())
    }
}

/// Bounds for a caller-overridable environment value.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EnvironmentRule {
    maximum_bytes: usize,
    allow_empty: bool,
}

impl EnvironmentRule {
    /// Creates a non-zero value bound.
    pub(crate) fn new(
        maximum_bytes: usize,
        allow_empty: bool,
    ) -> Result<Self, ProcessManagerError> {
        if maximum_bytes == 0 || maximum_bytes > MAX_ENVIRONMENT_VALUE_BYTES {
            return Err(ProcessManagerError::InvalidEnvironmentPolicy);
        }
        Ok(Self {
            maximum_bytes,
            allow_empty,
        })
    }

    fn validate(self, value: &str) -> bool {
        value.len() <= self.maximum_bytes
            && (self.allow_empty || !value.is_empty())
            && !value.contains('\0')
    }
}

/// Explicit stdin disposition for a registered profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StdinPolicy {
    /// Attach `/dev/null`; release-one launches cannot inject hidden stdin.
    Null,
}

/// Untrusted-free specification used to register one image-owned application.
#[derive(Clone, Debug)]
pub(crate) struct ApplicationProfileSpec {
    pub(crate) application_id: String,
    pub(crate) executable: PathBuf,
    pub(crate) fixed_arguments: Vec<String>,
    pub(crate) argument_schema: ArgumentSchema,
    pub(crate) base_environment: BTreeMap<String, String>,
    pub(crate) allowed_environment: BTreeMap<String, EnvironmentRule>,
    pub(crate) working_directory_roots: Vec<PathBuf>,
    pub(crate) default_working_directory: PathBuf,
    pub(crate) stdin: StdinPolicy,
}

/// Canonical, validated application profile held by the manager.
#[derive(Clone, Debug)]
pub(crate) struct ApplicationProfile {
    application_id: String,
    executable: PathBuf,
    fixed_arguments: Vec<String>,
    argument_schema: ArgumentSchema,
    base_environment: BTreeMap<String, String>,
    allowed_environment: BTreeMap<String, EnvironmentRule>,
    working_directory_roots: Vec<PathBuf>,
    default_working_directory: PathBuf,
    stdin: StdinPolicy,
}

impl ApplicationProfile {
    /// Validates and canonicalizes a trusted profile during startup.
    ///
    /// This performs filesystem metadata work and must be called before entering
    /// latency-sensitive async request handling (or from `spawn_blocking`).
    pub(crate) fn register(spec: ApplicationProfileSpec) -> Result<Self, ProcessManagerError> {
        validate_application_id(&spec.application_id)?;
        spec.argument_schema.validate_definition()?;
        if spec
            .fixed_arguments
            .len()
            .saturating_add(spec.argument_schema.maximum_count())
            > MAX_ARGUMENT_COUNT
        {
            return Err(ProcessManagerError::InvalidArgumentSchema);
        }
        let mut fixed_bytes = 0usize;
        for value in &spec.fixed_arguments {
            validate_argument_storage(value)?;
            fixed_bytes = fixed_bytes.saturating_add(value.len().saturating_add(1));
        }
        if fixed_bytes > MAX_ARGV_BYTES {
            return Err(ProcessManagerError::ArgumentBytesExceeded);
        }

        let executable = canonical_executable(&spec.executable)?;
        if spec.working_directory_roots.is_empty()
            || spec.working_directory_roots.len() > MAX_WORKING_ROOTS
        {
            return Err(ProcessManagerError::InvalidWorkingRoots);
        }
        let mut roots = Vec::with_capacity(spec.working_directory_roots.len());
        for root in spec.working_directory_roots {
            let canonical = canonical_directory(&root)
                .map_err(|source| ProcessManagerError::WorkingDirectoryIo { source })?;
            roots.push(canonical);
        }
        roots.sort();
        roots.dedup();
        let default_working_directory = canonical_directory(&spec.default_working_directory)
            .map_err(|source| ProcessManagerError::WorkingDirectoryIo { source })?;
        if !is_beneath_any(&default_working_directory, &roots) {
            return Err(ProcessManagerError::WorkingDirectoryOutsideRoots);
        }

        validate_environment_definition(&spec.base_environment, &spec.allowed_environment)?;
        Ok(Self {
            application_id: spec.application_id,
            executable,
            fixed_arguments: spec.fixed_arguments,
            argument_schema: spec.argument_schema,
            base_environment: spec.base_environment,
            allowed_environment: spec.allowed_environment,
            working_directory_roots: roots,
            default_working_directory,
            stdin: spec.stdin,
        })
    }

    fn validate_request(&self, request: &LaunchRequest) -> Result<(), ProcessManagerError> {
        self.argument_schema.validate(&request.arguments)?;
        let argv_bytes = self
            .fixed_arguments
            .iter()
            .chain(request.arguments.iter())
            .fold(0usize, |total, value| {
                total.saturating_add(value.len().saturating_add(1))
            });
        if argv_bytes > MAX_ARGV_BYTES {
            return Err(ProcessManagerError::ArgumentBytesExceeded);
        }
        if request.environment.len() > self.allowed_environment.len()
            || request.environment.len() > MAX_ENVIRONMENT_KEYS
        {
            return Err(ProcessManagerError::EnvironmentLimitExceeded);
        }
        for (key, value) in &request.environment {
            if is_injection_environment_key(key) {
                return Err(ProcessManagerError::EnvironmentKeyForbidden);
            }
            let rule = self
                .allowed_environment
                .get(key)
                .ok_or(ProcessManagerError::EnvironmentKeyNotAllowed)?;
            if !rule.validate(value) {
                return Err(ProcessManagerError::InvalidEnvironmentValue);
            }
        }
        let environment_bytes = self
            .base_environment
            .iter()
            .chain(request.environment.iter())
            .fold(0usize, |total, (key, value)| {
                total
                    .saturating_add(key.len())
                    .saturating_add(value.len())
                    .saturating_add(2)
            });
        if environment_bytes > MAX_ENVIRONMENT_BYTES {
            return Err(ProcessManagerError::EnvironmentLimitExceeded);
        }
        Ok(())
    }
}

/// Caller-controlled values admitted by one registered application profile.
#[derive(Clone, Debug)]
pub(crate) struct LaunchRequest {
    principal_id: String,
    application_id: String,
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
    working_directory: Option<PathBuf>,
}

impl LaunchRequest {
    /// Creates a request with no optional arguments, environment, or cwd.
    pub(crate) fn new(principal_id: impl Into<String>, application_id: impl Into<String>) -> Self {
        Self {
            principal_id: principal_id.into(),
            application_id: application_id.into(),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            working_directory: None,
        }
    }

    /// Adds the complete caller argument vector.
    pub(crate) fn with_arguments(mut self, arguments: Vec<String>) -> Self {
        self.arguments = arguments;
        self
    }

    /// Adds caller environment overrides, all of which require profile rules.
    pub(crate) fn with_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        self.environment = environment;
        self
    }

    /// Selects an existing working directory below a profile root.
    pub(crate) fn with_working_directory(mut self, working_directory: PathBuf) -> Self {
        self.working_directory = Some(working_directory);
        self
    }
}

/// Stable, generation-fenced identity for a process launch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProcessRef {
    desktop_generation: DesktopGeneration,
    pid: u32,
    proc_start_ticks: u64,
    launch_id: Uuid,
}

impl ProcessRef {
    pub(crate) const fn from_parts(
        desktop_generation: DesktopGeneration,
        pid: u32,
        proc_start_ticks: u64,
        launch_id: Uuid,
    ) -> Self {
        Self {
            desktop_generation,
            pid,
            proc_start_ticks,
            launch_id,
        }
    }

    /// Returns the desktop lifetime owning this process.
    pub(crate) const fn desktop_generation(&self) -> DesktopGeneration {
        self.desktop_generation
    }

    /// Returns the observed Linux process identifier. It is evidence, not an API
    /// target; callers can only terminate through this full reference.
    pub(crate) const fn pid(&self) -> u32 {
        self.pid
    }

    /// Returns field 22 from the leader's launch-time `/proc/<pid>/stat`.
    pub(crate) const fn proc_start_ticks(&self) -> u64 {
        self.proc_start_ticks
    }

    /// Returns the unguessable identity for this manager-owned launch.
    pub(crate) const fn launch_id(&self) -> Uuid {
        self.launch_id
    }
}

/// One manager-internal PID correlation result in request order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedPidCorrelation {
    pub(crate) pid: u32,
    pub(crate) evidence: ManagedPidCorrelationEvidence,
}

/// Non-authoritative identity evidence for one live PID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManagedPidCorrelationEvidence {
    /// PID and `/proc` start time exactly match a managed leader.
    Leader(ProcessRef),
    /// The live PID belongs to a uniquely verified managed process group.
    ProcessGroup(ProcessRef),
    /// The live PID does not correlate to a retained running record.
    NoMatch,
}

/// Bounded bytes captured from one child stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapturedOutput {
    pub(crate) bytes: Vec<u8>,
    pub(crate) total_bytes: u64,
    pub(crate) truncated: bool,
    pub(crate) complete: bool,
    pub(crate) read_failed: bool,
}

impl CapturedOutput {
    fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            total_bytes: 0,
            truncated: false,
            complete: false,
            read_failed: false,
        }
    }
}

/// Terminal information retained for a managed launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessExit {
    pub(crate) process: ProcessRef,
    pub(crate) principal_id: String,
    pub(crate) application_id: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) core_dumped: bool,
    pub(crate) success: bool,
    pub(crate) termination_requested: bool,
    pub(crate) forced_escalation: bool,
    pub(crate) stdout: CapturedOutput,
    pub(crate) stderr: CapturedOutput,
}

/// Current manager view for an exact process reference.
#[derive(Clone, Debug)]
pub(crate) enum ProcessStatus {
    /// The owned leader task has not completed and been reaped.
    Running {
        process: ProcessRef,
        application_id: String,
    },
    /// A bounded TERM/grace/KILL transaction owns the process group.
    Terminating {
        process: ProcessRef,
        application_id: String,
    },
    /// The leader was reaped and its bounded terminal record is retained.
    Exited(Arc<ProcessExit>),
}

/// One globally ordered process-lifecycle event retained by the manager.
#[derive(Clone, Debug)]
pub(crate) struct SequencedProcessEvent {
    pub(crate) sequence: u64,
    pub(crate) exit: Arc<ProcessExit>,
}

/// Complete manager replay or an explicit retained-history gap.
#[derive(Clone, Debug)]
pub(crate) enum ProcessEventReplay {
    Events {
        latest_sequence: u64,
        events: Vec<Arc<SequencedProcessEvent>>,
    },
    ResyncRequired {
        dropped_through: u64,
        latest_sequence: u64,
    },
}

/// Atomic retained replay plus bounded live delivery.
pub(crate) struct ProcessEventSubscription {
    pub(crate) replay: ProcessEventReplay,
    pub(crate) live: broadcast::Receiver<Arc<SequencedProcessEvent>>,
}

/// Process count, queue, output, and shutdown limits.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcessManagerLimits {
    maximum_processes: usize,
    request_queue_capacity: usize,
    exit_history_capacity: usize,
    event_capacity: usize,
    maximum_output_bytes_per_stream: usize,
    termination_grace: Duration,
    output_drain_timeout: Duration,
}

impl ProcessManagerLimits {
    /// Creates a complete non-zero limit set.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        maximum_processes: usize,
        request_queue_capacity: usize,
        exit_history_capacity: usize,
        event_capacity: usize,
        maximum_output_bytes_per_stream: usize,
        termination_grace: Duration,
        output_drain_timeout: Duration,
    ) -> Result<Self, ProcessManagerError> {
        if maximum_processes == 0
            || request_queue_capacity == 0
            || exit_history_capacity == 0
            || event_capacity == 0
            || maximum_output_bytes_per_stream == 0
            || termination_grace.is_zero()
            || output_drain_timeout.is_zero()
        {
            return Err(ProcessManagerError::InvalidLimits);
        }
        Ok(Self {
            maximum_processes,
            request_queue_capacity,
            exit_history_capacity,
            event_capacity,
            maximum_output_bytes_per_stream,
            termination_grace,
            output_drain_timeout,
        })
    }

    /// Maximum successful launch identities that can still be named by the
    /// manager: every live process plus every retained terminal record.
    pub(crate) const fn launch_identity_capacity(self) -> usize {
        self.maximum_processes
            .saturating_add(self.exit_history_capacity)
    }

    /// Bounds cached launch failures independently from successful identities.
    pub(crate) const fn launch_failure_capacity(self) -> usize {
        self.request_queue_capacity
    }
}

impl Default for ProcessManagerLimits {
    fn default() -> Self {
        Self {
            maximum_processes: 32,
            request_queue_capacity: 64,
            exit_history_capacity: 64,
            event_capacity: 128,
            maximum_output_bytes_per_stream: 256 * 1024,
            termination_grace: Duration::from_secs(3),
            output_drain_timeout: Duration::from_secs(1),
        }
    }
}

/// Cloneable command boundary for the sole process-manager owner task.
#[derive(Clone)]
pub(crate) struct ProcessManagerHandle {
    requests: mpsc::Sender<ManagerRequest>,
}

impl ProcessManagerHandle {
    /// Launches one registered application after all admission checks pass.
    pub(crate) async fn launch(
        &self,
        request: LaunchRequest,
    ) -> Result<ProcessRef, ProcessManagerError> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(ManagerRequest::Launch { request, reply })
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)?;
        result
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)?
    }

    /// Returns running or retained terminal state for an exact reference.
    pub(crate) async fn status(
        &self,
        process: ProcessRef,
    ) -> Result<ProcessStatus, ProcessManagerError> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(ManagerRequest::Status { process, reply })
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)?;
        result
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)?
    }

    /// Correlates a bounded PID batch without granting process authority.
    pub(crate) async fn correlate_pids(
        &self,
        desktop_generation: DesktopGeneration,
        pids: Vec<u32>,
    ) -> Result<Vec<ManagedPidCorrelation>, ProcessManagerError> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(ManagerRequest::CorrelatePids {
                desktop_generation,
                pids,
                reply,
            })
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)?;
        result
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)?
    }

    /// Terminates only the verified group named by a manager-issued reference.
    pub(crate) async fn terminate(
        &self,
        process: ProcessRef,
        grace_override: Option<Duration>,
    ) -> Result<Arc<ProcessExit>, ProcessManagerError> {
        if grace_override
            .is_some_and(|grace| grace > Duration::from_millis(MAX_TERMINATION_GRACE_MS.into()))
        {
            return Err(ProcessManagerError::TerminationGraceExceeded);
        }
        let (reply, result) = oneshot::channel();
        self.requests
            .send(ManagerRequest::Terminate {
                process,
                grace_override,
                reply,
            })
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)?;
        result
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)?
    }

    /// Atomically installs live delivery before capturing a bounded replay.
    pub(crate) async fn subscribe_events(
        &self,
        since_sequence: Option<u64>,
    ) -> Result<ProcessEventSubscription, ProcessManagerError> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(ManagerRequest::SubscribeEvents {
                since_sequence,
                reply,
            })
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)?;
        result
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)
    }
}

/// Owned cancellation and join boundary for the process manager.
pub(crate) struct ProcessManagerJoin {
    cancellation: CancellationToken,
    join: Option<JoinHandle<Result<(), ProcessManagerError>>>,
}

impl ProcessManagerJoin {
    /// Cancels admission, terminates all verified process groups, and reaps every
    /// directly owned child before returning.
    pub(crate) async fn shutdown(mut self) -> Result<(), ProcessManagerError> {
        self.cancellation.cancel();
        let join = self
            .join
            .take()
            .ok_or(ProcessManagerError::ManagerUnavailable)?;
        join.await
            .map_err(|_| ProcessManagerError::ManagerTaskPanicked)?
    }
}

impl Drop for ProcessManagerJoin {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Starts the sole manager owner task for one desktop generation.
pub(crate) fn spawn_process_manager(
    desktop_generation: DesktopGeneration,
    profiles: impl IntoIterator<Item = ApplicationProfile>,
    child_identity: ChildIdentity,
    limits: ProcessManagerLimits,
) -> Result<(ProcessManagerHandle, ProcessManagerJoin), ProcessManagerError> {
    let mut profiles_by_id = HashMap::new();
    for profile in profiles {
        let id = profile.application_id.clone();
        if profiles_by_id.insert(id, profile).is_some() {
            return Err(ProcessManagerError::DuplicateApplicationProfile);
        }
    }
    if profiles_by_id.is_empty() {
        return Err(ProcessManagerError::NoApplicationProfiles);
    }

    let (request_tx, request_rx) = mpsc::channel(limits.request_queue_capacity);
    let (event_tx, _) = broadcast::channel(limits.event_capacity);
    let cancellation = CancellationToken::new();
    let actor_cancellation = cancellation.clone();
    let actor_events = event_tx.clone();
    let join = tokio::spawn(async move {
        ProcessManagerActor {
            desktop_generation,
            child_identity,
            profiles: profiles_by_id,
            limits,
            requests: request_rx,
            events: actor_events,
            cancellation: actor_cancellation,
            running: HashMap::new(),
            exited: VecDeque::new(),
            latest_event_sequence: 0,
            dropped_events_through: 0,
        }
        .run()
        .await
    });
    Ok((
        ProcessManagerHandle {
            requests: request_tx,
        },
        ProcessManagerJoin {
            cancellation,
            join: Some(join),
        },
    ))
}

enum ManagerRequest {
    Launch {
        request: LaunchRequest,
        reply: oneshot::Sender<Result<ProcessRef, ProcessManagerError>>,
    },
    Status {
        process: ProcessRef,
        reply: oneshot::Sender<Result<ProcessStatus, ProcessManagerError>>,
    },
    CorrelatePids {
        desktop_generation: DesktopGeneration,
        pids: Vec<u32>,
        reply: oneshot::Sender<Result<Vec<ManagedPidCorrelation>, ProcessManagerError>>,
    },
    Terminate {
        process: ProcessRef,
        grace_override: Option<Duration>,
        reply: oneshot::Sender<Result<Arc<ProcessExit>, ProcessManagerError>>,
    },
    SubscribeEvents {
        since_sequence: Option<u64>,
        reply: oneshot::Sender<ProcessEventSubscription>,
    },
}

struct ManagedProcess {
    process: ProcessRef,
    application_id: String,
    cancellation: CancellationToken,
    control: mpsc::Sender<ProcessControl>,
    termination_requested: bool,
    join: JoinHandle<Result<Arc<ProcessExit>, ProcessManagerError>>,
}

enum ProcessControl {
    Terminate {
        grace: Duration,
        reply: oneshot::Sender<Result<Arc<ProcessExit>, ProcessManagerError>>,
    },
}

struct ProcessManagerActor {
    desktop_generation: DesktopGeneration,
    child_identity: ChildIdentity,
    profiles: HashMap<String, ApplicationProfile>,
    limits: ProcessManagerLimits,
    requests: mpsc::Receiver<ManagerRequest>,
    events: broadcast::Sender<Arc<SequencedProcessEvent>>,
    cancellation: CancellationToken,
    running: HashMap<Uuid, ManagedProcess>,
    exited: VecDeque<Arc<SequencedProcessEvent>>,
    latest_event_sequence: u64,
    dropped_events_through: u64,
}

impl ProcessManagerActor {
    async fn run(mut self) -> Result<(), ProcessManagerError> {
        let mut poll = interval(PROCESS_POLL_INTERVAL);
        poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let result = loop {
            tokio::select! {
                () = self.cancellation.cancelled() => break Ok(()),
                request = self.requests.recv() => {
                    let Some(request) = request else {
                        break Ok(());
                    };
                    if let Err(error) = self.collect_finished().await {
                        break Err(error);
                    }
                    self.handle_request(request).await;
                }
                _ = poll.tick(), if !self.running.is_empty() => {
                    if let Err(error) = self.collect_finished().await {
                        break Err(error);
                    }
                }
            }
        };
        self.shutdown_all().await?;
        result
    }

    async fn handle_request(&mut self, request: ManagerRequest) {
        match request {
            ManagerRequest::Launch { request, reply } => {
                let result = self.launch(request).await;
                let _ignored = reply.send(result);
            }
            ManagerRequest::Status { process, reply } => {
                let result = self.status(&process);
                let _ignored = reply.send(result);
            }
            ManagerRequest::CorrelatePids {
                desktop_generation,
                pids,
                reply,
            } => {
                let result = self.correlate_pids(desktop_generation, pids).await;
                let _ignored = reply.send(result);
            }
            ManagerRequest::Terminate {
                process,
                grace_override,
                reply,
            } => {
                self.request_termination(process, grace_override, reply);
            }
            ManagerRequest::SubscribeEvents {
                since_sequence,
                reply,
            } => {
                // Subscribe before taking the actor-owned snapshot. No exit can
                // be recorded between these operations, so replay + live is
                // gap-free (live duplicates are identified by sequence).
                let live = self.events.subscribe();
                let replay = self.replay_events(since_sequence);
                let _ignored = reply.send(ProcessEventSubscription { replay, live });
            }
        }
    }

    async fn launch(&mut self, request: LaunchRequest) -> Result<ProcessRef, ProcessManagerError> {
        if self.running.len() >= self.limits.maximum_processes {
            return Err(ProcessManagerError::ProcessLimitExceeded);
        }
        validate_principal_id(&request.principal_id)?;
        let profile = self
            .profiles
            .get(&request.application_id)
            .cloned()
            .ok_or(ProcessManagerError::ApplicationNotRegistered)?;
        profile.validate_request(&request)?;
        let requested_cwd = request
            .working_directory
            .clone()
            .unwrap_or_else(|| profile.default_working_directory.clone());
        let roots = profile.working_directory_roots.clone();
        let working_directory = tokio::task::spawn_blocking(move || {
            let canonical = canonical_directory(&requested_cwd)
                .map_err(|source| ProcessManagerError::WorkingDirectoryIo { source })?;
            if !is_beneath_any(&canonical, &roots) {
                return Err(ProcessManagerError::WorkingDirectoryOutsideRoots);
            }
            Ok(canonical)
        })
        .await
        .map_err(|_| ProcessManagerError::FilesystemWorkerPanicked)??;

        let mut command = Command::new(&profile.executable);
        command
            .args(&profile.fixed_arguments)
            .args(&request.arguments)
            .env_clear()
            .envs(&profile.base_environment)
            .envs(&request.environment)
            .current_dir(working_directory)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .uid(self.child_identity.uid)
            .gid(self.child_identity.gid)
            .process_group(0);
        match profile.stdin {
            StdinPolicy::Null => {
                command.stdin(Stdio::null());
            }
        }

        let mut child = command
            .spawn()
            .map_err(|source| ProcessManagerError::Spawn { source })?;
        let pid = child
            .id()
            .ok_or(ProcessManagerError::ProcessIdUnavailable)?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ProcessManagerError::PipeUnavailable("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(ProcessManagerError::PipeUnavailable("stderr"))?;
        let stdout_capture = SharedOutput::new();
        let stderr_capture = SharedOutput::new();
        let stdout_task = tokio::spawn(drain_output(
            stdout,
            stdout_capture.clone(),
            self.limits.maximum_output_bytes_per_stream,
        ));
        let stderr_task = tokio::spawn(drain_output(
            stderr,
            stderr_capture.clone(),
            self.limits.maximum_output_bytes_per_stream,
        ));

        let identity = match read_proc_identity(pid).await {
            Ok(identity) if identity.process_group == pid && identity.start_ticks != 0 => identity,
            Ok(_) => {
                cleanup_failed_launch(&mut child, stdout_task, stderr_task).await;
                return Err(ProcessManagerError::InvalidLaunchIdentity);
            }
            Err(error) => {
                cleanup_failed_launch(&mut child, stdout_task, stderr_task).await;
                return Err(error.into());
            }
        };
        let process = ProcessRef {
            desktop_generation: self.desktop_generation,
            pid,
            proc_start_ticks: identity.start_ticks,
            launch_id: Uuid::new_v4(),
        };
        let process_cancellation = CancellationToken::new();
        let supervisor_cancellation = process_cancellation.clone();
        let (control_tx, control_rx) = mpsc::channel(1);
        let supervisor_process = process.clone();
        let supervisor_principal_id = request.principal_id;
        let application_id = profile.application_id.clone();
        let supervisor_application_id = application_id.clone();
        let limits = self.limits;
        let join = tokio::spawn(async move {
            supervise_process(ProcessSupervisor {
                child,
                process: supervisor_process,
                principal_id: supervisor_principal_id,
                application_id: supervisor_application_id,
                stdout_capture,
                stderr_capture,
                stdout_task,
                stderr_task,
                control: control_rx,
                cancellation: supervisor_cancellation,
                limits,
            })
            .await
        });
        let prior = self.running.insert(
            process.launch_id,
            ManagedProcess {
                process: process.clone(),
                application_id,
                cancellation: process_cancellation,
                control: control_tx,
                termination_requested: false,
                join,
            },
        );
        if prior.is_some() {
            return Err(ProcessManagerError::LaunchIdCollision);
        }
        Ok(process)
    }

    fn status(&self, process: &ProcessRef) -> Result<ProcessStatus, ProcessManagerError> {
        self.validate_generation(process)?;
        if let Some(managed) = self.running.get(&process.launch_id) {
            validate_reference_match(process, &managed.process)?;
            return Ok(if managed.termination_requested {
                ProcessStatus::Terminating {
                    process: managed.process.clone(),
                    application_id: managed.application_id.clone(),
                }
            } else {
                ProcessStatus::Running {
                    process: managed.process.clone(),
                    application_id: managed.application_id.clone(),
                }
            });
        }
        if let Some(exit) = self
            .exited
            .iter()
            .find(|event| event.exit.process.launch_id == process.launch_id)
        {
            validate_reference_match(process, &exit.exit.process)?;
            return Ok(ProcessStatus::Exited(Arc::clone(&exit.exit)));
        }
        Err(ProcessManagerError::ProcessNotManaged)
    }

    async fn correlate_pids(
        &self,
        desktop_generation: DesktopGeneration,
        pids: Vec<u32>,
    ) -> Result<Vec<ManagedPidCorrelation>, ProcessManagerError> {
        validate_correlation_batch(self.desktop_generation, desktop_generation, &pids)?;
        let managed = self
            .running
            .values()
            .map(|record| record.process.clone())
            .collect::<Vec<_>>();
        let manager_generation = self.desktop_generation;
        tokio::task::spawn_blocking(move || {
            correlate_pid_identities(
                manager_generation,
                desktop_generation,
                &pids,
                &managed,
                read_proc_identity_sync,
            )
        })
        .await
        .map_err(|_| ProcessManagerError::FilesystemWorkerPanicked)?
    }

    fn request_termination(
        &mut self,
        process: ProcessRef,
        grace_override: Option<Duration>,
        reply: oneshot::Sender<Result<Arc<ProcessExit>, ProcessManagerError>>,
    ) {
        if let Err(error) = self.validate_generation(&process) {
            let _ignored = reply.send(Err(error));
            return;
        }
        let Some(managed) = self.running.get_mut(&process.launch_id) else {
            let _ignored = reply.send(Err(ProcessManagerError::ProcessNotManaged));
            return;
        };
        if let Err(error) = validate_reference_match(&process, &managed.process) {
            let _ignored = reply.send(Err(error));
            return;
        }
        if managed.termination_requested {
            let _ignored = reply.send(Err(ProcessManagerError::TerminationAlreadyInProgress));
            return;
        }
        let grace = effective_termination_grace(grace_override, self.limits.termination_grace);
        match managed
            .control
            .try_send(ProcessControl::Terminate { grace, reply })
        {
            Ok(()) => managed.termination_requested = true,
            Err(mpsc::error::TrySendError::Full(ProcessControl::Terminate { reply, .. })) => {
                let _ignored = reply.send(Err(ProcessManagerError::TerminationAlreadyInProgress));
            }
            Err(mpsc::error::TrySendError::Closed(ProcessControl::Terminate { reply, .. })) => {
                let _ignored = reply.send(Err(ProcessManagerError::ProcessNotManaged));
            }
        }
    }

    fn validate_generation(&self, process: &ProcessRef) -> Result<(), ProcessManagerError> {
        if process.desktop_generation != self.desktop_generation {
            return Err(ProcessManagerError::WrongDesktopGeneration);
        }
        Ok(())
    }

    async fn collect_finished(&mut self) -> Result<(), ProcessManagerError> {
        let finished = self
            .running
            .iter()
            .filter_map(|(launch_id, process)| process.join.is_finished().then_some(*launch_id))
            .collect::<Vec<_>>();
        for launch_id in finished {
            let managed = self
                .running
                .remove(&launch_id)
                .ok_or(ProcessManagerError::ProcessNotManaged)?;
            let exit = managed
                .join
                .await
                .map_err(|_| ProcessManagerError::ProcessTaskPanicked)??;
            self.record_exit(exit)?;
        }
        Ok(())
    }

    fn record_exit(&mut self, exit: Arc<ProcessExit>) -> Result<(), ProcessManagerError> {
        let sequence = self
            .latest_event_sequence
            .checked_add(1)
            .ok_or(ProcessManagerError::EventSequenceExhausted)?;
        self.latest_event_sequence = sequence;
        let event = Arc::new(SequencedProcessEvent { sequence, exit });
        self.exited.push_back(Arc::clone(&event));
        while self.exited.len() > self.limits.exit_history_capacity {
            let evicted = self
                .exited
                .pop_front()
                .ok_or(ProcessManagerError::EventHistoryInvariant)?;
            self.dropped_events_through = self.dropped_events_through.max(evicted.sequence);
        }
        let _no_receivers_or_lagged = self.events.send(event);
        Ok(())
    }

    fn replay_events(&self, since_sequence: Option<u64>) -> ProcessEventReplay {
        let Some(since_sequence) = since_sequence else {
            return ProcessEventReplay::Events {
                latest_sequence: self.latest_event_sequence,
                events: Vec::new(),
            };
        };
        if since_sequence > self.latest_event_sequence
            || since_sequence < self.dropped_events_through
        {
            return ProcessEventReplay::ResyncRequired {
                dropped_through: self.dropped_events_through,
                latest_sequence: self.latest_event_sequence,
            };
        }
        ProcessEventReplay::Events {
            latest_sequence: self.latest_event_sequence,
            events: self
                .exited
                .iter()
                .filter(|event| event.sequence > since_sequence)
                .cloned()
                .collect(),
        }
    }

    async fn shutdown_all(&mut self) -> Result<(), ProcessManagerError> {
        self.requests.close();
        for managed in self.running.values() {
            managed.cancellation.cancel();
        }
        let running = std::mem::take(&mut self.running);
        let mut first_error = None;
        for (_, managed) in running {
            match managed.join.await {
                Ok(Ok(exit)) => {
                    if let Err(error) = self.record_exit(exit)
                        && first_error.is_none()
                    {
                        first_error = Some(error);
                    }
                }
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
                Err(_) => {
                    if first_error.is_none() {
                        first_error = Some(ProcessManagerError::ProcessTaskPanicked);
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn effective_termination_grace(requested: Option<Duration>, policy_maximum: Duration) -> Duration {
    requested.unwrap_or(policy_maximum).min(policy_maximum)
}

struct ProcessSupervisor {
    child: Child,
    process: ProcessRef,
    principal_id: String,
    application_id: String,
    stdout_capture: SharedOutput,
    stderr_capture: SharedOutput,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    control: mpsc::Receiver<ProcessControl>,
    cancellation: CancellationToken,
    limits: ProcessManagerLimits,
}

async fn supervise_process(
    supervisor: ProcessSupervisor,
) -> Result<Arc<ProcessExit>, ProcessManagerError> {
    let ProcessSupervisor {
        mut child,
        process,
        principal_id,
        application_id,
        stdout_capture,
        stderr_capture,
        stdout_task,
        stderr_task,
        mut control,
        cancellation,
        limits,
    } = supervisor;
    let (status, termination_requested, forced_escalation) = tokio::select! {
        observed = wait_for_unreaped_exit(process.pid) => {
            observed?;
            // WNOWAIT keeps the dead leader's PID/start-time/PGID allocated.
            // Kill the exact still-reserved group once, then reap the leader.
            // There is no later retry after reaping where PGID reuse could race.
            let _cleaned_descendants = signal_group_present(process.pid, Signal::SIGKILL)?;
            let status = child.wait().await.map_err(|source| ProcessManagerError::Wait { source })?;
            (status, false, false)
        }
        command = control.recv() => {
            match command {
                Some(ProcessControl::Terminate { grace, reply }) => {
                    match terminate_and_reap(&mut child, &process, grace).await {
                        Ok((status, forced)) => {
                            let exit = finish_exit(
                                process,
                                principal_id,
                                application_id,
                                status,
                                true,
                                forced,
                                stdout_capture,
                                stderr_capture,
                                stdout_task,
                                stderr_task,
                                limits.output_drain_timeout,
                            ).await;
                            let _ignored = reply.send(Ok(Arc::clone(&exit)));
                            return Ok(exit);
                        }
                        Err(error) => {
                            let status = kill_owned_group_and_reap(&mut child).await?;
                            let exit = finish_exit(
                                process,
                                principal_id,
                                application_id,
                                status,
                                true,
                                true,
                                stdout_capture,
                                stderr_capture,
                                stdout_task,
                                stderr_task,
                                limits.output_drain_timeout,
                            ).await;
                            let _ignored = reply.send(Err(error));
                            return Ok(exit);
                        }
                    }
                }
                None => terminate_and_reap(&mut child, &process, limits.termination_grace)
                    .await
                    .map(|(status, forced)| (status, true, forced))?,
            }
        }
        () = cancellation.cancelled() => {
            terminate_and_reap(&mut child, &process, limits.termination_grace)
                .await
                .map(|(status, forced)| (status, true, forced))?
        }
    };
    Ok(finish_exit(
        process,
        principal_id,
        application_id,
        status,
        termination_requested,
        forced_escalation,
        stdout_capture,
        stderr_capture,
        stdout_task,
        stderr_task,
        limits.output_drain_timeout,
    )
    .await)
}

#[allow(clippy::too_many_arguments)]
async fn finish_exit(
    process: ProcessRef,
    principal_id: String,
    application_id: String,
    status: ExitStatus,
    termination_requested: bool,
    forced_escalation: bool,
    stdout_capture: SharedOutput,
    stderr_capture: SharedOutput,
    mut stdout_task: JoinHandle<()>,
    mut stderr_task: JoinHandle<()>,
    output_drain_timeout: Duration,
) -> Arc<ProcessExit> {
    finish_output_reader(&mut stdout_task, output_drain_timeout).await;
    finish_output_reader(&mut stderr_task, output_drain_timeout).await;
    Arc::new(ProcessExit {
        process,
        principal_id,
        application_id,
        exit_code: status.code(),
        signal: status.signal(),
        core_dumped: status.core_dumped(),
        success: status.success(),
        termination_requested,
        forced_escalation,
        stdout: stdout_capture.snapshot(),
        stderr: stderr_capture.snapshot(),
    })
}

async fn wait_for_unreaped_exit(pid: u32) -> Result<(), ProcessManagerError> {
    let pid = checked_nix_pid(pid)?;
    loop {
        match waitid(
            Id::Pid(pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG,
        ) {
            Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => return Ok(()),
            Ok(WaitStatus::StillAlive) => sleep(PROCESS_POLL_INTERVAL).await,
            Ok(_) => return Err(ProcessManagerError::UnexpectedWaitStatus),
            Err(source) => return Err(ProcessManagerError::ObserveExit { source }),
        }
    }
}

async fn finish_output_reader(reader: &mut JoinHandle<()>, drain_timeout: Duration) {
    if timeout(drain_timeout, &mut *reader).await.is_err() {
        reader.abort();
        let _observed_abort = reader.await;
    }
}

async fn terminate_and_reap(
    child: &mut Child,
    process: &ProcessRef,
    grace: Duration,
) -> Result<(ExitStatus, bool), ProcessManagerError> {
    match verify_live_process_group(process).await {
        Ok(()) => {}
        Err(ProcessManagerError::ProcIdentityUnavailable) => {
            if let Some(status) = child
                .try_wait()
                .map_err(|source| ProcessManagerError::Wait { source })?
            {
                return Ok((status, false));
            }
            return Err(ProcessManagerError::ProcIdentityUnavailable);
        }
        Err(error) => return Err(error),
    }
    signal_group(process.pid, Signal::SIGTERM)?;
    // Observe but do not reap during grace: WNOWAIT preserves the leader's PID,
    // start-time, and PGID fencing until any remaining group is cleaned.
    match timeout(grace, wait_for_unreaped_exit(process.pid)).await {
        Ok(Ok(())) => {
            let _cleaned_descendants = signal_group_present(process.pid, Signal::SIGKILL)?;
            let status = child
                .wait()
                .await
                .map_err(|source| ProcessManagerError::Wait { source })?;
            return Ok((status, false));
        }
        Ok(Err(error)) => return Err(error),
        Err(_) => {}
    }
    match verify_live_process_group(process).await {
        Ok(()) => signal_group(process.pid, Signal::SIGKILL)?,
        Err(ProcessManagerError::ProcIdentityUnavailable) => {
            if let Some(status) = child
                .try_wait()
                .map_err(|source| ProcessManagerError::Wait { source })?
            {
                return Ok((status, false));
            }
            return Err(ProcessManagerError::ProcIdentityUnavailable);
        }
        Err(error) => return Err(error),
    }
    let status = child
        .wait()
        .await
        .map_err(|source| ProcessManagerError::Wait { source })?;
    Ok((status, true))
}

async fn kill_owned_group_and_reap(child: &mut Child) -> Result<ExitStatus, ProcessManagerError> {
    let pid = child
        .id()
        .ok_or(ProcessManagerError::ProcessIdUnavailable)?;
    child
        .start_kill()
        .map_err(|source| ProcessManagerError::EmergencyKill { source })?;
    wait_for_unreaped_exit(pid).await?;
    let _cleaned_descendants = signal_group_present(pid, Signal::SIGKILL)?;
    child
        .wait()
        .await
        .map_err(|source| ProcessManagerError::Wait { source })
}

async fn cleanup_failed_launch(
    child: &mut Child,
    mut stdout_task: JoinHandle<()>,
    mut stderr_task: JoinHandle<()>,
) {
    let pid = child.id();
    let _kill = child.start_kill();
    if let Some(pid) = pid {
        let _observed = wait_for_unreaped_exit(pid).await;
        let _group_kill = signal_group_present(pid, Signal::SIGKILL);
    }
    let _wait = child.wait().await;
    finish_output_reader(&mut stdout_task, Duration::from_millis(250)).await;
    finish_output_reader(&mut stderr_task, Duration::from_millis(250)).await;
}

fn signal_group(pid: u32, signal: Signal) -> Result<(), ProcessManagerError> {
    let process_group = checked_nix_pid(pid)?;
    match killpg(process_group, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(source) => Err(ProcessManagerError::SignalGroup { source }),
    }
}

fn signal_group_present(pid: u32, signal: Signal) -> Result<bool, ProcessManagerError> {
    let process_group = checked_nix_pid(pid)?;
    match killpg(process_group, signal) {
        Ok(()) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(source) => Err(ProcessManagerError::SignalGroup { source }),
    }
}

fn checked_nix_pid(pid: u32) -> Result<Pid, ProcessManagerError> {
    let raw = i32::try_from(pid).map_err(|_| ProcessManagerError::InvalidProcessId)?;
    if raw <= 0 {
        return Err(ProcessManagerError::InvalidProcessId);
    }
    Ok(Pid::from_raw(raw))
}

#[derive(Clone)]
struct SharedOutput(Arc<Mutex<CapturedOutput>>);

impl SharedOutput {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(CapturedOutput::empty())))
    }

    fn lock(&self) -> MutexGuard<'_, CapturedOutput> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn snapshot(&self) -> CapturedOutput {
        self.lock().clone()
    }
}

async fn drain_output<R>(mut reader: R, capture: SharedOutput, maximum_bytes: usize)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                capture.lock().complete = true;
                return;
            }
            Ok(read) => {
                let mut output = capture.lock();
                output.total_bytes = output.total_bytes.saturating_add(read as u64);
                let remaining = maximum_bytes.saturating_sub(output.bytes.len());
                let retained = remaining.min(read);
                output.bytes.extend_from_slice(&buffer[..retained]);
                output.truncated |= retained < read;
            }
            Err(_) => {
                capture.lock().read_failed = true;
                return;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcIdentity {
    process_group: u32,
    start_ticks: u64,
}

#[derive(Debug)]
enum ProcReadError {
    Unavailable,
    Malformed,
}

impl From<ProcReadError> for ProcessManagerError {
    fn from(error: ProcReadError) -> Self {
        match error {
            ProcReadError::Unavailable => Self::ProcIdentityUnavailable,
            ProcReadError::Malformed => Self::ProcIdentityMalformed,
        }
    }
}

async fn read_proc_identity(pid: u32) -> Result<ProcIdentity, ProcReadError> {
    tokio::task::spawn_blocking(move || read_proc_identity_sync(pid))
        .await
        .map_err(|_| ProcReadError::Unavailable)?
}

fn read_proc_identity_sync(pid: u32) -> Result<ProcIdentity, ProcReadError> {
    let stat =
        fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|_| ProcReadError::Unavailable)?;
    parse_proc_stat(&stat)
}

fn validate_correlation_batch(
    manager_generation: DesktopGeneration,
    requested_generation: DesktopGeneration,
    pids: &[u32],
) -> Result<(), ProcessManagerError> {
    if requested_generation.as_uuid().is_nil() || manager_generation.as_uuid().is_nil() {
        return Err(ProcessManagerError::InvalidCorrelationBatch);
    }
    if requested_generation != manager_generation {
        return Err(ProcessManagerError::WrongDesktopGeneration);
    }
    if pids.is_empty() || pids.len() > crate::MAX_PROCESS_CORRELATION_PIDS {
        return Err(ProcessManagerError::InvalidCorrelationBatch);
    }
    let mut unique = BTreeSet::new();
    if pids.iter().any(|pid| *pid == 0 || !unique.insert(*pid)) {
        return Err(ProcessManagerError::InvalidCorrelationBatch);
    }
    Ok(())
}

fn correlate_pid_identities(
    manager_generation: DesktopGeneration,
    requested_generation: DesktopGeneration,
    pids: &[u32],
    managed: &[ProcessRef],
    mut read_identity: impl FnMut(u32) -> Result<ProcIdentity, ProcReadError>,
) -> Result<Vec<ManagedPidCorrelation>, ProcessManagerError> {
    validate_correlation_batch(manager_generation, requested_generation, pids)?;
    if managed
        .iter()
        .any(|process| process.desktop_generation != manager_generation)
    {
        return Err(ProcessManagerError::EventHistoryInvariant);
    }
    let mut correlations = Vec::with_capacity(pids.len());
    for &pid in pids {
        // Window PIDs are advisory and can disappear or become unreadable
        // independently. Preserve request ordering and let other entries prove
        // their own evidence instead of promoting one target-local race into a
        // batch transport failure.
        let identity = match read_identity(pid) {
            Ok(identity) => identity,
            Err(ProcReadError::Unavailable | ProcReadError::Malformed) => {
                correlations.push(ManagedPidCorrelation {
                    pid,
                    evidence: ManagedPidCorrelationEvidence::NoMatch,
                });
                continue;
            }
        };
        let exact = managed
            .iter()
            .filter(|process| {
                process.pid == pid && process.proc_start_ticks == identity.start_ticks
            })
            .collect::<Vec<_>>();
        if exact.len() > 1 {
            return Err(ProcessManagerError::AmbiguousProcessGroup);
        }
        if let Some(process) = exact.first() {
            correlations.push(ManagedPidCorrelation {
                pid,
                evidence: ManagedPidCorrelationEvidence::Leader((*process).clone()),
            });
            continue;
        }

        // A group leader whose start time does not match a managed record is a
        // reused or unrelated PID, never a descendant of the stale numeric PGID.
        if identity.process_group == pid {
            correlations.push(ManagedPidCorrelation {
                pid,
                evidence: ManagedPidCorrelationEvidence::NoMatch,
            });
            continue;
        }
        let group = managed
            .iter()
            .filter(|process| process.pid == identity.process_group)
            .collect::<Vec<_>>();
        if group.len() > 1 {
            return Err(ProcessManagerError::AmbiguousProcessGroup);
        }
        let evidence = if let Some(process) = group.first() {
            // A stale managed leader cannot authorize a numeric process-group
            // match. This invalidates only the requested PID; batch shape and
            // manager-owned ambiguity remain genuine whole-request failures.
            let leader = read_identity(process.pid);
            if !matches!(
                leader,
                Ok(identity)
                    if identity.start_ticks == process.proc_start_ticks
                        && identity.process_group == process.pid
            ) {
                ManagedPidCorrelationEvidence::NoMatch
            } else {
                ManagedPidCorrelationEvidence::ProcessGroup((*process).clone())
            }
        } else {
            ManagedPidCorrelationEvidence::NoMatch
        };
        correlations.push(ManagedPidCorrelation { pid, evidence });
    }
    Ok(correlations)
}

fn parse_proc_stat(stat: &str) -> Result<ProcIdentity, ProcReadError> {
    let (_, fields) = stat.rsplit_once(") ").ok_or(ProcReadError::Malformed)?;
    let fields = fields.split_ascii_whitespace().collect::<Vec<_>>();
    let process_group = fields
        .get(2)
        .ok_or(ProcReadError::Malformed)?
        .parse::<u32>()
        .map_err(|_| ProcReadError::Malformed)?;
    let start_ticks = fields
        .get(19)
        .ok_or(ProcReadError::Malformed)?
        .parse::<u64>()
        .map_err(|_| ProcReadError::Malformed)?;
    Ok(ProcIdentity {
        process_group,
        start_ticks,
    })
}

async fn verify_live_process_group(process: &ProcessRef) -> Result<(), ProcessManagerError> {
    let current = read_proc_identity(process.pid)
        .await
        .map_err(ProcessManagerError::from)?;
    if current.start_ticks != process.proc_start_ticks || current.process_group != process.pid {
        return Err(ProcessManagerError::ProcessIdentityChanged);
    }
    Ok(())
}

fn validate_reference_match(
    supplied: &ProcessRef,
    managed: &ProcessRef,
) -> Result<(), ProcessManagerError> {
    if supplied != managed {
        return Err(ProcessManagerError::ProcessReferenceMismatch);
    }
    Ok(())
}

fn validate_application_id(value: &str) -> Result<(), ProcessManagerError> {
    if value.is_empty()
        || value.len() > MAX_APPLICATION_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ProcessManagerError::InvalidApplicationId);
    }
    Ok(())
}

fn validate_principal_id(value: &str) -> Result<(), ProcessManagerError> {
    if value.is_empty()
        || value.len() > MAX_PRINCIPAL_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@')
        })
    {
        return Err(ProcessManagerError::InvalidPrincipalId);
    }
    Ok(())
}

fn validate_argument_storage(value: &str) -> Result<(), ProcessManagerError> {
    if value.len() > MAX_ARGUMENT_BYTES || value.contains('\0') {
        return Err(ProcessManagerError::InvalidArgumentSchema);
    }
    Ok(())
}

fn validate_environment_definition(
    base: &BTreeMap<String, String>,
    allowed: &BTreeMap<String, EnvironmentRule>,
) -> Result<(), ProcessManagerError> {
    if base.len() > MAX_ENVIRONMENT_KEYS || allowed.len() > MAX_ENVIRONMENT_KEYS {
        return Err(ProcessManagerError::EnvironmentLimitExceeded);
    }
    let mut total_bytes = 0usize;
    for (key, value) in base {
        validate_environment_key(key)?;
        if value.len() > MAX_ENVIRONMENT_VALUE_BYTES || value.contains('\0') {
            return Err(ProcessManagerError::InvalidEnvironmentValue);
        }
        total_bytes = total_bytes
            .saturating_add(key.len())
            .saturating_add(value.len())
            .saturating_add(2);
    }
    for (key, rule) in allowed {
        validate_environment_key(key)?;
        if is_injection_environment_key(key)
            || rule.maximum_bytes == 0
            || rule.maximum_bytes > MAX_ENVIRONMENT_VALUE_BYTES
        {
            return Err(ProcessManagerError::InvalidEnvironmentPolicy);
        }
    }
    if total_bytes > MAX_ENVIRONMENT_BYTES {
        return Err(ProcessManagerError::EnvironmentLimitExceeded);
    }
    Ok(())
}

fn validate_environment_key(key: &str) -> Result<(), ProcessManagerError> {
    let mut bytes = key.bytes();
    let first_is_valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if !first_is_valid
        || key.len() > MAX_ENVIRONMENT_KEY_BYTES
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(ProcessManagerError::InvalidEnvironmentPolicy);
    }
    Ok(())
}

fn is_injection_environment_key(key: &str) -> bool {
    key.starts_with("LD_")
        || key.starts_with("DYLD_")
        || matches!(
            key,
            "GCONV_PATH"
                | "GTK_MODULES"
                | "GTK_PATH"
                | "NODE_OPTIONS"
                | "PERL5LIB"
                | "PYTHONHOME"
                | "PYTHONPATH"
                | "QT_PLUGIN_PATH"
                | "QT_QPA_PLATFORM_PLUGIN_PATH"
                | "RUBYLIB"
        )
}

fn canonical_executable(path: &Path) -> Result<PathBuf, ProcessManagerError> {
    if !path.is_absolute() {
        return Err(ProcessManagerError::ExecutableMustBeAbsolute);
    }
    let canonical =
        fs::canonicalize(path).map_err(|source| ProcessManagerError::ExecutableIo { source })?;
    let metadata =
        fs::metadata(&canonical).map_err(|source| ProcessManagerError::ExecutableIo { source })?;
    let mode = metadata.permissions().mode();
    if !metadata.is_file() || mode & 0o111 == 0 || mode & 0o6000 != 0 {
        return Err(ProcessManagerError::ExecutablePolicyRejected);
    }
    Ok(canonical)
}

fn canonical_directory(path: &Path) -> Result<PathBuf, std::io::Error> {
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "working directory must be absolute",
        ));
    }
    let canonical = fs::canonicalize(path)?;
    if !fs::metadata(&canonical)?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "working directory is not a directory",
        ));
    }
    Ok(canonical)
}

fn is_beneath_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// Process manager startup, admission, identity, or lifecycle failure.
#[derive(Debug, Error)]
pub(crate) enum ProcessManagerError {
    #[error("process manager limits must all be non-zero")]
    InvalidLimits,
    #[error("at least one registered application profile is required")]
    NoApplicationProfiles,
    #[error("registered application profile IDs must be unique")]
    DuplicateApplicationProfile,
    #[error("application ID is invalid")]
    InvalidApplicationId,
    #[error("authenticated principal ID is invalid")]
    InvalidPrincipalId,
    #[error("registered executable path must be absolute")]
    ExecutableMustBeAbsolute,
    #[error("could not inspect registered executable")]
    ExecutableIo { source: std::io::Error },
    #[error("registered executable is not a regular non-setid executable")]
    ExecutablePolicyRejected,
    #[error("argument schema is invalid")]
    InvalidArgumentSchema,
    #[error("argument count {actual} is outside allowed interval {minimum}..={maximum}")]
    InvalidArgumentCount {
        minimum: usize,
        maximum: usize,
        actual: usize,
    },
    #[error("argument {index} violates profile rule {reason}")]
    InvalidArgument { index: usize, reason: &'static str },
    #[error("aggregate argv byte limit exceeded")]
    ArgumentBytesExceeded,
    #[error("environment policy is invalid")]
    InvalidEnvironmentPolicy,
    #[error("environment key is not allowed for this application")]
    EnvironmentKeyNotAllowed,
    #[error("environment key is an injection-sensitive override")]
    EnvironmentKeyForbidden,
    #[error("environment value violates its profile rule")]
    InvalidEnvironmentValue,
    #[error("environment count or aggregate byte limit exceeded")]
    EnvironmentLimitExceeded,
    #[error("working-directory roots are invalid")]
    InvalidWorkingRoots,
    #[error("could not validate working directory")]
    WorkingDirectoryIo { source: std::io::Error },
    #[error("working directory is outside registered roots")]
    WorkingDirectoryOutsideRoots,
    #[error("application is not registered")]
    ApplicationNotRegistered,
    #[error("managed process count limit exceeded")]
    ProcessLimitExceeded,
    #[error("process manager is unavailable")]
    ManagerUnavailable,
    #[error("process manager task panicked")]
    ManagerTaskPanicked,
    #[error("process supervisor task panicked")]
    ProcessTaskPanicked,
    #[error("filesystem validation worker panicked")]
    FilesystemWorkerPanicked,
    #[error("could not spawn registered application")]
    Spawn { source: std::io::Error },
    #[error("spawned process did not expose a process ID")]
    ProcessIdUnavailable,
    #[error("spawned process did not expose its configured {0} pipe")]
    PipeUnavailable(&'static str),
    #[error("spawned leader was not the expected process-group leader")]
    InvalidLaunchIdentity,
    #[error("process identity is no longer available in /proc")]
    ProcIdentityUnavailable,
    #[error("process identity in /proc is malformed")]
    ProcIdentityMalformed,
    #[error("managed process identity or group changed before signaling")]
    ProcessIdentityChanged,
    #[error("process reference belongs to a different desktop generation")]
    WrongDesktopGeneration,
    #[error("process correlation batch is invalid")]
    InvalidCorrelationBatch,
    #[error("process correlation found ambiguous managed group ownership")]
    AmbiguousProcessGroup,
    #[error("process reference fields do not match the managed launch")]
    ProcessReferenceMismatch,
    #[error("process reference is not managed or has left retained history")]
    ProcessNotManaged,
    #[error("termination is already in progress")]
    TerminationAlreadyInProgress,
    #[error("termination grace exceeds the protocol maximum")]
    TerminationGraceExceeded,
    #[error("process ID cannot name a safe process group")]
    InvalidProcessId,
    #[error("could not signal managed process group")]
    SignalGroup { source: Errno },
    #[error("could not wait for managed process")]
    Wait { source: std::io::Error },
    #[error("could not observe managed process exit before reaping")]
    ObserveExit { source: Errno },
    #[error("managed process returned an unexpected wait status")]
    UnexpectedWaitStatus,
    #[error("could not emergency-kill owned process leader")]
    EmergencyKill { source: std::io::Error },
    #[error("launch identifier collision")]
    LaunchIdCollision,
    #[error("registered profile environment is missing {0}")]
    ProfileEnvironmentMissing(&'static str),
    #[error("managed process returned an invalid terminal status")]
    InvalidExitStatus,
    #[error("managed process event sequence exhausted")]
    EventSequenceExhausted,
    #[error("managed process event history invariant failed")]
    EventHistoryInvariant,
}

#[cfg(test)]
#[path = "process_manager/correlation_tests.rs"]
mod correlation_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_schema_rejects_option_and_path_injection() -> Result<(), ProcessManagerError> {
        let schema = ArgumentSchema::exact(vec![
            ArgumentRule::Text {
                maximum_bytes: 16,
                allow_empty: false,
                allow_leading_hyphen: false,
            },
            ArgumentRule::RelativePath { maximum_bytes: 32 },
        ]);
        schema.validate(&["hello".to_owned(), "folder/file".to_owned()])?;
        assert!(matches!(
            schema.validate(&["--help".to_owned(), "folder/file".to_owned()]),
            Err(ProcessManagerError::InvalidArgument { index: 0, .. })
        ));
        assert!(matches!(
            schema.validate(&["hello".to_owned(), "../escape".to_owned()]),
            Err(ProcessManagerError::InvalidArgument { index: 1, .. })
        ));
        Ok(())
    }

    #[test]
    fn injection_environment_cannot_be_caller_overridable() -> Result<(), ProcessManagerError> {
        let mut allowed = BTreeMap::new();
        allowed.insert("LD_PRELOAD".to_owned(), EnvironmentRule::new(128, false)?);
        assert!(matches!(
            validate_environment_definition(&BTreeMap::new(), &allowed),
            Err(ProcessManagerError::InvalidEnvironmentPolicy)
        ));
        Ok(())
    }

    #[test]
    fn proc_stat_parser_handles_spaces_and_parentheses_in_comm() -> Result<(), ProcessManagerError>
    {
        let stat = "123 (odd ) process name) S 1 123 123 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 987654 0";
        let identity = parse_proc_stat(stat).map_err(ProcessManagerError::from)?;
        assert_eq!(identity.process_group, 123);
        assert_eq!(identity.start_ticks, 987_654);
        Ok(())
    }

    #[tokio::test]
    async fn launches_captures_and_reaps_registered_process() -> Result<(), ProcessManagerError> {
        let profile = test_profile(
            "print",
            Path::new("/usr/bin/printf"),
            vec!["%s".to_owned(), "hello".to_owned()],
            ArgumentSchema::default(),
        )?;
        let (manager, join) = spawn_process_manager(
            DesktopGeneration::new(),
            [profile],
            test_child_identity()?,
            test_limits()?,
        )?;
        let mut subscription = manager.subscribe_events(Some(0)).await?;
        let process = manager
            .launch(LaunchRequest::new("test-owner", "print"))
            .await?;
        let exit = loop {
            match manager.status(process.clone()).await? {
                ProcessStatus::Running { .. } | ProcessStatus::Terminating { .. } => {
                    sleep(Duration::from_millis(10)).await;
                }
                ProcessStatus::Exited(exit) => break exit,
            }
        };
        assert_eq!(exit.stdout.bytes, b"hello");
        assert!(exit.stdout.complete);
        assert!(exit.success);
        let event = subscription
            .live
            .recv()
            .await
            .map_err(|_| ProcessManagerError::ManagerUnavailable)?;
        assert_eq!(event.sequence, 1);
        assert_eq!(event.exit.principal_id, "test-owner");
        assert!(!Path::new(&format!("/proc/{}", process.pid())).exists());
        join.shutdown().await
    }

    #[tokio::test]
    async fn retained_event_replay_is_ordered_and_reports_eviction()
    -> Result<(), ProcessManagerError> {
        let profile = test_profile(
            "exit",
            Path::new("/usr/bin/printf"),
            Vec::new(),
            ArgumentSchema::default(),
        )?;
        let limits = ProcessManagerLimits::new(
            2,
            8,
            1,
            1,
            1_024,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )?;
        let (manager, join) = spawn_process_manager(
            DesktopGeneration::new(),
            [profile],
            test_child_identity()?,
            limits,
        )?;

        for owner in ["owner-one", "owner-two"] {
            let process = manager.launch(LaunchRequest::new(owner, "exit")).await?;
            loop {
                if matches!(
                    manager.status(process.clone()).await?,
                    ProcessStatus::Exited(_)
                ) {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        }

        let evicted = manager.subscribe_events(Some(0)).await?;
        assert!(matches!(
            evicted.replay,
            ProcessEventReplay::ResyncRequired {
                dropped_through: 1,
                latest_sequence: 2,
            }
        ));
        let retained = manager.subscribe_events(Some(1)).await?;
        match retained.replay {
            ProcessEventReplay::Events {
                latest_sequence,
                events,
            } => {
                assert_eq!(latest_sequence, 2);
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].sequence, 2);
                assert_eq!(events[0].exit.principal_id, "owner-two");
            }
            ProcessEventReplay::ResyncRequired { .. } => {
                return Err(ProcessManagerError::EventHistoryInvariant);
            }
        }
        assert!(matches!(
            manager.subscribe_events(Some(3)).await?.replay,
            ProcessEventReplay::ResyncRequired {
                dropped_through: 1,
                latest_sequence: 2,
            }
        ));
        join.shutdown().await
    }

    #[tokio::test]
    async fn launch_owner_is_bounded_and_unambiguous() -> Result<(), ProcessManagerError> {
        let profile = test_profile(
            "print",
            Path::new("/usr/bin/printf"),
            Vec::new(),
            ArgumentSchema::default(),
        )?;
        let (manager, join) = spawn_process_manager(
            DesktopGeneration::new(),
            [profile],
            test_child_identity()?,
            test_limits()?,
        )?;
        assert!(matches!(
            manager
                .launch(LaunchRequest::new("bad owner", "print"))
                .await,
            Err(ProcessManagerError::InvalidPrincipalId)
        ));
        assert!(matches!(
            manager
                .launch(LaunchRequest::new(
                    "x".repeat(MAX_PRINCIPAL_ID_BYTES + 1),
                    "print",
                ))
                .await,
            Err(ProcessManagerError::InvalidPrincipalId)
        ));
        join.shutdown().await
    }

    #[tokio::test]
    async fn terminate_verifies_group_and_reaps_leader() -> Result<(), ProcessManagerError> {
        let profile = test_profile(
            "sleep",
            Path::new("/usr/bin/sleep"),
            Vec::new(),
            ArgumentSchema::exact(vec![ArgumentRule::Integer {
                minimum: 1,
                maximum: 60,
            }]),
        )?;
        let (manager, join) = spawn_process_manager(
            DesktopGeneration::new(),
            [profile],
            test_child_identity()?,
            test_limits()?,
        )?;
        let process = manager
            .launch(LaunchRequest::new("test-owner", "sleep").with_arguments(vec!["30".to_owned()]))
            .await?;
        let exit = manager.terminate(process.clone(), None).await?;
        assert!(exit.termination_requested);
        assert!(!exit.forced_escalation);
        assert!(!Path::new(&format!("/proc/{}", process.pid())).exists());
        join.shutdown().await
    }

    #[tokio::test]
    async fn terminating_state_rejects_a_second_owner() -> Result<(), ProcessManagerError> {
        let profile = test_profile(
            "sleep.single-terminator",
            Path::new("/usr/bin/sleep"),
            vec!["30".to_owned()],
            ArgumentSchema::default(),
        )?;
        let (manager, join) = spawn_process_manager(
            DesktopGeneration::new(),
            [profile],
            test_child_identity()?,
            test_limits()?,
        )?;
        let process = manager
            .launch(LaunchRequest::new("test-owner", "sleep.single-terminator"))
            .await?;
        let first_manager = manager.clone();
        let first_process = process.clone();
        let first = tokio::spawn(async move { first_manager.terminate(first_process, None).await });

        loop {
            match manager.status(process.clone()).await? {
                ProcessStatus::Running { .. } => tokio::task::yield_now().await,
                ProcessStatus::Terminating { .. } => break,
                ProcessStatus::Exited(_) => return Err(ProcessManagerError::ProcessNotManaged),
            }
        }
        assert!(matches!(
            manager.terminate(process, Some(Duration::ZERO)).await,
            Err(ProcessManagerError::TerminationAlreadyInProgress)
        ));
        first
            .await
            .map_err(|_| ProcessManagerError::ProcessTaskPanicked)??;
        join.shutdown().await
    }

    #[tokio::test]
    async fn terminate_carries_zero_grace_and_rejects_over_protocol_maximum()
    -> Result<(), ProcessManagerError> {
        let profile = test_profile(
            "sleep.override",
            Path::new("/usr/bin/sleep"),
            vec!["30".to_owned()],
            ArgumentSchema::default(),
        )?;
        let limits = ProcessManagerLimits::new(
            4,
            8,
            8,
            8,
            1_024,
            Duration::from_secs(5),
            Duration::from_millis(100),
        )?;
        let (manager, join) = spawn_process_manager(
            DesktopGeneration::new(),
            [profile],
            test_child_identity()?,
            limits,
        )?;
        let process = manager
            .launch(LaunchRequest::new("test-owner", "sleep.override"))
            .await?;
        assert!(matches!(
            manager
                .terminate(
                    process.clone(),
                    Some(Duration::from_millis(
                        u64::from(MAX_TERMINATION_GRACE_MS) + 1
                    ))
                )
                .await,
            Err(ProcessManagerError::TerminationGraceExceeded)
        ));
        let exit = timeout(
            Duration::from_secs(1),
            manager.terminate(process, Some(Duration::ZERO)),
        )
        .await
        .map_err(|_| ProcessManagerError::TerminationGraceExceeded)??;
        assert!(exit.termination_requested);
        assert!(exit.forced_escalation);
        join.shutdown().await
    }

    #[test]
    fn requested_termination_grace_is_capped_by_manager_policy() {
        let policy = Duration::from_millis(250);
        assert_eq!(effective_termination_grace(None, policy), policy);
        assert_eq!(
            effective_termination_grace(Some(Duration::from_secs(30)), policy),
            policy
        );
        assert_eq!(
            effective_termination_grace(Some(Duration::ZERO), policy),
            Duration::ZERO
        );
    }

    #[tokio::test]
    async fn natural_leader_exit_kills_unreaped_descendants_before_pid_release()
    -> Result<(), ProcessManagerError> {
        let profile = test_profile(
            "fork",
            Path::new("/bin/sh"),
            vec![
                "-c".to_owned(),
                "sleep 30 & child=$!; printf '%s' \"$child\"; exit 0".to_owned(),
            ],
            ArgumentSchema::default(),
        )?;
        let (manager, join) = spawn_process_manager(
            DesktopGeneration::new(),
            [profile],
            test_child_identity()?,
            test_limits()?,
        )?;
        let process = manager
            .launch(LaunchRequest::new("test-owner", "fork"))
            .await?;
        let exit = loop {
            match manager.status(process.clone()).await? {
                ProcessStatus::Running { .. } | ProcessStatus::Terminating { .. } => {
                    sleep(Duration::from_millis(10)).await;
                }
                ProcessStatus::Exited(exit) => break exit,
            }
        };
        let descendant_pid = String::from_utf8(exit.stdout.bytes.clone())
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or(ProcessManagerError::InvalidProcessId)?;
        assert!(!exit.forced_escalation);
        for _ in 0..50 {
            if !Path::new(&format!("/proc/{descendant_pid}")).exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert!(!Path::new(&format!("/proc/{descendant_pid}")).exists());
        assert!(!Path::new(&format!("/proc/{}", process.pid())).exists());
        join.shutdown().await
    }

    #[tokio::test]
    async fn output_is_drained_but_retained_only_to_the_configured_bound()
    -> Result<(), ProcessManagerError> {
        let profile = test_profile(
            "output",
            Path::new("/usr/bin/printf"),
            vec!["%02048d".to_owned(), "0".to_owned()],
            ArgumentSchema::default(),
        )?;
        let limits = ProcessManagerLimits::new(
            4,
            8,
            8,
            8,
            128,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )?;
        let (manager, join) = spawn_process_manager(
            DesktopGeneration::new(),
            [profile],
            test_child_identity()?,
            limits,
        )?;
        let process = manager
            .launch(LaunchRequest::new("test-owner", "output"))
            .await?;
        let exit = loop {
            match manager.status(process.clone()).await? {
                ProcessStatus::Running { .. } | ProcessStatus::Terminating { .. } => {
                    sleep(Duration::from_millis(10)).await;
                }
                ProcessStatus::Exited(exit) => break exit,
            }
        };
        assert_eq!(exit.stdout.bytes.len(), 128);
        assert_eq!(exit.stdout.total_bytes, 2_048);
        assert!(exit.stdout.truncated);
        assert!(exit.stdout.complete);
        join.shutdown().await
    }

    #[tokio::test]
    async fn child_executes_with_the_configured_desktop_uid_and_gid()
    -> Result<(), ProcessManagerError> {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata("/proc/self")
            .map_err(|source| ProcessManagerError::WorkingDirectoryIo { source })?;
        let identity = ChildIdentity::new(metadata.uid(), metadata.gid());
        for (application_id, flag, expected) in [
            ("identity.uid", "-u", identity.uid),
            ("identity.gid", "-g", identity.gid),
        ] {
            let profile = test_profile(
                application_id,
                Path::new("/usr/bin/id"),
                vec![flag.to_owned()],
                ArgumentSchema::default(),
            )?;
            let (manager, join) = spawn_process_manager(
                DesktopGeneration::new(),
                [profile],
                identity,
                test_limits()?,
            )?;
            let process = manager
                .launch(LaunchRequest::new("test-owner", application_id))
                .await?;
            let exit = loop {
                match manager.status(process.clone()).await? {
                    ProcessStatus::Running { .. } | ProcessStatus::Terminating { .. } => {
                        sleep(Duration::from_millis(10)).await;
                    }
                    ProcessStatus::Exited(exit) => break exit,
                }
            };
            assert_eq!(
                String::from_utf8_lossy(&exit.stdout.bytes).trim(),
                expected.to_string()
            );
            join.shutdown().await?;
        }
        Ok(())
    }

    fn test_profile(
        application_id: &str,
        executable: &Path,
        fixed_arguments: Vec<String>,
        argument_schema: ArgumentSchema,
    ) -> Result<ApplicationProfile, ProcessManagerError> {
        ApplicationProfile::register(ApplicationProfileSpec {
            application_id: application_id.to_owned(),
            executable: executable.to_owned(),
            fixed_arguments,
            argument_schema,
            base_environment: BTreeMap::new(),
            allowed_environment: BTreeMap::new(),
            working_directory_roots: vec![PathBuf::from("/tmp")],
            default_working_directory: PathBuf::from("/tmp"),
            stdin: StdinPolicy::Null,
        })
    }

    fn test_limits() -> Result<ProcessManagerLimits, ProcessManagerError> {
        ProcessManagerLimits::new(
            4,
            8,
            8,
            8,
            1024,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
    }

    fn test_child_identity() -> Result<ChildIdentity, ProcessManagerError> {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata("/proc/self")
            .map_err(|source| ProcessManagerError::WorkingDirectoryIo { source })?;
        Ok(ChildIdentity::new(metadata.uid(), metadata.gid()))
    }
}
