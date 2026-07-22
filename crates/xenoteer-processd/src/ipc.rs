//! Length-bounded, peer-credential-authenticated broker IPC.

use core::fmt;
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Deserializer, Serialize, de};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Mutex, Semaphore},
    task::JoinSet,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;
use xenoteer_protocol::{
    ApplicationId, ApplicationLaunchCommand, CommandId, DesktopGeneration, LaunchId,
    MAX_TERMINATION_GRACE_MS, ProcessExit as ProtocolProcessExit, ProcessExitedEvent,
    ProcessRef as ProtocolProcessRef, ProcessState, ProcessTerminateCommand, ProcessView,
};

use crate::process_manager::{
    ApplicationProfile, ChildIdentity, LaunchRequest, ManagedPidCorrelation,
    ManagedPidCorrelationEvidence, ProcessEventReplay, ProcessExit, ProcessManagerError,
    ProcessManagerHandle, ProcessManagerJoin, ProcessManagerLimits,
    ProcessRef as ManagedProcessRef, ProcessStatus, SequencedProcessEvent, spawn_process_manager,
};

/// Private root/daemon-group socket used inside the container.
pub const DEFAULT_BROKER_SOCKET: &str = "/run/xenoteer/processd/broker.sock";

// The public launch shape permits 64 independently bounded 1 KiB arguments;
// JSON escaping and envelope fields must still reach the manager so its
// stricter aggregate argv policy can return a typed rejection.
const MAX_FRAME_BYTES: usize = 256 * 1024;
const MAX_CONNECTIONS: usize = 32;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const LAUNCH_RECONCILIATION_ATTEMPTS: usize = 2;
const MAX_PRINCIPAL_ID_BYTES: usize = 128;

/// Immutable socket, peer, child-identity, and resource policy.
#[derive(Clone, Debug)]
pub struct BrokerConfig {
    socket_path: PathBuf,
    expected_peer_uid: u32,
    expected_peer_gid: u32,
    child_identity: ChildIdentity,
    process_limits: ProcessManagerLimits,
}

impl BrokerConfig {
    /// Creates a broker boundary with the default bounded process policy.
    #[must_use]
    pub fn new(
        socket_path: PathBuf,
        expected_peer_uid: u32,
        expected_peer_gid: u32,
        child_uid: u32,
        child_gid: u32,
    ) -> Self {
        Self {
            socket_path,
            expected_peer_uid,
            expected_peer_gid,
            child_identity: ChildIdentity::new(child_uid, child_gid),
            process_limits: ProcessManagerLimits::default(),
        }
    }

    #[cfg(test)]
    fn with_process_limits(mut self, limits: ProcessManagerLimits) -> Self {
        self.process_limits = limits;
        self
    }
}

/// Bound process-broker server and its sole manager-generation owner.
pub struct BrokerServer {
    listener: UnixListener,
    state: Arc<BrokerState>,
    connection_limit: Arc<Semaphore>,
    socket_path: PathBuf,
}

impl BrokerServer {
    /// Binds the configured fixed socket and validates the image-owned registry.
    pub(crate) async fn bind(
        config: BrokerConfig,
        profiles: Vec<ApplicationProfile>,
    ) -> Result<Self, BrokerServerError> {
        if profiles.is_empty() {
            return Err(BrokerServerError::Process);
        }
        prepare_socket_path(&config.socket_path)?;
        let listener = UnixListener::bind(&config.socket_path).map_err(BrokerServerError::Bind)?;
        fs::set_permissions(&config.socket_path, fs::Permissions::from_mode(0o660))
            .map_err(BrokerServerError::SocketMetadata)?;
        std::os::unix::fs::chown(&config.socket_path, None, Some(config.expected_peer_gid))
            .map_err(BrokerServerError::SocketMetadata)?;
        let socket_path = config.socket_path.clone();
        let launch_ledger = LaunchLedger::new(
            config.process_limits.launch_identity_capacity(),
            config.process_limits.launch_failure_capacity(),
        );
        Ok(Self {
            listener,
            state: Arc::new(BrokerState {
                config,
                profiles,
                active: Mutex::new(None),
                launch_ledger: Mutex::new(launch_ledger),
                event_stream_limit: Arc::new(Semaphore::new(1)),
                #[cfg(test)]
                drop_next_replies: AtomicUsize::new(0),
            }),
            connection_limit: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            socket_path,
        })
    }

    /// Serves until cancellation, then terminates and reaps all owned children.
    pub async fn serve(self, cancellation: CancellationToken) -> Result<(), BrokerServerError> {
        let mut connections = JoinSet::new();
        let result = loop {
            tokio::select! {
                () = cancellation.cancelled() => break Ok(()),
                accepted = self.listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(connection) => connection,
                        Err(error) => break Err(BrokerServerError::Accept(error)),
                    };
                    let permit = match Arc::clone(&self.connection_limit).try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => continue,
                    };
                    let state = Arc::clone(&self.state);
                    let _connection = connections.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = handle_connection(stream, state).await {
                            tracing::warn!(error = %error, "broker IPC request rejected");
                        }
                    });
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::error!(error = %error, "broker IPC connection task panicked");
                    }
                }
            }
        };
        cancellation.cancel();
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        self.state.shutdown().await?;
        let _removed = remove_owned_socket(&self.socket_path);
        result
    }
}

impl Drop for BrokerServer {
    fn drop(&mut self) {
        let _removed = remove_owned_socket(&self.socket_path);
    }
}

struct BrokerState {
    config: BrokerConfig,
    profiles: Vec<ApplicationProfile>,
    active: Mutex<Option<ActiveManager>>,
    launch_ledger: Mutex<LaunchLedger>,
    event_stream_limit: Arc<Semaphore>,
    #[cfg(test)]
    drop_next_replies: AtomicUsize,
}

struct ActiveManager {
    generation: DesktopGeneration,
    handle: ProcessManagerHandle,
    join: ProcessManagerJoin,
}

impl BrokerState {
    async fn manager_for(
        &self,
        generation: DesktopGeneration,
    ) -> Result<ProcessManagerHandle, ProcessManagerError> {
        let mut active = self.active.lock().await;
        if let Some(manager) = active.as_ref() {
            if manager.generation != generation {
                return Err(ProcessManagerError::WrongDesktopGeneration);
            }
            return Ok(manager.handle.clone());
        }
        let (handle, join) = spawn_process_manager(
            generation,
            self.profiles.clone(),
            self.config.child_identity,
            self.config.process_limits,
        )?;
        *active = Some(ActiveManager {
            generation,
            handle: handle.clone(),
            join,
        });
        Ok(handle)
    }

    async fn current_manager(
        &self,
        process: &ProtocolProcessRef,
    ) -> Result<ProcessManagerHandle, ProcessManagerError> {
        let active = self.active.lock().await;
        let manager = active
            .as_ref()
            .ok_or(ProcessManagerError::ProcessNotManaged)?;
        if manager.generation != process.desktop_generation {
            return Err(ProcessManagerError::WrongDesktopGeneration);
        }
        Ok(manager.handle.clone())
    }

    async fn current_manager_for_generation(
        &self,
        generation: DesktopGeneration,
    ) -> Result<ProcessManagerHandle, ProcessManagerError> {
        let active = self.active.lock().await;
        let manager = active
            .as_ref()
            .ok_or(ProcessManagerError::ProcessNotManaged)?;
        if manager.generation != generation {
            return Err(ProcessManagerError::WrongDesktopGeneration);
        }
        Ok(manager.handle.clone())
    }

    async fn shutdown(&self) -> Result<(), ProcessManagerError> {
        let manager = self.active.lock().await.take();
        if let Some(manager) = manager {
            manager.join.shutdown().await?;
        }
        Ok(())
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    state: Arc<BrokerState>,
) -> Result<(), BrokerServerError> {
    let credentials = stream
        .peer_cred()
        .map_err(BrokerServerError::PeerCredentials)?;
    if credentials.uid() != state.config.expected_peer_uid
        || credentials.gid() != state.config.expected_peer_gid
    {
        return Err(BrokerServerError::PeerRejected);
    }
    let request: BrokerRequest = read_frame(&mut stream).await?;
    if let BrokerRequest::SubscribeEvents {
        desktop_generation,
        since_sequence,
    } = &request
    {
        if desktop_generation.as_uuid().is_nil() {
            write_frame(
                &mut stream,
                &BrokerResponse::Error {
                    code: BrokerErrorCode::InvalidRequest,
                },
            )
            .await?;
            return Ok(());
        }
        let permit = match Arc::clone(&state.event_stream_limit).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                write_frame(
                    &mut stream,
                    &BrokerResponse::Error {
                        code: BrokerErrorCode::ManagerUnavailable,
                    },
                )
                .await?;
                return Ok(());
            }
        };
        let result =
            serve_event_stream(&mut stream, &state, *desktop_generation, *since_sequence).await;
        drop(permit);
        return result;
    }
    let response = dispatch(request, &state).await;
    #[cfg(test)]
    if state
        .drop_next_replies
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
    {
        return Ok(());
    }
    write_frame(&mut stream, &response).await?;
    stream.shutdown().await.map_err(BrokerServerError::Write)?;
    Ok(())
}

async fn serve_event_stream(
    stream: &mut UnixStream,
    state: &BrokerState,
    generation: DesktopGeneration,
    since_sequence: u64,
) -> Result<(), BrokerServerError> {
    let manager = match state.manager_for(generation).await {
        Ok(manager) => manager,
        Err(error) => {
            write_frame(
                stream,
                &BrokerResponse::Error {
                    code: BrokerErrorCode::from(&error),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let subscription = manager.subscribe_events(Some(since_sequence)).await?;
    let mut live = subscription.live;
    let replay = event_replay_to_broker(subscription.replay)?;
    let mut last_sequence = replay.latest_sequence();
    let resync_required = matches!(replay, BrokerEventReplay::ResyncRequired { .. });
    write_frame(
        stream,
        &BrokerResponse::Success {
            reply: BrokerReply::EventSubscription { replay },
        },
    )
    .await?;
    if resync_required {
        stream.shutdown().await.map_err(BrokerServerError::Write)?;
        return Ok(());
    }

    loop {
        enum StreamWake {
            Event(Result<Arc<SequencedProcessEvent>, tokio::sync::broadcast::error::RecvError>),
            Peer(Result<(), std::io::Error>),
        }
        let wake = tokio::select! {
            event = live.recv() => StreamWake::Event(event),
            ready = stream.readable() => StreamWake::Peer(ready),
        };
        let event = match wake {
            StreamWake::Event(event) => event,
            StreamWake::Peer(Err(error)) => return Err(BrokerServerError::Read(error)),
            StreamWake::Peer(Ok(())) => {
                let mut unexpected = [0_u8; 1];
                match stream.try_read(&mut unexpected) {
                    Ok(0) => return Ok(()),
                    Ok(_) => return Err(BrokerServerError::UnexpectedStreamInput),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(error) => return Err(BrokerServerError::Read(error)),
                }
            }
        };
        let needs_replay = match event {
            Ok(event) if event.sequence <= last_sequence => false,
            Ok(event) if event.sequence == last_sequence.saturating_add(1) => {
                write_frame(
                    stream,
                    &BrokerStreamFrame::Event {
                        event: event_to_broker(&event)?,
                    },
                )
                .await?;
                last_sequence = event.sequence;
                false
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        };
        if !needs_replay {
            continue;
        }

        // Replace the receiver with another atomic replay/live handoff. A
        // bounded retained suffix repairs ordinary broadcast lag without
        // exposing a false public gap.
        let recovered = manager.subscribe_events(Some(last_sequence)).await?;
        live = recovered.live;
        match event_replay_to_broker(recovered.replay)? {
            BrokerEventReplay::Events {
                latest_sequence,
                events,
            } => {
                for event in events {
                    if event.sequence() <= last_sequence {
                        continue;
                    }
                    if event.sequence() != last_sequence.saturating_add(1) {
                        return Err(BrokerServerError::Process);
                    }
                    last_sequence = event.sequence();
                    write_frame(stream, &BrokerStreamFrame::Event { event }).await?;
                }
                if last_sequence != latest_sequence {
                    return Err(BrokerServerError::Process);
                }
            }
            BrokerEventReplay::ResyncRequired {
                dropped_through,
                latest_sequence,
            } => {
                write_frame(
                    stream,
                    &BrokerStreamFrame::ResyncRequired {
                        dropped_through,
                        latest_sequence,
                    },
                )
                .await?;
                stream.shutdown().await.map_err(BrokerServerError::Write)?;
                return Ok(());
            }
        }
    }
}

async fn dispatch(request: BrokerRequest, state: &BrokerState) -> BrokerResponse {
    let result = match request {
        BrokerRequest::Probe => Ok(BrokerReply::Probe),
        BrokerRequest::Launch {
            principal_id,
            operation_id,
            desktop_generation,
            command,
        } => {
            launch(
                state,
                principal_id,
                operation_id,
                desktop_generation,
                command,
            )
            .await
        }
        BrokerRequest::Status { process } => status(state, process)
            .await
            .map_err(|error| BrokerErrorCode::from(&error)),
        BrokerRequest::CorrelatePids {
            desktop_generation,
            pids,
        } => correlate_pids(state, desktop_generation, pids)
            .await
            .map_err(|error| BrokerErrorCode::from(&error)),
        BrokerRequest::Terminate { command } => terminate(state, command)
            .await
            .map_err(|error| BrokerErrorCode::from(&error)),
        BrokerRequest::SubscribeEvents { .. } => Err(BrokerErrorCode::Internal),
    };
    match result {
        Ok(reply) => BrokerResponse::Success { reply },
        Err(code) => BrokerResponse::Error { code },
    }
}

async fn correlate_pids(
    state: &BrokerState,
    desktop_generation: DesktopGeneration,
    pids: CorrelationPids,
) -> Result<BrokerReply, ProcessManagerError> {
    if desktop_generation.as_uuid().is_nil() {
        return Err(ProcessManagerError::InvalidCorrelationBatch);
    }
    let manager = state
        .current_manager_for_generation(desktop_generation)
        .await?;
    let entries = manager
        .correlate_pids(desktop_generation, pids.into_vec())
        .await?
        .into_iter()
        .map(BrokerPidCorrelation::from)
        .collect();
    Ok(BrokerReply::PidCorrelations { entries })
}

async fn launch(
    state: &BrokerState,
    principal_id: String,
    operation_id: CommandId,
    generation: DesktopGeneration,
    command: ApplicationLaunchCommand,
) -> Result<BrokerReply, BrokerErrorCode> {
    if operation_id.as_uuid().is_nil() || !valid_principal_id(&principal_id) {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    command
        .validate()
        .map_err(|_| BrokerErrorCode::InvalidRequest)?;
    let operation = LaunchOperation {
        generation,
        command,
    };
    // The mutex deliberately spans lookup, spawn, and record publication. The
    // process manager serializes launches already; this additionally makes
    // concurrent retries of one operation observe exactly one execution.
    let mut ledger = state.launch_ledger.lock().await;
    if let Some(cached) = ledger.lookup(&principal_id, operation_id, &operation) {
        match cached {
            LaunchLookup::Conflict => return Err(BrokerErrorCode::OperationIdConflict),
            LaunchLookup::Rejected(code) => return Err(code),
            LaunchLookup::Process(process) => {
                let retained = match state.current_manager(&process).await {
                    Ok(manager) => match manager.status(process_from_protocol(process)).await {
                        Ok(_) => true,
                        Err(ProcessManagerError::ProcessNotManaged) => false,
                        Err(error) => return Err(BrokerErrorCode::from(&error)),
                    },
                    Err(ProcessManagerError::ProcessNotManaged) => false,
                    Err(error) => return Err(BrokerErrorCode::from(&error)),
                };
                if retained {
                    return Ok(BrokerReply::Process { process });
                }
                ledger.remove(&principal_id, operation_id);
            }
        }
    }

    let manager = state
        .manager_for(generation)
        .await
        .map_err(|error| BrokerErrorCode::from(&error))?;
    ledger.reconcile_successes(&manager).await;
    let command = &operation.command;
    let request = LaunchRequest::new(&principal_id, command.application.as_str()).with_arguments(
        command
            .arguments
            .iter()
            .map(|argument| argument.as_str().to_owned())
            .collect(),
    );
    match manager.launch(request).await {
        Ok(process) => {
            let process = process_to_protocol(&process);
            // The launch request itself collects newly finished children and
            // can evict an older exit after the pre-launch reconciliation.
            // Reconcile once more before publication so the successful side
            // of the ledger never exceeds live + retained exit identities.
            ledger.reconcile_successes(&manager).await;
            ledger.record_process(principal_id, operation_id, operation, process);
            Ok(BrokerReply::Process { process })
        }
        Err(error) => {
            let code = BrokerErrorCode::from(&error);
            ledger.record_rejection(principal_id, operation_id, operation, code);
            Err(code)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaunchOperation {
    generation: DesktopGeneration,
    command: ApplicationLaunchCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchOutcome {
    Process(ProtocolProcessRef),
    Rejected(BrokerErrorCode),
}

#[derive(Clone, Debug)]
struct LaunchLedgerEntry {
    principal_id: String,
    operation_id: CommandId,
    operation: LaunchOperation,
    outcome: LaunchOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchLookup {
    Process(ProtocolProcessRef),
    Rejected(BrokerErrorCode),
    Conflict,
}

struct LaunchLedger {
    entries: VecDeque<LaunchLedgerEntry>,
    identity_capacity: usize,
    failure_capacity: usize,
}

impl LaunchLedger {
    fn new(identity_capacity: usize, failure_capacity: usize) -> Self {
        debug_assert!(identity_capacity > 0);
        debug_assert!(failure_capacity > 0);
        Self {
            entries: VecDeque::new(),
            identity_capacity,
            failure_capacity,
        }
    }

    fn lookup(
        &self,
        principal_id: &str,
        operation_id: CommandId,
        operation: &LaunchOperation,
    ) -> Option<LaunchLookup> {
        let entry = self.entries.iter().find(|entry| {
            entry.principal_id == principal_id && entry.operation_id == operation_id
        })?;
        if &entry.operation != operation {
            return Some(LaunchLookup::Conflict);
        }
        Some(match entry.outcome {
            LaunchOutcome::Process(process) => LaunchLookup::Process(process),
            LaunchOutcome::Rejected(code) => LaunchLookup::Rejected(code),
        })
    }

    fn remove(&mut self, principal_id: &str, operation_id: CommandId) {
        self.entries.retain(|entry| {
            entry.principal_id != principal_id || entry.operation_id != operation_id
        });
    }

    async fn reconcile_successes(&mut self, manager: &ProcessManagerHandle) {
        let successes = self
            .entries
            .iter()
            .filter_map(|entry| match entry.outcome {
                LaunchOutcome::Process(process) => {
                    Some((entry.principal_id.clone(), entry.operation_id, process))
                }
                LaunchOutcome::Rejected(_) => None,
            })
            .collect::<Vec<_>>();
        for (principal_id, operation_id, process) in successes {
            if matches!(
                manager.status(process_from_protocol(process)).await,
                Err(ProcessManagerError::ProcessNotManaged)
            ) {
                self.remove(&principal_id, operation_id);
            }
        }
    }

    fn record_process(
        &mut self,
        principal_id: String,
        operation_id: CommandId,
        operation: LaunchOperation,
        process: ProtocolProcessRef,
    ) {
        debug_assert!(
            self.entries
                .iter()
                .filter(|entry| matches!(entry.outcome, LaunchOutcome::Process(_)))
                .count()
                < self.identity_capacity
        );
        self.entries.push_back(LaunchLedgerEntry {
            principal_id,
            operation_id,
            operation,
            outcome: LaunchOutcome::Process(process),
        });
    }

    fn record_rejection(
        &mut self,
        principal_id: String,
        operation_id: CommandId,
        operation: LaunchOperation,
        code: BrokerErrorCode,
    ) {
        while self
            .entries
            .iter()
            .filter(|entry| matches!(entry.outcome, LaunchOutcome::Rejected(_)))
            .count()
            >= self.failure_capacity
        {
            if let Some(index) = self
                .entries
                .iter()
                .position(|entry| matches!(entry.outcome, LaunchOutcome::Rejected(_)))
            {
                self.entries.remove(index);
            }
        }
        self.entries.push_back(LaunchLedgerEntry {
            principal_id,
            operation_id,
            operation,
            outcome: LaunchOutcome::Rejected(code),
        });
    }
}

async fn status(
    state: &BrokerState,
    process: ProtocolProcessRef,
) -> Result<BrokerReply, ProcessManagerError> {
    process
        .validate()
        .map_err(|_| ProcessManagerError::ProcessReferenceMismatch)?;
    let manager = state.current_manager(&process).await?;
    let status = manager.status(process_from_protocol(process)).await?;
    Ok(BrokerReply::Status {
        process: status_to_protocol(status)?,
    })
}

async fn terminate(
    state: &BrokerState,
    command: ProcessTerminateCommand,
) -> Result<BrokerReply, ProcessManagerError> {
    command
        .process
        .validate()
        .map_err(|_| ProcessManagerError::ProcessReferenceMismatch)?;
    if command
        .grace_ms
        .is_some_and(|grace| grace > MAX_TERMINATION_GRACE_MS)
    {
        return Err(ProcessManagerError::TerminationGraceExceeded);
    }
    let process = command.process;
    let manager = state.current_manager(&process).await?;
    let grace_override = command
        .grace_ms
        .map(|milliseconds| Duration::from_millis(milliseconds.into()));
    let exit = manager
        .terminate(process_from_protocol(process), grace_override)
        .await?;
    Ok(BrokerReply::Status {
        process: exit_to_protocol(&exit)?,
    })
}

fn process_to_protocol(process: &ManagedProcessRef) -> ProtocolProcessRef {
    ProtocolProcessRef {
        desktop_generation: process.desktop_generation(),
        pid: process.pid(),
        proc_start_ticks: process.proc_start_ticks(),
        launch_id: LaunchId::from_uuid(process.launch_id()),
    }
}

fn process_from_protocol(process: ProtocolProcessRef) -> ManagedProcessRef {
    ManagedProcessRef::from_parts(
        process.desktop_generation,
        process.pid,
        process.proc_start_ticks,
        process.launch_id.as_uuid(),
    )
}

fn status_to_protocol(status: ProcessStatus) -> Result<ProcessView, ProcessManagerError> {
    match status {
        ProcessStatus::Running { process, .. } => Ok(ProcessView {
            process: process_to_protocol(&process),
            state: ProcessState::Running,
            exit: None,
        }),
        ProcessStatus::Terminating { process, .. } => Ok(ProcessView {
            process: process_to_protocol(&process),
            state: ProcessState::Terminating,
            exit: None,
        }),
        ProcessStatus::Exited(exit) => exit_to_protocol(&exit),
    }
}

fn exit_to_protocol(exit: &ProcessExit) -> Result<ProcessView, ProcessManagerError> {
    let signal = exit
        .signal
        .map(u8::try_from)
        .transpose()
        .map_err(|_| ProcessManagerError::InvalidExitStatus)?;
    Ok(ProcessView {
        process: process_to_protocol(&exit.process),
        state: ProcessState::Exited,
        exit: Some(ProtocolProcessExit {
            code: exit.exit_code,
            signal,
            core_dumped: exit.core_dumped,
        }),
    })
}

fn event_to_broker(
    event: &SequencedProcessEvent,
) -> Result<BrokerProcessEvent, ProcessManagerError> {
    let payload = ProcessExitedEvent {
        application: ApplicationId::new(event.exit.application_id.clone())
            .map_err(|_| ProcessManagerError::InvalidExitStatus)?,
        process: exit_to_protocol(&event.exit)?,
        termination_requested: event.exit.termination_requested,
        forced_escalation: event.exit.forced_escalation,
    };
    payload
        .validate()
        .map_err(|_| ProcessManagerError::InvalidExitStatus)?;
    Ok(BrokerProcessEvent {
        sequence: event.sequence,
        principal_id: event.exit.principal_id.clone(),
        payload,
    })
}

fn event_replay_to_broker(
    replay: ProcessEventReplay,
) -> Result<BrokerEventReplay, ProcessManagerError> {
    match replay {
        ProcessEventReplay::Events {
            latest_sequence,
            events,
        } => Ok(BrokerEventReplay::Events {
            latest_sequence,
            events: events
                .iter()
                .map(|event| event_to_broker(event))
                .collect::<Result<Vec<_>, _>>()?,
        }),
        ProcessEventReplay::ResyncRequired {
            dropped_through,
            latest_sequence,
        } => Ok(BrokerEventReplay::ResyncRequired {
            dropped_through,
            latest_sequence,
        }),
    }
}

/// Output-free process lifecycle record carried only on authenticated local IPC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerProcessEvent {
    sequence: u64,
    principal_id: String,
    payload: ProcessExitedEvent,
}

impl BrokerProcessEvent {
    /// Returns the broker-local ordered event sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the authenticated launch owner used only for daemon-side routing.
    #[must_use]
    pub fn principal_id(&self) -> &str {
        &self.principal_id
    }

    /// Returns the public output-free lifecycle payload.
    #[must_use]
    pub const fn payload(&self) -> &ProcessExitedEvent {
        &self.payload
    }

    /// Splits routing authority from the public lifecycle payload.
    #[must_use]
    pub fn into_parts(self) -> (String, ProcessExitedEvent) {
        (self.principal_id, self.payload)
    }
}

/// Bounded initial broker replay, or proof that retained history was lost.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrokerEventReplay {
    /// Complete suffix after the caller cursor.
    Events {
        /// Latest broker sequence captured with the suffix.
        latest_sequence: u64,
        /// Ordered output-free process events.
        events: Vec<BrokerProcessEvent>,
    },
    /// At least one required event has left bounded retention.
    ResyncRequired {
        /// Highest broker event sequence known to be unavailable.
        dropped_through: u64,
        /// Latest broker event sequence at the boundary.
        latest_sequence: u64,
    },
}

impl BrokerEventReplay {
    const fn latest_sequence(&self) -> u64 {
        match self {
            Self::Events {
                latest_sequence, ..
            }
            | Self::ResyncRequired {
                latest_sequence, ..
            } => *latest_sequence,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum BrokerStreamFrame {
    Event {
        event: BrokerProcessEvent,
    },
    ResyncRequired {
        dropped_through: u64,
        latest_sequence: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum BrokerRequest {
    Probe,
    Launch {
        principal_id: String,
        operation_id: CommandId,
        desktop_generation: DesktopGeneration,
        command: ApplicationLaunchCommand,
    },
    Status {
        process: ProtocolProcessRef,
    },
    CorrelatePids {
        desktop_generation: DesktopGeneration,
        pids: CorrelationPids,
    },
    Terminate {
        command: ProcessTerminateCommand,
    },
    SubscribeEvents {
        desktop_generation: DesktopGeneration,
        since_sequence: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum BrokerResponse {
    Success { reply: BrokerReply },
    Error { code: BrokerErrorCode },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum BrokerReply {
    Probe,
    Process { process: ProtocolProcessRef },
    Status { process: ProcessView },
    PidCorrelations { entries: Vec<BrokerPidCorrelation> },
    EventSubscription { replay: BrokerEventReplay },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
struct CorrelationPids(Vec<u32>);

impl CorrelationPids {
    fn new(pids: Vec<u32>) -> Result<Self, ()> {
        if pids.is_empty() || pids.len() > crate::MAX_PROCESS_CORRELATION_PIDS {
            return Err(());
        }
        let mut unique = BTreeSet::new();
        if pids.iter().any(|pid| *pid == 0 || !unique.insert(*pid)) {
            return Err(());
        }
        Ok(Self(pids))
    }

    #[cfg(test)]
    fn as_slice(&self) -> &[u32] {
        &self.0
    }

    fn into_vec(self) -> Vec<u32> {
        self.0
    }
}

impl<'de> Deserialize<'de> for CorrelationPids {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PidsVisitor;

        impl<'de> de::Visitor<'de> for PidsVisitor {
            type Value = CorrelationPids;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "one to {} unique nonzero process IDs",
                    crate::MAX_PROCESS_CORRELATION_PIDS
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut pids = Vec::with_capacity(
                    sequence
                        .size_hint()
                        .unwrap_or(0)
                        .min(crate::MAX_PROCESS_CORRELATION_PIDS),
                );
                let mut unique = BTreeSet::new();
                while let Some(pid) = sequence.next_element::<u32>()? {
                    if pid == 0
                        || pids.len() >= crate::MAX_PROCESS_CORRELATION_PIDS
                        || !unique.insert(pid)
                    {
                        return Err(de::Error::invalid_value(
                            de::Unexpected::Unsigned(u64::from(pid)),
                            &self,
                        ));
                    }
                    pids.push(pid);
                }
                CorrelationPids::new(pids).map_err(|()| de::Error::invalid_length(0, &self))
            }
        }

        deserializer.deserialize_seq(PidsVisitor)
    }
}

/// Correlation evidence for one requested live PID, preserving request order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerPidCorrelation {
    /// The queried nonzero Linux PID.
    pub pid: u32,
    /// Typed non-authoritative manager correlation evidence.
    pub evidence: BrokerPidCorrelationEvidence,
}

/// Typed evidence only; possession never authorizes process operations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "match", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrokerPidCorrelationEvidence {
    /// PID and start time exactly matched one managed leader.
    ManagedLeader {
        /// Full exact generation/PID/start-time/launch identity.
        process: ProtocolProcessRef,
    },
    /// Live PGID matched one uniquely verified managed process group.
    ManagedProcessGroup {
        /// Full exact reference for the owning managed leader.
        process: ProtocolProcessRef,
    },
    /// The PID was live but did not match a managed leader or process group.
    NoMatch,
}

impl From<ManagedPidCorrelation> for BrokerPidCorrelation {
    fn from(value: ManagedPidCorrelation) -> Self {
        let evidence = match value.evidence {
            ManagedPidCorrelationEvidence::Leader(process) => {
                BrokerPidCorrelationEvidence::ManagedLeader {
                    process: process_to_protocol(&process),
                }
            }
            ManagedPidCorrelationEvidence::ProcessGroup(process) => {
                BrokerPidCorrelationEvidence::ManagedProcessGroup {
                    process: process_to_protocol(&process),
                }
            }
            ManagedPidCorrelationEvidence::NoMatch => BrokerPidCorrelationEvidence::NoMatch,
        };
        Self {
            pid: value.pid,
            evidence,
        }
    }
}

/// Stable, non-disclosing broker rejection category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerErrorCode {
    /// The request violated a registered profile or wire invariant.
    InvalidRequest,
    /// A retained operation ID was reused with different launch content.
    OperationIdConflict,
    /// The requested image profile does not exist.
    ApplicationNotRegistered,
    /// The bounded live-process ceiling was reached.
    ProcessLimitExceeded,
    /// The reference belongs to another desktop lifetime.
    WrongDesktopGeneration,
    /// The exact process reference is not retained by this broker.
    ProcessNotManaged,
    /// One or more supplied identity-fencing fields changed.
    ProcessReferenceMismatch,
    /// A termination operation already owns this process.
    TerminationInProgress,
    /// The broker's manager actor is unavailable.
    ManagerUnavailable,
    /// The registered executable could not be spawned.
    SpawnFailed,
    /// A non-disclosing internal broker failure occurred.
    Internal,
}

impl From<&ProcessManagerError> for BrokerErrorCode {
    fn from(error: &ProcessManagerError) -> Self {
        match error {
            ProcessManagerError::ApplicationNotRegistered => Self::ApplicationNotRegistered,
            ProcessManagerError::ProcessLimitExceeded => Self::ProcessLimitExceeded,
            ProcessManagerError::WrongDesktopGeneration => Self::WrongDesktopGeneration,
            ProcessManagerError::ProcessNotManaged => Self::ProcessNotManaged,
            ProcessManagerError::ProcessReferenceMismatch => Self::ProcessReferenceMismatch,
            ProcessManagerError::TerminationAlreadyInProgress => Self::TerminationInProgress,
            ProcessManagerError::ManagerUnavailable => Self::ManagerUnavailable,
            ProcessManagerError::Spawn { .. } => Self::SpawnFailed,
            ProcessManagerError::InvalidPrincipalId
            | ProcessManagerError::InvalidCorrelationBatch
            | ProcessManagerError::InvalidArgumentSchema
            | ProcessManagerError::InvalidArgumentCount { .. }
            | ProcessManagerError::InvalidArgument { .. }
            | ProcessManagerError::ArgumentBytesExceeded
            | ProcessManagerError::EnvironmentKeyNotAllowed
            | ProcessManagerError::EnvironmentKeyForbidden
            | ProcessManagerError::InvalidEnvironmentValue
            | ProcessManagerError::EnvironmentLimitExceeded
            | ProcessManagerError::WorkingDirectoryIo { .. }
            | ProcessManagerError::WorkingDirectoryOutsideRoots
            | ProcessManagerError::TerminationGraceExceeded => Self::InvalidRequest,
            _ => Self::Internal,
        }
    }
}

/// Cloneable daemon-side client. It contains no spawn or signal primitive.
#[derive(Clone, Debug)]
pub struct BrokerClient {
    socket_path: PathBuf,
}

/// Atomic retained broker replay plus its sole bounded live stream.
pub struct BrokerEventSubscription {
    /// Complete retained suffix, or an explicit upstream resync boundary.
    pub replay: BrokerEventReplay,
    /// Live process events following the replay snapshot.
    pub live: BrokerEventStream,
}

/// One live broker stream item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokerLiveEvent {
    /// One ordered, output-free process exit.
    Event(BrokerProcessEvent),
    /// The broker could not repair a lag from retained history.
    ResyncRequired {
        /// Highest broker sequence known to be unavailable.
        dropped_through: u64,
        /// Latest broker sequence at the resync boundary.
        latest_sequence: u64,
    },
    /// Broker shutdown or orderly stream closure.
    Closed,
}

/// Single-owner authenticated local IPC event stream.
pub struct BrokerEventStream {
    stream: UnixStream,
}

impl BrokerEventStream {
    /// Waits for one live event, resync boundary, or orderly closure.
    pub async fn receive(&mut self) -> Result<BrokerLiveEvent, BrokerClientError> {
        match read_idle_frame::<BrokerStreamFrame>(&mut self.stream).await {
            Ok(BrokerStreamFrame::Event { event }) => Ok(BrokerLiveEvent::Event(event)),
            Ok(BrokerStreamFrame::ResyncRequired {
                dropped_through,
                latest_sequence,
            }) => Ok(BrokerLiveEvent::ResyncRequired {
                dropped_through,
                latest_sequence,
            }),
            Err(BrokerServerError::Read(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                Ok(BrokerLiveEvent::Closed)
            }
            Err(error) => Err(BrokerClientError::Transport(error)),
        }
    }
}

impl BrokerClient {
    /// Selects the fixed private socket path.
    #[must_use]
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// Verifies the live broker protocol without disclosing credentials.
    pub async fn probe(&self) -> Result<(), BrokerClientError> {
        match self.call(BrokerRequest::Probe).await? {
            BrokerReply::Probe => Ok(()),
            _ => Err(BrokerClientError::UnexpectedReply),
        }
    }

    /// Requests a registered application launch for one desktop generation.
    ///
    /// Exact retries must reuse `operation_id`; the broker returns the same
    /// retained process reference and rejects changed content under that ID.
    /// Launch is a bounded atomic stage: at most two complete five-second IPC
    /// attempts are allowed so an ambiguous first reply can be reconciled.
    pub async fn launch(
        &self,
        principal_id: &str,
        operation_id: CommandId,
        desktop_generation: DesktopGeneration,
        command: ApplicationLaunchCommand,
    ) -> Result<ProtocolProcessRef, BrokerClientError> {
        let request = BrokerRequest::Launch {
            principal_id: principal_id.to_owned(),
            operation_id,
            desktop_generation,
            command,
        };
        let mut last_ambiguous = None;
        for attempt in 0..LAUNCH_RECONCILIATION_ATTEMPTS {
            let result = self.call_until(request.clone(), None).await;
            match result {
                Ok(BrokerReply::Process { process }) => return Ok(process),
                Ok(_) => return Err(BrokerClientError::UnexpectedReply),
                Err(error)
                    if attempt + 1 < LAUNCH_RECONCILIATION_ATTEMPTS
                        && error.is_ambiguous_transport() =>
                {
                    // A retry is safe only because it preserves the exact
                    // operation ID and canonical typed launch content.
                    last_ambiguous = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_ambiguous.unwrap_or(BrokerClientError::Timeout))
    }

    /// Opens the broker's sole replayable process-event stream.
    pub async fn subscribe_events(
        &self,
        desktop_generation: DesktopGeneration,
        since_sequence: u64,
    ) -> Result<BrokerEventSubscription, BrokerClientError> {
        let mut stream = timeout(IO_TIMEOUT, UnixStream::connect(&self.socket_path))
            .await
            .map_err(|_| BrokerClientError::Timeout)?
            .map_err(BrokerClientError::Connect)?;
        write_frame(
            &mut stream,
            &BrokerRequest::SubscribeEvents {
                desktop_generation,
                since_sequence,
            },
        )
        .await
        .map_err(BrokerClientError::Transport)?;
        let response = read_frame(&mut stream)
            .await
            .map_err(BrokerClientError::Transport)?;
        match response {
            BrokerResponse::Success {
                reply: BrokerReply::EventSubscription { replay },
            } => Ok(BrokerEventSubscription {
                replay,
                live: BrokerEventStream { stream },
            }),
            BrokerResponse::Success { .. } => Err(BrokerClientError::UnexpectedReply),
            BrokerResponse::Error { code } => Err(BrokerClientError::Rejected { code }),
        }
    }

    async fn call_until(
        &self,
        request: BrokerRequest,
        deadline: Option<Instant>,
    ) -> Result<BrokerReply, BrokerClientError> {
        let budget = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(IO_TIMEOUT)
            .min(IO_TIMEOUT);
        if budget.is_zero() {
            return Err(BrokerClientError::Timeout);
        }
        timeout(budget, self.call_once(request))
            .await
            .map_err(|_| BrokerClientError::Timeout)?
    }

    async fn call_once(&self, request: BrokerRequest) -> Result<BrokerReply, BrokerClientError> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(BrokerClientError::Connect)?;
        write_frame(&mut stream, &request)
            .await
            .map_err(BrokerClientError::Transport)?;
        match read_frame(&mut stream)
            .await
            .map_err(BrokerClientError::Transport)?
        {
            BrokerResponse::Success { reply } => Ok(reply),
            BrokerResponse::Error { code } => Err(BrokerClientError::Rejected { code }),
        }
    }

    /// Reads current state for an exact generation/PID/start-time/launch ID.
    pub async fn status(
        &self,
        process: ProtocolProcessRef,
    ) -> Result<ProcessView, BrokerClientError> {
        match self.call(BrokerRequest::Status { process }).await? {
            BrokerReply::Status { process } => Ok(process),
            _ => Err(BrokerClientError::UnexpectedReply),
        }
    }

    /// Correlates a bounded set of live PIDs to managed identity evidence.
    ///
    /// The returned references are evidence only. Every authoritative process
    /// operation still requires its ordinary capability and exact-reference checks.
    pub async fn correlate_pids(
        &self,
        desktop_generation: DesktopGeneration,
        pids: Vec<u32>,
    ) -> Result<Vec<BrokerPidCorrelation>, BrokerClientError> {
        if desktop_generation.as_uuid().is_nil() {
            return Err(BrokerClientError::Rejected {
                code: BrokerErrorCode::InvalidRequest,
            });
        }
        let pids = CorrelationPids::new(pids).map_err(|()| BrokerClientError::Rejected {
            code: BrokerErrorCode::InvalidRequest,
        })?;
        match self
            .call(BrokerRequest::CorrelatePids {
                desktop_generation,
                pids,
            })
            .await?
        {
            BrokerReply::PidCorrelations { entries } => Ok(entries),
            _ => Err(BrokerClientError::UnexpectedReply),
        }
    }

    /// Requests bounded TERM/KILL cleanup for an exact managed reference.
    pub async fn terminate(
        &self,
        process: ProtocolProcessRef,
    ) -> Result<ProcessView, BrokerClientError> {
        self.terminate_command(ProcessTerminateCommand {
            process,
            grace_ms: None,
        })
        .await
    }

    /// Requests termination with a protocol-bounded per-command grace override.
    pub async fn terminate_command(
        &self,
        command: ProcessTerminateCommand,
    ) -> Result<ProcessView, BrokerClientError> {
        match self.call(BrokerRequest::Terminate { command }).await? {
            BrokerReply::Status { process } => Ok(process),
            _ => Err(BrokerClientError::UnexpectedReply),
        }
    }

    async fn call(&self, request: BrokerRequest) -> Result<BrokerReply, BrokerClientError> {
        self.call_until(request, None).await
    }
}

async fn read_frame<T: for<'de> Deserialize<'de>>(
    stream: &mut UnixStream,
) -> Result<T, BrokerServerError> {
    let mut header = [0_u8; 4];
    timeout(IO_TIMEOUT, stream.read_exact(&mut header))
        .await
        .map_err(|_| BrokerServerError::Timeout)?
        .map_err(BrokerServerError::Read)?;
    let frame_bytes = usize::try_from(u32::from_be_bytes(header))
        .map_err(|_| BrokerServerError::FrameTooLarge)?;
    if frame_bytes == 0 || frame_bytes > MAX_FRAME_BYTES {
        return Err(BrokerServerError::FrameTooLarge);
    }
    let mut frame = vec![0_u8; frame_bytes];
    timeout(IO_TIMEOUT, stream.read_exact(&mut frame))
        .await
        .map_err(|_| BrokerServerError::Timeout)?
        .map_err(BrokerServerError::Read)?;
    serde_json::from_slice(&frame).map_err(BrokerServerError::Decode)
}

// An idle event stream may legitimately have no bytes indefinitely. Once a
// frame header arrives, however, its bounded body must complete promptly.
async fn read_idle_frame<T: for<'de> Deserialize<'de>>(
    stream: &mut UnixStream,
) -> Result<T, BrokerServerError> {
    let mut header = [0_u8; 4];
    stream
        .read_exact(&mut header)
        .await
        .map_err(BrokerServerError::Read)?;
    let frame_bytes = usize::try_from(u32::from_be_bytes(header))
        .map_err(|_| BrokerServerError::FrameTooLarge)?;
    if frame_bytes == 0 || frame_bytes > MAX_FRAME_BYTES {
        return Err(BrokerServerError::FrameTooLarge);
    }
    let mut frame = vec![0_u8; frame_bytes];
    timeout(IO_TIMEOUT, stream.read_exact(&mut frame))
        .await
        .map_err(|_| BrokerServerError::Timeout)?
        .map_err(BrokerServerError::Read)?;
    serde_json::from_slice(&frame).map_err(BrokerServerError::Decode)
}

async fn write_frame<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
) -> Result<(), BrokerServerError> {
    let frame = serde_json::to_vec(value).map_err(BrokerServerError::Encode)?;
    if frame.is_empty() || frame.len() > MAX_FRAME_BYTES {
        return Err(BrokerServerError::FrameTooLarge);
    }
    let length = u32::try_from(frame.len()).map_err(|_| BrokerServerError::FrameTooLarge)?;
    timeout(IO_TIMEOUT, async {
        stream.write_all(&length.to_be_bytes()).await?;
        stream.write_all(&frame).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| BrokerServerError::Timeout)?
    .map_err(BrokerServerError::Write)
}

fn prepare_socket_path(path: &Path) -> Result<(), BrokerServerError> {
    let parent = path.parent().ok_or(BrokerServerError::InvalidSocketPath)?;
    let parent_metadata =
        fs::symlink_metadata(parent).map_err(BrokerServerError::SocketMetadata)?;
    let process_uid = fs::metadata("/proc/self")
        .map_err(BrokerServerError::SocketMetadata)?
        .uid();
    if !parent_metadata.is_dir()
        || parent_metadata.file_type().is_symlink()
        || parent_metadata.uid() != process_uid
        || parent_metadata.permissions().mode() & 0o022 != 0
    {
        return Err(BrokerServerError::InvalidSocketPath);
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == process_uid => {
            fs::remove_file(path).map_err(BrokerServerError::SocketMetadata)
        }
        Ok(_) => Err(BrokerServerError::InvalidSocketPath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BrokerServerError::SocketMetadata(error)),
    }
}

fn remove_owned_socket(path: &Path) -> Result<(), std::io::Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn valid_principal_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PRINCIPAL_ID_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@')
        })
}

/// Broker listener, peer-authentication, framing, or manager failure.
#[derive(Debug, Error)]
pub enum BrokerServerError {
    /// The fixed socket parent or stale entry is unsafe.
    #[error("broker socket path failed ownership/type policy")]
    InvalidSocketPath,
    /// The socket could not be created.
    #[error("could not bind broker socket")]
    Bind(#[source] std::io::Error),
    /// Socket permissions or ownership could not be applied.
    #[error("could not prepare broker socket metadata")]
    SocketMetadata(#[source] std::io::Error),
    /// A connection could not be accepted.
    #[error("could not accept broker connection")]
    Accept(#[source] std::io::Error),
    /// Unix peer credentials could not be obtained.
    #[error("could not obtain broker peer credentials")]
    PeerCredentials(#[source] std::io::Error),
    /// The connecting process was not the dedicated daemon identity.
    #[error("broker peer credential policy rejected the connection")]
    PeerRejected,
    /// The peer exceeded the fixed IPC deadline.
    #[error("broker IPC deadline exceeded")]
    Timeout,
    /// A request or response frame was empty or oversized.
    #[error("broker IPC frame exceeds its fixed bound")]
    FrameTooLarge,
    /// A frame could not be read.
    #[error("could not read broker IPC frame")]
    Read(#[source] std::io::Error),
    /// A frame could not be written.
    #[error("could not write broker IPC frame")]
    Write(#[source] std::io::Error),
    /// JSON was not a valid strict request/response shape.
    #[error("could not decode broker IPC frame")]
    Decode(#[source] serde_json::Error),
    /// An event subscriber sent data after its one subscription request.
    #[error("broker event stream received unexpected client data")]
    UnexpectedStreamInput,
    /// A bounded reply could not be encoded.
    #[error("could not encode broker IPC frame")]
    Encode(#[source] serde_json::Error),
    /// The process manager failed without exposing internal details.
    #[error("application process manager failed")]
    Process,
}

impl From<ProcessManagerError> for BrokerServerError {
    fn from(_error: ProcessManagerError) -> Self {
        Self::Process
    }
}

/// Daemon-side connection, framing, or broker rejection.
#[derive(Debug, Error)]
pub enum BrokerClientError {
    /// The private socket was unavailable.
    #[error("could not connect to application broker")]
    Connect(#[source] std::io::Error),
    /// The bounded IPC deadline elapsed.
    #[error("application broker request timed out")]
    Timeout,
    /// The local or remote frame was invalid.
    #[error("application broker transport failed")]
    Transport(#[source] BrokerServerError),
    /// The broker returned a safe stable rejection code.
    #[error("application broker rejected request: {code:?}")]
    Rejected {
        /// Stable public reason returned by the privileged broker.
        code: BrokerErrorCode,
    },
    /// A valid response carried the wrong operation payload.
    #[error("application broker returned an unexpected reply")]
    UnexpectedReply,
}

impl BrokerClientError {
    const fn is_ambiguous_transport(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::Timeout | Self::Transport(_))
    }
}

#[cfg(test)]
#[path = "ipc/correlation_tests.rs"]
mod correlation_tests;

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, os::unix::fs::MetadataExt};

    use super::*;
    use crate::process_manager::{
        ApplicationProfileSpec, ArgumentRule, ArgumentSchema, StdinPolicy,
    };
    use xenoteer_protocol::{ApplicationArgument, ApplicationId};

    #[tokio::test]
    async fn authenticated_ipc_launch_status_and_terminate_are_bounded()
    -> Result<(), Box<dyn std::error::Error>> {
        let identity = fs::metadata("/proc/self")?;
        let uid = identity.uid();
        let gid = identity.gid();
        let directory =
            std::env::temp_dir().join(format!("xenoteer-processd-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let socket = directory.join("broker.sock");
        let limits = ProcessManagerLimits::new(
            4,
            8,
            8,
            8,
            1_024,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )?;
        let config =
            BrokerConfig::new(socket.clone(), uid, gid, uid, gid).with_process_limits(limits);
        let profile = ApplicationProfile::register(ApplicationProfileSpec {
            application_id: "sleep".to_owned(),
            executable: PathBuf::from("/usr/bin/sleep"),
            fixed_arguments: Vec::new(),
            argument_schema: ArgumentSchema::exact(vec![ArgumentRule::Integer {
                minimum: 1,
                maximum: 60,
            }]),
            base_environment: BTreeMap::new(),
            allowed_environment: BTreeMap::new(),
            working_directory_roots: vec![PathBuf::from("/tmp")],
            default_working_directory: PathBuf::from("/tmp"),
            stdin: StdinPolicy::Null,
        })?;
        let server = BrokerServer::bind(config, vec![profile]).await?;
        let cancellation = CancellationToken::new();
        let server_cancellation = cancellation.clone();
        let task = tokio::spawn(server.serve(server_cancellation));
        let client = BrokerClient::new(&socket);
        client.probe().await?;
        let generation = DesktopGeneration::new();
        let BrokerEventSubscription { replay, mut live } =
            client.subscribe_events(generation, 0).await?;
        assert_eq!(
            replay,
            BrokerEventReplay::Events {
                latest_sequence: 0,
                events: Vec::new(),
            }
        );
        assert!(matches!(
            client.subscribe_events(generation, 0).await,
            Err(BrokerClientError::Rejected {
                code: BrokerErrorCode::ManagerUnavailable,
            })
        ));
        let operation_id = CommandId::new();
        let process = client
            .launch(
                "test-owner",
                operation_id,
                generation,
                ApplicationLaunchCommand {
                    application: ApplicationId::new("sleep")?,
                    arguments: vec![ApplicationArgument::new("30")?],
                },
            )
            .await?;
        assert_eq!(
            client
                .correlate_pids(generation, vec![process.pid, std::process::id()])
                .await?,
            vec![
                BrokerPidCorrelation {
                    pid: process.pid,
                    evidence: BrokerPidCorrelationEvidence::ManagedLeader { process },
                },
                BrokerPidCorrelation {
                    pid: std::process::id(),
                    evidence: BrokerPidCorrelationEvidence::NoMatch,
                },
            ]
        );
        assert!(matches!(
            client
                .correlate_pids(DesktopGeneration::new(), vec![process.pid])
                .await,
            Err(BrokerClientError::Rejected {
                code: BrokerErrorCode::WrongDesktopGeneration
            })
        ));
        assert_eq!(client.status(process).await?.state, ProcessState::Running);
        assert!(matches!(
            client
                .terminate_command(ProcessTerminateCommand {
                    process,
                    grace_ms: Some(MAX_TERMINATION_GRACE_MS + 1),
                })
                .await,
            Err(BrokerClientError::Rejected {
                code: BrokerErrorCode::InvalidRequest
            })
        ));
        assert_eq!(
            client
                .terminate_command(ProcessTerminateCommand {
                    process,
                    grace_ms: Some(0),
                })
                .await?
                .state,
            ProcessState::Exited
        );
        let event = timeout(Duration::from_secs(2), live.receive()).await??;
        let BrokerLiveEvent::Event(event) = event else {
            return Err("missing process exit event".into());
        };
        assert_eq!(event.sequence(), 1);
        assert_eq!(event.principal_id(), "test-owner");
        assert!(event.payload().termination_requested);
        assert!(event.payload().forced_escalation);
        event.payload().validate()?;
        let encoded = serde_json::to_string(&event)?;
        assert!(!encoded.contains("stdout"));
        assert!(!encoded.contains("stderr"));
        drop(live);

        // Closing the peer releases the dedicated stream admission slot even
        // while there are no further process events to wake the broker.
        let mut reopened = None;
        for _ in 0..50 {
            match client.subscribe_events(generation, 1).await {
                Ok(subscription) => {
                    reopened = Some(subscription);
                    break;
                }
                Err(BrokerClientError::Rejected {
                    code: BrokerErrorCode::ManagerUnavailable,
                }) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(error) => return Err(error.into()),
            }
        }
        assert!(reopened.is_some(), "event stream slot was not released");
        drop(reopened);
        cancellation.cancel();
        task.await??;
        fs::remove_dir(&directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn launch_retries_execute_once_recover_lost_replies_and_reject_changed_content()
    -> Result<(), Box<dyn std::error::Error>> {
        let identity = fs::metadata("/proc/self")?;
        let uid = identity.uid();
        let gid = identity.gid();
        let directory =
            std::env::temp_dir().join(format!("xenoteer-processd-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let socket = directory.join("broker.sock");
        let limits = ProcessManagerLimits::new(
            4,
            4,
            4,
            4,
            1_024,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )?;
        let config =
            BrokerConfig::new(socket.clone(), uid, gid, uid, gid).with_process_limits(limits);
        let profile = ApplicationProfile::register(ApplicationProfileSpec {
            application_id: "sleep".to_owned(),
            executable: PathBuf::from("/usr/bin/sleep"),
            fixed_arguments: Vec::new(),
            argument_schema: ArgumentSchema::exact(vec![ArgumentRule::Integer {
                minimum: 1,
                maximum: 60,
            }]),
            base_environment: BTreeMap::new(),
            allowed_environment: BTreeMap::new(),
            working_directory_roots: vec![PathBuf::from("/tmp")],
            default_working_directory: PathBuf::from("/tmp"),
            stdin: StdinPolicy::Null,
        })?;
        let server = BrokerServer::bind(config, vec![profile]).await?;
        let server_state = Arc::clone(&server.state);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(server.serve(cancellation.clone()));
        let client = BrokerClient::new(&socket);
        let generation = DesktopGeneration::new();
        let command = ApplicationLaunchCommand {
            application: ApplicationId::new("sleep")?,
            arguments: vec![ApplicationArgument::new("30")?],
        };

        let concurrent_id = CommandId::new();
        let first_client = client.clone();
        let first_command = command.clone();
        let second_client = client.clone();
        let second_command = command.clone();
        let (first, second) = tokio::join!(
            first_client.launch("test-owner", concurrent_id, generation, first_command),
            second_client.launch("test-owner", concurrent_id, generation, second_command),
        );
        let first = first?;
        let second = second?;
        assert_eq!(
            first, second,
            "concurrent duplicates must publish one launch"
        );

        let other_owner = client
            .launch("other-owner", concurrent_id, generation, command.clone())
            .await?;
        assert_ne!(
            first, other_owner,
            "command IDs are deduplicated only within one principal"
        );

        let lost_reply_id = CommandId::new();
        server_state.drop_next_replies.store(1, Ordering::Release);
        let recovered = client
            .launch("test-owner", lost_reply_id, generation, command.clone())
            .await?;
        let first_attempt = server_state.launch_ledger.lock().await.lookup(
            "test-owner",
            lost_reply_id,
            &LaunchOperation {
                generation,
                command: command.clone(),
            },
        );
        assert_eq!(first_attempt, Some(LaunchLookup::Process(recovered)));
        assert_eq!(client.status(recovered).await?.state, ProcessState::Running);

        let changed = ApplicationLaunchCommand {
            application: ApplicationId::new("sleep")?,
            arguments: vec![ApplicationArgument::new("29")?],
        };
        assert!(matches!(
            client
                .launch("test-owner", lost_reply_id, generation, changed)
                .await,
            Err(BrokerClientError::Rejected {
                code: BrokerErrorCode::OperationIdConflict
            })
        ));

        for process in [first, other_owner, recovered] {
            assert_eq!(
                client
                    .terminate_command(ProcessTerminateCommand {
                        process,
                        grace_ms: Some(0),
                    })
                    .await?
                    .state,
                ProcessState::Exited
            );
        }
        cancellation.cancel();
        task.await??;
        fs::remove_dir(&directory)?;
        Ok(())
    }

    #[test]
    fn launch_ledger_bounds_failures_and_conflicts_on_changed_content()
    -> Result<(), Box<dyn std::error::Error>> {
        let generation = DesktopGeneration::new();
        let operation = LaunchOperation {
            generation,
            command: ApplicationLaunchCommand {
                application: ApplicationId::new("missing")?,
                arguments: Vec::new(),
            },
        };
        let first_id = CommandId::new();
        let second_id = CommandId::new();
        let third_id = CommandId::new();
        let mut ledger = LaunchLedger::new(2, 2);
        ledger.record_rejection(
            "test-owner".to_owned(),
            first_id,
            operation.clone(),
            BrokerErrorCode::ApplicationNotRegistered,
        );
        ledger.record_rejection(
            "test-owner".to_owned(),
            second_id,
            operation.clone(),
            BrokerErrorCode::ApplicationNotRegistered,
        );
        ledger.record_rejection(
            "test-owner".to_owned(),
            third_id,
            operation.clone(),
            BrokerErrorCode::ApplicationNotRegistered,
        );
        assert_eq!(ledger.entries.len(), 2);
        assert_eq!(ledger.lookup("test-owner", first_id, &operation), None);
        assert_eq!(
            ledger.lookup("test-owner", third_id, &operation),
            Some(LaunchLookup::Rejected(
                BrokerErrorCode::ApplicationNotRegistered
            ))
        );
        let changed = LaunchOperation {
            generation,
            command: ApplicationLaunchCommand {
                application: ApplicationId::new("other")?,
                arguments: Vec::new(),
            },
        };
        assert_eq!(
            ledger.lookup("test-owner", third_id, &changed),
            Some(LaunchLookup::Conflict)
        );
        Ok(())
    }

    #[test]
    fn peer_policy_requires_both_dedicated_uid_and_primary_gid() {
        let config = BrokerConfig::new(PathBuf::from("/tmp/test.sock"), 1_001, 1_001, 1_000, 1_000);
        assert!(config.expected_peer_uid != 1_000);
        assert!(config.expected_peer_gid != 1_000);
    }
}
