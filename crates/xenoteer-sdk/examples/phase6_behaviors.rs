// SPDX-License-Identifier: Apache-2.0

//! Installed-artifact qualification for the ten public Phase-6 SDK behaviors.
//!
//! The container harness starts the image-resident GTK3 fixture. Everything
//! else in this example crosses only the public, typed Xenoteer SDK boundary.

use std::{
    env, fmt,
    path::PathBuf,
    pin::Pin,
    process::ExitCode,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use tokio::io::AsyncWrite;
use xenoteer_sdk::{
    AccessibilityQueryLimits, ApplicationArgument, ApplicationId, ArtifactRef, CapabilityStatus,
    Client, Command, CommandHandle, CommandLifecycle, CommandOutcome, CommandResult,
    CommandSubmission, ControlScopeError, Desktop, DesktopProbeCommand, DesktopState,
    EditableTextSelectionPolicy, EffectStage, ElementActionOperation, ElementActionTarget,
    ElementClickPointPolicy, ElementClickScrollPolicy, ElementInvokeCommand,
    ElementOcclusionPolicy, ElementPhysicalClickCommand, ElementPostcondition, ElementPredicate,
    ElementResolveRequest, ElementScope, ElementSelector, ElementSnapshotExpansion, ElementState,
    ElementStringMatch, ElementWaitPredicate, ElementWaitQuantifier, ElementWaitRequest,
    ElementWaitStatus, ElementWaitTarget, ElementWindowActivationPolicy, ErrorCode, PointerCurve,
    PointerLogicalButton, ProcessRef, ProcessState, ProtocolVersion, ScopedCommandSubmission,
    ScopedControl, ScreenshotDelivery, ScreenshotFormat, ScreenshotRequest, ScreenshotTarget,
    SdkError, SecretInlineText, SemanticTextInsertOptions, SemanticTextInsertionPoint,
    TextInsertCommand, TextSource, TextStrategy, TextTarget, ViewerMode, ViewerTicketAudience,
    ViewerTicketRequest, ViewerTicketUsePolicy, WindowOrder, WindowPredicate, WindowRef,
    WindowResolveRequest, WindowSelector, WindowSingleMatchPolicy, WindowStringMatch,
    WindowTextField, WindowWaitPredicate, WindowWaitRequest, WindowWaitSelectorQuantifier,
    WindowWaitStatus, WindowWaitTarget, XenoteerClient,
};

const BEHAVIORS: [&str; 10] = [
    "status-capabilities",
    "scoped-lease-fixture-launch",
    "exact-window-element",
    "semantic-invoke",
    "smooth-physical-click-postcondition",
    "unicode-text-strategy",
    "screenshot-on-failure",
    "reconnect-known-command",
    "stale-reference-restart",
    "view-only-browser-ticket",
];
const GTK_WINDOW_TITLE: &str = "Xenoteer GTK3 Fixture — Main";
const VIEWER_ORIGIN: &str = "https://viewer.example";
const XMESSAGE_TITLE: &str = "xmessage";
const XMESSAGE_BODY: &str = "Xenoteer Phase 6 SDK fixture";
const UNICODE_TEXT: &str = "Xenoteer — العربية — 中文 — e\u{301} — 😀";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Default)]
struct OwnedResources {
    state: Arc<Mutex<OwnedResourceState>>,
}

#[derive(Clone, Default)]
struct OwnedResourceState {
    processes: Vec<ProcessRef>,
    artifacts: Vec<ArtifactRef>,
}

impl OwnedResources {
    fn record_process(&self, process: ProcessRef) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.processes.contains(&process) {
            state.processes.push(process);
        }
    }

    fn forget_process(&self, process: ProcessRef) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .processes
            .retain(|candidate| *candidate != process);
    }

    fn record_artifact(&self, artifact: ArtifactRef) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state
            .artifacts
            .iter()
            .any(|candidate| candidate.artifact_id == artifact.artifact_id)
        {
            state.artifacts.push(artifact);
        }
    }

    fn forget_artifact(&self, artifact: &ArtifactRef) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .artifacts
            .retain(|candidate| candidate.artifact_id != artifact.artifact_id);
    }

    fn snapshot(&self) -> OwnedResourceState {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[derive(Debug)]
struct ExampleCleanupError {
    failures: Vec<String>,
}

impl ExampleCleanupError {
    fn new(failures: Vec<String>) -> Self {
        debug_assert!(!failures.is_empty());
        Self { failures }
    }

    #[cfg(test)]
    fn failures(&self) -> &[String] {
        &self.failures
    }
}

impl fmt::Display for ExampleCleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} cleanup failure(s): {}",
            self.failures.len(),
            self.failures.join(" | ")
        )
    }
}

impl std::error::Error for ExampleCleanupError {}

#[derive(Debug)]
enum ExampleRunError {
    Operation(String),
    Cleanup(ExampleCleanupError),
    OperationAndCleanup {
        operation: String,
        cleanup: ExampleCleanupError,
    },
}

impl fmt::Display for ExampleRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operation(operation) => formatter.write_str(operation),
            Self::Cleanup(cleanup) => write!(formatter, "resource cleanup failed: {cleanup}"),
            Self::OperationAndCleanup { operation, cleanup } => {
                write!(
                    formatter,
                    "{operation}; resource cleanup also failed: {cleanup}"
                )
            }
        }
    }
}

impl std::error::Error for ExampleRunError {}

fn finish_example(
    operation: Result<(), String>,
    cleanup_failures: Vec<String>,
) -> Result<(), ExampleRunError> {
    match (operation, cleanup_failures.is_empty()) {
        (Ok(()), true) => Ok(()),
        (Err(operation), true) => Err(ExampleRunError::Operation(operation)),
        (Ok(()), false) => Err(ExampleRunError::Cleanup(ExampleCleanupError::new(
            cleanup_failures,
        ))),
        (Err(operation), false) => Err(ExampleRunError::OperationAndCleanup {
            operation,
            cleanup: ExampleCleanupError::new(cleanup_failures),
        }),
    }
}

#[derive(Default)]
struct VerifiedBytes(Vec<u8>);

impl AsyncWrite for VerifiedBytes {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        self.0.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

fn required(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("required environment is missing: {name}"))
}

fn verify_installed_origin() -> Result<(), String> {
    let root = PathBuf::from(required("XENOTEER_EXPECTED_INSTALL_ROOT")?)
        .canonicalize()
        .map_err(|_| "staged Rust archive root is unavailable".to_owned())?;
    for package in ["xenoteer-sdk", "xenoteer-protocol"] {
        if !root.join(package).join("Cargo.toml").is_file() {
            return Err(format!("staged Rust archive omitted {package}/Cargo.toml"));
        }
    }
    Ok(())
}

fn emit(language: &str, behavior: &str) {
    println!("quickstart-ok language={language} behavior={behavior}");
}

fn safe_sdk(context: &str, error: SdkError) -> String {
    format!("{context}: {error}")
}

async fn connect_result(
    base: &str,
    token: &str,
) -> Result<Result<XenoteerClient, SdkError>, String> {
    let transport = Client::new(base, token.as_bytes())
        .and_then(|client| client.with_request_timeout(Duration::from_secs(5)))
        .map_err(|error| safe_sdk("could not prepare bounded SDK transport", error))?;
    tokio::time::timeout(
        Duration::from_secs(6),
        XenoteerClient::from_transport(transport),
    )
    .await
    .map_err(|_| "SDK connection exceeded its outer bound".to_owned())
}

async fn terminal(mut handle: CommandHandle) -> Result<CommandResult, String> {
    handle
        .wait_terminal(COMMAND_TIMEOUT)
        .await
        .map_err(|error| safe_sdk("command wait failed", error))?;
    Ok(handle.latest().clone())
}

async fn send(submission: CommandSubmission) -> Result<CommandResult, String> {
    let handle = submission
        .send()
        .await
        .map_err(|error| safe_sdk("command submission failed", error))?;
    terminal(handle).await
}

async fn send_success(submission: CommandSubmission) -> Result<CommandResult, String> {
    let result = send(submission).await?;
    if result.lifecycle() != CommandLifecycle::Succeeded {
        let code = result
            .error()
            .map(|problem| problem.code().as_str())
            .unwrap_or("missing_problem");
        return Err(format!("command did not succeed: {code}"));
    }
    Ok(result)
}

async fn send_scoped_success(
    submission: ScopedCommandSubmission<'_>,
) -> Result<CommandResult, String> {
    let handle = submission
        .send()
        .await
        .map_err(|error| safe_sdk("scoped command submission failed", error))?;
    let result = terminal(handle).await?;
    if result.lifecycle() != CommandLifecycle::Succeeded {
        let code = result
            .error()
            .map(|problem| problem.code().as_str())
            .unwrap_or("missing_problem");
        return Err(format!("scoped command did not succeed: {code}"));
    }
    Ok(result)
}

fn title_selector(title: &str, process: Option<ProcessRef>) -> WindowSelector {
    let title = WindowSelector::Predicate {
        predicate: WindowPredicate::Text {
            field: WindowTextField::Title,
            matcher: WindowStringMatch::Exact {
                value: title.to_owned(),
                case_sensitive: true,
            },
        },
    };
    let Some(process) = process else {
        return title;
    };
    WindowSelector::All {
        selectors: vec![
            title,
            WindowSelector::Predicate {
                predicate: WindowPredicate::ManagedProcess { process },
            },
        ],
    }
}

async fn resolve_window_exact(
    desktop: &Desktop,
    title: &str,
    process: Option<ProcessRef>,
) -> Result<xenoteer_sdk::WindowResolveResult, String> {
    let selector = title_selector(title, process);
    let wait = WindowWaitRequest {
        desktop_id: desktop.id(),
        desktop_generation: desktop.generation(),
        target: WindowWaitTarget::Selector {
            selector: selector.clone(),
            quantifier: WindowWaitSelectorQuantifier::Any,
        },
        predicate: WindowWaitPredicate::Exists,
        after_revision: None,
        timeout_ms: 10_000,
    };
    let waited = desktop
        .windows()
        .wait(&wait)
        .await
        .map_err(|error| safe_sdk("window wait failed", error))?;
    if waited.status != WindowWaitStatus::Matched || !waited.predicate_satisfied {
        return Err(format!("window did not appear exactly: {title}"));
    }

    let request = WindowResolveRequest {
        desktop_id: desktop.id(),
        desktop_generation: desktop.generation(),
        selector,
        order: WindowOrder::CreationAscending,
        match_policy: WindowSingleMatchPolicy::ExactlyOne,
    };
    let resolved = desktop
        .windows()
        .resolve(&request)
        .await
        .map_err(|error| safe_sdk("exact window resolution failed", error))?;
    let observed_title = resolved
        .window
        .snapshot
        .metadata
        .title
        .as_ref()
        .map(|value| value.value.as_str());
    if observed_title != Some(title) {
        return Err("resolved window did not preserve the exact title".to_owned());
    }
    Ok(resolved)
}

fn element_selector(name: &str) -> ElementSelector {
    ElementSelector {
        scope: ElementScope::Desktop,
        predicates: vec![ElementPredicate::Name {
            matcher: ElementStringMatch::Exact {
                value: name.to_owned(),
                case_sensitive: true,
            },
        }],
        order: xenoteer_sdk::ElementOrder::Preorder,
        result_index: None,
    }
}

async fn resolve_element_exact(
    desktop: &Desktop,
    name: &str,
) -> Result<xenoteer_sdk::ElementResolveResult, String> {
    let selector = element_selector(name);
    let expansion = ElementSnapshotExpansion::default();
    let limits = AccessibilityQueryLimits::default();
    let wait = ElementWaitRequest {
        desktop_id: desktop.id(),
        desktop_generation: desktop.generation(),
        target: ElementWaitTarget::Selector {
            selector: selector.clone(),
            quantifier: ElementWaitQuantifier::Any,
        },
        predicate: ElementWaitPredicate::Exists,
        after_revision: None,
        timeout_ms: 10_000,
        allow_poll_fallback: true,
        expansion,
        limits,
    };
    let waited = desktop
        .accessibility()
        .wait(&wait)
        .await
        .map_err(|error| safe_sdk("element wait failed", error))?;
    if waited.status != ElementWaitStatus::Matched || !waited.predicate_satisfied {
        return Err(format!("element did not appear exactly: {name}"));
    }

    let request = ElementResolveRequest {
        desktop_id: desktop.id(),
        desktop_generation: desktop.generation(),
        selector,
        expansion,
        limits,
    };
    let resolved = desktop
        .accessibility()
        .resolve(&request)
        .await
        .map_err(|error| safe_sdk("exact element resolution failed", error))?;
    if resolved.element.snapshot.name.as_deref() != Some(name) {
        return Err("resolved element did not preserve the exact name".to_owned());
    }
    Ok(resolved)
}

async fn require_activation_count(desktop: &Desktop, count: u32) -> Result<(), String> {
    let expected = format!("Activation Count {count}");
    resolve_element_exact(desktop, &expected).await?;
    Ok(())
}

fn launched_process(result: &CommandResult) -> Result<ProcessRef, String> {
    match result.outcome() {
        Some(CommandOutcome::ApplicationLaunched { process }) => Ok(*process),
        _ => Err("launch succeeded without a managed process reference".to_owned()),
    }
}

async fn launch_xmessage(desktop: &Desktop) -> Result<ProcessRef, String> {
    if XMESSAGE_BODY.starts_with('-') || XMESSAGE_BODY.chars().any(char::is_control) {
        return Err("xmessage fixture argument violates the image profile".to_owned());
    }
    let submission = desktop
        .applications()
        .launch(
            ApplicationId::new("xmessage")
                .map_err(|error| format!("invalid registered application: {error}"))?,
            vec![
                ApplicationArgument::new(XMESSAGE_BODY)
                    .map_err(|error| format!("invalid fixture argument: {error}"))?,
            ],
        )
        .map_err(|error| safe_sdk("could not prepare application launch", error))?;
    launched_process(&send_success(submission).await?)
}

async fn terminate_process(
    desktop: &Desktop,
    process: ProcessRef,
    window: &WindowRef,
) -> Result<(), String> {
    let submission = desktop
        .applications()
        .terminate(process, Some(2_000))
        .map_err(|error| safe_sdk("could not prepare process termination", error))?;
    let result = send_success(submission).await?;
    match result.outcome() {
        Some(CommandOutcome::ProcessTerminated { process: view })
            if view.process == process && view.state == ProcessState::Exited => {}
        _ => return Err("termination omitted the exact exited process".to_owned()),
    }

    let wait = WindowWaitRequest {
        desktop_id: desktop.id(),
        desktop_generation: desktop.generation(),
        target: WindowWaitTarget::Reference {
            window: window.clone(),
        },
        predicate: WindowWaitPredicate::Closed,
        after_revision: None,
        timeout_ms: 10_000,
    };
    let waited = desktop
        .windows()
        .wait(&wait)
        .await
        .map_err(|error| safe_sdk("closed-window wait failed", error))?;
    if !matches!(
        waited.status,
        WindowWaitStatus::Matched | WindowWaitStatus::TargetVanished
    ) {
        return Err("terminated process window remained live".to_owned());
    }
    Ok(())
}

async fn cleanup_process(desktop: &Desktop, process: ProcessRef) -> Result<(), String> {
    let submission = desktop
        .applications()
        .terminate(process, Some(2_000))
        .map_err(|error| safe_sdk("could not prepare cleanup process termination", error))?;
    let result = send(submission).await?;
    match (result.lifecycle(), result.outcome(), result.error()) {
        (
            CommandLifecycle::Succeeded,
            Some(CommandOutcome::ProcessTerminated { process: view }),
            _,
        ) if view.process == process && view.state == ProcessState::Exited => Ok(()),
        (CommandLifecycle::Failed, _, Some(problem))
            if matches!(
                problem.code(),
                ErrorCode::NotFound | ErrorCode::StaleReference
            ) =>
        {
            Ok(())
        }
        (_, _, problem) => Err(format!(
            "cleanup termination for pid {} did not prove exit: {}",
            process.pid,
            problem
                .map(|problem| problem.code().as_str())
                .unwrap_or("missing_problem")
        )),
    }
}

async fn cleanup_artifact(desktop: &Desktop, artifact: &ArtifactRef) -> Result<(), String> {
    match desktop.artifacts().delete(artifact).await {
        Ok(()) => Ok(()),
        Err(SdkError::Problem(problem))
            if matches!(
                problem.code(),
                ErrorCode::NotFound | ErrorCode::StaleReference
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(safe_sdk("artifact cleanup deletion failed", error)),
    }
}

fn cleanup_connection_failures(state: &OwnedResourceState, cause: &str) -> Vec<String> {
    let artifact_failures = state.artifacts.iter().map(|artifact| {
        format!(
            "artifact {} cleanup unavailable: {cause}",
            artifact.artifact_id
        )
    });
    let process_failures = state
        .processes
        .iter()
        .map(|process| format!("process pid {} cleanup unavailable: {cause}", process.pid));
    artifact_failures.chain(process_failures).collect()
}

async fn cleanup_owned_resources(
    base: &str,
    token: &str,
    resources: &OwnedResources,
) -> Vec<String> {
    let state = resources.snapshot();
    if state.artifacts.is_empty() && state.processes.is_empty() {
        return Vec::new();
    }

    let cleanup_client = match connect_result(base, token).await {
        Ok(Ok(client)) => client,
        Ok(Err(error)) => {
            return cleanup_connection_failures(
                &state,
                &safe_sdk("cleanup SDK connection failed", error),
            );
        }
        Err(error) => return cleanup_connection_failures(&state, &error),
    };
    let desktop = match cleanup_client.desktop() {
        Ok(desktop) => desktop,
        Err(error) => {
            let failures = cleanup_connection_failures(
                &state,
                &safe_sdk("cleanup connection omitted the desktop", error),
            );
            cleanup_client.close().await;
            return failures;
        }
    };

    let mut failures = Vec::new();
    for artifact in &state.artifacts {
        match cleanup_artifact(&desktop, artifact).await {
            Ok(()) => resources.forget_artifact(artifact),
            Err(error) => failures.push(format!("artifact {}: {error}", artifact.artifact_id)),
        }
    }
    for process in state.processes {
        match cleanup_process(&desktop, process).await {
            Ok(()) => resources.forget_process(process),
            Err(error) => failures.push(format!("process pid {}: {error}", process.pid)),
        }
    }
    cleanup_client.close().await;
    failures
}

fn require_capabilities(client: &XenoteerClient) -> Result<(), String> {
    let required = [
        "accessibility.atspi",
        "application.registered.launch",
        "artifact.private.store",
        "capture.screenshot",
        "input.pointer.smooth",
        "viewer.novnc.view_only",
    ];
    let capabilities = client.status().capabilities.capabilities();
    for identifier in required {
        let capability = capabilities
            .iter()
            .find(|capability| capability.id().as_str() == identifier)
            .ok_or_else(|| format!("status omitted required capability: {identifier}"))?;
        if capability.status() != CapabilityStatus::Available {
            return Err(format!(
                "required capability is not available: {identifier}"
            ));
        }
    }
    Ok(())
}

struct ControlledBehaviorState {
    process: ProcessRef,
    window: xenoteer_sdk::WindowResolveResult,
}

fn scoped_failure(error: ControlScopeError<String>) -> String {
    match error {
        ControlScopeError::Acquisition(error) => {
            safe_sdk("scoped control acquisition failed", error)
        }
        ControlScopeError::Operation(error) => error,
        ControlScopeError::Cleanup(cleanup) => {
            format!("scoped control cleanup failed: {cleanup}")
        }
        ControlScopeError::OperationAndCleanup { operation, cleanup } => {
            format!("{operation}; scoped control cleanup also failed: {cleanup}")
        }
    }
}

async fn exercise_controlled_behaviors(
    language: &str,
    desktop: &Desktop,
    control: ScopedControl<'_>,
    resources: OwnedResources,
) -> Result<ControlledBehaviorState, String> {
    control
        .ensure_healthy()
        .map_err(|error| safe_sdk("new scoped control was unhealthy", error))?;
    let first_process = launch_xmessage(desktop).await?;
    resources.record_process(first_process);
    emit(language, BEHAVIORS[1]);

    let xmessage_window =
        resolve_window_exact(desktop, XMESSAGE_TITLE, Some(first_process)).await?;
    if xmessage_window.window.snapshot.process.managed_process != Some(first_process) {
        return Err("xmessage window was not bound to its exact managed process".to_owned());
    }
    let gtk_window = resolve_window_exact(desktop, GTK_WINDOW_TITLE, None).await?;
    let button = resolve_element_exact(desktop, "Stable Button").await?;
    let _entry = resolve_element_exact(desktop, "Stable Entry").await?;
    emit(language, BEHAVIORS[2]);

    let invoke = Command::ElementInvoke(ElementInvokeCommand {
        element: button.element.snapshot.element.clone(),
        action: ElementActionTarget::Default,
        allow_disabled: false,
        postcondition: None,
    });
    let result = send_success(
        desktop
            .submit(invoke)
            .map_err(|error| safe_sdk("could not prepare semantic invoke", error))?,
    )
    .await?;
    match result.outcome() {
        Some(CommandOutcome::ElementAction { result })
            if result.operation == ElementActionOperation::Invoke
                && result.element == button.element.snapshot.element
                && result.evidence.backend_accepted => {}
        _ => return Err("semantic invoke omitted exact accepted evidence".to_owned()),
    }
    require_activation_count(desktop, 1).await?;
    emit(language, BEHAVIORS[3]);

    let button = resolve_element_exact(desktop, "Stable Button").await?;
    let physical = Command::ElementPhysicalClick(ElementPhysicalClickCommand {
        element: button.element.snapshot.element.clone(),
        window: Some(gtk_window.window.snapshot.window.clone()),
        minimum_correlation: xenoteer_sdk::WindowCorrelationConfidence::Strong,
        point_policy: ElementClickPointPolicy::Center,
        scroll_policy: ElementClickScrollPolicy::IfNeeded,
        activation_policy: ElementWindowActivationPolicy::IfNeeded,
        occlusion_policy: ElementOcclusionPolicy::Ignore,
        button: PointerLogicalButton::Left,
        count: 1,
        interval_ms: 0,
        move_duration_ms: Some(250),
        curve: PointerCurve::Smooth,
        settle_timeout_ms: 5_000,
        postcondition: Some(ElementPostcondition {
            predicate: ElementWaitPredicate::State {
                state: ElementState::Focused,
                value: true,
            },
            timeout_ms: 5_000,
            allow_poll_fallback: true,
        }),
    });
    let result = send_scoped_success(
        control
            .submit(physical)
            .map_err(|error| safe_sdk("could not prepare physical click", error))?,
    )
    .await?;
    match result.outcome() {
        Some(CommandOutcome::ElementPhysicalClick { result })
            if result.element == button.element.snapshot.element
                && result.window == gtk_window.window.snapshot.window
                && result.pointer_interpolated
                && result.count == 1
                && result.postcondition_satisfied == Some(true) => {}
        _ => return Err("physical click omitted interpolation/postcondition evidence".to_owned()),
    }
    require_activation_count(desktop, 2).await?;
    emit(language, BEHAVIORS[4]);

    let entry = resolve_element_exact(desktop, "Stable Entry").await?;
    let clear: Command = serde_json::from_value(serde_json::json!({
        "type": "element_set_text",
        "element": entry.element.snapshot.element,
        "text": "",
        "selection": "collapse_after",
        "verify_length_only": false,
        "postcondition": null
    }))
    .map_err(|error| format!("could not construct typed clear-text command: {error}"))?;
    let cleared = send_success(
        desktop
            .submit(clear)
            .map_err(|error| safe_sdk("could not prepare clear-text command", error))?,
    )
    .await?;
    match cleared.outcome() {
        Some(CommandOutcome::ElementAction { result })
            if result.operation == ElementActionOperation::SetText
                && result.evidence.backend_accepted
                && result.evidence.observed_text_length == Some(0) => {}
        _ => return Err("semantic clear did not verify an empty target".to_owned()),
    }

    let entry = resolve_element_exact(desktop, "Stable Entry").await?;
    let unicode_bytes = UNICODE_TEXT.len() as u64;
    let unicode_scalars = UNICODE_TEXT.chars().count() as u64;
    let insert = Command::TextInsert(TextInsertCommand {
        text: TextSource::Inline {
            text: SecretInlineText::new(UNICODE_TEXT)
                .map_err(|error| format!("invalid exact Unicode fixture: {error}"))?,
        },
        target: TextTarget::Element {
            element: Box::new(entry.element.snapshot.element.clone()),
            window_fallback: None,
        },
        strategy: TextStrategy::Semantic,
        clipboard_options: None,
        semantic_options: Some(SemanticTextInsertOptions {
            insertion_point: SemanticTextInsertionPoint::Offset { offset: 0 },
            selection: EditableTextSelectionPolicy::CollapseAfter,
            verify_length_only: false,
            postcondition: None,
        }),
        auto_policy: None,
    });
    let inserted = send_success(
        desktop
            .submit(insert)
            .map_err(|error| safe_sdk("could not prepare exact Unicode insertion", error))?,
    )
    .await?;
    match inserted.outcome() {
        Some(CommandOutcome::TextInserted { evidence })
            if evidence.selected_strategy == TextStrategy::Semantic
                && evidence.utf8_bytes == unicode_bytes
                && evidence.unicode_scalars == unicode_scalars
                && evidence.completed_scalars == unicode_scalars
                && evidence.semantic.as_ref().is_some_and(|semantic| {
                    semantic.element == entry.element.snapshot.element
                        && semantic.backend_accepted
                        && semantic.insertion_offset == 0
                        && semantic.character_count_before == 0
                        && u64::from(semantic.character_count_after) == unicode_scalars
                        && !semantic.verified_length_only
                }) => {}
        _ => return Err("Unicode insertion omitted exact semantic strategy evidence".to_owned()),
    }
    emit(language, BEHAVIORS[5]);

    Ok(ControlledBehaviorState {
        process: first_process,
        window: xmessage_window,
    })
}

async fn exercise_success(
    language: &str,
    base: &str,
    token: &str,
    client: XenoteerClient,
) -> Result<(), String> {
    let resources = OwnedResources::default();
    let operation =
        exercise_success_operation(language, base, token, client, resources.clone()).await;
    let cleanup_failures = cleanup_owned_resources(base, token, &resources).await;
    finish_example(operation, cleanup_failures).map_err(|error| error.to_string())
}

async fn exercise_success_operation(
    language: &str,
    base: &str,
    token: &str,
    client: XenoteerClient,
    resources: OwnedResources,
) -> Result<(), String> {
    if client.negotiated_protocol() != ProtocolVersion::V1_0 {
        return Err("server did not negotiate frozen protocol v1.0".to_owned());
    }
    if client.status().desktop.state != DesktopState::Ready {
        return Err("desktop was not ready".to_owned());
    }
    require_capabilities(&client)?;
    let desktop = client
        .desktop()
        .map_err(|error| safe_sdk("ready status omitted a desktop", error))?;
    emit(language, BEHAVIORS[0]);

    let controlled_desktop = desktop.clone();
    let controlled_language = language.to_owned();
    let controlled_resources = resources.clone();
    let controlled = desktop
        .with_control(60_000, move |control| {
            Box::pin(async move {
                exercise_controlled_behaviors(
                    &controlled_language,
                    &controlled_desktop,
                    control,
                    controlled_resources,
                )
                .await
            })
        })
        .await
        .map_err(scoped_failure)?;
    let first_process = controlled.process;
    let xmessage_window = controlled.window;

    let button = resolve_element_exact(&desktop, "Stable Button").await?;
    let impossible = Command::ElementInvoke(ElementInvokeCommand {
        element: button.element.snapshot.element,
        action: ElementActionTarget::Default,
        allow_disabled: false,
        postcondition: Some(ElementPostcondition {
            predicate: ElementWaitPredicate::State {
                state: ElementState::Checked,
                value: true,
            },
            timeout_ms: 500,
            allow_poll_fallback: true,
        }),
    });
    let failed = send(
        desktop
            .submit(impossible)
            .map_err(|error| safe_sdk("could not prepare failing postcondition", error))?,
    )
    .await?;
    let problem = failed
        .error()
        .ok_or_else(|| "impossible postcondition unexpectedly succeeded".to_owned())?;
    if failed.lifecycle() != CommandLifecycle::Failed
        || problem.code() != ErrorCode::SemanticPostconditionFailed
        || problem.effect_stage() != EffectStage::SemanticActionDispatched
    {
        return Err("postcondition failure lacked bounded after-effect evidence".to_owned());
    }

    let screenshot = desktop
        .capture()
        .screenshot(&ScreenshotRequest {
            target: ScreenshotTarget::Root,
            region: None,
            format: ScreenshotFormat::Png,
            include_cursor: true,
            scale: None,
            max_bytes: Some(8 * 1_024 * 1_024),
        })
        .await
        .map_err(|error| safe_sdk("failure screenshot capture failed", error))?;
    let artifact = match &screenshot.delivery {
        ScreenshotDelivery::Artifact { artifact } => artifact.clone(),
        _ => return Err("failure screenshot was not delivered as a private artifact".to_owned()),
    };
    resources.record_artifact(artifact.clone());
    let mut bytes = VerifiedBytes::default();
    desktop
        .artifacts()
        .download_to(&artifact, &mut bytes)
        .await
        .map_err(|error| safe_sdk("screenshot artifact verification failed", error))?;
    if bytes.0.len() as u64 != artifact.content_length || !bytes.0.starts_with(b"\x89PNG\r\n\x1a\n")
    {
        return Err("verified screenshot artifact was not the declared PNG".to_owned());
    }
    desktop
        .artifacts()
        .delete(&artifact)
        .await
        .map_err(|error| safe_sdk("screenshot artifact deletion failed", error))?;
    resources.forget_artifact(&artifact);
    let mut deleted = VerifiedBytes::default();
    match desktop
        .artifacts()
        .download_to(&artifact, &mut deleted)
        .await
    {
        Err(SdkError::Problem(problem)) if problem.code() == ErrorCode::NotFound => {}
        Err(error) => {
            return Err(safe_sdk(
                "deleted screenshot returned the wrong typed failure",
                error,
            ));
        }
        Ok(()) => return Err("deleted screenshot artifact remained downloadable".to_owned()),
    }
    emit(language, BEHAVIORS[6]);

    let probe_submission = desktop
        .submit(Command::DesktopProbe(DesktopProbeCommand {}))
        .map_err(|error| safe_sdk("could not prepare reconnect probe", error))?;
    let known_command_id = probe_submission.id();
    let probe = send_success(probe_submission).await?;
    match probe.outcome() {
        Some(CommandOutcome::Probe { ready: true }) => {}
        _ => return Err("known command probe did not report readiness".to_owned()),
    }
    client.close().await;

    let reconnected = connect_result(base, token)
        .await?
        .map_err(|error| safe_sdk("SDK reconnect failed", error))?;
    let desktop = reconnected
        .desktop()
        .map_err(|error| safe_sdk("reconnect omitted the desktop", error))?;
    let recovered = desktop
        .command(known_command_id)
        .await
        .map_err(|error| safe_sdk("known command lookup failed", error))?;
    let recovered = terminal(recovered).await?;
    if recovered.command_id() != known_command_id
        || recovered.lifecycle() != CommandLifecycle::Succeeded
    {
        return Err("reconnect did not recover the exact known command".to_owned());
    }
    emit(language, BEHAVIORS[7]);

    let old_window = xmessage_window.window.snapshot.window.clone();
    let old_token = xmessage_window.window.reference_token.clone();
    terminate_process(&desktop, first_process, &old_window).await?;
    resources.forget_process(first_process);
    let restarted_process = launch_xmessage(&desktop).await?;
    resources.record_process(restarted_process);
    if restarted_process == first_process {
        return Err("application restart reused the managed process identity".to_owned());
    }
    let restarted = resolve_window_exact(&desktop, XMESSAGE_TITLE, Some(restarted_process)).await?;
    if restarted.window.snapshot.window == old_window {
        return Err("application restart reused the exact window-birth identity".to_owned());
    }
    match desktop.windows().snapshot(&old_token).await {
        Err(SdkError::Problem(problem)) if problem.code() == ErrorCode::StaleReference => {}
        Err(error) => {
            return Err(safe_sdk(
                "old window returned the wrong typed failure",
                error,
            ));
        }
        Ok(_) => return Err("old window reference remained live after restart".to_owned()),
    }
    emit(language, BEHAVIORS[8]);

    let ticket = desktop
        .viewer()
        .ticket(
            VIEWER_ORIGIN,
            &ViewerTicketRequest {
                desktop_id: desktop.id(),
                desktop_generation: desktop.generation(),
                mode: ViewerMode::ViewOnly,
            },
        )
        .await
        .map_err(|error| safe_sdk("view-only ticket issuance failed", error))?;
    if ticket.origin.as_str() != VIEWER_ORIGIN
        || ticket.mode != ViewerMode::ViewOnly
        || ticket.audience != ViewerTicketAudience::ViewerWebsocket
        || ticket.use_policy != ViewerTicketUsePolicy::SingleUse
        || ticket.ticket.expose_secret().len() < xenoteer_sdk::MIN_VIEWER_TICKET_BYTES
    {
        return Err("viewer ticket did not preserve exact view-only claims".to_owned());
    }
    terminate_process(
        &desktop,
        restarted_process,
        &restarted.window.snapshot.window,
    )
    .await?;
    resources.forget_process(restarted_process);
    reconnected.close().await;
    emit(language, BEHAVIORS[9]);
    println!("quickstart-ok language={language} mode=success");
    Ok(())
}

async fn exercise() -> Result<(), String> {
    verify_installed_origin()?;
    let language = required("XENOTEER_QUICKSTART_LANGUAGE")?;
    if !language
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
    {
        return Err("quick-start language label is invalid".to_owned());
    }
    let base = required("XENOTEER_API_BASE")?;
    let token = required("XENOTEER_TOKEN")?;
    let expect_auth_failure = required("XENOTEER_EXPECT_AUTH_FAILURE")? == "1";
    let connection = connect_result(&base, &token).await?;

    if expect_auth_failure {
        match connection {
            Err(SdkError::Problem(problem))
                if problem.status() == 401
                    && problem.code() == ErrorCode::AuthenticationRequired =>
            {
                println!("quickstart-ok language={language} mode=auth-failure");
                return Ok(());
            }
            Ok(client) => {
                client.close().await;
                return Err("invalid bearer unexpectedly authenticated".to_owned());
            }
            Err(error) => {
                return Err(safe_sdk(
                    "invalid bearer returned the wrong safe SDK failure",
                    error,
                ));
            }
        }
    }

    let client = connection.map_err(|error| safe_sdk("SDK connection failed", error))?;
    exercise_success(&language, &base, &token, client).await
}

#[tokio::main]
async fn main() -> ExitCode {
    match tokio::time::timeout(Duration::from_secs(110), exercise()).await {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => {
            eprintln!("public Rust quick-start failed: {error}");
            ExitCode::FAILURE
        }
        Err(_) => {
            eprintln!("public Rust quick-start failed: overall behavior deadline exceeded");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_requires_operation_and_cleanup_to_both_succeed() {
        assert!(finish_example(Ok(()), Vec::new()).is_ok());
        match finish_example(Err("behavior failed".to_owned()), Vec::new()) {
            Err(ExampleRunError::Operation(operation)) => {
                assert_eq!(operation, "behavior failed");
            }
            other => panic!("unexpected example result: {other:?}"),
        }
        match finish_example(Ok(()), vec!["artifact cleanup failed".to_owned()]) {
            Err(ExampleRunError::Cleanup(cleanup)) => {
                assert_eq!(cleanup.failures(), ["artifact cleanup failed"]);
            }
            other => panic!("unexpected example result: {other:?}"),
        }
    }

    #[test]
    fn operation_and_every_cleanup_failure_are_preserved() {
        let result = finish_example(
            Err("behavior failed".to_owned()),
            vec![
                "artifact cleanup failed".to_owned(),
                "process cleanup failed".to_owned(),
            ],
        );
        match result {
            Err(ExampleRunError::OperationAndCleanup { operation, cleanup }) => {
                assert_eq!(operation, "behavior failed");
                assert_eq!(
                    cleanup.failures(),
                    ["artifact cleanup failed", "process cleanup failed"]
                );
                let rendered = cleanup.to_string();
                assert!(rendered.contains("artifact cleanup failed"));
                assert!(rendered.contains("process cleanup failed"));
            }
            other => panic!("unexpected example result: {other:?}"),
        }
    }
}
