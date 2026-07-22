use super::*;

use xenoteer_protocol::{
    Rect, WindowAtomName, WindowGeometry, WindowIdentityHash, WindowMapState, WindowMetadata,
    WindowObservedState, WindowProcessConfidence, WindowProcessCorrelation,
};
use xenoteer_x11::FocusAncestryStatus;
use xenoteer_x11::RawWindowBooleanObservation;

fn snapshot(
    xid: u32,
    workspace: Option<u32>,
) -> Result<WindowSnapshot, Box<dyn std::error::Error>> {
    let window = WindowRef {
        desktop_id: DesktopId::new(),
        desktop_generation: DesktopGeneration::new(),
        xid,
        observed_generation: 1,
        identity_hash: WindowIdentityHash::new("a".repeat(64))?,
    };
    Ok(WindowSnapshot {
        xid_hex: window.xid_hex(),
        window,
        model_revision: xenoteer_protocol::WindowModelRevision::new(3)?,
        metadata: WindowMetadata {
            title: None,
            visible_title: None,
            icon_title: None,
            class: None,
            client_machine: None,
            window_types: Vec::new(),
            states: Vec::new(),
            allowed_actions: Vec::new(),
            protocols: Vec::new(),
        },
        process: WindowProcessCorrelation {
            reported_pid: None,
            managed_process: None,
            confidence: WindowProcessConfidence::None,
            evidence: Vec::new(),
            conflict: false,
        },
        state: WindowObservedState {
            map_state: WindowMapState::Viewable,
            minimized: false,
            hidden: false,
            urgent: false,
            modal: false,
            sticky: false,
            active: false,
            focused: false,
        },
        geometry: Some(WindowGeometry {
            client_rect: WindowRect::new(
                CoordinateSpace::RootPhysical,
                Rect::new(10, 20, 200, 100)?,
            )?,
            frame_rect: None,
            content_rect: WindowRect::new(
                CoordinateSpace::RootPhysical,
                Rect::new(10, 20, 200, 100)?,
            )?,
            frame_extents: None,
        }),
        workspace,
        client_leader: None,
        transient_for: None,
        group_leader: None,
        stacking_index: Some(0),
        has_accessibility_application: false,
        warnings: Vec::new(),
    })
}

fn raw_evidence(
    requested: RawWindowControlRequest,
    outcome: RawWindowControlOutcome,
    observed: RawWindowControlObservation,
) -> RawWindowControlEvidence {
    RawWindowControlEvidence {
        requested,
        outcome,
        observed,
        capabilities: None,
        warnings: Vec::new(),
    }
}

#[test]
fn window_mutations_are_cancellable_completions_with_conservative_effects() {
    let completed = complete_window_mutation(
        RuntimeResult::success(
            CommandOutcome::Acknowledged,
            EffectStage::WindowStateChanged,
        ),
        false,
    );
    assert!(matches!(
        completed,
        ExecutionOutcome::Completed {
            effect: CommandEffect::AfterEffect,
            ..
        }
    ));

    let stopped = complete_window_mutation(
        RuntimeResult::success(
            CommandOutcome::Acknowledged,
            EffectStage::WindowStateChanged,
        ),
        true,
    );
    assert_eq!(
        stopped,
        ExecutionOutcome::Stopped {
            effect: CommandEffect::AfterEffect,
        }
    );

    let unknown = complete_window_mutation(backend_failure(EffectStage::OutcomeUnknown), true);
    assert_eq!(
        unknown,
        ExecutionOutcome::Stopped {
            effect: CommandEffect::AfterEffect,
        }
    );
}

#[test]
fn focus_fallback_and_frame_bounds_policies_are_preserved_for_the_raw_actor()
-> Result<(), Box<dyn std::error::Error>> {
    let observed = snapshot(42, Some(1))?;
    let activation = Command::WindowActivate(xenoteer_protocol::WindowActivateCommand {
        window: observed.window.clone(),
        switch_workspace: false,
        fallback: WindowFocusFallback::AllowSetInputFocus,
    });
    let activation_request = prepare_raw_window_request(&activation, &observed)
        .map_err(|_| std::io::Error::other("focus fallback was rejected"))?;
    let RawWindowControlOperation::Activate {
        allow_set_input_focus,
        ..
    } = activation_request.operation
    else {
        return Err("focus fallback produced the wrong raw operation".into());
    };
    assert!(allow_set_input_focus);

    let geometry = Command::WindowMoveResize(xenoteer_protocol::WindowMoveResizeCommand {
        window: observed.window.clone(),
        relative_to: WindowGeometryTarget::Frame,
        geometry: WindowGeometryRequest {
            x: Some(30),
            y: None,
            width: None,
            height: None,
        },
        bounds_policy: WindowScreenBoundsPolicy::ClampToRoot,
    });
    let geometry_request = prepare_raw_window_request(&geometry, &observed)
        .map_err(|_| std::io::Error::other("frame normalization was rejected"))?;
    let RawWindowControlOperation::MoveResize {
        relative_to,
        geometry: raw_geometry,
        bounds_policy,
    } = geometry_request.operation
    else {
        return Err("frame request produced the wrong raw operation".into());
    };
    assert_eq!(relative_to, WindowGeometryTarget::Frame);
    assert_eq!(
        raw_geometry,
        WindowGeometryRequest {
            x: Some(30),
            y: None,
            width: None,
            height: None,
        }
    );
    assert_eq!(bounds_policy, WindowScreenBoundsPolicy::ClampToRoot);
    Ok(())
}

fn decorated_snapshot(
    xid: u32,
    client_x: i32,
) -> Result<WindowSnapshot, Box<dyn std::error::Error>> {
    let mut value = snapshot(xid, Some(1))?;
    let client = WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(client_x, 20, 200, 100)?,
    )?;
    let frame = WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(client_x - 5, 0, 210, 125)?,
    )?;
    value.geometry = Some(WindowGeometry {
        client_rect: client,
        frame_rect: Some(frame),
        content_rect: client,
        frame_extents: Some(xenoteer_protocol::WindowFrameExtents {
            left: 5,
            right: 5,
            top: 20,
            bottom: 5,
        }),
    });
    Ok(value)
}

#[test]
fn frame_clamp_translation_preserves_desired_effective_and_observed_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    let before = decorated_snapshot(42, 10)?;
    let after = decorated_snapshot(42, 5)?;
    let command = xenoteer_protocol::WindowMoveResizeCommand {
        window: before.window.clone(),
        relative_to: WindowGeometryTarget::Frame,
        geometry: WindowGeometryRequest {
            x: Some(-50),
            y: None,
            width: None,
            height: None,
        },
        bounds_policy: WindowScreenBoundsPolicy::ClampToRoot,
    };
    let request = prepare_raw_window_request(&Command::WindowMoveResize(command.clone()), &before)
        .map_err(|_| std::io::Error::other("frame preparation failed"))?;
    let observed = after.geometry.clone().ok_or("missing test geometry")?;
    let effective = observed.frame_rect.ok_or("missing test frame")?;
    let result = translate_window_evidence(
        Command::WindowMoveResize(command),
        &before,
        Some(after),
        raw_evidence(
            request,
            RawWindowControlOutcome::Converged,
            RawWindowControlObservation::Geometry(xenoteer_x11::RawWindowGeometryObservation {
                observed,
                effective,
                client_request: WindowGeometryRequest {
                    x: Some(5),
                    y: None,
                    width: None,
                    height: None,
                },
                bounds_constrained: true,
                quiet: true,
            }),
        ),
    )
    .map_err(|stage| std::io::Error::other(format!("frame translation: {stage:?}")))?;
    result.validate()?;
    let WindowControlResult::GeometryChanged(result) = result else {
        return Err("wrong result family".into());
    };
    assert_eq!(result.desired.x, Some(-50));
    assert_eq!(result.effective, effective);
    assert_eq!(result.observed.frame_rect, Some(effective));
    assert!(result.constrained);
    assert!(result.converged);
    assert!(
        result
            .warnings
            .contains(&WindowControlWarning::GeometryConstrained)
    );
    Ok(())
}

#[test]
fn matching_geometry_without_a_quiet_window_is_not_reported_converged()
-> Result<(), Box<dyn std::error::Error>> {
    let before = snapshot(42, Some(1))?;
    let command = xenoteer_protocol::WindowMoveResizeCommand {
        window: before.window.clone(),
        relative_to: WindowGeometryTarget::Client,
        geometry: WindowGeometryRequest {
            x: Some(10),
            y: None,
            width: None,
            height: None,
        },
        bounds_policy: WindowScreenBoundsPolicy::AllowOffscreen,
    };
    let request = prepare_raw_window_request(&Command::WindowMoveResize(command.clone()), &before)
        .map_err(|_| std::io::Error::other("client preparation failed"))?;
    let observed = before.geometry.clone().ok_or("missing test geometry")?;
    let result = translate_window_evidence(
        Command::WindowMoveResize(command),
        &before,
        Some(before.clone()),
        raw_evidence(
            request,
            RawWindowControlOutcome::TimedOut,
            RawWindowControlObservation::Geometry(xenoteer_x11::RawWindowGeometryObservation {
                effective: observed.client_rect,
                observed,
                client_request: WindowGeometryRequest {
                    x: Some(10),
                    y: None,
                    width: None,
                    height: None,
                },
                bounds_constrained: false,
                quiet: false,
            }),
        ),
    )
    .map_err(|stage| std::io::Error::other(format!("client translation: {stage:?}")))?;
    let WindowControlResult::GeometryChanged(result) = result else {
        return Err("wrong result family".into());
    };
    assert!(!result.converged);
    assert!(!result.constrained);
    result.validate()?;
    Ok(())
}

#[test]
fn activation_normalizes_proven_descendant_focus_to_the_exact_top_level_birth()
-> Result<(), Box<dyn std::error::Error>> {
    let observed = snapshot(42, Some(1))?;
    let command = Command::WindowActivate(xenoteer_protocol::WindowActivateCommand {
        window: observed.window.clone(),
        switch_workspace: false,
        fallback: WindowFocusFallback::EwmhOnly,
    });
    let request = prepare_raw_window_request(&command, &observed)
        .map_err(|_| std::io::Error::other("activation preparation failed"))?;
    let result = translate_window_evidence(
        command,
        &observed,
        Some(observed.clone()),
        raw_evidence(
            request,
            RawWindowControlOutcome::Converged,
            RawWindowControlObservation::Activation {
                current_active_sent: None,
                timestamp_sent: 0,
                active: Some(42),
                focused: Some(77),
                focus_within_target: true,
                focus_ancestry_status: FocusAncestryStatus::Resolved,
                current_workspace: Some(1),
            },
        ),
    )
    .map_err(|stage| std::io::Error::other(format!("activation translation: {stage:?}")))?;
    let WindowControlResult::Activated(result) = result else {
        return Err("wrong result family".into());
    };
    assert!(result.converged);
    assert_eq!(result.observed_focused, Some(observed.window));
    assert!(
        !result
            .warnings
            .contains(&WindowControlWarning::FocusNotAcquired)
    );
    Ok(())
}

#[test]
fn workspace_nonconvergence_is_model_backed_and_explicitly_warned()
-> Result<(), Box<dyn std::error::Error>> {
    let before = snapshot(42, Some(1))?;
    let after = before.clone();
    let command = Command::WindowMoveToWorkspace(xenoteer_protocol::WindowMoveToWorkspaceCommand {
        window: before.window.clone(),
        workspace: 2,
    });
    let request = prepare_raw_window_request(&command, &before)
        .map_err(|_| std::io::Error::other("workspace preparation failed"))?;
    let result = translate_window_evidence(
        command,
        &before,
        Some(after),
        raw_evidence(
            request,
            RawWindowControlOutcome::TimedOut,
            RawWindowControlObservation::Workspace(Some(1)),
        ),
    )
    .map_err(|stage| std::io::Error::other(format!("workspace translation: {stage:?}")))?;
    result.validate()?;
    let WindowControlResult::WorkspaceChanged(result) = result else {
        return Err("wrong result family".into());
    };
    assert!(!result.converged);
    assert_eq!(result.observed_workspace, Some(1));
    assert!(
        result
            .warnings
            .contains(&WindowControlWarning::WorkspaceNotConfirmed)
    );
    Ok(())
}

#[test]
fn state_result_uses_the_normalized_post_effect_snapshot() -> Result<(), Box<dyn std::error::Error>>
{
    let before = snapshot(42, Some(1))?;
    let mut after = before.clone();
    after.metadata.states = vec![WindowAtomName::new("_NET_WM_STATE_FULLSCREEN")?];
    let command = Command::WindowSetState(xenoteer_protocol::WindowSetStateCommand {
        window: before.window.clone(),
        state: WindowManagerState::Fullscreen,
        desired: true,
    });
    let request = prepare_raw_window_request(&command, &before)
        .map_err(|_| std::io::Error::other("state preparation failed"))?;
    let result = translate_window_evidence(
        command,
        &before,
        Some(after),
        raw_evidence(
            request,
            RawWindowControlOutcome::Converged,
            RawWindowControlObservation::State(RawWindowBooleanObservation::Enabled),
        ),
    )
    .map_err(|stage| std::io::Error::other(format!("state translation: {stage:?}")))?;
    result.validate()?;
    let WindowControlResult::StateChanged(result) = result else {
        return Err("wrong result family".into());
    };
    assert!(result.converged);
    assert_eq!(result.observed, WindowStateObservation::Enabled);
    Ok(())
}

#[test]
fn effect_stage_never_claims_postcondition_for_nonconverged_request()
-> Result<(), Box<dyn std::error::Error>> {
    let result = WindowControlResult::WorkspaceChanged(Box::new(WindowMoveToWorkspaceResult {
        requested: snapshot(42, Some(1))?.window,
        desired_workspace: 2,
        observed_workspace: Some(1),
        observed_revision: xenoteer_protocol::WindowModelRevision::new(3)?,
        converged: false,
        warnings: vec![WindowControlWarning::WorkspaceNotConfirmed],
    }));
    assert_eq!(
        window_result_stage(&result, RawWindowControlOutcome::TimedOut),
        EffectStage::WindowRequestSent
    );
    Ok(())
}

#[test]
fn set_input_focus_warning_is_accepted_only_for_the_opted_in_command()
-> Result<(), Box<dyn std::error::Error>> {
    let observed = snapshot(42, Some(1))?;
    for (fallback, accepted) in [
        (WindowFocusFallback::AllowSetInputFocus, true),
        (WindowFocusFallback::EwmhOnly, false),
    ] {
        let command = Command::WindowActivate(xenoteer_protocol::WindowActivateCommand {
            window: observed.window.clone(),
            switch_workspace: false,
            fallback,
        });
        let request = prepare_raw_window_request(&command, &observed)
            .map_err(|_| std::io::Error::other("activation preparation failed"))?;
        let mut evidence = raw_evidence(
            request,
            RawWindowControlOutcome::Converged,
            RawWindowControlObservation::Activation {
                current_active_sent: None,
                timestamp_sent: 0,
                active: Some(42),
                focused: Some(42),
                focus_within_target: true,
                focus_ancestry_status: FocusAncestryStatus::Resolved,
                current_workspace: Some(1),
            },
        );
        evidence
            .warnings
            .push(WindowControlWarning::UsedSetInputFocusFallback);
        let translated =
            translate_window_evidence(command, &observed, Some(observed.clone()), evidence);
        if accepted {
            let WindowControlResult::Activated(result) = translated
                .map_err(|stage| std::io::Error::other(format!("activation: {stage:?}")))?
            else {
                return Err("wrong result family".into());
            };
            assert!(result.converged);
            assert!(
                result
                    .warnings
                    .contains(&WindowControlWarning::UsedSetInputFocusFallback)
            );
        } else {
            assert!(matches!(translated, Err(EffectStage::OutcomeUnknown)));
        }
    }
    Ok(())
}

#[test]
fn raw_and_model_geometry_disagreement_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
    let before = decorated_snapshot(42, 10)?;
    let raw_after = decorated_snapshot(42, 5)?;
    let command = xenoteer_protocol::WindowMoveResizeCommand {
        window: before.window.clone(),
        relative_to: WindowGeometryTarget::Frame,
        geometry: WindowGeometryRequest {
            x: Some(0),
            y: None,
            width: None,
            height: None,
        },
        bounds_policy: WindowScreenBoundsPolicy::ClampToRoot,
    };
    let request = prepare_raw_window_request(&Command::WindowMoveResize(command.clone()), &before)
        .map_err(|_| std::io::Error::other("frame preparation failed"))?;
    let raw_geometry = raw_after.geometry.ok_or("missing raw geometry")?;
    let raw_effective = raw_geometry.frame_rect.ok_or("missing raw frame")?;
    let translated = translate_window_evidence(
        Command::WindowMoveResize(command),
        &before,
        Some(before.clone()),
        raw_evidence(
            request,
            RawWindowControlOutcome::Converged,
            RawWindowControlObservation::Geometry(xenoteer_x11::RawWindowGeometryObservation {
                observed: raw_geometry,
                effective: raw_effective,
                client_request: WindowGeometryRequest {
                    x: Some(5),
                    y: None,
                    width: None,
                    height: None,
                },
                bounds_constrained: false,
                quiet: true,
            }),
        ),
    );
    assert!(matches!(translated, Err(EffectStage::OutcomeUnknown)));
    Ok(())
}
