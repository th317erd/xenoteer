//! Adversarial proofs for physical element-click admission and revalidation.

use xenoteer_core::{
    ElementClickObservation, ElementClickOcclusionSnapshot, ElementClickPlanError,
    MAX_ELEMENT_CLICK_OCCLUDERS, plan_physical_element_click, revalidate_physical_element_click,
};
use xenoteer_protocol::{
    AccessibilityIdentityHash, AccessibilityRevision, ApplicationRef, AtspiBusName,
    AtspiGeneration, AtspiObjectPath, DesktopGeneration, DesktopId, ElementClickPointPolicy,
    ElementOcclusionPolicy, ElementRef, ElementWindowCorrelation, OcclusionCheckResult, Point,
    Rect, WindowCorrelationConfidence, WindowCorrelationEvidence, WindowCorrelationSignal,
    WindowIdentityHash, WindowRef,
};

struct Fixture {
    element: ElementRef,
    window: WindowRef,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let atspi_generation = AtspiGeneration::new(1)?;
        let application = ApplicationRef {
            desktop_id,
            desktop_generation,
            atspi_generation,
            unique_bus_name: AtspiBusName::new(":1.42")?,
            root_object_path: AtspiObjectPath::new("/org/example/App")?,
            app_instance_generation: 1,
            identity_hash: AccessibilityIdentityHash::new("a".repeat(64))?,
        };
        let element = ElementRef {
            desktop_id,
            desktop_generation,
            atspi_generation,
            application,
            object_path: AtspiObjectPath::new("/org/example/App/button")?,
            object_identity_hash: AccessibilityIdentityHash::new("b".repeat(64))?,
            cache_sequence: 7,
        };
        let window = WindowRef {
            desktop_id,
            desktop_generation,
            xid: 42,
            observed_generation: 3,
            identity_hash: WindowIdentityHash::new("c".repeat(64))?,
        };
        Ok(Self { element, window })
    }

    fn correlation(&self) -> ElementWindowCorrelation {
        ElementWindowCorrelation {
            window: Some(self.window.clone()),
            confidence: WindowCorrelationConfidence::Strong,
            evidence: vec![WindowCorrelationEvidence {
                signal: WindowCorrelationSignal::ExplicitCallerReference,
                matched: true,
                detail: None,
            }],
            conflicting_evidence: false,
        }
    }

    fn observation(&self) -> Result<ElementClickObservation, Box<dyn std::error::Error>> {
        Ok(ElementClickObservation {
            element: self.element.clone(),
            revision: AccessibilityRevision::new(10)?,
            read_epoch: 100,
            element_extents: Rect::new(10, 20, 20, 10)?,
            root_bounds: Rect::new(-100, -100, 400, 400)?,
            correlated_client_bounds: Some(Rect::new(0, 0, 100, 100)?),
            correlation: self.correlation(),
        })
    }
}

fn plan(
    fixture: &Fixture,
    observation: &ElementClickObservation,
    point_policy: ElementClickPointPolicy,
) -> Result<xenoteer_core::PhysicalElementClickPlan, ElementClickPlanError> {
    plan_physical_element_click(
        observation,
        Some(&fixture.window),
        WindowCorrelationConfidence::Strong,
        &point_policy,
        ElementOcclusionPolicy::Ignore,
        None,
    )
}

#[test]
fn center_uses_checked_three_way_intersection_and_upper_left_bias()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    observation.element_extents = Rect::new(-10, -4, 20, 12)?;
    observation.root_bounds = Rect::new(-8, -3, 15, 10)?;
    observation.correlated_client_bounds = Some(Rect::new(-5, -2, 20, 9)?);

    let plan = plan(&fixture, &observation, ElementClickPointPolicy::Center)?;

    assert_eq!(plan.geometry().visible_bounds, Rect::new(-5, -2, 12, 9)?);
    assert_eq!(plan.click_point(), Point::new(0, 2));
    Ok(())
}

#[test]
fn inset_center_has_explicit_empty_boundary() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    observation.element_extents = Rect::new(0, 0, 4, 4)?;
    observation.correlated_client_bounds = None;

    assert_eq!(
        plan(
            &fixture,
            &observation,
            ElementClickPointPolicy::InsetCenter { inset_pixels: 2 }
        ),
        Err(ElementClickPlanError::InsetExhausted)
    );
    let admitted = plan(
        &fixture,
        &observation,
        ElementClickPointPolicy::InsetCenter { inset_pixels: 1 },
    )?;
    assert_eq!(admitted.click_point(), Point::new(1, 1));
    Ok(())
}

#[test]
fn offset_is_relative_to_original_element_and_must_be_visible()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    observation.element_extents = Rect::new(-10, 20, 30, 10)?;
    observation.root_bounds = Rect::new(0, 0, 100, 100)?;
    observation.correlated_client_bounds = None;

    let admitted = plan(
        &fixture,
        &observation,
        ElementClickPointPolicy::Offset {
            offset: Point::new(10, 0),
        },
    )?;
    assert_eq!(admitted.click_point(), Point::new(0, 20));
    assert_eq!(
        plan(
            &fixture,
            &observation,
            ElementClickPointPolicy::Offset {
                offset: Point::new(9, 0)
            }
        ),
        Err(ElementClickPlanError::PointOutsideVisibleBounds)
    );
    Ok(())
}

#[test]
fn overflowing_offset_is_rejected_before_narrowing() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    observation.element_extents = Rect::new(i32::MAX - 10, 0, 11, 1)?;
    observation.root_bounds = observation.element_extents;
    observation.correlated_client_bounds = None;

    assert_eq!(
        plan(
            &fixture,
            &observation,
            ElementClickPointPolicy::Offset {
                offset: Point::new(i32::MAX, 0)
            }
        ),
        Err(ElementClickPlanError::PointOverflow)
    );
    Ok(())
}

#[test]
fn negatively_overflowing_offset_is_rejected_before_narrowing()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    observation.element_extents = Rect::new(i32::MIN, 0, 1, 1)?;
    observation.root_bounds = observation.element_extents;
    observation.correlated_client_bounds = None;

    assert_eq!(
        plan(
            &fixture,
            &observation,
            ElementClickPointPolicy::Offset {
                offset: Point::new(-1, 0)
            }
        ),
        Err(ElementClickPlanError::PointOverflow)
    );
    Ok(())
}

#[test]
fn nearest_visible_projects_the_unclipped_element_center() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    observation.element_extents = Rect::new(-20, 20, 40, 20)?;
    observation.root_bounds = Rect::new(10, 0, 100, 100)?;
    observation.correlated_client_bounds = None;

    let admitted = plan(
        &fixture,
        &observation,
        ElementClickPointPolicy::NearestVisible,
    )?;
    assert_eq!(
        admitted.geometry().visible_bounds,
        Rect::new(10, 20, 10, 20)?
    );
    assert_eq!(admitted.click_point(), Point::new(10, 29));
    Ok(())
}

#[test]
fn fully_offscreen_or_client_clipped_element_is_rejected() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    observation.root_bounds = Rect::new(100, 100, 10, 10)?;
    assert_eq!(
        plan(&fixture, &observation, ElementClickPointPolicy::Center),
        Err(ElementClickPlanError::NotVisible)
    );

    observation.root_bounds = Rect::new(0, 0, 200, 200)?;
    observation.correlated_client_bounds = Some(Rect::new(100, 100, 10, 10)?);
    assert_eq!(
        plan(&fixture, &observation, ElementClickPointPolicy::Center),
        Err(ElementClickPlanError::NotVisible)
    );
    Ok(())
}

#[test]
fn deserialized_empty_and_overflowing_rectangles_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    observation.root_bounds = serde_json::from_str(r#"{"x":0,"y":0,"width":0,"height":1}"#)?;
    assert_eq!(
        plan(&fixture, &observation, ElementClickPointPolicy::Center),
        Err(ElementClickPlanError::InvalidGeometry)
    );

    observation = fixture.observation()?;
    observation.element_extents =
        serde_json::from_str(r#"{"x":2147483647,"y":0,"width":2,"height":1}"#)?;
    assert_eq!(
        plan(&fixture, &observation, ElementClickPointPolicy::Center),
        Err(ElementClickPlanError::InvalidGeometry)
    );
    Ok(())
}

#[test]
fn weak_or_conflicting_correlation_never_admits_a_click() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    observation.correlation.confidence = WindowCorrelationConfidence::Weak;
    assert_eq!(
        plan(&fixture, &observation, ElementClickPointPolicy::Center),
        Err(ElementClickPlanError::UnauthorizedCorrelation)
    );
    observation.correlation.confidence = WindowCorrelationConfidence::Strong;
    observation.correlation.conflicting_evidence = true;
    assert_eq!(
        plan(&fixture, &observation, ElementClickPointPolicy::Center),
        Err(ElementClickPlanError::UnauthorizedCorrelation)
    );
    Ok(())
}

#[test]
fn minimum_correlation_rules_match_the_protocol_contract() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;

    assert_eq!(
        plan_physical_element_click(
            &observation,
            None,
            WindowCorrelationConfidence::Weak,
            &ElementClickPointPolicy::Center,
            ElementOcclusionPolicy::Ignore,
            None,
        ),
        Err(ElementClickPlanError::InvalidMinimumCorrelation)
    );
    assert_eq!(
        plan_physical_element_click(
            &observation,
            Some(&fixture.window),
            WindowCorrelationConfidence::None,
            &ElementClickPointPolicy::Center,
            ElementOcclusionPolicy::Ignore,
            None,
        ),
        Err(ElementClickPlanError::InvalidMinimumCorrelation)
    );
    let admitted = plan_physical_element_click(
        &observation,
        Some(&fixture.window),
        WindowCorrelationConfidence::Weak,
        &ElementClickPointPolicy::Center,
        ElementOcclusionPolicy::Ignore,
        None,
    )?;
    assert_eq!(
        admitted.minimum_correlation(),
        WindowCorrelationConfidence::Weak
    );
    Ok(())
}

#[test]
fn exact_process_minimum_is_enforced_at_admission_and_queue_head()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    assert_eq!(
        plan_physical_element_click(
            &observation,
            Some(&fixture.window),
            WindowCorrelationConfidence::ExactProcess,
            &ElementClickPointPolicy::Center,
            ElementOcclusionPolicy::Ignore,
            None,
        ),
        Err(ElementClickPlanError::CorrelationBelowMinimum)
    );

    observation.correlation.confidence = WindowCorrelationConfidence::ExactProcess;
    let admitted = plan_physical_element_click(
        &observation,
        Some(&fixture.window),
        WindowCorrelationConfidence::ExactProcess,
        &ElementClickPointPolicy::Center,
        ElementOcclusionPolicy::Ignore,
        None,
    )?;
    let mut fresh = observation.clone();
    fresh.read_epoch = 101;
    fresh.correlation.confidence = WindowCorrelationConfidence::Strong;
    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::CorrelationBelowMinimum)
    );
    Ok(())
}

#[test]
fn explicit_window_must_match_the_correlated_exact_birth() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let mut reborn_window = fixture.window.clone();
    reborn_window.observed_generation += 1;
    assert_eq!(
        plan_physical_element_click(
            &observation,
            Some(&reborn_window),
            WindowCorrelationConfidence::Strong,
            &ElementClickPointPolicy::Center,
            ElementOcclusionPolicy::Ignore,
            None,
        ),
        Err(ElementClickPlanError::WindowBindingChanged)
    );
    Ok(())
}

#[test]
fn rectangular_occlusion_uses_half_open_pixel_boundaries() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let boundary = [Rect::new(18, 24, 1, 1)?, Rect::new(19, 23, 1, 1)?];
    let clear = plan_physical_element_click(
        &observation,
        Some(&fixture.window),
        WindowCorrelationConfidence::Strong,
        &ElementClickPointPolicy::Center,
        ElementOcclusionPolicy::BestEffortReject,
        Some(ElementClickOcclusionSnapshot {
            target_window: &fixture.window,
            stacking_epoch: 10,
            rectangles_above: &boundary,
            stacking_complete: true,
        }),
    )?;
    assert_eq!(clear.click_point(), Point::new(19, 24));
    assert_eq!(clear.occlusion_check(), OcclusionCheckResult::Clear);
    Ok(())
}

#[test]
fn known_occlusion_rejects_both_checking_policies() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let covering = [Rect::new(19, 24, 1, 1)?];
    for policy in [
        ElementOcclusionPolicy::BestEffortReject,
        ElementOcclusionPolicy::RequireUnoccluded,
    ] {
        assert_eq!(
            plan_physical_element_click(
                &observation,
                Some(&fixture.window),
                WindowCorrelationConfidence::Strong,
                &ElementClickPointPolicy::Center,
                policy,
                Some(ElementClickOcclusionSnapshot {
                    target_window: &fixture.window,
                    stacking_epoch: 10,
                    rectangles_above: &covering,
                    stacking_complete: true,
                }),
            ),
            Err(ElementClickPlanError::Occluded)
        );
    }
    Ok(())
}

#[test]
fn incomplete_or_over_budget_occlusion_is_inconclusive_and_require_fails_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let incomplete = ElementClickOcclusionSnapshot {
        target_window: &fixture.window,
        stacking_epoch: 10,
        rectangles_above: &[],
        stacking_complete: false,
    };
    let admitted = plan_physical_element_click(
        &observation,
        Some(&fixture.window),
        WindowCorrelationConfidence::Strong,
        &ElementClickPointPolicy::Center,
        ElementOcclusionPolicy::BestEffortReject,
        Some(incomplete),
    )?;
    assert_eq!(
        admitted.occlusion_check(),
        OcclusionCheckResult::Inconclusive
    );
    assert_eq!(
        plan_physical_element_click(
            &observation,
            Some(&fixture.window),
            WindowCorrelationConfidence::Strong,
            &ElementClickPointPolicy::Center,
            ElementOcclusionPolicy::RequireUnoccluded,
            Some(incomplete),
        ),
        Err(ElementClickPlanError::OcclusionInconclusive)
    );

    let over_budget = vec![Rect::new(0, 0, 1, 1)?; MAX_ELEMENT_CLICK_OCCLUDERS + 1];
    let snapshot = ElementClickOcclusionSnapshot {
        target_window: &fixture.window,
        stacking_epoch: 10,
        rectangles_above: &over_budget,
        stacking_complete: true,
    };
    let admitted = plan_physical_element_click(
        &observation,
        Some(&fixture.window),
        WindowCorrelationConfidence::Strong,
        &ElementClickPointPolicy::Center,
        ElementOcclusionPolicy::BestEffortReject,
        Some(snapshot),
    )?;
    assert_eq!(
        admitted.occlusion_check(),
        OcclusionCheckResult::Inconclusive
    );
    assert_eq!(
        plan_physical_element_click(
            &observation,
            Some(&fixture.window),
            WindowCorrelationConfidence::Strong,
            &ElementClickPointPolicy::Center,
            ElementOcclusionPolicy::RequireUnoccluded,
            Some(snapshot),
        ),
        Err(ElementClickPlanError::OcclusionInconclusive)
    );
    Ok(())
}

#[test]
fn complete_non_covering_rectangles_are_clear_only_in_the_supplied_model()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let above = [Rect::new(20, 24, 1, 1)?, Rect::new(19, 25, 1, 1)?];
    let admitted = plan_physical_element_click(
        &observation,
        Some(&fixture.window),
        WindowCorrelationConfidence::Strong,
        &ElementClickPointPolicy::Center,
        ElementOcclusionPolicy::RequireUnoccluded,
        Some(ElementClickOcclusionSnapshot {
            target_window: &fixture.window,
            stacking_epoch: 10,
            rectangles_above: &above,
            stacking_complete: true,
        }),
    )?;
    assert_eq!(admitted.occlusion_check(), OcclusionCheckResult::Clear);
    Ok(())
}

#[test]
fn occluder_cap_preserves_known_positive_evidence() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let mut exactly_at_cap = vec![Rect::new(0, 0, 1, 1)?; MAX_ELEMENT_CLICK_OCCLUDERS];

    let clear = plan_physical_element_click(
        &observation,
        Some(&fixture.window),
        WindowCorrelationConfidence::Strong,
        &ElementClickPointPolicy::Center,
        ElementOcclusionPolicy::BestEffortReject,
        Some(ElementClickOcclusionSnapshot {
            target_window: &fixture.window,
            stacking_epoch: 10,
            rectangles_above: &exactly_at_cap,
            stacking_complete: true,
        }),
    )?;
    assert_eq!(clear.occlusion_check(), OcclusionCheckResult::Clear);

    exactly_at_cap[0] = Rect::new(19, 24, 1, 1)?;
    assert_eq!(
        plan_physical_element_click(
            &observation,
            Some(&fixture.window),
            WindowCorrelationConfidence::Strong,
            &ElementClickPointPolicy::Center,
            ElementOcclusionPolicy::BestEffortReject,
            Some(ElementClickOcclusionSnapshot {
                target_window: &fixture.window,
                stacking_epoch: 11,
                rectangles_above: &exactly_at_cap,
                stacking_complete: true,
            }),
        ),
        Err(ElementClickPlanError::Occluded)
    );

    exactly_at_cap.push(Rect::new(0, 0, 1, 1)?);
    assert_eq!(
        plan_physical_element_click(
            &observation,
            Some(&fixture.window),
            WindowCorrelationConfidence::Strong,
            &ElementClickPointPolicy::Center,
            ElementOcclusionPolicy::BestEffortReject,
            Some(ElementClickOcclusionSnapshot {
                target_window: &fixture.window,
                stacking_epoch: 12,
                rectangles_above: &exactly_at_cap,
                stacking_complete: true,
            }),
        ),
        Err(ElementClickPlanError::Occluded)
    );
    Ok(())
}

#[test]
fn actor_read_epochs_must_be_nonzero_and_advance() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut observation = fixture.observation()?;
    observation.read_epoch = 0;
    assert_eq!(
        plan(&fixture, &observation, ElementClickPointPolicy::Center),
        Err(ElementClickPlanError::InvalidReadEpoch)
    );

    observation.read_epoch = 100;
    let admitted = plan(&fixture, &observation, ElementClickPointPolicy::Center)?;
    assert_eq!(admitted.read_epoch_before_queue(), 100);
    assert_eq!(
        revalidate_physical_element_click(&admitted, &observation, None),
        Err(ElementClickPlanError::ObservationNotFresh)
    );

    let mut fresh = observation;
    fresh.read_epoch = 101;
    let result = revalidate_physical_element_click(&admitted, &fresh, None)?;
    assert_eq!(result.revision_after_queue, AccessibilityRevision::new(10)?);
    Ok(())
}

#[test]
fn queue_head_rejects_changed_client_bounds_and_correlation_evidence()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let admitted = plan(&fixture, &observation, ElementClickPointPolicy::Center)?;

    let mut fresh = observation.clone();
    fresh.read_epoch = 101;
    fresh.correlated_client_bounds = None;
    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::GeometryChanged)
    );

    fresh = observation.clone();
    fresh.read_epoch = 101;
    fresh.correlated_client_bounds = Some(Rect::new(0, 0, 99, 100)?);
    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::GeometryChanged)
    );

    fresh = observation.clone();
    fresh.read_epoch = 101;
    fresh.correlation.evidence.clear();
    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::UnauthorizedCorrelation)
    );

    fresh = observation;
    fresh.read_epoch = 101;
    fresh.correlation.conflicting_evidence = true;
    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::UnauthorizedCorrelation)
    );
    Ok(())
}

#[test]
fn queue_head_occlusion_snapshot_is_target_bound_and_fresh()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let admitted = plan_physical_element_click(
        &observation,
        Some(&fixture.window),
        WindowCorrelationConfidence::Strong,
        &ElementClickPointPolicy::Center,
        ElementOcclusionPolicy::BestEffortReject,
        Some(ElementClickOcclusionSnapshot {
            target_window: &fixture.window,
            stacking_epoch: 10,
            rectangles_above: &[],
            stacking_complete: true,
        }),
    )?;
    assert_eq!(admitted.occlusion_epoch_before_queue(), Some(10));
    let mut fresh = observation;
    fresh.read_epoch = 101;

    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::MissingQueueHeadOcclusionSnapshot)
    );
    assert_eq!(
        revalidate_physical_element_click(
            &admitted,
            &fresh,
            Some(ElementClickOcclusionSnapshot {
                target_window: &fixture.window,
                stacking_epoch: 10,
                rectangles_above: &[],
                stacking_complete: true,
            }),
        ),
        Err(ElementClickPlanError::OcclusionSnapshotNotFresh)
    );

    let mut other_window = fixture.window.clone();
    other_window.observed_generation += 1;
    assert_eq!(
        revalidate_physical_element_click(
            &admitted,
            &fresh,
            Some(ElementClickOcclusionSnapshot {
                target_window: &other_window,
                stacking_epoch: 11,
                rectangles_above: &[],
                stacking_complete: true,
            }),
        ),
        Err(ElementClickPlanError::OcclusionTargetChanged)
    );

    let result = revalidate_physical_element_click(
        &admitted,
        &fresh,
        Some(ElementClickOcclusionSnapshot {
            target_window: &fixture.window,
            stacking_epoch: 11,
            rectangles_above: &[],
            stacking_complete: false,
        }),
    )?;
    assert_eq!(result.occlusion_check, OcclusionCheckResult::Inconclusive);
    Ok(())
}

#[test]
fn admission_rejects_invalid_or_cross_wired_occlusion_snapshots()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let mut other_window = fixture.window.clone();
    other_window.observed_generation += 1;

    for (snapshot, expected) in [
        (
            ElementClickOcclusionSnapshot {
                target_window: &fixture.window,
                stacking_epoch: 0,
                rectangles_above: &[],
                stacking_complete: true,
            },
            ElementClickPlanError::InvalidOcclusionEpoch,
        ),
        (
            ElementClickOcclusionSnapshot {
                target_window: &other_window,
                stacking_epoch: 1,
                rectangles_above: &[],
                stacking_complete: true,
            },
            ElementClickPlanError::OcclusionTargetChanged,
        ),
    ] {
        assert_eq!(
            plan_physical_element_click(
                &observation,
                Some(&fixture.window),
                WindowCorrelationConfidence::Strong,
                &ElementClickPointPolicy::Center,
                ElementOcclusionPolicy::BestEffortReject,
                Some(snapshot),
            ),
            Err(expected)
        );
    }
    Ok(())
}

#[test]
fn queue_head_allows_revision_advance_only_when_identity_and_geometry_are_unchanged()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let admitted = plan(&fixture, &observation, ElementClickPointPolicy::Center)?;
    let mut fresh = observation.clone();
    fresh.revision = AccessibilityRevision::new(11)?;
    fresh.read_epoch = 101;

    let result = revalidate_physical_element_click(&admitted, &fresh, None)?;
    assert_eq!(result.revision_after_queue, AccessibilityRevision::new(11)?);
    assert_eq!(result.extents_after_queue, observation.element_extents);
    Ok(())
}

#[test]
fn queue_head_result_reports_fresh_correlation_confidence_not_admission_confidence()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let admitted = plan(&fixture, &observation, ElementClickPointPolicy::Center)?;
    assert_eq!(admitted.correlation(), WindowCorrelationConfidence::Strong);
    let mut fresh = observation;
    fresh.read_epoch = 101;
    fresh.correlation.confidence = WindowCorrelationConfidence::ExactProcess;

    let result = revalidate_physical_element_click(&admitted, &fresh, None)?;
    assert_eq!(
        result.correlation,
        WindowCorrelationConfidence::ExactProcess
    );
    Ok(())
}

#[test]
fn queue_head_rejects_revision_regression_movement_and_bounds_change()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let admitted = plan(&fixture, &observation, ElementClickPointPolicy::Center)?;

    let mut fresh = observation.clone();
    fresh.revision = AccessibilityRevision::new(9)?;
    fresh.read_epoch = 101;
    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::RevisionRegression)
    );

    fresh = observation.clone();
    fresh.read_epoch = 101;
    fresh.element_extents = Rect::new(11, 20, 20, 10)?;
    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::GeometryChanged)
    );

    fresh = observation.clone();
    fresh.read_epoch = 101;
    fresh.root_bounds = Rect::new(-99, -100, 399, 400)?;
    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::GeometryChanged)
    );
    Ok(())
}

#[test]
fn queue_head_rejects_reborn_element_and_window() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let admitted = plan(&fixture, &observation, ElementClickPointPolicy::Center)?;

    let mut fresh = observation.clone();
    fresh.read_epoch = 101;
    fresh.element.cache_sequence += 1;
    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::ElementBirthChanged)
    );

    fresh = observation;
    fresh.read_epoch = 101;
    let mut reborn_window = fixture.window.clone();
    reborn_window.observed_generation += 1;
    fresh.correlation.window = Some(reborn_window);
    assert_eq!(
        revalidate_physical_element_click(&admitted, &fresh, None),
        Err(ElementClickPlanError::WindowBindingChanged)
    );
    Ok(())
}

#[test]
fn queue_head_rechecks_occlusion_before_effect() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let observation = fixture.observation()?;
    let admitted = plan_physical_element_click(
        &observation,
        Some(&fixture.window),
        WindowCorrelationConfidence::Strong,
        &ElementClickPointPolicy::Center,
        ElementOcclusionPolicy::RequireUnoccluded,
        Some(ElementClickOcclusionSnapshot {
            target_window: &fixture.window,
            stacking_epoch: 10,
            rectangles_above: &[],
            stacking_complete: true,
        }),
    )?;
    let covering = [Rect::new(19, 24, 1, 1)?];
    let mut fresh = observation.clone();
    fresh.read_epoch = 101;

    assert_eq!(
        revalidate_physical_element_click(
            &admitted,
            &fresh,
            Some(ElementClickOcclusionSnapshot {
                target_window: &fixture.window,
                stacking_epoch: 11,
                rectangles_above: &covering,
                stacking_complete: true,
            }),
        ),
        Err(ElementClickPlanError::Occluded)
    );
    Ok(())
}
