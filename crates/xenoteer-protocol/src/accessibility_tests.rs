use serde_json::json;

use crate::*;

fn application(
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    atspi_generation: AtspiGeneration,
) -> Result<ApplicationRef, AccessibilityValidationError> {
    Ok(ApplicationRef {
        desktop_id,
        desktop_generation,
        atspi_generation,
        unique_bus_name: AtspiBusName::new(":1.42")?,
        root_object_path: AtspiObjectPath::new("/org/a11y/atspi/accessible/root")?,
        app_instance_generation: 1,
        identity_hash: AccessibilityIdentityHash::new("a".repeat(64))?,
    })
}

fn element() -> Result<ElementRef, AccessibilityValidationError> {
    let desktop_id = DesktopId::new();
    let desktop_generation = DesktopGeneration::new();
    let atspi_generation = AtspiGeneration::new(1)?;
    Ok(ElementRef {
        desktop_id,
        desktop_generation,
        atspi_generation,
        application: application(desktop_id, desktop_generation, atspi_generation)?,
        object_path: AtspiObjectPath::new("/org/a11y/atspi/accessible/42")?,
        object_identity_hash: AccessibilityIdentityHash::new("b".repeat(64))?,
        cache_sequence: 7,
    })
}

fn snapshot() -> Result<ElementSnapshot, Box<dyn std::error::Error>> {
    Ok(ElementSnapshot {
        element: element()?,
        parent: None,
        index_in_parent: Some(0),
        child_count: Some(0),
        role: ElementRoleSnapshot {
            role: ElementRole::Entry,
            raw_name: Some("entry".to_owned()),
            raw_numeric: Some(42),
        },
        name: Some("Account".to_owned()),
        description: None,
        accessible_id: Some("account-entry".to_owned()),
        locale: Some("en-US".to_owned()),
        states: vec![ElementState::Editable, ElementState::Enabled],
        interfaces: vec![ElementInterface::Accessible, ElementInterface::EditableText],
        actions: Vec::new(),
        value: None,
        text: Some(ElementTextSnapshot {
            character_count: 6,
            caret_offset: 6,
            selections: Vec::new(),
            content: None,
            content_truncated: false,
            protected: true,
        }),
        component: Some(ElementComponentSnapshot {
            coordinate_space: CoordinateSpace::AtspiScreen,
            extents: Some(Rect::new(1, 2, 100, 20)?),
            layer: Some("widget".to_owned()),
            z_order: Some(0),
            alpha: Some(255),
        }),
        attributes: Vec::new(),
        relations: Vec::new(),
        window_correlation: ElementWindowCorrelation {
            window: None,
            confidence: WindowCorrelationConfidence::None,
            evidence: Vec::new(),
            conflicting_evidence: false,
        },
        revision: AccessibilityRevision::new(3)?,
        completeness: ElementCompleteness::Complete,
        truncated: false,
        warnings: Vec::new(),
    })
}

#[test]
fn nested_application_and_element_generations_must_agree() -> Result<(), Box<dyn std::error::Error>>
{
    let mut element = element()?;
    element.application.atspi_generation = AtspiGeneration::new(2)?;
    assert_eq!(
        element.validate(),
        Err(AccessibilityValidationError::ReferenceScope)
    );
    Ok(())
}

#[test]
fn protected_snapshot_and_event_content_cannot_be_exposed() -> Result<(), Box<dyn std::error::Error>>
{
    let secret = "canary-secret";
    let content = AccessibleTextContent::new(secret)?;
    assert!(!format!("{content:?}").contains(secret));

    let mut snapshot = snapshot()?;
    let text = snapshot
        .text
        .as_mut()
        .ok_or("snapshot fixture must contain text")?;
    text.content = Some(content.clone());
    assert_eq!(
        snapshot.validate(),
        Err(AccessibilityValidationError::ProtectedTextExposed)
    );

    let detail = AccessibilityTextEventDetail {
        start: 0,
        length: 1,
        content: Some(content),
        redacted: true,
    };
    assert_eq!(
        detail.validate(),
        Err(AccessibilityValidationError::ProtectedTextExposed)
    );
    Ok(())
}

#[test]
fn text_offsets_must_fit_the_reported_character_count() {
    let valid_no_caret = ElementTextSnapshot {
        character_count: 3,
        caret_offset: -1,
        selections: vec![ElementTextRange { start: 0, end: 3 }],
        content: None,
        content_truncated: false,
        protected: false,
    };
    assert!(valid_no_caret.validate().is_ok());

    let mut invalid_caret = valid_no_caret.clone();
    invalid_caret.caret_offset = 4;
    assert_eq!(
        invalid_caret.validate(),
        Err(AccessibilityValidationError::Text)
    );

    let mut invalid_selection = valid_no_caret;
    invalid_selection.selections = vec![ElementTextRange { start: 1, end: 4 }];
    assert_eq!(
        invalid_selection.validate(),
        Err(AccessibilityValidationError::Text)
    );
}

#[test]
fn password_role_forces_content_value_and_attribute_redaction()
-> Result<(), Box<dyn std::error::Error>> {
    let mut protected = snapshot()?;
    protected.role.role = ElementRole::PasswordText;
    let text = protected.text.as_mut().ok_or("text fixture")?;
    text.protected = false;
    text.content = Some(AccessibleTextContent::new("canary")?);
    assert!(protected.is_protected());
    assert_eq!(
        protected.validate(),
        Err(AccessibilityValidationError::ProtectedTextExposed)
    );

    protected.text.as_mut().ok_or("text fixture")?.content = None;
    protected.value = Some(ElementValueSnapshot {
        current: 1.0,
        minimum: None,
        maximum: None,
        increment: None,
        text: None,
    });
    assert_eq!(
        protected.validate(),
        Err(AccessibilityValidationError::ProtectedTextExposed)
    );
    protected.value = None;
    protected.attributes.push(ElementAttribute {
        name: "secret".to_owned(),
        value: "canary".to_owned(),
    });
    assert_eq!(
        protected.validate(),
        Err(AccessibilityValidationError::ProtectedTextExposed)
    );
    Ok(())
}

#[test]
fn correlation_confidence_and_window_presence_are_bidirectional()
-> Result<(), Box<dyn std::error::Error>> {
    let mut snapshot = snapshot()?;
    snapshot.window_correlation.confidence = WindowCorrelationConfidence::Strong;
    assert_eq!(
        snapshot.validate(),
        Err(AccessibilityValidationError::Correlation)
    );
    snapshot.window_correlation.window = Some(WindowRef {
        desktop_id: snapshot.element.desktop_id,
        desktop_generation: snapshot.element.desktop_generation,
        xid: 42,
        observed_generation: 1,
        identity_hash: WindowIdentityHash::new("d".repeat(64))?,
    });
    assert!(snapshot.validate().is_ok());
    snapshot.window_correlation.confidence = WindowCorrelationConfidence::None;
    assert_eq!(
        snapshot.validate(),
        Err(AccessibilityValidationError::Correlation)
    );
    Ok(())
}

#[test]
fn geometry_predicates_require_explicit_atspi_screen_coordinates()
-> Result<(), Box<dyn std::error::Error>> {
    let rect = Rect::new(0, 0, 10, 10)?;
    let selector = ElementPredicate::ComponentIntersects {
        coordinate_space: CoordinateSpace::AtspiScreen,
        rect,
    };
    assert!(selector.validate().is_ok());
    let wait = ElementWaitPredicate::Geometry {
        coordinate_space: CoordinateSpace::RootPhysical,
        intersects: rect,
    };
    assert_eq!(wait.validate(), Err(AccessibilityValidationError::Geometry));
    let wait = ElementWaitPredicate::Geometry {
        coordinate_space: CoordinateSpace::AtspiScreen,
        intersects: rect,
    };
    assert!(wait.validate().is_ok());
    Ok(())
}

#[test]
fn snapshot_validation_is_recursive_and_aggregate_bounded() -> Result<(), Box<dyn std::error::Error>>
{
    let mut mislabeled_component = snapshot()?;
    mislabeled_component
        .component
        .as_mut()
        .ok_or("fixture component missing")?
        .coordinate_space = CoordinateSpace::RootPhysical;
    assert_eq!(
        mislabeled_component.validate(),
        Err(AccessibilityValidationError::Geometry)
    );

    let mut invalid_nested = snapshot()?;
    invalid_nested.actions.push(ElementActionSnapshot {
        name: "x".repeat(MAX_ACCESSIBILITY_SHORT_TEXT_BYTES + 1),
        description: None,
        key_binding: None,
    });
    assert_eq!(
        invalid_nested.validate(),
        Err(AccessibilityValidationError::Text)
    );

    let mut aggregate = snapshot()?;
    aggregate.text.as_mut().ok_or("text fixture")?.protected = false;
    aggregate.attributes = (0..MAX_ACCESSIBILITY_ATTRIBUTES)
        .map(|index| ElementAttribute {
            name: format!("key-{index}"),
            value: "v".repeat(MAX_ACCESSIBILITY_SHORT_TEXT_BYTES),
        })
        .collect();
    assert_eq!(
        aggregate.validate(),
        Err(AccessibilityValidationError::ElementEncoding)
    );
    Ok(())
}

#[test]
fn strict_requests_reject_nested_reference_and_policy_additions()
-> Result<(), Box<dyn std::error::Error>> {
    let element_value = serde_json::to_value(element()?)?;
    let mut request = json!({
        "desktop_id": element_value["desktop_id"],
        "desktop_generation": element_value["desktop_generation"],
        "element": element_value,
        "expansion": {
            "actions": false,
            "value": false,
            "text_metadata": false,
            "text_content": false,
            "attributes": false,
            "relations": false,
            "component": true
        }
    });
    request["element"]["application"]["future_identity"] = json!(true);
    assert!(serde_json::from_value::<ElementSnapshotRequest>(request).is_err());

    let mut write = json!({
        "element": serde_json::to_value(element()?)?,
        "text": "secret",
        "selection": "collapse_after",
        "verify_length_only": true,
        "postcondition": null,
        "future_policy_bypass": true
    });
    assert!(serde_json::from_value::<ElementSetTextCommand>(write.take()).is_err());
    Ok(())
}

#[test]
fn semantic_text_rejects_nul_without_weakening_debug_redaction()
-> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        SemanticTextInput::new("before\0after"),
        Err(AccessibilityActionValidationError::Text)
    );
    assert!(serde_json::from_str::<SemanticTextInput>(r#""before\u0000after""#).is_err());

    let input = SemanticTextInput::new("semantic-text-canary")?;
    let debug = format!("{input:?}");
    assert_eq!(debug, "SemanticTextInput(<redacted>)");
    assert!(!debug.contains("semantic-text-canary"));
    Ok(())
}

#[test]
fn physical_element_click_remains_a_lease_gated_command() -> Result<(), Box<dyn std::error::Error>>
{
    let element = element()?;
    let command = Command::ElementPhysicalClick(ElementPhysicalClickCommand {
        element: element.clone(),
        window: None,
        minimum_correlation: WindowCorrelationConfidence::Strong,
        point_policy: ElementClickPointPolicy::Center,
        scroll_policy: ElementClickScrollPolicy::IfNeeded,
        activation_policy: ElementWindowActivationPolicy::IfNeeded,
        occlusion_policy: ElementOcclusionPolicy::BestEffortReject,
        button: PointerLogicalButton::Left,
        count: 1,
        interval_ms: 0,
        move_duration_ms: Some(100),
        curve: PointerCurve::Smooth,
        settle_timeout_ms: 1_000,
        postcondition: None,
    });
    assert!(command.requires_control_lease());
    let mut wire = serde_json::to_value(&command)?;
    assert_eq!(wire["button"], json!("left"));
    wire["button"] = json!(1);
    assert!(serde_json::from_value::<Command>(wire).is_err());
    assert_eq!(
        CommandEnvelope::new(
            ProtocolVersion::V1_0,
            RequestId::new(),
            CommandId::new(),
            element.desktop_id,
            element.desktop_generation,
            command,
        ),
        Err(EnvelopeValidationError::LeaseRequired)
    );
    Ok(())
}

#[test]
fn physical_element_click_validation_enforces_target_and_interpolation_policy()
-> Result<(), Box<dyn std::error::Error>> {
    let element = element()?;
    let window = WindowRef {
        desktop_id: element.desktop_id,
        desktop_generation: element.desktop_generation,
        xid: 42,
        observed_generation: 3,
        identity_hash: WindowIdentityHash::new("c".repeat(64))?,
    };
    let base = ElementPhysicalClickCommand {
        element,
        window: Some(window),
        minimum_correlation: WindowCorrelationConfidence::Strong,
        point_policy: ElementClickPointPolicy::Center,
        scroll_policy: ElementClickScrollPolicy::IfNeeded,
        activation_policy: ElementWindowActivationPolicy::IfNeeded,
        occlusion_policy: ElementOcclusionPolicy::BestEffortReject,
        button: PointerLogicalButton::Left,
        count: 1,
        interval_ms: 0,
        move_duration_ms: Some(100),
        curve: PointerCurve::Smooth,
        settle_timeout_ms: 1_000,
        postcondition: None,
    };
    assert_eq!(base.validate(), Ok(()));

    let mut invalid = base.clone();
    invalid.minimum_correlation = WindowCorrelationConfidence::None;
    assert_eq!(
        invalid.validate(),
        Err(AccessibilityActionValidationError::PhysicalClick)
    );

    invalid = base.clone();
    invalid.minimum_correlation = WindowCorrelationConfidence::Weak;
    invalid.window = None;
    assert_eq!(
        invalid.validate(),
        Err(AccessibilityActionValidationError::PhysicalClick)
    );

    let mut weak_with_exact_target = base.clone();
    weak_with_exact_target.minimum_correlation = WindowCorrelationConfidence::Weak;
    assert_eq!(weak_with_exact_target.validate(), Ok(()));

    let mut maximum_compound = base.clone();
    maximum_compound.count = MAX_POINTER_CLICK_COUNT;
    maximum_compound.interval_ms = MAX_PHYSICAL_ELEMENT_CLICK_INTERVAL_MS;
    assert_eq!(maximum_compound.validate(), Ok(()));

    invalid = base.clone();
    invalid.count = MAX_POINTER_CLICK_COUNT + 1;
    assert_eq!(
        invalid.validate(),
        Err(AccessibilityActionValidationError::PhysicalClick)
    );

    invalid = base.clone();
    invalid.interval_ms = u32::from(DEFAULT_DOUBLE_CLICK_THRESHOLD_MS);
    assert_eq!(
        invalid.validate(),
        Err(AccessibilityActionValidationError::PhysicalClick)
    );

    invalid = base.clone();
    invalid.curve = PointerCurve::Instant;
    assert_eq!(
        invalid.validate(),
        Err(AccessibilityActionValidationError::PhysicalClick)
    );

    invalid = base.clone();
    invalid.move_duration_ms = Some(0);
    assert_eq!(
        invalid.validate(),
        Err(AccessibilityActionValidationError::PhysicalClick)
    );

    invalid = base;
    invalid.move_duration_ms = Some(MAX_POINTER_MOVE_DURATION_MS + 1);
    assert_eq!(
        invalid.validate(),
        Err(AccessibilityActionValidationError::PhysicalClick)
    );
    Ok(())
}

#[test]
fn atspi_geometry_postconditions_are_valid_while_text_content_waits_are_reserved()
-> Result<(), Box<dyn std::error::Error>> {
    let element = element()?;
    let reserved = ElementScrollCommand {
        element: element.clone(),
        target: ElementScrollTarget::Alignment {
            alignment: ElementScrollAlignment::Anywhere,
        },
        postcondition: Some(ElementPostcondition {
            predicate: ElementWaitPredicate::Text {
                matcher: ElementStringMatch::Exact {
                    value: "protocol-shape".to_owned(),
                    case_sensitive: true,
                },
            },
            timeout_ms: 1_000,
            allow_poll_fallback: false,
        }),
    };
    assert_eq!(
        reserved.validate(),
        Err(AccessibilityActionValidationError::Accessibility(
            AccessibilityValidationError::UnsupportedFeature
        ))
    );

    let predicates = [ElementWaitPredicate::Geometry {
        coordinate_space: CoordinateSpace::AtspiScreen,
        intersects: Rect::new(0, 0, 100, 100)?,
    }];
    for predicate in predicates {
        let postcondition = ElementPostcondition {
            predicate,
            timeout_ms: 1_000,
            allow_poll_fallback: false,
        };
        let semantic = ElementScrollCommand {
            element: element.clone(),
            target: ElementScrollTarget::Alignment {
                alignment: ElementScrollAlignment::Anywhere,
            },
            postcondition: Some(postcondition.clone()),
        };
        assert_eq!(semantic.validate(), Ok(()));

        let physical = ElementPhysicalClickCommand {
            element: element.clone(),
            window: None,
            minimum_correlation: WindowCorrelationConfidence::Strong,
            point_policy: ElementClickPointPolicy::Center,
            scroll_policy: ElementClickScrollPolicy::Always,
            activation_policy: ElementWindowActivationPolicy::Require,
            occlusion_policy: ElementOcclusionPolicy::BestEffortReject,
            button: PointerLogicalButton::Left,
            count: 1,
            interval_ms: 0,
            move_duration_ms: Some(100),
            curve: PointerCurve::Smooth,
            settle_timeout_ms: 1_000,
            postcondition: Some(postcondition),
        };
        assert_eq!(physical.validate(), Ok(()));
    }
    Ok(())
}

#[test]
fn physical_click_result_requires_effect_authorizing_fresh_correlation()
-> Result<(), Box<dyn std::error::Error>> {
    let element = element()?;
    let window = WindowRef {
        desktop_id: element.desktop_id,
        desktop_generation: element.desktop_generation,
        xid: 42,
        observed_generation: 3,
        identity_hash: WindowIdentityHash::new("c".repeat(64))?,
    };
    let mut result = ElementPhysicalClickResult {
        element,
        window,
        correlation: WindowCorrelationConfidence::Strong,
        revision_before_queue: AccessibilityRevision::new(7)?,
        revision_after_queue: AccessibilityRevision::new(8)?,
        extents_before_queue: Rect::new(10, 20, 40, 30)?,
        extents_after_queue: Rect::new(10, 20, 40, 30)?,
        click_point: Point::new(30, 35),
        occlusion_check: OcclusionCheckResult::Clear,
        scrolled: false,
        window_activated: false,
        pointer_interpolated: true,
        button: PointerLogicalButton::Left,
        count: 1,
        postcondition_satisfied: None,
        final_snapshot: None,
    };
    assert_eq!(result.validate(), Ok(()));
    result.correlation = WindowCorrelationConfidence::Weak;
    assert_eq!(
        result.validate(),
        Err(AccessibilityActionValidationError::ResultEvidence)
    );
    Ok(())
}

#[test]
fn cursor_contract_is_explicitly_short_lived_and_bounded() {
    const {
        assert!(ACCESSIBILITY_CURSOR_TTL_MS <= MAX_ACCESSIBILITY_WAIT_TIMEOUT_MS);
        assert!(MAX_ACCESSIBILITY_CURSORS_PER_PRINCIPAL <= 64);
    }
    assert!(AccessibilityPageCursor::new("short").is_err());
    assert!(AccessibilityPageCursor::new("A_valid_cursor_123").is_ok());
}

#[test]
fn atspi_addresses_and_snapshot_expansion_are_canonical() {
    assert!(AtspiBusName::new(":1.42").is_ok());
    assert!(AtspiBusName::new(":1").is_err());
    assert!(AtspiBusName::new(":1..42").is_err());
    assert!(AtspiObjectPath::new("/").is_ok());
    assert!(AtspiObjectPath::new("/org/a11y/root").is_ok());
    assert!(AtspiObjectPath::new("/org//root").is_err());
    assert!(AtspiObjectPath::new("/org/root/").is_err());

    let expansion = ElementSnapshotExpansion {
        text_content: true,
        text_metadata: false,
        ..ElementSnapshotExpansion::default()
    };
    assert_eq!(
        expansion.validate(),
        Err(AccessibilityValidationError::UnsupportedFeature)
    );

    let supported = serde_json::from_value::<ElementSnapshotExpansion>(json!({
        "value": true,
        "text_metadata": true,
        "component": true
    }));
    assert!(supported.is_ok());
    assert!(
        serde_json::from_value::<ElementSnapshotExpansion>(json!({
            "value": false,
            "text_metadata": true,
            "text_content": true,
            "component": true
        }))
        .is_err()
    );

    assert!(
        serde_json::from_value::<ElementPredicate>(json!({
            "type": "accessible_id",
            "matcher": {
                "type": "exact",
                "value": "reserved",
                "case_sensitive": true
            }
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ElementWaitPredicate>(json!({
            "type": "text",
            "matcher": {
                "type": "exact",
                "value": "reserved",
                "case_sensitive": true
            }
        }))
        .is_err()
    );
}

#[test]
fn exact_one_resolve_cannot_hide_ambiguity_with_result_index_or_one_match_budget()
-> Result<(), Box<dyn std::error::Error>> {
    let element = element()?;
    let selector = ElementSelector {
        scope: ElementScope::Application {
            application: element.application.clone(),
        },
        predicates: vec![ElementPredicate::Role {
            roles: vec![ElementRole::Entry],
        }],
        order: ElementOrder::Preorder,
        result_index: None,
    };
    let mut request = ElementResolveRequest {
        desktop_id: element.desktop_id,
        desktop_generation: element.desktop_generation,
        selector,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits {
            max_matches: 2,
            ..AccessibilityQueryLimits::default()
        },
    };
    assert!(request.validate().is_ok());

    request.selector.result_index = Some(0);
    assert_eq!(
        request.validate(),
        Err(AccessibilityValidationError::ExactOnePolicy)
    );
    request.selector.result_index = None;
    request.limits.max_matches = 1;
    assert_eq!(
        request.validate(),
        Err(AccessibilityValidationError::ExactOnePolicy)
    );
    Ok(())
}

#[test]
fn exact_one_resolve_result_binds_exact_snapshot_revision_and_generations()
-> Result<(), Box<dyn std::error::Error>> {
    let snapshot = snapshot()?;
    let mut result = ElementResolveResult {
        desktop_id: snapshot.element.desktop_id,
        desktop_generation: snapshot.element.desktop_generation,
        atspi_generation: snapshot.element.atspi_generation,
        snapshot_revision: snapshot.revision,
        element: ElementSnapshotEntry { snapshot },
    };
    assert!(result.validate().is_ok());

    result.snapshot_revision = AccessibilityRevision::new(result.snapshot_revision.get() + 1)?;
    assert_eq!(
        result.validate(),
        Err(AccessibilityValidationError::ResultShape)
    );
    result.snapshot_revision = result.element.snapshot.revision;
    result.atspi_generation = AtspiGeneration::new(result.atspi_generation.get() + 1)?;
    assert_eq!(
        result.validate(),
        Err(AccessibilityValidationError::ResultShape)
    );
    Ok(())
}

#[test]
fn page_and_wait_results_revalidate_every_nested_snapshot() -> Result<(), Box<dyn std::error::Error>>
{
    let snapshot = snapshot()?;
    let mut page = ElementListPage {
        desktop_id: snapshot.element.desktop_id,
        desktop_generation: snapshot.element.desktop_generation,
        atspi_generation: snapshot.element.atspi_generation,
        snapshot_revision: AccessibilityRevision::new(3)?,
        order: ElementOrder::Preorder,
        elements: vec![ElementSnapshotEntry {
            snapshot: snapshot.clone(),
        }],
        next_cursor: None,
        visited_nodes: 1,
        truncated: false,
        warnings: Vec::new(),
    };
    assert_eq!(page.validate(), Ok(()));
    page.elements.push(page.elements[0].clone());
    assert_eq!(
        page.validate(),
        Err(AccessibilityValidationError::ResultShape)
    );

    let wait = ElementWaitResult {
        desktop_id: snapshot.element.desktop_id,
        desktop_generation: snapshot.element.desktop_generation,
        atspi_generation: snapshot.element.atspi_generation,
        status: ElementWaitStatus::Matched,
        evaluated_revision: AccessibilityRevision::new(3)?,
        predicate_satisfied: true,
        matched_count: 1,
        elements: vec![ElementSnapshotEntry { snapshot }],
        poll_fallback_used: false,
        truncated: false,
        warnings: Vec::new(),
    };
    assert_eq!(wait.validate(), Ok(()));
    Ok(())
}

fn accessibility_event(
    kind: AccessibilityEventKind,
) -> Result<AccessibilityEvent, AccessibilityValidationError> {
    let source = element()?;
    Ok(AccessibilityEvent {
        desktop_id: source.desktop_id,
        desktop_generation: source.desktop_generation,
        atspi_generation: source.atspi_generation,
        raw_source: Some(AccessibilityRawSource {
            bus_name: source.application.unique_bus_name.clone(),
            object_path: source.object_path.clone(),
        }),
        source: Some(source),
        kind,
        resync_reason: None,
        detail: AccessibilityEventDetail {
            property: None,
            state: None,
            enabled: None,
            child: None,
            text: None,
            value: None,
            bounds: None,
        },
        revision: AccessibilityRevision::new(4)?,
        cache_sequence: 8,
        source_stale: false,
    })
}

#[test]
fn accessibility_event_source_shapes_are_truthful() -> Result<(), Box<dyn std::error::Error>> {
    let resolved = accessibility_event(AccessibilityEventKind::StateChanged)?;
    assert_eq!(resolved.validate(), Ok(()));

    let mut unresolved = resolved.clone();
    unresolved.source = None;
    unresolved.source_stale = true;
    assert_eq!(unresolved.validate(), Ok(()));

    let mut global_resync = accessibility_event(AccessibilityEventKind::ResyncRequired)?;
    global_resync.source = None;
    global_resync.raw_source = None;
    global_resync.resync_reason = Some(AccessibilityResyncReason::EventQueueOverflow);
    global_resync.source_stale = false;
    assert_eq!(global_resync.validate(), Ok(()));
    assert!(
        serde_json::to_value(&global_resync)?
            .get("raw_source")
            .is_none(),
        "a source-less resync must not serialize a fabricated raw source"
    );
    Ok(())
}

#[test]
fn accessibility_event_rejects_inconsistent_source_shapes() -> Result<(), Box<dyn std::error::Error>>
{
    let resolved = accessibility_event(AccessibilityEventKind::StateChanged)?;

    let mut missing_raw = resolved.clone();
    missing_raw.raw_source = None;
    assert_eq!(
        missing_raw.validate(),
        Err(AccessibilityValidationError::EventSource)
    );

    let mut source_less_non_resync = resolved.clone();
    source_less_non_resync.source = None;
    source_less_non_resync.raw_source = None;
    assert_eq!(
        source_less_non_resync.validate(),
        Err(AccessibilityValidationError::EventSource)
    );

    let mut resync_without_reason = accessibility_event(AccessibilityEventKind::ResyncRequired)?;
    resync_without_reason.source = None;
    resync_without_reason.raw_source = None;
    assert_eq!(
        resync_without_reason.validate(),
        Err(AccessibilityValidationError::EventSource)
    );

    let mut ordinary_with_reason = resolved.clone();
    ordinary_with_reason.resync_reason = Some(AccessibilityResyncReason::ActorSignal);
    assert_eq!(
        ordinary_with_reason.validate(),
        Err(AccessibilityValidationError::EventSource)
    );

    let mut unresolved_not_stale = resolved.clone();
    unresolved_not_stale.source = None;
    assert_eq!(
        unresolved_not_stale.validate(),
        Err(AccessibilityValidationError::EventSource)
    );

    let mut resolved_but_stale = resolved.clone();
    resolved_but_stale.source_stale = true;
    assert_eq!(
        resolved_but_stale.validate(),
        Err(AccessibilityValidationError::EventSource)
    );

    let mut mismatched_raw = resolved;
    mismatched_raw.raw_source = Some(AccessibilityRawSource {
        bus_name: AtspiBusName::new(":1.99")?,
        object_path: AtspiObjectPath::new("/org/a11y/atspi/accessible/99")?,
    });
    assert_eq!(
        mismatched_raw.validate(),
        Err(AccessibilityValidationError::EventSource)
    );
    Ok(())
}
