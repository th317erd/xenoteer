//! Adversarial proofs for the bounded accessibility identity and query model.

use xenoteer_core::{
    AccessibilityCache, AccessibilityModelError, AccessibilityModelLimits,
    AccessibilityQueryDeadline, AccessibilityQueryError, AccessibilityResyncReason,
    AccessibilityTraversalOrder, AccessibilityWaitRegistrationError, AccessibilityWaitRegistry,
    QueryLimit,
};
use xenoteer_protocol::{
    AccessibilityIdentityHash, AccessibilityQueryLimits, AccessibilityRevision,
    AccessibleTextContent, ApplicationRef, AtspiBusName, AtspiGeneration, AtspiObjectPath,
    CoordinateSpace, DesktopGeneration, DesktopId, ElementActionSnapshot, ElementAttribute,
    ElementCompleteness, ElementComponentSnapshot, ElementInterface, ElementOrder,
    ElementPredicate, ElementRef, ElementRelation, ElementRelationType, ElementRole,
    ElementRoleSnapshot, ElementScope, ElementSelector, ElementSnapshot, ElementSnapshotExpansion,
    ElementState, ElementStringMatch, ElementTextSnapshot, ElementValueSnapshot,
    ElementWaitPredicate, ElementWaitRequest, ElementWaitTarget, ElementWindowCorrelation, Rect,
    WindowCorrelationConfidence, WindowIdentityHash, WindowRef,
};

struct Fixture {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    cache: AccessibilityCache,
    application: ApplicationRef,
}

impl Fixture {
    fn new(limits: AccessibilityModelLimits) -> Result<Self, Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let mut cache = AccessibilityCache::new(
            desktop_id,
            desktop_generation,
            AtspiGeneration::new(1)?,
            limits,
        )?;
        let application = cache.register_application(
            AtspiBusName::new(":1.42")?,
            AtspiObjectPath::new("/org/example/App")?,
            hash('a')?,
        )?;
        Ok(Self {
            desktop_id,
            desktop_generation,
            cache,
            application,
        })
    }

    fn reference(
        &self,
        path: &str,
        sequence: u64,
        identity: char,
    ) -> Result<ElementRef, Box<dyn std::error::Error>> {
        Ok(ElementRef {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            atspi_generation: self.cache.atspi_generation(),
            application: self.application.clone(),
            object_path: AtspiObjectPath::new(path)?,
            object_identity_hash: hash(identity)?,
            cache_sequence: sequence,
        })
    }
}

fn hash(byte: char) -> Result<AccessibilityIdentityHash, Box<dyn std::error::Error>> {
    Ok(AccessibilityIdentityHash::new(byte.to_string().repeat(64))?)
}

fn snapshot(
    element: ElementRef,
    parent: Option<ElementRef>,
    index: Option<u32>,
    role: ElementRole,
    name: &str,
) -> Result<ElementSnapshot, Box<dyn std::error::Error>> {
    let snapshot = ElementSnapshot {
        element,
        parent,
        index_in_parent: index,
        child_count: None,
        role: ElementRoleSnapshot {
            role,
            raw_name: None,
            raw_numeric: None,
        },
        name: Some(name.to_owned()),
        description: None,
        accessible_id: None,
        locale: None,
        states: vec![ElementState::Enabled, ElementState::Visible],
        interfaces: vec![ElementInterface::Accessible],
        actions: Vec::new(),
        value: None,
        text: None,
        component: None,
        attributes: Vec::new(),
        relations: Vec::new(),
        window_correlation: ElementWindowCorrelation {
            window: None,
            confidence: WindowCorrelationConfidence::None,
            evidence: Vec::new(),
            conflicting_evidence: false,
        },
        revision: AccessibilityRevision::new(1)?,
        completeness: ElementCompleteness::Complete,
        truncated: false,
        warnings: Vec::new(),
    };
    snapshot.validate()?;
    Ok(snapshot)
}

fn selector(predicates: Vec<ElementPredicate>) -> ElementSelector {
    ElementSelector {
        scope: ElementScope::Desktop,
        predicates,
        order: ElementOrder::Preorder,
        result_index: None,
    }
}

fn supported_expansion() -> ElementSnapshotExpansion {
    ElementSnapshotExpansion {
        actions: false,
        value: true,
        text_metadata: true,
        text_content: false,
        attributes: false,
        relations: false,
        component: true,
    }
}

#[test]
fn path_reuse_never_retargets_a_stale_reference() -> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let first = fixture.reference("/org/example/App/button", 1, 'b')?;
    fixture.cache.observe(snapshot(
        first.clone(),
        None,
        None,
        ElementRole::Button,
        "first",
    )?)?;
    fixture.cache.remove(&first)?;
    let second = fixture.reference("/org/example/App/button", 2, 'c')?;
    fixture.cache.observe(snapshot(
        second.clone(),
        None,
        None,
        ElementRole::Button,
        "second",
    )?)?;

    assert_eq!(
        fixture.cache.resolve_exact(&first),
        Err(AccessibilityModelError::StaleReference)
    );
    assert_eq!(
        fixture.cache.resolve_exact(&second)?.name.as_deref(),
        Some("second")
    );
    Ok(())
}

#[test]
fn application_restart_and_bus_reset_fence_every_old_reference()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let old = fixture.reference("/org/example/App/root", 1, 'b')?;
    fixture.cache.observe(snapshot(
        old.clone(),
        None,
        None,
        ElementRole::Application,
        "old",
    )?)?;
    let replacement = fixture.cache.restart_application(
        fixture.application.unique_bus_name.clone(),
        fixture.application.root_object_path.clone(),
        fixture.application.identity_hash.clone(),
    )?;
    assert_eq!(replacement.app_instance_generation, 2);
    assert_eq!(
        fixture.cache.resolve_exact(&old),
        Err(AccessibilityModelError::StaleApplication)
    );

    let barrier = fixture.cache.reset_bus()?;
    assert_eq!(barrier.reason, AccessibilityResyncReason::BusReset);
    assert_eq!(barrier.atspi_generation.get(), 2);
    assert_eq!(
        fixture.cache.next_element_ref(
            &replacement,
            AtspiObjectPath::new("/org/example/App/new")?,
            hash('d')?,
        ),
        Err(AccessibilityModelError::StaleGeneration)
    );
    Ok(())
}

#[test]
fn stale_application_scope_fails_instead_of_masquerading_as_empty()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let stale_application = fixture.application.clone();
    fixture.application = fixture.cache.restart_application(
        stale_application.unique_bus_name.clone(),
        stale_application.root_object_path.clone(),
        hash('b')?,
    )?;
    let selector = ElementSelector {
        scope: ElementScope::Application {
            application: stale_application,
        },
        predicates: Vec::new(),
        order: ElementOrder::Preorder,
        result_index: None,
    };
    assert_eq!(
        fixture.cache.snapshot().query(
            &selector,
            ElementSnapshotExpansion::default(),
            AccessibilityQueryLimits::default(),
            1,
            0,
        ),
        Err(AccessibilityQueryError::StaleReference)
    );
    Ok(())
}

#[test]
fn window_scope_starts_at_highest_correlated_descendants() -> Result<(), Box<dyn std::error::Error>>
{
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let application = fixture.reference("/org/example/App/root", 1, 'b')?;
    let top_level = fixture.reference("/org/example/App/window", 2, 'c')?;
    let button = fixture.reference("/org/example/App/window/button", 3, 'd')?;
    fixture.cache.observe(snapshot(
        application.clone(),
        None,
        None,
        ElementRole::Application,
        "app",
    )?)?;
    let window = WindowRef {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        xid: 42,
        observed_generation: 1,
        identity_hash: WindowIdentityHash::new("e".repeat(64))?,
    };
    let mut top_level_snapshot = snapshot(
        top_level.clone(),
        Some(application),
        Some(0),
        ElementRole::Window,
        "window",
    )?;
    top_level_snapshot.window_correlation.window = Some(window.clone());
    top_level_snapshot.window_correlation.confidence = WindowCorrelationConfidence::Strong;
    fixture.cache.observe(top_level_snapshot)?;
    fixture.cache.observe(snapshot(
        button.clone(),
        Some(top_level),
        Some(0),
        ElementRole::Button,
        "Save",
    )?)?;
    let selector = ElementSelector {
        scope: ElementScope::Window { window },
        predicates: vec![ElementPredicate::Role {
            roles: vec![ElementRole::Button],
        }],
        order: ElementOrder::Preorder,
        result_index: None,
    };
    let result = fixture.cache.snapshot().query(
        &selector,
        ElementSnapshotExpansion::default(),
        AccessibilityQueryLimits::default(),
        1,
        0,
    )?;
    assert_eq!(result.total_matches, 1);
    assert_eq!(result.elements[0].snapshot.element, button);
    Ok(())
}

#[test]
fn cycles_orphans_and_self_parents_are_bounded_and_request_resync()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let first = fixture.reference("/org/example/App/a", 1, 'b')?;
    let second = fixture.reference("/org/example/App/b", 2, 'c')?;
    fixture.cache.observe(snapshot(
        first.clone(),
        Some(second.clone()),
        Some(0),
        ElementRole::Panel,
        "a",
    )?)?;
    fixture.cache.observe(snapshot(
        second.clone(),
        Some(first.clone()),
        Some(0),
        ElementRole::Panel,
        "b",
    )?)?;
    let third = fixture.reference("/org/example/App/c", 3, 'd')?;
    fixture.cache.observe(snapshot(
        third.clone(),
        Some(third),
        Some(0),
        ElementRole::Panel,
        "c",
    )?)?;
    let orphan = fixture.reference("/org/example/App/orphan", 4, 'e')?;
    let missing_parent = ElementRef {
        object_path: AtspiObjectPath::new("/org/example/App/missing")?,
        cache_sequence: 99,
        ..orphan.clone()
    };
    fixture.cache.observe(snapshot(
        orphan,
        Some(missing_parent),
        Some(0),
        ElementRole::Panel,
        "orphan",
    )?)?;

    let view = fixture.cache.snapshot();
    assert!(view.graph_status().dirty);
    assert!(view.graph_status().resync_required);
    assert!(
        view.graph_status()
            .warnings
            .iter()
            .any(|warning| warning.code == "accessibility.cycle")
    );
    assert!(
        view.graph_status()
            .warnings
            .iter()
            .any(|warning| warning.code == "accessibility.self_parent")
    );
    assert!(
        view.graph_status()
            .warnings
            .iter()
            .any(|warning| warning.code == "accessibility.orphan")
    );
    for order in [
        AccessibilityTraversalOrder::Preorder,
        AccessibilityTraversalOrder::Postorder,
        AccessibilityTraversalOrder::BreadthFirst,
    ] {
        let traversed = view.traverse(order);
        assert_eq!(traversed.len(), 4);
        let unique = traversed
            .iter()
            .map(|snapshot| snapshot.element.clone())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), 4);
    }
    Ok(())
}

#[test]
fn child_indices_define_deterministic_pre_post_and_breadth_orders()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let root = fixture.reference("/org/example/App/root", 1, 'b')?;
    fixture.cache.observe(snapshot(
        root.clone(),
        None,
        None,
        ElementRole::Application,
        "root",
    )?)?;
    let late = fixture.reference("/org/example/App/z", 2, 'c')?;
    fixture.cache.observe(snapshot(
        late,
        Some(root.clone()),
        Some(2),
        ElementRole::Button,
        "late",
    )?)?;
    let early = fixture.reference("/org/example/App/a", 3, 'd')?;
    fixture.cache.observe(snapshot(
        early,
        Some(root),
        Some(1),
        ElementRole::Button,
        "early",
    )?)?;
    let view = fixture.cache.snapshot();
    let names = |order| {
        view.traverse(order)
            .into_iter()
            .map(|snapshot| snapshot.name.as_deref().unwrap_or_default())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(AccessibilityTraversalOrder::Preorder),
        ["root", "early", "late"]
    );
    assert_eq!(
        names(AccessibilityTraversalOrder::Postorder),
        ["early", "late", "root"]
    );
    assert_eq!(
        names(AccessibilityTraversalOrder::BreadthFirst),
        ["root", "early", "late"]
    );
    Ok(())
}

#[test]
fn every_supported_selector_predicate_matches_exact_snapshot_fields()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let element = fixture.reference("/org/example/App/submit", 1, 'b')?;
    let mut candidate = snapshot(
        element.clone(),
        None,
        Some(7),
        ElementRole::Button,
        "Submit order",
    )?;
    candidate.description = Some("Final action".to_owned());
    candidate.accessible_id = Some("submit-order".to_owned());
    candidate.states.push(ElementState::Focused);
    candidate.interfaces.push(ElementInterface::Action);
    candidate.actions.push(ElementActionSnapshot {
        name: "press".to_owned(),
        description: None,
        key_binding: None,
    });
    candidate.value = Some(ElementValueSnapshot {
        current: 5.0,
        minimum: Some(0.0),
        maximum: Some(10.0),
        increment: Some(1.0),
        text: Some("five".to_owned()),
    });
    candidate.child_count = Some(2);
    candidate.attributes.push(ElementAttribute {
        name: "automation".to_owned(),
        value: "primary-submit".to_owned(),
    });
    candidate.relations.push(ElementRelation {
        relation: ElementRelationType::MemberOf,
        targets: vec![element.clone()],
    });
    candidate.component = Some(ElementComponentSnapshot {
        coordinate_space: CoordinateSpace::AtspiScreen,
        extents: Some(Rect::new(10, 10, 50, 20)?),
        layer: None,
        z_order: None,
        alpha: Some(255),
    });
    fixture.cache.observe(candidate)?;
    let predicates = vec![
        ElementPredicate::Role {
            roles: vec![ElementRole::Button],
        },
        ElementPredicate::Name {
            matcher: ElementStringMatch::Regex {
                pattern: "^submit\\s+order$".to_owned(),
                case_sensitive: false,
            },
        },
        ElementPredicate::Description {
            matcher: ElementStringMatch::Prefix {
                value: "Final".to_owned(),
                case_sensitive: true,
            },
        },
        ElementPredicate::State {
            state: ElementState::Focused,
            value: true,
        },
        ElementPredicate::Interface {
            interface: ElementInterface::Action,
        },
        ElementPredicate::ValueRange {
            minimum: Some(4.0),
            maximum: Some(6.0),
        },
        ElementPredicate::IndexInParent { index: 7 },
        ElementPredicate::ChildCount {
            minimum: Some(1),
            maximum: Some(3),
        },
        ElementPredicate::ComponentIntersects {
            coordinate_space: CoordinateSpace::AtspiScreen,
            rect: Rect::new(20, 15, 2, 2)?,
        },
    ];
    let result = fixture.cache.snapshot().query(
        &selector(predicates),
        supported_expansion(),
        AccessibilityQueryLimits::default(),
        10,
        0,
    )?;
    assert_eq!(result.total_matches, 1);
    Ok(())
}

#[test]
fn query_limits_and_exact_one_ambiguity_are_explicit() -> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    for (sequence, name, identity) in [(1, "one", 'b'), (2, "two", 'c')] {
        let element = fixture.reference(&format!("/org/example/App/{name}"), sequence, identity)?;
        fixture
            .cache
            .observe(snapshot(element, None, None, ElementRole::Button, name)?)?;
    }
    let view = fixture.cache.snapshot();
    let buttons = selector(vec![ElementPredicate::Role {
        roles: vec![ElementRole::Button],
    }]);
    assert_eq!(
        view.resolve_exactly_one(
            &buttons,
            ElementSnapshotExpansion::default(),
            AccessibilityQueryLimits::default()
        ),
        Err(AccessibilityQueryError::Ambiguous { matches: 2 })
    );
    let tiny = AccessibilityQueryLimits {
        max_visited_nodes: 1,
        ..AccessibilityQueryLimits::default()
    };
    assert_eq!(
        view.query(&buttons, ElementSnapshotExpansion::default(), tiny, 1, 0),
        Err(AccessibilityQueryError::LimitExceeded(
            QueryLimit::VisitedNodes
        ))
    );
    let one_match = AccessibilityQueryLimits {
        max_matches: 1,
        ..AccessibilityQueryLimits::default()
    };
    assert_eq!(
        view.query(
            &buttons,
            ElementSnapshotExpansion::default(),
            one_match,
            1,
            0
        ),
        Err(AccessibilityQueryError::LimitExceeded(QueryLimit::Matches))
    );
    Ok(())
}

#[test]
fn expired_absolute_query_deadline_has_a_typed_limit_error()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let deadline = AccessibilityQueryDeadline::at(std::time::Instant::now());
    assert_eq!(
        fixture.cache.snapshot().query_with_deadline(
            &selector(Vec::new()),
            ElementSnapshotExpansion::default(),
            AccessibilityQueryLimits::default(),
            1,
            0,
            deadline,
        ),
        Err(AccessibilityQueryError::LimitExceeded(QueryLimit::Timeout))
    );
    Ok(())
}

#[test]
fn depth_budget_is_enforced_before_returning_a_partial_deep_tree()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let root = fixture.reference("/org/example/App/root", 1, 'b')?;
    fixture.cache.observe(snapshot(
        root.clone(),
        None,
        None,
        ElementRole::Application,
        "root",
    )?)?;
    let child = fixture.reference("/org/example/App/child", 2, 'c')?;
    fixture.cache.observe(snapshot(
        child.clone(),
        Some(root),
        Some(0),
        ElementRole::Panel,
        "child",
    )?)?;
    let leaf = fixture.reference("/org/example/App/leaf", 3, 'd')?;
    fixture.cache.observe(snapshot(
        leaf,
        Some(child),
        Some(0),
        ElementRole::Button,
        "leaf",
    )?)?;
    let limits = AccessibilityQueryLimits {
        max_depth: 1,
        ..AccessibilityQueryLimits::default()
    };
    assert_eq!(
        fixture.cache.snapshot().query(
            &selector(Vec::new()),
            ElementSnapshotExpansion::default(),
            limits,
            10,
            0,
        ),
        Err(AccessibilityQueryError::LimitExceeded(QueryLimit::Depth))
    );
    Ok(())
}

#[test]
fn adversarial_parent_graphs_never_panic_or_duplicate_traversal()
-> Result<(), Box<dyn std::error::Error>> {
    for seed in 0_usize..32 {
        let count = 1 + (seed * 11 % 47);
        let mut fixture = Fixture::new(AccessibilityModelLimits {
            max_live_nodes: 64,
            max_tombstones: 64,
        })?;
        let references = (0..count)
            .map(|index| {
                fixture.reference(
                    &format!("/org/example/App/node_{index}"),
                    index as u64 + 1,
                    'b',
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (index, element) in references.iter().cloned().enumerate() {
            let parent_slot = (index * 17 + seed * 13) % (count + 7);
            let parent = if (index + seed) % 5 == 0 {
                None
            } else if parent_slot < count {
                Some(references[parent_slot].clone())
            } else {
                Some(ElementRef {
                    object_path: AtspiObjectPath::new(format!(
                        "/org/example/App/missing_{parent_slot}"
                    ))?,
                    cache_sequence: parent_slot as u64 + 100,
                    ..element.clone()
                })
            };
            fixture.cache.observe(snapshot(
                element,
                parent,
                Some(index as u32),
                ElementRole::Panel,
                &format!("node-{index}"),
            )?)?;
        }
        let view = fixture.cache.snapshot();
        for order in [
            AccessibilityTraversalOrder::Preorder,
            AccessibilityTraversalOrder::Postorder,
            AccessibilityTraversalOrder::BreadthFirst,
        ] {
            let traversed = view.traverse(order);
            assert_eq!(traversed.len(), count);
            assert_eq!(
                traversed
                    .iter()
                    .map(|snapshot| snapshot.element.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                count
            );
        }
    }
    Ok(())
}

#[test]
fn immutable_views_and_wait_recheck_close_the_registration_race()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let element = fixture.reference("/org/example/App/status", 1, 'b')?;
    fixture.cache.observe(snapshot(
        element.clone(),
        None,
        None,
        ElementRole::Label,
        "pending",
    )?)?;
    let frozen = fixture.cache.snapshot();
    let request = ElementWaitRequest {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        target: ElementWaitTarget::Reference {
            element: element.clone(),
        },
        predicate: ElementWaitPredicate::Name {
            matcher: ElementStringMatch::Exact {
                value: "ready".to_owned(),
                case_sensitive: true,
            },
        },
        after_revision: None,
        timeout_ms: 1_000,
        allow_poll_fallback: false,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    };
    let prepared = fixture.cache.prepare_wait(request.clone())?;
    assert!(!prepared.initial_evaluation().predicate_satisfied);
    let mut registry = AccessibilityWaitRegistry::new(1)?;
    let token = registry.register(prepared)?;
    assert_eq!(
        registry.register(fixture.cache.prepare_wait(request)?),
        Err(AccessibilityWaitRegistrationError::CapacityExhausted)
    );
    fixture
        .cache
        .observe(snapshot(element, None, None, ElementRole::Label, "ready")?)?;
    let registered = registry
        .get(token)
        .ok_or("registered wait disappeared before its second check")?;
    assert!(fixture.cache.recheck_wait(registered)?.predicate_satisfied);
    assert!(registry.remove(token).is_some());
    assert_eq!(
        frozen.traverse(AccessibilityTraversalOrder::Preorder)[0]
            .name
            .as_deref(),
        Some("pending")
    );
    Ok(())
}

#[test]
fn wait_requires_a_revision_strictly_after_its_boundary() -> Result<(), Box<dyn std::error::Error>>
{
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let element = fixture.reference("/org/example/App/status", 1, 'b')?;
    fixture.cache.observe(snapshot(
        element.clone(),
        None,
        None,
        ElementRole::Label,
        "ready",
    )?)?;
    let boundary = fixture.cache.revision();
    let request = ElementWaitRequest {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        target: ElementWaitTarget::Reference {
            element: element.clone(),
        },
        predicate: ElementWaitPredicate::Name {
            matcher: ElementStringMatch::Exact {
                value: "ready".to_owned(),
                case_sensitive: true,
            },
        },
        after_revision: Some(boundary),
        timeout_ms: 1_000,
        allow_poll_fallback: false,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    };
    assert!(
        !fixture
            .cache
            .snapshot()
            .evaluate_wait(&request)?
            .predicate_satisfied
    );
    fixture
        .cache
        .observe(snapshot(element, None, None, ElementRole::Label, "ready")?)?;
    assert!(
        fixture
            .cache
            .snapshot()
            .evaluate_wait(&request)?
            .predicate_satisfied
    );
    Ok(())
}

#[test]
fn continuation_is_bound_to_the_original_immutable_revision()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    for (sequence, name, identity) in [(1, "one", 'b'), (2, "two", 'c')] {
        let element = fixture.reference(&format!("/org/example/App/{name}"), sequence, identity)?;
        fixture
            .cache
            .observe(snapshot(element, None, None, ElementRole::Button, name)?)?;
    }
    let selector = selector(Vec::new());
    let frozen = fixture.cache.snapshot();
    let first = frozen.query(
        &selector,
        ElementSnapshotExpansion::default(),
        AccessibilityQueryLimits::default(),
        1,
        0,
    )?;
    let continuation = first
        .continuation
        .as_ref()
        .ok_or("first page did not expose continuation state")?;
    assert_eq!(
        frozen
            .continue_query(
                &selector,
                AccessibilityQueryLimits::default(),
                1,
                continuation,
            )?
            .elements[0]
            .snapshot
            .name
            .as_deref(),
        Some("two")
    );

    let third = fixture.reference("/org/example/App/three", 3, 'd')?;
    fixture
        .cache
        .observe(snapshot(third, None, None, ElementRole::Button, "three")?)?;
    assert_eq!(
        fixture.cache.snapshot().continue_query(
            &selector,
            AccessibilityQueryLimits::default(),
            1,
            continuation,
        ),
        Err(AccessibilityQueryError::ContinuationMismatch)
    );
    Ok(())
}

#[test]
fn reference_gone_wait_is_true_only_after_exact_birth_removal()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let element = fixture.reference("/org/example/App/transient", 1, 'b')?;
    fixture.cache.observe(snapshot(
        element.clone(),
        None,
        None,
        ElementRole::Alert,
        "alert",
    )?)?;
    let request = ElementWaitRequest {
        desktop_id: fixture.desktop_id,
        desktop_generation: fixture.desktop_generation,
        target: ElementWaitTarget::Reference {
            element: element.clone(),
        },
        predicate: ElementWaitPredicate::Gone,
        after_revision: None,
        timeout_ms: 1_000,
        allow_poll_fallback: false,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    };
    assert!(
        !fixture
            .cache
            .snapshot()
            .evaluate_wait(&request)?
            .predicate_satisfied
    );
    fixture.cache.remove(&element)?;
    assert!(
        fixture
            .cache
            .snapshot()
            .evaluate_wait(&request)?
            .predicate_satisfied
    );
    Ok(())
}

#[test]
fn protected_content_is_removed_before_it_enters_public_snapshots()
-> Result<(), Box<dyn std::error::Error>> {
    let mut fixture = Fixture::new(AccessibilityModelLimits::default())?;
    let element = fixture.reference("/org/example/App/password", 1, 'b')?;
    let mut password = snapshot(
        element.clone(),
        None,
        None,
        ElementRole::PasswordText,
        "password",
    )?;
    password.text = Some(ElementTextSnapshot {
        character_count: 12,
        caret_offset: 12,
        selections: Vec::new(),
        content: Some(AccessibleTextContent::new("super-secret")?),
        content_truncated: false,
        protected: false,
    });
    password.value = Some(ElementValueSnapshot {
        current: 12.0,
        minimum: None,
        maximum: None,
        increment: None,
        text: Some("super-secret".to_owned()),
    });
    password.attributes.push(ElementAttribute {
        name: "raw-value".to_owned(),
        value: "super-secret".to_owned(),
    });
    fixture.cache.observe(password)?;
    let public = fixture
        .cache
        .snapshot()
        .resolve_exact(&element, supported_expansion())?
        .snapshot;
    assert!(
        public
            .text
            .as_ref()
            .is_some_and(|text| text.content.is_none() && text.protected)
    );
    assert!(public.value.is_none());
    assert!(public.attributes.is_empty());
    assert!(
        public
            .warnings
            .iter()
            .any(|warning| warning.code == "accessibility.protected_redacted")
    );

    // Protection is sticky for this exact object birth even if a broken or
    // hostile toolkit later clears every protection hint and changes the role.
    let mut dishonest_refresh = snapshot(
        element.clone(),
        None,
        None,
        ElementRole::Text,
        "not-a-password-anymore",
    )?;
    dishonest_refresh.text = Some(ElementTextSnapshot {
        character_count: 11,
        caret_offset: 11,
        selections: Vec::new(),
        content: Some(AccessibleTextContent::new("still-secret")?),
        content_truncated: false,
        protected: false,
    });
    dishonest_refresh.value = Some(ElementValueSnapshot {
        current: 11.0,
        minimum: None,
        maximum: None,
        increment: None,
        text: Some("still-secret".to_owned()),
    });
    dishonest_refresh.attributes.push(ElementAttribute {
        name: "raw-value".to_owned(),
        value: "still-secret".to_owned(),
    });
    fixture.cache.observe(dishonest_refresh)?;
    let sticky = fixture
        .cache
        .snapshot()
        .resolve_exact(&element, supported_expansion())?
        .snapshot;
    assert!(
        sticky
            .text
            .as_ref()
            .is_some_and(|text| text.content.is_none() && text.protected)
    );
    assert!(sticky.value.is_none());
    assert!(sticky.attributes.is_empty());
    Ok(())
}

#[test]
fn live_capacity_and_event_gap_cross_generation_barriers() -> Result<(), Box<dyn std::error::Error>>
{
    let mut fixture = Fixture::new(AccessibilityModelLimits {
        max_live_nodes: 1,
        max_tombstones: 1,
    })?;
    let first = fixture.reference("/org/example/App/one", 1, 'b')?;
    fixture
        .cache
        .observe(snapshot(first, None, None, ElementRole::Button, "one")?)?;
    let second = fixture.reference("/org/example/App/two", 2, 'c')?;
    let error =
        match fixture
            .cache
            .observe(snapshot(second, None, None, ElementRole::Button, "two")?)
        {
            Ok(_) => return Err("capacity unexpectedly accepted a second live node".into()),
            Err(error) => error,
        };
    assert!(matches!(
        error,
        AccessibilityModelError::ResyncRequired(barrier)
            if barrier.reason == AccessibilityResyncReason::LiveCapacity
                && barrier.atspi_generation.get() == 2
    ));
    assert_eq!(fixture.cache.counts(), (0, 0));
    let barrier = fixture.cache.event_gap()?;
    assert_eq!(barrier.reason, AccessibilityResyncReason::EventGap);
    assert_eq!(barrier.atspi_generation.get(), 3);
    Ok(())
}
