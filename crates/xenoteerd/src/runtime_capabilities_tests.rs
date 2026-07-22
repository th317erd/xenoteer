use super::*;
use xenoteer_x11::RawWindowManagerCapabilities;

fn baseline() -> RuntimeCapabilitySnapshot {
    RuntimeCapabilitySnapshot {
        desktop: ProjectedStatus::AVAILABLE,
        artifact: ProjectedStatus::AVAILABLE,
        process: ProjectedStatus::AVAILABLE,
        observation: ProjectedStatus::AVAILABLE,
        input: ProjectedStatus::AVAILABLE,
        input_reset: ProjectedStatus::AVAILABLE,
        physical_text: ProjectedStatus::AVAILABLE,
        capture: ProjectedStatus::AVAILABLE,
        clipboard: ProjectedStatus::AVAILABLE,
        viewer: ProjectedStatus::AVAILABLE,
        window_actor: ProjectedStatus::AVAILABLE,
        window_capabilities: WindowCapabilitySnapshot {
            evidence_state: WindowCapabilityEvidenceState::Current,
            capabilities: Some(RawWindowManagerCapabilities {
                supported: vec![
                    WindowManagerCapability::Activate,
                    WindowManagerCapability::Close,
                    WindowManagerCapability::StateMaximized,
                    WindowManagerCapability::StateFullscreen,
                    WindowManagerCapability::StateAbove,
                    WindowManagerCapability::StateSticky,
                    WindowManagerCapability::MoveResize,
                    WindowManagerCapability::MoveToWorkspace,
                    WindowManagerCapability::StackingList,
                ],
                restack: true,
            }),
        },
    }
}

fn status(report: &CapabilityReport, id: &str) -> Option<CapabilityStatus> {
    report
        .capabilities()
        .iter()
        .find(|capability| capability.id().as_str() == id)
        .map(Capability::status)
}

fn reason<'a>(report: &'a CapabilityReport, id: &str) -> Option<Option<&'a str>> {
    report
        .capabilities()
        .iter()
        .find(|capability| capability.id().as_str() == id)
        .map(Capability::reason_code)
}

#[test]
fn window_support_is_projected_per_operation_instead_of_broadly()
-> Result<(), RuntimeCapabilityError> {
    let mut snapshot = baseline();
    snapshot.window_capabilities.capabilities = Some(RawWindowManagerCapabilities {
        supported: vec![WindowManagerCapability::Activate],
        restack: false,
    });
    let report = build_report(&snapshot)?;

    assert_eq!(
        status(&report, "window.ewmh.activate"),
        Some(CapabilityStatus::Available)
    );
    assert_eq!(
        status(&report, "window.ewmh.close"),
        Some(CapabilityStatus::Unavailable)
    );
    assert_eq!(
        reason(&report, "window.ewmh.close"),
        Some(Some("unsupported_by_window_manager"))
    );
    assert_eq!(
        status(&report, "window.control.ewmh"),
        Some(CapabilityStatus::Degraded)
    );
    Ok(())
}

#[test]
fn stale_window_evidence_degrades_only_previously_supported_operations()
-> Result<(), RuntimeCapabilityError> {
    let mut snapshot = baseline();
    snapshot.window_capabilities.evidence_state = WindowCapabilityEvidenceState::Stale;
    snapshot.window_capabilities.capabilities = Some(RawWindowManagerCapabilities {
        supported: vec![WindowManagerCapability::Activate],
        restack: false,
    });
    let report = build_report(&snapshot)?;

    assert_eq!(
        status(&report, "window.ewmh.activate"),
        Some(CapabilityStatus::Degraded)
    );
    assert_eq!(
        reason(&report, "window.ewmh.activate"),
        Some(Some("stale_window_manager_evidence"))
    );
    assert_eq!(
        status(&report, "window.ewmh.close"),
        Some(CapabilityStatus::Unavailable)
    );
    Ok(())
}

#[test]
fn target_dependent_window_fallbacks_are_not_overstated() -> Result<(), RuntimeCapabilityError> {
    let mut snapshot = baseline();
    snapshot.window_capabilities.capabilities = Some(RawWindowManagerCapabilities {
        supported: Vec::new(),
        restack: false,
    });
    let report = build_report(&snapshot)?;

    assert_eq!(
        status(&report, "window.control.close"),
        Some(CapabilityStatus::Degraded)
    );
    assert_eq!(
        reason(&report, "window.control.close"),
        Some(Some("target_dependent_wm_delete"))
    );
    assert_eq!(
        status(&report, "window.icccm.minimize"),
        Some(CapabilityStatus::Degraded)
    );
    assert_eq!(
        status(&report, "window.control.stack"),
        Some(CapabilityStatus::Degraded)
    );
    Ok(())
}

#[test]
fn physical_text_remains_available_when_clipboard_is_lost() -> Result<(), RuntimeCapabilityError> {
    let mut snapshot = baseline();
    snapshot.clipboard = ProjectedStatus::unavailable("clipboard_actor_unavailable");
    let report = build_report(&snapshot)?;

    assert_eq!(
        status(&report, "input.text.physical"),
        Some(CapabilityStatus::Available)
    );
    assert_eq!(
        status(&report, "input.text.clipboard"),
        Some(CapabilityStatus::Unavailable)
    );
    Ok(())
}

#[test]
fn viewer_distinguishes_configuration_policy_from_backend_loss()
-> Result<(), RuntimeCapabilityError> {
    let mut snapshot = baseline();
    snapshot.viewer = ProjectedStatus::new(CapabilityStatus::Disabled, Some("viewer_disabled"));
    let disabled = build_report(&snapshot)?;
    assert_eq!(
        status(&disabled, "viewer.novnc.view_only"),
        Some(CapabilityStatus::Disabled)
    );

    snapshot.viewer = ProjectedStatus::unavailable("viewer_backend_unavailable");
    let unavailable = build_report(&snapshot)?;
    assert_eq!(
        status(&unavailable, "viewer.novnc.view_only"),
        Some(CapabilityStatus::Unavailable)
    );
    Ok(())
}

#[test]
fn desktop_loss_does_not_hide_the_private_artifact_store() -> Result<(), RuntimeCapabilityError> {
    let mut snapshot = baseline();
    snapshot.desktop = ProjectedStatus::unavailable("desktop_not_ready");
    snapshot.input = snapshot.desktop;
    snapshot.input_reset = snapshot.desktop;
    snapshot.physical_text = snapshot.desktop;
    snapshot.capture = snapshot.desktop;
    snapshot.clipboard = snapshot.desktop;
    snapshot.observation = snapshot.desktop;
    snapshot.window_actor = snapshot.desktop;
    let report = build_report(&snapshot)?;

    assert_eq!(
        status(&report, "window.observe.inventory"),
        Some(CapabilityStatus::Unavailable)
    );
    assert_eq!(
        status(&report, "artifact.private.store"),
        Some(CapabilityStatus::Available)
    );
    Ok(())
}

#[test]
fn artifact_loss_only_hides_operations_that_exchange_artifacts()
-> Result<(), RuntimeCapabilityError> {
    let mut snapshot = baseline();
    snapshot.artifact = ProjectedStatus::unavailable("artifact_backend_unavailable");
    let report = build_report(&snapshot)?;

    for id in [
        "artifact.private.store",
        "capture.screenshot",
        "capture.root.raw",
        "capture.window.visible",
    ] {
        assert_eq!(status(&report, id), Some(CapabilityStatus::Unavailable));
    }
    for id in [
        "clipboard.selection.read",
        "clipboard.selection.write",
        "input.text.clipboard",
    ] {
        assert_eq!(status(&report, id), Some(CapabilityStatus::Degraded));
        assert_eq!(
            reason(&report, id),
            Some(Some("artifact_payload_variant_unavailable"))
        );
    }
    assert_eq!(
        status(&report, "input.text.physical"),
        Some(CapabilityStatus::Available)
    );
    assert_eq!(
        status(&report, "process.managed.status"),
        Some(CapabilityStatus::Available)
    );
    assert_eq!(
        status(&report, "window.observe.inventory"),
        Some(CapabilityStatus::Available)
    );
    Ok(())
}

#[test]
fn observation_loss_preserves_root_capture_but_hides_target_dependent_operations()
-> Result<(), RuntimeCapabilityError> {
    let mut snapshot = baseline();
    snapshot.observation = ProjectedStatus::unavailable("observation_backend_unavailable");
    snapshot.window_actor = snapshot.observation;
    let report = build_report(&snapshot)?;

    assert_eq!(
        status(&report, "window.observe.query"),
        Some(CapabilityStatus::Unavailable)
    );
    assert_eq!(
        status(&report, "capture.window.drawable"),
        Some(CapabilityStatus::Unavailable)
    );
    assert_eq!(
        status(&report, "window.ewmh.activate"),
        Some(CapabilityStatus::Unavailable)
    );
    assert_eq!(
        status(&report, "capture.root.png"),
        Some(CapabilityStatus::Available)
    );
    assert_eq!(
        status(&report, "clipboard.selection.read"),
        Some(CapabilityStatus::Available)
    );
    Ok(())
}

#[test]
fn stale_process_evidence_degrades_process_operations_without_flattening_desktop()
-> Result<(), RuntimeCapabilityError> {
    let mut snapshot = baseline();
    snapshot.process = ProjectedStatus::degraded("stale_process_backend_evidence");
    let report = build_report(&snapshot)?;

    assert_eq!(
        status(&report, "application.registered.launch"),
        Some(CapabilityStatus::Degraded)
    );
    assert_eq!(
        status(&report, "process.managed.terminate"),
        Some(CapabilityStatus::Degraded)
    );
    assert_eq!(
        status(&report, "window.observe.wait"),
        Some(CapabilityStatus::Available)
    );
    assert_eq!(
        status(&report, "artifact.private.store"),
        Some(CapabilityStatus::Available)
    );
    Ok(())
}
