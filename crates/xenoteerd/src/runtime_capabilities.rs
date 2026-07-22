//! Synchronous projection of live daemon/backend evidence into public capabilities.

use std::sync::Arc;

use tokio::sync::watch;
use xenoteer_core::input::InputHealth;
use xenoteer_protocol::{
    Capability, CapabilityId, CapabilityIdError, CapabilityReport, CapabilityReportError,
    CapabilityStatus, WindowManagerCapability,
};
use xenoteer_server::{CapabilityProvider, DesktopReadiness, ReadinessHandle};
use xenoteer_x11::{
    ClipboardActorHandle, ClipboardActorState, WindowControlActorHandle, WindowControlActorState,
    capture::{CaptureActorHandle, CaptureActorState},
    input::{ActorThreadState, InputActorHandle},
    keyboard::KeyboardModelAvailability,
};

use crate::capability_monitor::{
    BackendCapabilityEvidenceState, OperationBackendReader, WindowCapabilityEvidenceState,
    WindowCapabilityReader, WindowCapabilitySnapshot,
};
use crate::observation_plane::{DaemonObservationService, ObservationServiceState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProjectedStatus {
    status: CapabilityStatus,
    reason_code: Option<&'static str>,
}

impl ProjectedStatus {
    const AVAILABLE: Self = Self::new(CapabilityStatus::Available, None);

    const fn new(status: CapabilityStatus, reason_code: Option<&'static str>) -> Self {
        Self {
            status,
            reason_code,
        }
    }

    const fn unavailable(reason_code: &'static str) -> Self {
        Self::new(CapabilityStatus::Unavailable, Some(reason_code))
    }

    const fn degraded(reason_code: &'static str) -> Self {
        Self::new(CapabilityStatus::Degraded, Some(reason_code))
    }
}

#[derive(Clone, Debug)]
struct RuntimeCapabilitySnapshot {
    desktop: ProjectedStatus,
    artifact: ProjectedStatus,
    process: ProjectedStatus,
    observation: ProjectedStatus,
    input: ProjectedStatus,
    input_reset: ProjectedStatus,
    physical_text: ProjectedStatus,
    capture: ProjectedStatus,
    clipboard: ProjectedStatus,
    viewer: ProjectedStatus,
    window_actor: ProjectedStatus,
    window_capabilities: WindowCapabilitySnapshot,
}

/// Live capability source. Reads are cheap snapshots and never perform X11 I/O.
pub(crate) struct RuntimeCapabilityProvider {
    readiness: ReadinessHandle,
    viewer_configured: bool,
    input: watch::Receiver<Option<InputActorHandle>>,
    observation: Arc<DaemonObservationService>,
    capture: CaptureActorHandle,
    clipboard: ClipboardActorHandle,
    window: WindowControlActorHandle,
    operation_backends: OperationBackendReader,
    window_capabilities: WindowCapabilityReader,
    invariant_fallback: CapabilityReport,
}

/// Live operation backends grouped separately from desktop policy/readiness.
pub(crate) struct RuntimeCapabilityBackends {
    observation: Arc<DaemonObservationService>,
    capture: CaptureActorHandle,
    clipboard: ClipboardActorHandle,
    window: WindowControlActorHandle,
    operation_backends: OperationBackendReader,
    window_capabilities: WindowCapabilityReader,
}

impl RuntimeCapabilityBackends {
    pub(crate) fn new(
        observation: Arc<DaemonObservationService>,
        capture: CaptureActorHandle,
        clipboard: ClipboardActorHandle,
        window: WindowControlActorHandle,
        operation_backends: OperationBackendReader,
        window_capabilities: WindowCapabilityReader,
    ) -> Self {
        Self {
            observation,
            capture,
            clipboard,
            window,
            operation_backends,
            window_capabilities,
        }
    }
}

impl RuntimeCapabilityProvider {
    pub(crate) fn new(
        readiness: ReadinessHandle,
        viewer_configured: bool,
        input: watch::Receiver<Option<InputActorHandle>>,
        backends: RuntimeCapabilityBackends,
    ) -> Result<Self, RuntimeCapabilityError> {
        Ok(Self {
            readiness,
            viewer_configured,
            input,
            observation: backends.observation,
            capture: backends.capture,
            clipboard: backends.clipboard,
            window: backends.window,
            operation_backends: backends.operation_backends,
            window_capabilities: backends.window_capabilities,
            invariant_fallback: CapabilityReport::checked(Vec::new())?,
        })
    }

    fn live_snapshot(&self) -> RuntimeCapabilitySnapshot {
        let readiness = self.readiness.snapshot();
        let desktop = if readiness.is_ready() {
            ProjectedStatus::AVAILABLE
        } else {
            ProjectedStatus::unavailable("desktop_not_ready")
        };
        let operation_backends = self.operation_backends.snapshot();
        let artifact = project_backend_evidence(
            operation_backends.artifact,
            "artifact_backend_evidence_pending",
            "stale_artifact_backend_evidence",
            "artifact_backend_unavailable",
        );
        let process = project_backend_evidence(
            operation_backends.process,
            "process_backend_evidence_pending",
            "stale_process_backend_evidence",
            "process_backend_unavailable",
        );
        let observation = project_actor(
            desktop,
            self.observation.health() == ObservationServiceState::Healthy,
            "observation_backend_unavailable",
        );
        let input_handle = self.input.borrow();
        let (input, input_reset, physical_text) = input_handle.as_ref().map_or_else(
            || {
                let unavailable = ProjectedStatus::unavailable("input_actor_unavailable");
                (unavailable, unavailable, unavailable)
            },
            |handle| project_input(handle, desktop),
        );
        let capture = project_actor(
            desktop,
            self.capture.health().state == CaptureActorState::Healthy,
            "capture_actor_unavailable",
        );
        let clipboard = project_actor(
            desktop,
            self.clipboard.health().state == ClipboardActorState::Healthy,
            "clipboard_actor_unavailable",
        );
        let window_actor = combine(
            project_actor(
                desktop,
                self.window.health().state == WindowControlActorState::Healthy,
                "window_actor_unavailable",
            ),
            observation,
        );
        let viewer = if !self.viewer_configured {
            ProjectedStatus::new(CapabilityStatus::Disabled, Some("viewer_disabled"))
        } else if !readiness.is_ready()
            || (readiness.state == DesktopReadiness::Degraded
                && readiness.reason_code.as_deref() == Some("optional_viewer_unavailable"))
        {
            ProjectedStatus::unavailable("viewer_backend_unavailable")
        } else {
            ProjectedStatus::AVAILABLE
        };

        RuntimeCapabilitySnapshot {
            desktop,
            artifact,
            process,
            observation,
            input,
            input_reset,
            physical_text,
            capture,
            clipboard,
            viewer,
            window_actor,
            window_capabilities: self.window_capabilities.snapshot(),
        }
    }
}

impl CapabilityProvider for RuntimeCapabilityProvider {
    fn capabilities(&self) -> CapabilityReport {
        match build_report(&self.live_snapshot()) {
            Ok(report) => report,
            Err(error) => {
                tracing::error!(%error, "runtime capability projection invariant failed");
                self.invariant_fallback.clone()
            }
        }
    }
}

fn project_backend_evidence(
    evidence: BackendCapabilityEvidenceState,
    pending_reason: &'static str,
    stale_reason: &'static str,
    unavailable_reason: &'static str,
) -> ProjectedStatus {
    match evidence {
        BackendCapabilityEvidenceState::Pending => ProjectedStatus::unavailable(pending_reason),
        BackendCapabilityEvidenceState::Current => ProjectedStatus::AVAILABLE,
        BackendCapabilityEvidenceState::Stale => ProjectedStatus::degraded(stale_reason),
        BackendCapabilityEvidenceState::Unavailable => {
            ProjectedStatus::unavailable(unavailable_reason)
        }
    }
}

fn project_actor(
    desktop: ProjectedStatus,
    actor_healthy: bool,
    unavailable_reason: &'static str,
) -> ProjectedStatus {
    if !actor_healthy {
        ProjectedStatus::unavailable(unavailable_reason)
    } else if desktop.status != CapabilityStatus::Available {
        desktop
    } else {
        ProjectedStatus::AVAILABLE
    }
}

fn project_input(
    handle: &InputActorHandle,
    desktop: ProjectedStatus,
) -> (ProjectedStatus, ProjectedStatus, ProjectedStatus) {
    let health = handle.health();
    if health.thread != ActorThreadState::Running {
        let unavailable = ProjectedStatus::unavailable("input_actor_unavailable");
        return (unavailable, unavailable, unavailable);
    }
    if desktop.status != CapabilityStatus::Available {
        return (desktop, desktop, desktop);
    }

    let (input, reset) = match health.input {
        InputHealth::Healthy => (ProjectedStatus::AVAILABLE, ProjectedStatus::AVAILABLE),
        InputHealth::ResetRequired(_) => (
            ProjectedStatus::degraded("input_reset_required"),
            ProjectedStatus::AVAILABLE,
        ),
        InputHealth::Poisoned(_) => {
            let unavailable = ProjectedStatus::unavailable("input_actor_poisoned");
            (unavailable, unavailable)
        }
    };
    let physical_text = if input.status == CapabilityStatus::Unavailable {
        input
    } else if health.keyboard_model.availability != KeyboardModelAvailability::Available
        || health.keyboard_model.generation.is_none()
    {
        ProjectedStatus::unavailable("keyboard_model_unavailable")
    } else {
        input
    };
    (input, reset, physical_text)
}

fn build_report(
    snapshot: &RuntimeCapabilitySnapshot,
) -> Result<CapabilityReport, RuntimeCapabilityError> {
    let mut capabilities = Vec::with_capacity(32);
    for (id, status) in [
        (
            "application.registered.launch",
            combine(snapshot.desktop, snapshot.process),
        ),
        ("artifact.private.store", snapshot.artifact),
        ("process.managed.status", snapshot.process),
        ("process.managed.terminate", snapshot.process),
        ("window.observe.inventory", snapshot.observation),
        ("window.observe.query", snapshot.observation),
        ("window.observe.wait", snapshot.observation),
        ("input.pointer.smooth", snapshot.input),
        ("input.pointer.xtest", snapshot.input),
        ("input.keyboard.xtest", snapshot.input),
        ("input.keyboard.raw_keycode", snapshot.input),
        ("input.reset.owned", snapshot.input_reset),
        ("input.text.physical", snapshot.physical_text),
        ("input.text.physical_extended", snapshot.physical_text),
        (
            "clipboard.selection.read",
            combine_optional_artifact(snapshot.clipboard, snapshot.artifact),
        ),
        (
            "clipboard.selection.write",
            combine_optional_artifact(snapshot.clipboard, snapshot.artifact),
        ),
        (
            "input.text.clipboard",
            combine_optional_artifact(
                combine(snapshot.input, snapshot.clipboard),
                snapshot.artifact,
            ),
        ),
        (
            "capture.screenshot",
            combine(snapshot.capture, snapshot.artifact),
        ),
        (
            "capture.root.png",
            combine(snapshot.capture, snapshot.artifact),
        ),
        (
            "capture.root.raw",
            combine(snapshot.capture, snapshot.artifact),
        ),
        (
            "capture.window.visible",
            combine(
                combine(snapshot.capture, snapshot.artifact),
                snapshot.observation,
            ),
        ),
        (
            "capture.window.drawable",
            combine(
                combine(snapshot.capture, snapshot.artifact),
                snapshot.observation,
            ),
        ),
        ("viewer.novnc.view_only", snapshot.viewer),
    ] {
        capabilities.push(capability(id, status)?);
    }

    append_window_capabilities(&mut capabilities, snapshot)?;
    Ok(CapabilityReport::checked(capabilities)?)
}

fn combine(left: ProjectedStatus, right: ProjectedStatus) -> ProjectedStatus {
    use CapabilityStatus::{Available, Degraded, Disabled, Unavailable};
    match (left.status, right.status) {
        (Unavailable, _) | (_, Unavailable) => {
            ProjectedStatus::unavailable("capability_dependency_unavailable")
        }
        (Disabled, _) | (_, Disabled) => {
            ProjectedStatus::new(Disabled, Some("capability_dependency_disabled"))
        }
        (Degraded, _) | (_, Degraded) => {
            ProjectedStatus::degraded("capability_dependency_degraded")
        }
        (Available, Available) => ProjectedStatus::AVAILABLE,
    }
}

fn combine_optional_artifact(
    primary: ProjectedStatus,
    artifact: ProjectedStatus,
) -> ProjectedStatus {
    if primary.status == CapabilityStatus::Unavailable
        || primary.status == CapabilityStatus::Disabled
    {
        return primary;
    }
    match artifact.status {
        CapabilityStatus::Available => primary,
        CapabilityStatus::Degraded | CapabilityStatus::Unavailable | CapabilityStatus::Disabled => {
            ProjectedStatus::degraded("artifact_payload_variant_unavailable")
        }
    }
}

fn append_window_capabilities(
    output: &mut Vec<Capability>,
    snapshot: &RuntimeCapabilitySnapshot,
) -> Result<(), RuntimeCapabilityError> {
    let operations = [
        ("window.ewmh.activate", WindowManagerCapability::Activate),
        ("window.ewmh.close", WindowManagerCapability::Close),
        (
            "window.ewmh.state.maximized",
            WindowManagerCapability::StateMaximized,
        ),
        (
            "window.ewmh.state.fullscreen",
            WindowManagerCapability::StateFullscreen,
        ),
        (
            "window.ewmh.state.above",
            WindowManagerCapability::StateAbove,
        ),
        (
            "window.ewmh.state.sticky",
            WindowManagerCapability::StateSticky,
        ),
        (
            "window.ewmh.move_resize",
            WindowManagerCapability::MoveResize,
        ),
        (
            "window.ewmh.move_to_workspace",
            WindowManagerCapability::MoveToWorkspace,
        ),
    ];
    let operation_count = operations.len();
    let mut supported_count = 0;
    for (id, operation) in operations {
        let status = window_operation_status(snapshot, operation);
        supported_count += usize::from(matches!(
            status.status,
            CapabilityStatus::Available | CapabilityStatus::Degraded
        ));
        output.push(capability(id, status)?);
    }

    output.push(capability(
        "window.control.close",
        window_close_status(snapshot),
    )?);
    output.push(capability(
        "window.icccm.minimize",
        window_minimize_status(snapshot),
    )?);
    output.push(capability(
        "window.control.stack",
        window_stack_status(snapshot),
    )?);

    let legacy = if snapshot.window_actor.status != CapabilityStatus::Available {
        snapshot.window_actor
    } else {
        match snapshot.window_capabilities.evidence_state {
            WindowCapabilityEvidenceState::Current if supported_count == operation_count => {
                ProjectedStatus::AVAILABLE
            }
            WindowCapabilityEvidenceState::Current if supported_count > 0 => {
                ProjectedStatus::degraded("partial_window_manager_support")
            }
            WindowCapabilityEvidenceState::Stale if supported_count > 0 => {
                ProjectedStatus::degraded("stale_window_manager_evidence")
            }
            WindowCapabilityEvidenceState::Pending
            | WindowCapabilityEvidenceState::Unavailable
            | WindowCapabilityEvidenceState::Stale
            | WindowCapabilityEvidenceState::Current => {
                ProjectedStatus::unavailable("window_manager_capabilities_unavailable")
            }
        }
    };
    output.push(capability("window.control.ewmh", legacy)?);
    Ok(())
}

fn window_close_status(snapshot: &RuntimeCapabilitySnapshot) -> ProjectedStatus {
    if snapshot.window_actor.status != CapabilityStatus::Available {
        return snapshot.window_actor;
    }
    let ewmh = window_operation_status(snapshot, WindowManagerCapability::Close);
    match ewmh.status {
        CapabilityStatus::Available => ProjectedStatus::AVAILABLE,
        CapabilityStatus::Degraded => ewmh,
        CapabilityStatus::Unavailable | CapabilityStatus::Disabled => {
            ProjectedStatus::degraded("target_dependent_wm_delete")
        }
    }
}

fn window_minimize_status(snapshot: &RuntimeCapabilitySnapshot) -> ProjectedStatus {
    if snapshot.window_actor.status != CapabilityStatus::Available {
        return snapshot.window_actor;
    }
    let activation = window_operation_status(snapshot, WindowManagerCapability::Activate);
    match activation.status {
        CapabilityStatus::Available => ProjectedStatus::AVAILABLE,
        CapabilityStatus::Degraded => activation,
        CapabilityStatus::Unavailable | CapabilityStatus::Disabled => {
            ProjectedStatus::degraded("restore_requires_window_activation")
        }
    }
}

fn window_stack_status(snapshot: &RuntimeCapabilitySnapshot) -> ProjectedStatus {
    if snapshot.window_actor.status != CapabilityStatus::Available {
        return snapshot.window_actor;
    }
    let Some(capabilities) = snapshot.window_capabilities.capabilities.as_ref() else {
        return ProjectedStatus::degraded("window_stack_evidence_unavailable");
    };
    let stacking_list = capabilities
        .supported
        .contains(&WindowManagerCapability::StackingList);
    match snapshot.window_capabilities.evidence_state {
        WindowCapabilityEvidenceState::Current if capabilities.restack && stacking_list => {
            ProjectedStatus::AVAILABLE
        }
        WindowCapabilityEvidenceState::Current if stacking_list => {
            ProjectedStatus::degraded("raw_window_stack_fallback")
        }
        WindowCapabilityEvidenceState::Current => {
            ProjectedStatus::degraded("window_stack_convergence_unavailable")
        }
        WindowCapabilityEvidenceState::Stale => {
            ProjectedStatus::degraded("stale_window_manager_evidence")
        }
        WindowCapabilityEvidenceState::Pending | WindowCapabilityEvidenceState::Unavailable => {
            ProjectedStatus::degraded("window_stack_evidence_unavailable")
        }
    }
}

fn window_operation_status(
    snapshot: &RuntimeCapabilitySnapshot,
    operation: WindowManagerCapability,
) -> ProjectedStatus {
    if snapshot.window_actor.status != CapabilityStatus::Available {
        return snapshot.window_actor;
    }
    let supported = snapshot
        .window_capabilities
        .capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.supported.contains(&operation));
    match snapshot.window_capabilities.evidence_state {
        WindowCapabilityEvidenceState::Current if supported => ProjectedStatus::AVAILABLE,
        WindowCapabilityEvidenceState::Current => {
            ProjectedStatus::unavailable("unsupported_by_window_manager")
        }
        WindowCapabilityEvidenceState::Stale if supported => {
            ProjectedStatus::degraded("stale_window_manager_evidence")
        }
        WindowCapabilityEvidenceState::Pending
        | WindowCapabilityEvidenceState::Stale
        | WindowCapabilityEvidenceState::Unavailable => {
            ProjectedStatus::unavailable("window_manager_capabilities_unavailable")
        }
    }
}

fn capability(
    id: &'static str,
    projected: ProjectedStatus,
) -> Result<Capability, RuntimeCapabilityError> {
    let capability = Capability::new(CapabilityId::new(id)?, projected.status);
    match projected.reason_code {
        Some(reason) => capability.with_reason_code(reason).map_err(Into::into),
        None => Ok(capability),
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RuntimeCapabilityError {
    #[error(transparent)]
    Identifier(#[from] CapabilityIdError),
    #[error(transparent)]
    Capability(#[from] xenoteer_protocol::CapabilityValidationError),
    #[error(transparent)]
    Report(#[from] CapabilityReportError),
}

#[cfg(test)]
#[path = "runtime_capabilities_tests.rs"]
mod tests;
