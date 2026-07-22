use super::*;

use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard, atomic::AtomicU8},
    task::Poll,
};

use xenoteer_protocol::{
    ArtifactContentType, ArtifactId, ArtifactPurpose, SecretInlineText, SelectionClearCommand,
    SelectionTransferFailureReason, SelectionTransferMode, TextInsertOptions, WindowIdentityHash,
};
use xenoteer_x11::{
    ClipboardContentDigest,
    input::{InputEffectEvidence, KeyboardModelDiagnostics, KeyboardOutcomeEvidence},
    keyboard::KeyboardModelAvailability,
};

enum ArmScript {
    Reject(ClipboardRuntimeError),
    Observe(Result<RawClipboardPasteObservation, ClipboardRuntimeError>),
}

struct FakeRuntime {
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    artifacts: Mutex<VecDeque<Result<Vec<u8>, ControlPlaneError>>>,
    sets: Mutex<VecDeque<Result<ClipboardOwnershipEvidence, ClipboardRuntimeError>>>,
    clears: Mutex<VecDeque<Result<ClipboardOwnershipEvidence, ClipboardRuntimeError>>>,
    reads: Mutex<VecDeque<Result<RawClipboardReadResult, ClipboardRuntimeError>>>,
    arms: Mutex<VecDeque<ArmScript>>,
    focuses: Mutex<VecDeque<Result<EffectStage, RuntimeResult>>>,
    keyboards: Mutex<VecDeque<Result<InputOutcome, ClipboardInputError>>>,
    keyboard_preconditions: Mutex<Vec<WindowInputPreconditionSpec>>,
    calls: Mutex<Vec<&'static str>>,
    cancel_after_temporary_set: Option<Arc<AtomicU8>>,
    stop_after_api_set: Option<(Arc<AtomicU8>, u8)>,
}

impl FakeRuntime {
    fn new(desktop_id: DesktopId, generation: DesktopGeneration) -> Self {
        Self {
            desktop_id,
            generation,
            artifacts: Mutex::new(VecDeque::new()),
            sets: Mutex::new(VecDeque::new()),
            clears: Mutex::new(VecDeque::new()),
            reads: Mutex::new(VecDeque::new()),
            arms: Mutex::new(VecDeque::new()),
            focuses: Mutex::new(VecDeque::new()),
            keyboards: Mutex::new(VecDeque::new()),
            keyboard_preconditions: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            cancel_after_temporary_set: None,
            stop_after_api_set: None,
        }
    }

    fn record(&self, call: &'static str) {
        mutex(&self.calls).push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        mutex(&self.calls).clone()
    }

    fn keyboard_preconditions(&self) -> Vec<WindowInputPreconditionSpec> {
        mutex(&self.keyboard_preconditions).clone()
    }
}

impl ClipboardExecutionRuntime for FakeRuntime {
    fn desktop_id(&self) -> DesktopId {
        self.desktop_id
    }

    fn generation(&self) -> DesktopGeneration {
        self.generation
    }

    fn read_artifact<'a>(
        &'a self,
        _principal: &'a PrincipalId,
        _expected: &'a ArtifactRef,
        _maximum_bytes: u64,
    ) -> ClipboardRuntimeFuture<'a, Result<Vec<u8>, ControlPlaneError>> {
        self.record("artifact");
        let result = mutex(&self.artifacts)
            .pop_front()
            .unwrap_or(Err(ControlPlaneError::Internal));
        Box::pin(async move { result })
    }

    fn set<'a>(
        &'a self,
        request: ClipboardSetRequest,
        _deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<ClipboardOwnershipEvidence, ClipboardRuntimeError>> {
        let call = match request.source {
            ClipboardOwnershipSource::Api => "set_api",
            ClipboardOwnershipSource::TemporaryPaste => "set_temporary",
            ClipboardOwnershipSource::RestoredSnapshot => "set_restore",
        };
        self.record(call);
        if request.source == ClipboardOwnershipSource::TemporaryPaste
            && let Some(stop) = &self.cancel_after_temporary_set
        {
            stop.store(1, Ordering::SeqCst);
        }
        if request.source == ClipboardOwnershipSource::Api
            && let Some((stop, reason)) = &self.stop_after_api_set
        {
            stop.store(*reason, Ordering::SeqCst);
        }
        let result = mutex(&self.sets)
            .pop_front()
            .unwrap_or(Err(ClipboardRuntimeError::Closed));
        Box::pin(async move { result })
    }

    fn clear<'a>(
        &'a self,
        _selection: SelectionName,
        _deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<ClipboardOwnershipEvidence, ClipboardRuntimeError>> {
        self.record("clear");
        let result = mutex(&self.clears)
            .pop_front()
            .unwrap_or(Err(ClipboardRuntimeError::Closed));
        Box::pin(async move { result })
    }

    fn read<'a>(
        &'a self,
        _request: ClipboardReadRawRequest,
        _deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<RawClipboardReadResult, ClipboardRuntimeError>> {
        self.record("read");
        let result =
            mutex(&self.reads)
                .pop_front()
                .unwrap_or(Err(ClipboardRuntimeError::Operation(
                    ClipboardActorFailureKind::SelectionHasNoOwner,
                )));
        Box::pin(async move { result })
    }

    fn arm_paste<'a>(
        &'a self,
        _request: ClipboardPasteObservationRequest,
        _deadline: Option<Instant>,
    ) -> ClipboardRuntimeFuture<'a, Result<ArmedPasteObservation, ClipboardRuntimeError>> {
        self.record("arm");
        let script = mutex(&self.arms)
            .pop_front()
            .unwrap_or(ArmScript::Reject(ClipboardRuntimeError::Closed));
        Box::pin(async move {
            match script {
                ArmScript::Reject(error) => Err(error),
                ArmScript::Observe(result) => Ok(ArmedPasteObservation {
                    wait: Box::pin(async move { result }),
                }),
            }
        })
    }

    fn ensure_focused<'a>(
        &'a self,
        _target: WindowRef,
    ) -> ClipboardRuntimeFuture<'a, Result<EffectStage, Box<RuntimeResult>>> {
        self.record("focus");
        let result = mutex(&self.focuses)
            .pop_front()
            .unwrap_or(Ok(EffectStage::None))
            .map_err(Box::new);
        Box::pin(async move { result })
    }

    fn keyboard<'a>(
        &'a self,
        _command_id: CommandId,
        _deadline: Option<Instant>,
        action: KeyboardAction,
        precondition: Option<WindowInputPreconditionSpec>,
        _cancellation: CancellationToken,
    ) -> ClipboardRuntimeFuture<'a, Result<InputOutcome, ClipboardInputError>> {
        if let Some(precondition) = precondition {
            mutex(&self.keyboard_preconditions).push(precondition);
        }
        self.record(if action.event_upper_bound() == 20 {
            "keyboard_paste"
        } else {
            "keyboard_physical"
        });
        let result = mutex(&self.keyboards)
            .pop_front()
            .unwrap_or(Err(ClipboardInputError::Unavailable));
        Box::pin(async move { result })
    }
}

struct FakeContext {
    state: Arc<AtomicU8>,
    deadline: Option<Instant>,
}

impl FakeContext {
    fn running() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(0)),
            deadline: None,
        }
    }

    fn stop_state(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.state)
    }
}

impl ClipboardExecutionContext for FakeContext {
    fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn stop_reason(&self) -> ExecutionStop {
        match self.state.load(Ordering::SeqCst) {
            0 => ExecutionStop::Continue,
            1 => ExecutionStop::Cancelled,
            _ => ExecutionStop::DeadlineExceeded,
        }
    }

    fn wait_for_stop<'a>(&'a mut self) -> ClipboardRuntimeFuture<'a, ExecutionStop> {
        let state = Arc::clone(&self.state);
        Box::pin(std::future::poll_fn(move |_| {
            let reason = match state.load(Ordering::SeqCst) {
                0 => return Poll::Pending,
                1 => ExecutionStop::Cancelled,
                _ => ExecutionStop::DeadlineExceeded,
            };
            Poll::Ready(reason)
        }))
    }
}

fn mutex<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    match value.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn window_ref(desktop_id: DesktopId, generation: DesktopGeneration) -> WindowRef {
    WindowRef {
        desktop_id,
        desktop_generation: generation,
        xid: 41,
        observed_generation: 7,
        identity_hash: WindowIdentityHash::new("a".repeat(64))
            .unwrap_or_else(|_| unreachable!("fixed identity hash is valid")),
    }
}

fn text_command(
    window: WindowRef,
    value: &str,
    strategy: TextStrategy,
    preserve_clipboard: bool,
) -> Command {
    Command::TextInsert(TextInsertCommand {
        text: TextSource::Inline {
            text: SecretInlineText::new(value)
                .unwrap_or_else(|_| unreachable!("bounded fixture text is valid")),
        },
        target: TextTarget::Window { window },
        strategy,
        clipboard_options: matches!(strategy, TextStrategy::Clipboard | TextStrategy::Auto)
            .then_some(TextInsertOptions {
                preserve_clipboard,
                paste_observation_timeout_ms: 250,
            }),
    })
}

fn ownership(owner: u32, revision: u64) -> ClipboardOwnershipEvidence {
    ClipboardOwnershipEvidence {
        selection: SelectionName::Clipboard,
        revision,
        owner,
        server_time: 55,
        verified: true,
    }
}

fn keyboard_outcome(
    command_id: CommandId,
    mode: Option<PhysicalTextMode>,
    scalars: usize,
    current: usize,
    temporary: usize,
) -> InputOutcome {
    let is_text = mode.is_some();
    InputOutcome {
        command_id,
        kind: InputOutcomeKind::Completed,
        events_emitted: if is_text {
            scalars.saturating_mul(2)
        } else {
            4
        },
        completed_units: if is_text {
            u16::try_from(scalars).unwrap_or(u16::MAX)
        } else {
            1
        },
        requested_pointer: None,
        observed_pointer: None,
        observed_logical_buttons_1_to_5: None,
        button_observation_partial: false,
        effects: InputEffectEvidence::RedactedKeyboard {
            provisional: 0,
            confirmed: if is_text {
                scalars.saturating_mul(2)
            } else {
                4
            },
        },
        keyboard: is_text.then(|| {
            Box::new(KeyboardOutcomeEvidence {
                model: KeyboardModelDiagnostics {
                    availability: KeyboardModelAvailability::Available,
                    generation: Some(9),
                    keymap_fingerprint: None,
                },
                bindings: Vec::new(),
                text_scalar_count: Some(scalars),
                requested_text_mode: mode,
                current_layout_scalars: current,
                temporary_mapping_scalars: temporary,
                temporary_mappings_installed: temporary,
                temporary_mappings_restored: temporary,
                temporary_mapping_restoration_proven: (temporary > 0).then_some(true),
            })
        }),
    }
}

fn input_failure(
    command_id: CommandId,
    kind: InputFailureKind,
    events_emitted: usize,
    completed_units: u16,
) -> InputFailure {
    InputFailure {
        command_id: Some(command_id),
        kind,
        events_emitted,
        completed_units,
        progress_known: true,
        requested_pointer: None,
        last_observed_pointer: None,
        observed_logical_buttons_1_to_5: None,
        button_observation_partial: false,
        effects: None,
        cleanup: None,
        keyboard: None,
    }
}

fn raw_digest(bytes: &[u8]) -> ClipboardContentDigest {
    let digest = Sha256::digest(bytes);
    let mut raw = [0_u8; 32];
    raw.copy_from_slice(&digest);
    ClipboardContentDigest::from_sha256_bytes(raw)
}

fn observed_paste(transfer: SelectionTransferMode) -> RawClipboardPasteObservation {
    let terminal_chunk_observed = matches!(transfer, SelectionTransferMode::Incr { .. });
    RawClipboardPasteObservation {
        selection: SelectionName::Clipboard,
        request_observed: true,
        requested_targets: vec![RawClipboardTarget::Utf8String],
        transfer: Some(RawSelectionTransferEvidence {
            target: RawClipboardTarget::Utf8String,
            transfer,
            content_length: 3,
            sha256: raw_digest(b"bot"),
            owner_changed: false,
            terminal_chunk_observed,
            terminal: SelectionTransferTerminal::Completed,
        }),
    }
}

fn no_paste_observed() -> RawClipboardPasteObservation {
    RawClipboardPasteObservation {
        selection: SelectionName::Clipboard,
        request_observed: false,
        requested_targets: Vec::new(),
        transfer: None,
    }
}

fn failed_paste_observed() -> RawClipboardPasteObservation {
    RawClipboardPasteObservation {
        selection: SelectionName::Clipboard,
        request_observed: true,
        requested_targets: vec![RawClipboardTarget::Utf8String],
        transfer: Some(RawSelectionTransferEvidence {
            target: RawClipboardTarget::Utf8String,
            transfer: SelectionTransferMode::Direct,
            content_length: 0,
            sha256: raw_digest(b""),
            owner_changed: false,
            terminal_chunk_observed: false,
            terminal: SelectionTransferTerminal::Failed {
                reason: SelectionTransferFailureReason::Timeout,
            },
        }),
    }
}

fn runtime_failure(result: ExecutionOutcome<RuntimeResult>) -> RuntimeFailure {
    let output = match result {
        ExecutionOutcome::Completed { output, .. }
        | ExecutionOutcome::AtomicCompleted { output, .. } => output,
        ExecutionOutcome::Stopped { .. } => unreachable!("expected a typed runtime failure"),
    };
    match output {
        RuntimeResult::Failure(failure) => failure,
        RuntimeResult::Success(_) => unreachable!("expected a failure result"),
    }
}

fn runtime_success(result: ExecutionOutcome<RuntimeResult>) -> RuntimeSuccess {
    let output = match result {
        ExecutionOutcome::Completed { output, .. }
        | ExecutionOutcome::AtomicCompleted { output, .. } => output,
        ExecutionOutcome::Stopped { .. } => unreachable!("expected a typed runtime success"),
    };
    match output {
        RuntimeResult::Success(success) => success,
        RuntimeResult::Failure(_) => unreachable!("expected a success result"),
    }
}

fn protocol_digest(bytes: &[u8]) -> Sha256Digest {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    Sha256Digest::new(encoded).unwrap_or_else(|_| unreachable!("SHA-256 is canonical"))
}

fn artifact_ref(desktop_id: DesktopId, generation: DesktopGeneration, body: &[u8]) -> ArtifactRef {
    ArtifactRef {
        artifact_id: ArtifactId::new(),
        purpose: ArtifactPurpose::ClipboardInput,
        desktop_id,
        desktop_generation: generation,
        content_type: ArtifactContentType::new("text/plain;charset=utf-8")
            .unwrap_or_else(|_| unreachable!("fixed media type is valid")),
        content_length: body.len() as u64,
        sha256: protocol_digest(body),
        created_at: Timestamp::from_unix_timestamp_nanos(1)
            .unwrap_or_else(|_| unreachable!("fixed timestamp is valid")),
        expires_at: Timestamp::from_unix_timestamp_nanos(2)
            .unwrap_or_else(|_| unreachable!("fixed timestamp is valid")),
    }
}

fn principal() -> PrincipalId {
    PrincipalId::new("clipboard-test").unwrap_or_else(|_| unreachable!("fixed principal is valid"))
}

#[tokio::test]
async fn artifact_tamper_expiry_and_wrong_generation_fail_before_effect() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let expected = b"expected";

    let tampered = FakeRuntime::new(desktop_id, generation);
    mutex(&tampered.artifacts).push_back(Ok(b"tampered".to_vec()));
    let command = Command::TextInsert(TextInsertCommand {
        text: TextSource::Artifact {
            artifact: artifact_ref(desktop_id, generation, expected),
        },
        target: TextTarget::Window {
            window: window_ref(desktop_id, generation),
        },
        strategy: TextStrategy::Physical,
        clipboard_options: None,
    });
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &tampered,
            CommandId::new(),
            principal(),
            command,
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.code, ErrorCode::StaleReference);
    assert_eq!(failure.effect_stage, EffectStage::None);
    assert_eq!(tampered.calls(), vec!["artifact"]);

    let expired = FakeRuntime::new(desktop_id, generation);
    mutex(&expired.artifacts).push_back(Err(ControlPlaneError::NotFound));
    let command = Command::TextInsert(TextInsertCommand {
        text: TextSource::Artifact {
            artifact: artifact_ref(desktop_id, generation, expected),
        },
        target: TextTarget::Window {
            window: window_ref(desktop_id, generation),
        },
        strategy: TextStrategy::Physical,
        clipboard_options: None,
    });
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &expired,
            CommandId::new(),
            principal(),
            command,
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.code, ErrorCode::StaleReference);

    let wrong_generation = DesktopGeneration::new();
    let scoped = FakeRuntime::new(desktop_id, generation);
    let command = Command::TextInsert(TextInsertCommand {
        text: TextSource::Artifact {
            artifact: artifact_ref(desktop_id, wrong_generation, expected),
        },
        target: TextTarget::Window {
            window: window_ref(desktop_id, generation),
        },
        strategy: TextStrategy::Physical,
        clipboard_options: None,
    });
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &scoped,
            CommandId::new(),
            principal(),
            command,
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.code, ErrorCode::InvalidRequest);
    assert!(scoped.calls().is_empty());
}

#[tokio::test]
async fn set_and_clear_require_exact_verified_ownership_evidence() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let runtime = FakeRuntime::new(desktop_id, generation);
    mutex(&runtime.sets).push_back(Ok(ownership(91, 1)));
    let set = Command::SelectionSet(SelectionSetCommand {
        selection: SelectionName::Clipboard,
        content: ClipboardWriteSource::InlineText {
            text: SecretInlineText::new("safe")
                .unwrap_or_else(|_| unreachable!("bounded fixture is valid")),
        },
    });
    let outcome = execute_clipboard_command_with_context(
        &runtime,
        CommandId::new(),
        principal(),
        set,
        &mut FakeContext::running(),
    )
    .await;
    assert!(matches!(outcome, ExecutionOutcome::Completed { .. }));
    let success = runtime_success(outcome);
    assert_eq!(success.outcome, CommandOutcome::Acknowledged);
    assert_eq!(success.effect_stage, EffectStage::ClipboardOwnershipChanged);

    mutex(&runtime.clears).push_back(Ok(ownership(0, 2)));
    let success = runtime_success(
        execute_clipboard_command_with_context(
            &runtime,
            CommandId::new(),
            principal(),
            Command::SelectionClear(SelectionClearCommand {
                selection: SelectionName::Clipboard,
            }),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(success.effect_stage, EffectStage::ClipboardOwnershipChanged);

    mutex(&runtime.sets).push_back(Ok(ClipboardOwnershipEvidence {
        verified: false,
        ..ownership(91, 3)
    }));
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &runtime,
            CommandId::new(),
            principal(),
            Command::SelectionSet(SelectionSetCommand {
                selection: SelectionName::Clipboard,
                content: ClipboardWriteSource::InlineText {
                    text: SecretInlineText::new("safe")
                        .unwrap_or_else(|_| unreachable!("bounded fixture is valid")),
                },
            }),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.effect_stage, EffectStage::OutcomeUnknown);
}

#[tokio::test]
async fn selection_stop_races_preserve_after_effect_without_atomic_success() {
    for reason in [1, 2] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let mut context = FakeContext::running();
        let mut runtime = FakeRuntime::new(desktop_id, generation);
        runtime.stop_after_api_set = Some((context.stop_state(), reason));
        mutex(&runtime.sets).push_back(Ok(ownership(91, 1)));

        let outcome = execute_clipboard_command_with_context(
            &runtime,
            CommandId::new(),
            principal(),
            Command::SelectionSet(SelectionSetCommand {
                selection: SelectionName::Clipboard,
                content: ClipboardWriteSource::InlineText {
                    text: SecretInlineText::new("safe")
                        .unwrap_or_else(|_| unreachable!("bounded fixture is valid")),
                },
            }),
            &mut context,
        )
        .await;

        assert_eq!(
            outcome,
            ExecutionOutcome::Stopped {
                effect: CommandEffect::AfterEffect,
            }
        );
        assert_eq!(runtime.calls(), vec!["set_api"]);
    }
}

#[tokio::test]
async fn selection_stop_before_submission_admits_no_backend_work() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let runtime = FakeRuntime::new(desktop_id, generation);
    let mut context = FakeContext::running();
    context.state.store(1, Ordering::SeqCst);

    let outcome = execute_clipboard_command_with_context(
        &runtime,
        CommandId::new(),
        principal(),
        Command::SelectionClear(SelectionClearCommand {
            selection: SelectionName::Clipboard,
        }),
        &mut context,
    )
    .await;

    assert_eq!(
        outcome,
        ExecutionOutcome::Stopped {
            effect: CommandEffect::BeforeEffect,
        }
    );
    assert!(runtime.calls().is_empty());
}

#[tokio::test]
async fn physical_and_extended_text_use_redacted_scalar_and_mapping_proofs() {
    for (strategy, mode, current, temporary) in [
        (
            TextStrategy::Physical,
            PhysicalTextMode::CurrentLayout,
            3,
            0,
        ),
        (
            TextStrategy::PhysicalExtended,
            PhysicalTextMode::ExtendedTemporaryMapping,
            1,
            2,
        ),
    ] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let runtime = FakeRuntime::new(desktop_id, generation);
        let command_id = CommandId::new();
        let target = window_ref(desktop_id, generation);
        mutex(&runtime.focuses).push_back(Ok(EffectStage::None));
        mutex(&runtime.keyboards).push_back(Ok(keyboard_outcome(
            command_id,
            Some(mode),
            3,
            current,
            temporary,
        )));
        let success = runtime_success(
            execute_clipboard_command_with_context(
                &runtime,
                command_id,
                principal(),
                text_command(target.clone(), "bot", strategy, false),
                &mut FakeContext::running(),
            )
            .await,
        );
        let CommandOutcome::TextInserted { evidence } = success.outcome else {
            unreachable!("text insertion returned the wrong outcome");
        };
        assert_eq!(evidence.selected_strategy, strategy);
        assert_eq!(evidence.completed_scalars, 3);
        assert_eq!(success.effect_stage, EffectStage::TextInserted);
        assert_eq!(runtime.calls(), vec!["focus", "keyboard_physical"]);
        let preconditions = runtime.keyboard_preconditions();
        assert_eq!(preconditions.len(), 1);
        assert_eq!(preconditions[0].target, target);
        assert!(preconditions[0].require_focus);
    }
}

#[tokio::test]
async fn physical_text_near_effect_focus_failure_preserves_activation_stage() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let command_id = CommandId::new();
    let runtime = FakeRuntime::new(desktop_id, generation);
    mutex(&runtime.focuses).push_back(Ok(EffectStage::WindowStateChanged));
    mutex(&runtime.keyboards).push_back(Err(ClipboardInputError::Failure(input_failure(
        command_id,
        InputFailureKind::FocusLost,
        0,
        0,
    ))));

    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &runtime,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Physical,
                false,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.code, ErrorCode::UnsupportedByTarget);
    assert_eq!(failure.effect_stage, EffectStage::WindowStateChanged);
    assert_eq!(runtime.keyboard_preconditions().len(), 1);
}

#[tokio::test]
async fn auto_falls_back_only_from_proven_zero_effect_unrepresentable_text() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let command_id = CommandId::new();
    let runtime = FakeRuntime::new(desktop_id, generation);
    for _ in 0..3 {
        mutex(&runtime.focuses).push_back(Ok(EffectStage::None));
    }
    mutex(&runtime.keyboards).push_back(Err(ClipboardInputError::Failure(input_failure(
        command_id,
        InputFailureKind::TextNotRepresentable,
        0,
        0,
    ))));
    mutex(&runtime.keyboards).push_back(Ok(keyboard_outcome(command_id, None, 0, 0, 0)));
    mutex(&runtime.sets).push_back(Ok(ownership(99, 1)));
    mutex(&runtime.clears).push_back(Ok(ownership(0, 2)));
    mutex(&runtime.arms).push_back(ArmScript::Observe(Ok(observed_paste(
        SelectionTransferMode::Direct,
    ))));

    let success = runtime_success(
        execute_clipboard_command_with_context(
            &runtime,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Auto,
                true,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    let CommandOutcome::TextInserted { evidence } = success.outcome else {
        unreachable!("auto insertion returned the wrong outcome");
    };
    assert_eq!(evidence.selected_strategy, TextStrategy::Clipboard);
    assert_eq!(
        evidence
            .clipboard
            .as_ref()
            .map(|paste| paste.restoration.kind),
        Some(ClipboardRestorationKind::RelinquishedNoOwner)
    );
    assert_eq!(
        runtime.calls(),
        vec![
            "focus",
            "keyboard_physical",
            "focus",
            "read",
            "set_temporary",
            "arm",
            "focus",
            "keyboard_paste",
            "clear",
        ]
    );

    let partial = FakeRuntime::new(desktop_id, generation);
    mutex(&partial.focuses).push_back(Ok(EffectStage::None));
    mutex(&partial.keyboards).push_back(Err(ClipboardInputError::Failure(input_failure(
        command_id,
        InputFailureKind::TextNotRepresentable,
        1,
        0,
    ))));
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &partial,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Auto,
                true,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.effect_stage, EffectStage::TextInserted);
    assert_eq!(partial.calls(), vec!["focus", "keyboard_physical"]);
}

#[tokio::test]
async fn clipboard_paste_accepts_direct_and_incr_and_reports_honest_restoration() {
    for transfer in [
        SelectionTransferMode::Direct,
        SelectionTransferMode::Incr {
            announced_minimum_bytes: 3,
            chunks: 1,
        },
    ] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let command_id = CommandId::new();
        let runtime = FakeRuntime::new(desktop_id, generation);
        let target = window_ref(desktop_id, generation);
        mutex(&runtime.focuses).extend([Ok(EffectStage::None), Ok(EffectStage::None)]);
        let previous = ClipboardPayload::utf8_text("old")
            .unwrap_or_else(|_| unreachable!("bounded fixture is valid"));
        mutex(&runtime.reads).push_back(Ok(RawClipboardReadResult {
            selection: SelectionName::Clipboard,
            revision: 3,
            payload: previous,
            evidence: RawSelectionTransferEvidence {
                target: RawClipboardTarget::Utf8String,
                transfer: SelectionTransferMode::Direct,
                content_length: 3,
                sha256: raw_digest(b"old"),
                owner_changed: false,
                terminal_chunk_observed: false,
                terminal: SelectionTransferTerminal::Completed,
            },
        }));
        mutex(&runtime.sets).extend([Ok(ownership(99, 4)), Ok(ownership(99, 5))]);
        mutex(&runtime.arms).push_back(ArmScript::Observe(Ok(observed_paste(transfer))));
        mutex(&runtime.keyboards).push_back(Ok(keyboard_outcome(command_id, None, 0, 0, 0)));

        let success = runtime_success(
            execute_clipboard_command_with_context(
                &runtime,
                command_id,
                principal(),
                text_command(target.clone(), "bot", TextStrategy::Clipboard, true),
                &mut FakeContext::running(),
            )
            .await,
        );
        let CommandOutcome::TextInserted { evidence } = success.outcome else {
            unreachable!("clipboard insertion returned the wrong outcome");
        };
        let paste = evidence
            .clipboard
            .unwrap_or_else(|| unreachable!("clipboard strategy carries evidence"));
        assert_eq!(
            paste.restoration.kind,
            ClipboardRestorationKind::PartialValueCopy
        );
        assert!(paste.transfer.is_some_and(|evidence| evidence.completed()));
        assert_eq!(
            runtime.calls(),
            vec![
                "focus",
                "read",
                "set_temporary",
                "arm",
                "focus",
                "keyboard_paste",
                "set_restore",
            ]
        );
        let preconditions = runtime.keyboard_preconditions();
        assert_eq!(preconditions.len(), 1);
        assert_eq!(preconditions[0].target, target);
        assert!(preconditions[0].require_focus);
    }
}

#[tokio::test]
async fn paste_near_effect_focus_failure_restores_preserved_no_owner_state() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let command_id = CommandId::new();
    let runtime = FakeRuntime::new(desktop_id, generation);
    mutex(&runtime.focuses).extend([Ok(EffectStage::None), Ok(EffectStage::None)]);
    mutex(&runtime.reads).push_back(Err(ClipboardRuntimeError::Operation(
        ClipboardActorFailureKind::SelectionHasNoOwner,
    )));
    mutex(&runtime.sets).push_back(Ok(ownership(99, 1)));
    mutex(&runtime.clears).push_back(Ok(ownership(0, 2)));
    mutex(&runtime.arms).push_back(ArmScript::Observe(Ok(observed_paste(
        SelectionTransferMode::Direct,
    ))));
    mutex(&runtime.keyboards).push_back(Err(ClipboardInputError::Failure(input_failure(
        command_id,
        InputFailureKind::FocusLost,
        0,
        0,
    ))));

    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &runtime,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Clipboard,
                true,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.code, ErrorCode::UnsupportedByTarget);
    assert_eq!(failure.effect_stage, EffectStage::ClipboardOwnershipChanged);
    assert_eq!(
        runtime.calls(),
        vec![
            "focus",
            "read",
            "set_temporary",
            "arm",
            "focus",
            "keyboard_paste",
            "clear",
        ]
    );
    assert_eq!(runtime.keyboard_preconditions().len(), 1);
}

#[tokio::test]
async fn paste_absence_failed_transfer_and_partial_input_restore_before_failing() {
    for observation in [no_paste_observed(), failed_paste_observed()] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let command_id = CommandId::new();
        let runtime = FakeRuntime::new(desktop_id, generation);
        mutex(&runtime.focuses).extend([Ok(EffectStage::None), Ok(EffectStage::None)]);
        mutex(&runtime.sets).push_back(Ok(ownership(99, 1)));
        mutex(&runtime.clears).push_back(Ok(ownership(0, 2)));
        mutex(&runtime.arms).push_back(ArmScript::Observe(Ok(observation)));
        mutex(&runtime.keyboards).push_back(Ok(keyboard_outcome(command_id, None, 0, 0, 0)));
        let failure = runtime_failure(
            execute_clipboard_command_with_context(
                &runtime,
                command_id,
                principal(),
                text_command(
                    window_ref(desktop_id, generation),
                    "bot",
                    TextStrategy::Clipboard,
                    true,
                ),
                &mut FakeContext::running(),
            )
            .await,
        );
        assert_eq!(failure.code, ErrorCode::BackendFailure);
        assert_eq!(failure.effect_stage, EffectStage::TextInserted);
        assert_eq!(runtime.calls().last(), Some(&"clear"));
    }

    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let command_id = CommandId::new();
    let runtime = FakeRuntime::new(desktop_id, generation);
    mutex(&runtime.focuses).extend([Ok(EffectStage::None), Ok(EffectStage::None)]);
    mutex(&runtime.sets).push_back(Ok(ownership(99, 1)));
    mutex(&runtime.clears).push_back(Ok(ownership(0, 2)));
    mutex(&runtime.arms).push_back(ArmScript::Observe(Ok(observed_paste(
        SelectionTransferMode::Direct,
    ))));
    mutex(&runtime.keyboards).push_back(Err(ClipboardInputError::Failure(input_failure(
        command_id,
        InputFailureKind::CheckedRequestFailed,
        2,
        0,
    ))));
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &runtime,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Clipboard,
                true,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.effect_stage, EffectStage::TextInserted);
    assert_eq!(runtime.calls().last(), Some(&"clear"));
}

#[tokio::test]
async fn cancellation_after_temporary_ownership_always_attempts_restoration() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let mut context = FakeContext::running();
    let mut runtime = FakeRuntime::new(desktop_id, generation);
    runtime.cancel_after_temporary_set = Some(context.stop_state());
    mutex(&runtime.focuses).push_back(Ok(EffectStage::None));
    mutex(&runtime.sets).push_back(Ok(ownership(99, 1)));
    mutex(&runtime.clears).push_back(Ok(ownership(0, 2)));

    let result = execute_clipboard_command_with_context(
        &runtime,
        CommandId::new(),
        principal(),
        text_command(
            window_ref(desktop_id, generation),
            "bot",
            TextStrategy::Clipboard,
            true,
        ),
        &mut context,
    )
    .await;
    assert_eq!(
        result,
        ExecutionOutcome::Stopped {
            effect: CommandEffect::AfterEffect,
        }
    );
    assert_eq!(
        runtime.calls(),
        vec!["focus", "read", "set_temporary", "clear"]
    );
}

#[tokio::test]
async fn deadline_before_focus_is_before_effect_and_admits_no_backend_work() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let runtime = FakeRuntime::new(desktop_id, generation);
    let mut context = FakeContext::running();
    context.state.store(2, Ordering::SeqCst);
    let result = execute_clipboard_command_with_context(
        &runtime,
        CommandId::new(),
        principal(),
        text_command(
            window_ref(desktop_id, generation),
            "bot",
            TextStrategy::Clipboard,
            true,
        ),
        &mut context,
    )
    .await;
    assert_eq!(
        result,
        ExecutionOutcome::Stopped {
            effect: CommandEffect::BeforeEffect,
        }
    );
    assert!(runtime.calls().is_empty());
}

#[tokio::test]
async fn arm_and_second_focus_failures_restore_and_never_inject() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let command_id = CommandId::new();

    let arm_failure = FakeRuntime::new(desktop_id, generation);
    mutex(&arm_failure.focuses).push_back(Ok(EffectStage::None));
    mutex(&arm_failure.sets).push_back(Ok(ownership(99, 1)));
    mutex(&arm_failure.clears).push_back(Ok(ownership(0, 2)));
    mutex(&arm_failure.arms).push_back(ArmScript::Reject(ClipboardRuntimeError::QueueFull));
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &arm_failure,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Clipboard,
                true,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.code, ErrorCode::ResourceExhausted);
    assert_eq!(failure.effect_stage, EffectStage::ClipboardOwnershipChanged);
    assert_eq!(arm_failure.calls().last(), Some(&"clear"));

    let focus_failure = FakeRuntime::new(desktop_id, generation);
    mutex(&focus_failure.focuses).extend([
        Ok(EffectStage::None),
        Err(text_target_not_focused(EffectStage::None)),
    ]);
    mutex(&focus_failure.sets).push_back(Ok(ownership(99, 1)));
    mutex(&focus_failure.clears).push_back(Ok(ownership(0, 2)));
    mutex(&focus_failure.arms).push_back(ArmScript::Observe(Ok(observed_paste(
        SelectionTransferMode::Direct,
    ))));
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &focus_failure,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Clipboard,
                true,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.code, ErrorCode::UnsupportedByTarget);
    assert_eq!(failure.effect_stage, EffectStage::ClipboardOwnershipChanged);
    assert!(!focus_failure.calls().contains(&"keyboard_paste"));
    assert_eq!(focus_failure.calls().last(), Some(&"clear"));
}

#[tokio::test]
async fn restoration_failure_is_reported_without_revoking_proven_paste_success() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let command_id = CommandId::new();
    let runtime = FakeRuntime::new(desktop_id, generation);
    mutex(&runtime.focuses).extend([Ok(EffectStage::None), Ok(EffectStage::None)]);
    let previous = ClipboardPayload::utf8_text("old")
        .unwrap_or_else(|_| unreachable!("bounded fixture is valid"));
    mutex(&runtime.reads).push_back(Ok(RawClipboardReadResult {
        selection: SelectionName::Clipboard,
        revision: 1,
        payload: previous,
        evidence: RawSelectionTransferEvidence {
            target: RawClipboardTarget::Utf8String,
            transfer: SelectionTransferMode::Direct,
            content_length: 3,
            sha256: raw_digest(b"old"),
            owner_changed: false,
            terminal_chunk_observed: false,
            terminal: SelectionTransferTerminal::Completed,
        },
    }));
    mutex(&runtime.sets).extend([
        Ok(ownership(99, 2)),
        Err(ClipboardRuntimeError::Operation(
            ClipboardActorFailureKind::OwnershipRace,
        )),
    ]);
    mutex(&runtime.arms).push_back(ArmScript::Observe(Ok(observed_paste(
        SelectionTransferMode::Direct,
    ))));
    mutex(&runtime.keyboards).push_back(Ok(keyboard_outcome(command_id, None, 0, 0, 0)));
    let success = runtime_success(
        execute_clipboard_command_with_context(
            &runtime,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Clipboard,
                true,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    let CommandOutcome::TextInserted { evidence } = success.outcome else {
        unreachable!("clipboard insertion returned the wrong outcome");
    };
    assert_eq!(
        evidence.clipboard.map(|paste| paste.restoration.kind),
        Some(ClipboardRestorationKind::Failed)
    );
}

#[tokio::test]
async fn preservation_disabled_intentionally_leaves_temporary_value() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let command_id = CommandId::new();
    let runtime = FakeRuntime::new(desktop_id, generation);
    mutex(&runtime.focuses).extend([Ok(EffectStage::None), Ok(EffectStage::None)]);
    mutex(&runtime.sets).push_back(Ok(ownership(99, 1)));
    mutex(&runtime.arms).push_back(ArmScript::Observe(Ok(observed_paste(
        SelectionTransferMode::Direct,
    ))));
    mutex(&runtime.keyboards).push_back(Ok(keyboard_outcome(command_id, None, 0, 0, 0)));
    let success = runtime_success(
        execute_clipboard_command_with_context(
            &runtime,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Clipboard,
                false,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    let CommandOutcome::TextInserted { evidence } = success.outcome else {
        unreachable!("clipboard insertion returned the wrong outcome");
    };
    assert_eq!(
        evidence.clipboard.map(|paste| paste.restoration.kind),
        Some(ClipboardRestorationKind::NotRequested)
    );
    assert!(!runtime.calls().contains(&"read"));
    assert!(!runtime.calls().contains(&"clear"));
    assert!(!runtime.calls().contains(&"set_restore"));
}

#[tokio::test]
async fn bounded_queue_rejections_never_claim_an_effect() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let runtime = FakeRuntime::new(desktop_id, generation);
    mutex(&runtime.sets).push_back(Err(ClipboardRuntimeError::QueueFull));
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &runtime,
            CommandId::new(),
            principal(),
            Command::SelectionSet(SelectionSetCommand {
                selection: SelectionName::Clipboard,
                content: ClipboardWriteSource::InlineText {
                    text: SecretInlineText::new("bot")
                        .unwrap_or_else(|_| unreachable!("bounded fixture is valid")),
                },
            }),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.code, ErrorCode::ResourceExhausted);
    assert_eq!(failure.effect_stage, EffectStage::None);
}

#[tokio::test]
async fn mutation_arm_and_observation_timeouts_all_take_the_restoration_path() {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let command_id = CommandId::new();

    let mutation_timeout = FakeRuntime::new(desktop_id, generation);
    mutex(&mutation_timeout.focuses).push_back(Ok(EffectStage::None));
    mutex(&mutation_timeout.sets).push_back(Err(ClipboardRuntimeError::ReplyTimedOut));
    mutex(&mutation_timeout.clears).push_back(Ok(ownership(0, 2)));
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &mutation_timeout,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Clipboard,
                true,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.code, ErrorCode::RequestOutcomeUnknown);
    assert_eq!(mutation_timeout.calls().last(), Some(&"clear"));

    let arm_timeout = FakeRuntime::new(desktop_id, generation);
    mutex(&arm_timeout.focuses).push_back(Ok(EffectStage::None));
    mutex(&arm_timeout.sets).push_back(Ok(ownership(99, 1)));
    mutex(&arm_timeout.clears).push_back(Ok(ownership(0, 2)));
    mutex(&arm_timeout.arms).push_back(ArmScript::Reject(ClipboardRuntimeError::ReplyTimedOut));
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &arm_timeout,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Clipboard,
                true,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.code, ErrorCode::BackendFailure);
    assert_eq!(arm_timeout.calls().last(), Some(&"clear"));

    let observation_timeout = FakeRuntime::new(desktop_id, generation);
    mutex(&observation_timeout.focuses).extend([Ok(EffectStage::None), Ok(EffectStage::None)]);
    mutex(&observation_timeout.sets).push_back(Ok(ownership(99, 1)));
    mutex(&observation_timeout.clears).push_back(Ok(ownership(0, 2)));
    mutex(&observation_timeout.arms).push_back(ArmScript::Observe(Err(
        ClipboardRuntimeError::ReplyTimedOut,
    )));
    mutex(&observation_timeout.keyboards)
        .push_back(Ok(keyboard_outcome(command_id, None, 0, 0, 0)));
    let failure = runtime_failure(
        execute_clipboard_command_with_context(
            &observation_timeout,
            command_id,
            principal(),
            text_command(
                window_ref(desktop_id, generation),
                "bot",
                TextStrategy::Clipboard,
                true,
            ),
            &mut FakeContext::running(),
        )
        .await,
    );
    assert_eq!(failure.effect_stage, EffectStage::TextInserted);
    assert_eq!(observation_timeout.calls().last(), Some(&"clear"));
}

#[test]
fn payload_command_action_and_failures_keep_secret_bytes_out_of_debug() {
    const SECRET: &str = "needle-super-secret-clipboard-value";
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let command = text_command(
        window_ref(desktop_id, generation),
        SECRET,
        TextStrategy::Physical,
        false,
    );
    let payload = ClipboardPayload::utf8_text(SECRET)
        .unwrap_or_else(|_| unreachable!("bounded fixture is valid"));
    let action = KeyboardAction::physical_text(
        SECRET,
        PhysicalTextMode::CurrentLayout,
        PHYSICAL_TEXT_INTERVAL_MS,
    )
    .unwrap_or_else(|_| unreachable!("bounded fixture is valid"));
    let failure = invalid_clipboard_request();
    for diagnostic in [
        format!("{command:?}"),
        format!("{payload:?}"),
        format!("{action:?}"),
        format!("{failure:?}"),
    ] {
        assert!(!diagnostic.contains(SECRET));
    }
}

#[test]
fn impossible_physical_progress_or_mapping_evidence_fails_closed() {
    let command_id = CommandId::new();
    let mut no_events =
        keyboard_outcome(command_id, Some(PhysicalTextMode::CurrentLayout), 3, 3, 0);
    no_events.events_emitted = 0;
    no_events.effects = InputEffectEvidence::RedactedKeyboard {
        provisional: 0,
        confirmed: 0,
    };
    let failure = runtime_failure(complete_physical_text(
        no_events,
        command_id,
        PhysicalTextMode::CurrentLayout,
        3,
        3,
        EffectStage::None,
    ));
    assert_eq!(failure.code, ErrorCode::BackendFailure);

    let mut bad_restoration = keyboard_outcome(
        command_id,
        Some(PhysicalTextMode::ExtendedTemporaryMapping),
        3,
        1,
        2,
    );
    let keyboard = bad_restoration
        .keyboard
        .as_mut()
        .unwrap_or_else(|| unreachable!("text fixture carries keyboard evidence"));
    keyboard.temporary_mapping_restoration_proven = Some(false);
    let failure = runtime_failure(complete_physical_text(
        bad_restoration,
        command_id,
        PhysicalTextMode::ExtendedTemporaryMapping,
        3,
        3,
        EffectStage::None,
    ));
    assert_eq!(failure.code, ErrorCode::BackendFailure);
}

#[test]
fn raw_clipboard_targets_never_project_internal_protocol_atoms() {
    assert!(public_clipboard_target(RawClipboardTarget::Targets).is_none());
    assert!(public_clipboard_target(RawClipboardTarget::Timestamp).is_none());
    assert!(public_clipboard_target(RawClipboardTarget::Multiple).is_none());
    assert_eq!(
        public_clipboard_target(RawClipboardTarget::Utf8String)
            .map(|target| target.as_str().to_owned()),
        Some("UTF8_STRING".to_owned())
    );
}
