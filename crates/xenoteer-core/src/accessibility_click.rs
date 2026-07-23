//! Pure admission and queue-head revalidation for physical element clicks.
//!
//! Geometry is expressed in root-physical pixels. Rectangles are interpreted
//! as half-open intervals, while returned points name concrete pixels. This
//! module does not query AT-SPI, X11, a compositor, or input backends; callers
//! must provide fresh actor-owned observations and must still resolve the exact
//! window and element references immediately before applying an input effect.

use thiserror::Error;
use xenoteer_protocol::{
    AccessibilityRevision, ElementClickPointPolicy, ElementOcclusionPolicy, ElementRef,
    ElementWindowCorrelation, OcclusionCheckResult, Point, Rect, WindowCorrelationConfidence,
    WindowRef,
};

use crate::correlation_authorizes_physical_effect;

/// Maximum number of higher-stacking rectangles examined for one click.
pub const MAX_ELEMENT_CLICK_OCCLUDERS: usize = 256;

/// Fresh geometry and correlation evidence for one exact accessible birth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementClickObservation {
    /// Exact element incarnation observed by the accessibility actor.
    pub element: ElementRef,
    /// Accessibility cache revision containing this observation.
    pub revision: AccessibilityRevision,
    /// Nonzero actor-issued epoch for this concrete accessibility read.
    ///
    /// Unlike the cache revision, this advances for every actor-owned read even
    /// when no accessibility event changed the cache.
    pub read_epoch: u64,
    /// Element component extents in root-physical pixels.
    pub element_extents: Rect,
    /// Current root-window bounds in root-physical pixels.
    pub root_bounds: Rect,
    /// Optional exact correlated client bounds in root-physical pixels.
    pub correlated_client_bounds: Option<Rect>,
    /// Fresh correlation to an exact live X11 window birth.
    pub correlation: ElementWindowCorrelation,
}

/// Bounded, best-effort stacking evidence supplied by the window actor.
///
/// A rectangle containing the click point is conservatively treated as a
/// potential occluder regardless of transparency or non-rectangular shape.
/// `Clear` means only that a complete supplied rectangle list did not contain
/// the point; it is not a compositor-level visibility claim.
#[derive(Debug, Clone, Copy)]
pub struct ElementClickOcclusionSnapshot<'a> {
    /// Exact target window birth whose higher-stacking rectangles were read.
    pub target_window: &'a WindowRef,
    /// Nonzero actor-issued epoch for this concrete stacking read.
    pub stacking_epoch: u64,
    /// Rectangular root-physical bounds for windows known to stack above the target.
    pub rectangles_above: &'a [Rect],
    /// Whether the supplied list is complete for the current stacking snapshot.
    pub stacking_complete: bool,
}

/// Geometry retained so queue-head revalidation can detect any movement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementClickGeometryEvidence {
    /// Root-physical element extents admitted before queueing.
    pub element_extents: Rect,
    /// Root bounds used to clip the element.
    pub root_bounds: Rect,
    /// Correlated client bounds used for optional additional clipping.
    pub correlated_client_bounds: Option<Rect>,
    /// Non-empty intersection eligible for point selection.
    pub visible_bounds: Rect,
}

/// Compact, side-effect-free plan produced at command admission.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalElementClickPlan {
    /// Exact accessibility birth authorized by the plan.
    element: ElementRef,
    /// Exact X11 window birth bound to the element.
    window: WindowRef,
    /// Correlation confidence at admission.
    correlation: WindowCorrelationConfidence,
    /// Caller-requested minimum confidence retained for queue-head enforcement.
    minimum_correlation: WindowCorrelationConfidence,
    /// Accessibility revision at admission.
    revision_before_queue: AccessibilityRevision,
    /// Accessibility actor read epoch at admission.
    read_epoch_before_queue: u64,
    /// Exact caller target, when the command supplied one.
    explicit_window: Option<WindowRef>,
    /// Point policy retained for deterministic revalidation.
    point_policy: ElementClickPointPolicy,
    /// Occlusion policy retained so queue-head checks cannot be downgraded.
    occlusion_policy: ElementOcclusionPolicy,
    /// Root-physical pixel selected for the pointer movement.
    click_point: Point,
    /// Geometry retained for exact queue-head comparison.
    geometry: ElementClickGeometryEvidence,
    /// Admission-time bounded occlusion outcome.
    occlusion_check: OcclusionCheckResult,
    /// Stacking actor read epoch at admission, when evidence was supplied.
    occlusion_epoch_before_queue: Option<u64>,
}

impl PhysicalElementClickPlan {
    /// Returns the exact accessibility birth authorized at admission.
    #[must_use]
    pub fn element(&self) -> &ElementRef {
        &self.element
    }

    /// Returns the exact X11 window birth authorized at admission.
    #[must_use]
    pub fn window(&self) -> &WindowRef {
        &self.window
    }

    /// Returns the admission correlation confidence.
    #[must_use]
    pub const fn correlation(&self) -> WindowCorrelationConfidence {
        self.correlation
    }

    /// Returns the caller-requested minimum correlation confidence.
    #[must_use]
    pub const fn minimum_correlation(&self) -> WindowCorrelationConfidence {
        self.minimum_correlation
    }

    /// Returns the accessibility cache revision at admission.
    #[must_use]
    pub const fn revision_before_queue(&self) -> AccessibilityRevision {
        self.revision_before_queue
    }

    /// Returns the accessibility actor read epoch at admission.
    #[must_use]
    pub const fn read_epoch_before_queue(&self) -> u64 {
        self.read_epoch_before_queue
    }

    /// Returns the exact caller target, when one was supplied.
    #[must_use]
    pub fn explicit_window(&self) -> Option<&WindowRef> {
        self.explicit_window.as_ref()
    }

    /// Returns the retained deterministic point-selection policy.
    #[must_use]
    pub fn point_policy(&self) -> &ElementClickPointPolicy {
        &self.point_policy
    }

    /// Returns the retained occlusion policy.
    #[must_use]
    pub const fn occlusion_policy(&self) -> ElementOcclusionPolicy {
        self.occlusion_policy
    }

    /// Returns the selected root-physical click pixel.
    #[must_use]
    pub const fn click_point(&self) -> Point {
        self.click_point
    }

    /// Returns the immutable geometry evidence retained at admission.
    #[must_use]
    pub const fn geometry(&self) -> &ElementClickGeometryEvidence {
        &self.geometry
    }

    /// Returns the bounded admission-time occlusion outcome.
    #[must_use]
    pub const fn occlusion_check(&self) -> OcclusionCheckResult {
        self.occlusion_check
    }

    /// Returns the stacking read epoch at admission, when one was supplied.
    #[must_use]
    pub const fn occlusion_epoch_before_queue(&self) -> Option<u64> {
        self.occlusion_epoch_before_queue
    }
}

/// Queue-head evidence safe to copy into the physical-click result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevalidatedElementClick {
    /// Fresh queue-head correlation confidence that authorized this effect.
    pub correlation: WindowCorrelationConfidence,
    /// Fresh accessibility revision observed at the queue head.
    pub revision_after_queue: AccessibilityRevision,
    /// Fresh element extents, proven identical to admission extents.
    pub extents_after_queue: Rect,
    /// Queue-head bounded occlusion outcome.
    pub occlusion_check: OcclusionCheckResult,
}

/// Closed failures from admission or queue-head revalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ElementClickPlanError {
    /// The exact element reference failed shape or scope validation.
    #[error("the exact element reference is malformed")]
    InvalidElementReference,
    /// The correlated or explicit window reference failed shape validation.
    #[error("the exact window reference is malformed")]
    InvalidWindowReference,
    /// An otherwise well-formed window is outside the element's desktop lifetime.
    #[error("the element and window references belong to different desktop lifetimes")]
    ReferenceScope,
    /// Correlation confidence and independent evidence cannot gate input.
    #[error("the element-to-window correlation cannot authorize a physical effect")]
    UnauthorizedCorrelation,
    /// The requested minimum confidence is invalid without an exact explicit target.
    #[error("the requested minimum correlation is invalid for this click")]
    InvalidMinimumCorrelation,
    /// Fresh correlation confidence is below the caller-requested minimum.
    #[error("element-to-window correlation is below the requested minimum")]
    CorrelationBelowMinimum,
    /// Exact correlation, explicit target, or queue-head target disagree.
    #[error("the explicit or revalidated window differs from the correlated exact birth")]
    WindowBindingChanged,
    /// At least one root-physical rectangle is empty or overflows.
    #[error("root-physical click geometry is invalid")]
    InvalidGeometry,
    /// Clipping removed the entire element rectangle.
    #[error("the element has no visible pixel in the permitted root/client bounds")]
    NotVisible,
    /// The requested inset left no concrete pixel.
    #[error("the inset removes every eligible click pixel")]
    InsetExhausted,
    /// Adding an element-relative offset produced no representable point.
    #[error("the element-relative offset is not a representable root-physical point")]
    PointOverflow,
    /// The chosen offset names a clipped or offscreen pixel.
    #[error("the selected point is outside the permitted visible bounds")]
    PointOutsideVisibleBounds,
    /// A known higher-stacking rectangle contains the click pixel.
    #[error("a supplied higher-stacking rectangle may occlude the selected point")]
    Occluded,
    /// Required complete evidence was absent, incomplete, or over budget.
    #[error("complete bounded occlusion evidence is required but unavailable")]
    OcclusionInconclusive,
    /// Exact accessibility identity differed at the queue head.
    #[error("the exact element generation or birth changed while queued")]
    ElementBirthChanged,
    /// The queue-head cache revision is older than the admitted revision.
    #[error("the accessibility revision regressed while queued")]
    RevisionRegression,
    /// An accessibility observation used a reserved zero read epoch.
    #[error("the accessibility read epoch must be nonzero")]
    InvalidReadEpoch,
    /// The queue-head accessibility observation was not a later actor read.
    #[error("the accessibility observation was not refreshed at the queue head")]
    ObservationNotFresh,
    /// Element, root, client, derived visible geometry, or point changed.
    #[error("click-relevant geometry changed while queued")]
    GeometryChanged,
    /// An occlusion snapshot used a reserved zero stacking epoch.
    #[error("the stacking read epoch must be nonzero")]
    InvalidOcclusionEpoch,
    /// Occlusion evidence was read for a different exact target birth.
    #[error("the occlusion snapshot target differs from the correlated exact window")]
    OcclusionTargetChanged,
    /// A checking policy did not receive queue-head stacking evidence.
    #[error("queue-head stacking evidence is required for this occlusion policy")]
    MissingQueueHeadOcclusionSnapshot,
    /// The queue-head stacking snapshot was not a later actor read.
    #[error("the stacking snapshot was not refreshed at the queue head")]
    OcclusionSnapshotNotFresh,
}

/// Produces a bounded physical-click plan without applying an input effect.
///
/// `Offset` is relative to the original element origin, not its clipped visible
/// origin. `Center` and `InsetCenter` operate on the visible intersection and
/// choose the upper-left of the two middle pixels on an even axis.
pub fn plan_physical_element_click(
    observation: &ElementClickObservation,
    explicit_window: Option<&WindowRef>,
    minimum_correlation: WindowCorrelationConfidence,
    point_policy: &ElementClickPointPolicy,
    occlusion_policy: ElementOcclusionPolicy,
    occlusion: Option<ElementClickOcclusionSnapshot<'_>>,
) -> Result<PhysicalElementClickPlan, ElementClickPlanError> {
    validate_read_epoch(observation.read_epoch)?;
    validate_minimum_request(minimum_correlation, explicit_window)?;
    let window = validate_binding(observation, explicit_window)?;
    require_minimum_confidence(observation.correlation.confidence, minimum_correlation)?;
    let geometry = derive_geometry(observation)?;
    let click_point = select_point(
        point_policy,
        observation.element_extents,
        geometry.visible_bounds,
    )?;
    let occlusion_check = evaluate_occlusion(
        click_point,
        occlusion_policy,
        occlusion,
        &window,
        None,
        false,
    )?;

    Ok(PhysicalElementClickPlan {
        element: observation.element.clone(),
        window,
        correlation: observation.correlation.confidence,
        minimum_correlation,
        revision_before_queue: observation.revision,
        read_epoch_before_queue: observation.read_epoch,
        explicit_window: explicit_window.cloned(),
        point_policy: point_policy.clone(),
        occlusion_policy,
        click_point,
        geometry,
        occlusion_check,
        occlusion_epoch_before_queue: occlusion.map(|snapshot| snapshot.stacking_epoch),
    })
}

/// Revalidates an admitted plan against a fresh queue-head observation.
///
/// Success proves that the exact element birth and window binding remain
/// authorized, the revision did not regress, and every click-relevant geometry
/// value is byte-for-byte unchanged. The accessibility read epoch must strictly
/// advance. For a checking occlusion policy, the queue-head stacking read is
/// mandatory, must name the exact target birth, and must strictly advance beyond
/// any admission stacking epoch. The caller must perform this check immediately
/// before the first input effect.
pub fn revalidate_physical_element_click(
    plan: &PhysicalElementClickPlan,
    observation: &ElementClickObservation,
    occlusion: Option<ElementClickOcclusionSnapshot<'_>>,
) -> Result<RevalidatedElementClick, ElementClickPlanError> {
    validate_read_epoch(observation.read_epoch)?;
    if observation.element != plan.element {
        return Err(ElementClickPlanError::ElementBirthChanged);
    }
    if observation.revision < plan.revision_before_queue {
        return Err(ElementClickPlanError::RevisionRegression);
    }
    if observation.read_epoch <= plan.read_epoch_before_queue {
        return Err(ElementClickPlanError::ObservationNotFresh);
    }

    let window = validate_binding(observation, plan.explicit_window.as_ref())?;
    if window != plan.window {
        return Err(ElementClickPlanError::WindowBindingChanged);
    }
    require_minimum_confidence(observation.correlation.confidence, plan.minimum_correlation)?;

    let geometry = derive_geometry(observation)?;
    if geometry != plan.geometry {
        return Err(ElementClickPlanError::GeometryChanged);
    }
    let click_point = select_point(
        &plan.point_policy,
        observation.element_extents,
        geometry.visible_bounds,
    )?;
    if click_point != plan.click_point {
        return Err(ElementClickPlanError::GeometryChanged);
    }
    let occlusion_check = evaluate_occlusion(
        click_point,
        plan.occlusion_policy,
        occlusion,
        &plan.window,
        plan.occlusion_epoch_before_queue,
        plan.occlusion_policy != ElementOcclusionPolicy::Ignore,
    )?;

    Ok(RevalidatedElementClick {
        correlation: observation.correlation.confidence,
        revision_after_queue: observation.revision,
        extents_after_queue: observation.element_extents,
        occlusion_check,
    })
}

fn validate_read_epoch(read_epoch: u64) -> Result<(), ElementClickPlanError> {
    if read_epoch == 0 {
        return Err(ElementClickPlanError::InvalidReadEpoch);
    }
    Ok(())
}

fn validate_minimum_request(
    minimum: WindowCorrelationConfidence,
    explicit_window: Option<&WindowRef>,
) -> Result<(), ElementClickPlanError> {
    if minimum == WindowCorrelationConfidence::None
        || (minimum == WindowCorrelationConfidence::Weak && explicit_window.is_none())
    {
        return Err(ElementClickPlanError::InvalidMinimumCorrelation);
    }
    Ok(())
}

fn require_minimum_confidence(
    actual: WindowCorrelationConfidence,
    minimum: WindowCorrelationConfidence,
) -> Result<(), ElementClickPlanError> {
    let meets = match minimum {
        WindowCorrelationConfidence::None => false,
        WindowCorrelationConfidence::Weak => actual != WindowCorrelationConfidence::None,
        WindowCorrelationConfidence::Strong => matches!(
            actual,
            WindowCorrelationConfidence::Strong | WindowCorrelationConfidence::ExactProcess
        ),
        WindowCorrelationConfidence::ExactProcess => {
            actual == WindowCorrelationConfidence::ExactProcess
        }
    };
    if !meets {
        return Err(ElementClickPlanError::CorrelationBelowMinimum);
    }
    Ok(())
}

fn validate_binding(
    observation: &ElementClickObservation,
    explicit_window: Option<&WindowRef>,
) -> Result<WindowRef, ElementClickPlanError> {
    observation
        .element
        .validate()
        .map_err(|_| ElementClickPlanError::InvalidElementReference)?;
    if !correlation_authorizes_physical_effect(&observation.correlation) {
        return Err(ElementClickPlanError::UnauthorizedCorrelation);
    }
    let window = observation
        .correlation
        .window
        .as_ref()
        .ok_or(ElementClickPlanError::UnauthorizedCorrelation)?;
    window
        .validate_shape()
        .map_err(|_| ElementClickPlanError::InvalidWindowReference)?;
    if window.desktop_id != observation.element.desktop_id
        || window.desktop_generation != observation.element.desktop_generation
    {
        return Err(ElementClickPlanError::ReferenceScope);
    }
    if let Some(explicit) = explicit_window {
        explicit
            .validate_shape()
            .map_err(|_| ElementClickPlanError::InvalidWindowReference)?;
        if explicit.desktop_id != observation.element.desktop_id
            || explicit.desktop_generation != observation.element.desktop_generation
        {
            return Err(ElementClickPlanError::ReferenceScope);
        }
        if explicit != window {
            return Err(ElementClickPlanError::WindowBindingChanged);
        }
    }
    Ok(window.clone())
}

fn derive_geometry(
    observation: &ElementClickObservation,
) -> Result<ElementClickGeometryEvidence, ElementClickPlanError> {
    validate_rect(observation.element_extents)?;
    validate_rect(observation.root_bounds)?;
    if let Some(client_bounds) = observation.correlated_client_bounds {
        validate_rect(client_bounds)?;
    }

    let mut visible = intersect(observation.element_extents, observation.root_bounds)?
        .ok_or(ElementClickPlanError::NotVisible)?;
    if let Some(client_bounds) = observation.correlated_client_bounds {
        visible = intersect(visible, client_bounds)?.ok_or(ElementClickPlanError::NotVisible)?;
    }
    Ok(ElementClickGeometryEvidence {
        element_extents: observation.element_extents,
        root_bounds: observation.root_bounds,
        correlated_client_bounds: observation.correlated_client_bounds,
        visible_bounds: visible,
    })
}

fn validate_rect(rect: Rect) -> Result<(), ElementClickPlanError> {
    rect.validate()
        .map_err(|_| ElementClickPlanError::InvalidGeometry)
}

fn intersect(left: Rect, right: Rect) -> Result<Option<Rect>, ElementClickPlanError> {
    let (left_x1, left_y1, left_x2, left_y2) = edges(left)?;
    let (right_x1, right_y1, right_x2, right_y2) = edges(right)?;
    let x1 = left_x1.max(right_x1);
    let y1 = left_y1.max(right_y1);
    let x2 = left_x2.min(right_x2);
    let y2 = left_y2.min(right_y2);
    if x1 >= x2 || y1 >= y2 {
        return Ok(None);
    }
    let x = i32::try_from(x1).map_err(|_| ElementClickPlanError::InvalidGeometry)?;
    let y = i32::try_from(y1).map_err(|_| ElementClickPlanError::InvalidGeometry)?;
    let width = u32::try_from(x2 - x1).map_err(|_| ElementClickPlanError::InvalidGeometry)?;
    let height = u32::try_from(y2 - y1).map_err(|_| ElementClickPlanError::InvalidGeometry)?;
    Rect::new(x, y, width, height)
        .map(Some)
        .map_err(|_| ElementClickPlanError::InvalidGeometry)
}

fn edges(rect: Rect) -> Result<(i64, i64, i64, i64), ElementClickPlanError> {
    validate_rect(rect)?;
    let origin = rect.origin();
    let size = rect
        .size()
        .map_err(|_| ElementClickPlanError::InvalidGeometry)?;
    let x1 = i64::from(origin.x());
    let y1 = i64::from(origin.y());
    let x2 = x1
        .checked_add(i64::from(size.width()))
        .ok_or(ElementClickPlanError::InvalidGeometry)?;
    let y2 = y1
        .checked_add(i64::from(size.height()))
        .ok_or(ElementClickPlanError::InvalidGeometry)?;
    Ok((x1, y1, x2, y2))
}

fn select_point(
    policy: &ElementClickPointPolicy,
    element_extents: Rect,
    visible_bounds: Rect,
) -> Result<Point, ElementClickPlanError> {
    let (visible_x1, visible_y1, visible_x2, visible_y2) = edges(visible_bounds)?;
    let (element_x1, element_y1, element_x2, element_y2) = edges(element_extents)?;
    let (x, y) = match policy {
        ElementClickPointPolicy::Center => {
            centered_pixel(visible_x1, visible_y1, visible_x2, visible_y2)
        }
        ElementClickPointPolicy::InsetCenter { inset_pixels } => {
            let inset = i64::from(*inset_pixels);
            let x1 = visible_x1 + inset;
            let y1 = visible_y1 + inset;
            let x2 = visible_x2 - inset;
            let y2 = visible_y2 - inset;
            if x1 >= x2 || y1 >= y2 {
                return Err(ElementClickPlanError::InsetExhausted);
            }
            centered_pixel(x1, y1, x2, y2)
        }
        ElementClickPointPolicy::Offset { offset } => {
            let x = element_x1
                .checked_add(i64::from(offset.x()))
                .ok_or(ElementClickPlanError::PointOverflow)?;
            let y = element_y1
                .checked_add(i64::from(offset.y()))
                .ok_or(ElementClickPlanError::PointOverflow)?;
            if i32::try_from(x).is_err() || i32::try_from(y).is_err() {
                return Err(ElementClickPlanError::PointOverflow);
            }
            (x, y)
        }
        ElementClickPointPolicy::NearestVisible => {
            let (center_x, center_y) =
                centered_pixel(element_x1, element_y1, element_x2, element_y2);
            (
                center_x.clamp(visible_x1, visible_x2 - 1),
                center_y.clamp(visible_y1, visible_y2 - 1),
            )
        }
    };

    if x < visible_x1 || x >= visible_x2 || y < visible_y1 || y >= visible_y2 {
        return Err(ElementClickPlanError::PointOutsideVisibleBounds);
    }
    Ok(Point::new(
        i32::try_from(x).map_err(|_| ElementClickPlanError::PointOverflow)?,
        i32::try_from(y).map_err(|_| ElementClickPlanError::PointOverflow)?,
    ))
}

fn centered_pixel(x1: i64, y1: i64, x2: i64, y2: i64) -> (i64, i64) {
    (x1 + (x2 - x1 - 1) / 2, y1 + (y2 - y1 - 1) / 2)
}

fn evaluate_occlusion(
    point: Point,
    policy: ElementOcclusionPolicy,
    snapshot: Option<ElementClickOcclusionSnapshot<'_>>,
    expected_window: &WindowRef,
    epoch_before_queue: Option<u64>,
    require_snapshot: bool,
) -> Result<OcclusionCheckResult, ElementClickPlanError> {
    if policy == ElementOcclusionPolicy::Ignore {
        return Ok(OcclusionCheckResult::NotRequested);
    }

    if require_snapshot && snapshot.is_none() {
        return Err(ElementClickPlanError::MissingQueueHeadOcclusionSnapshot);
    }

    let result = match snapshot {
        None => OcclusionCheckResult::Inconclusive,
        Some(snapshot) => {
            if snapshot.target_window != expected_window {
                return Err(ElementClickPlanError::OcclusionTargetChanged);
            }
            if snapshot.stacking_epoch == 0 {
                return Err(ElementClickPlanError::InvalidOcclusionEpoch);
            }
            if epoch_before_queue.is_some_and(|before| snapshot.stacking_epoch <= before) {
                return Err(ElementClickPlanError::OcclusionSnapshotNotFresh);
            }
            let examined_len = snapshot
                .rectangles_above
                .len()
                .min(MAX_ELEMENT_CLICK_OCCLUDERS);
            let examined = &snapshot.rectangles_above[..examined_len];
            for rectangle in examined {
                validate_rect(*rectangle)?;
            }
            if examined.iter().any(|rectangle| contains(*rectangle, point)) {
                OcclusionCheckResult::Occluded
            } else if snapshot.rectangles_above.len() > MAX_ELEMENT_CLICK_OCCLUDERS {
                OcclusionCheckResult::Inconclusive
            } else if snapshot.stacking_complete {
                OcclusionCheckResult::Clear
            } else {
                OcclusionCheckResult::Inconclusive
            }
        }
    };

    match (policy, result) {
        (_, OcclusionCheckResult::Occluded) => Err(ElementClickPlanError::Occluded),
        (ElementOcclusionPolicy::RequireUnoccluded, OcclusionCheckResult::Inconclusive) => {
            Err(ElementClickPlanError::OcclusionInconclusive)
        }
        _ => Ok(result),
    }
}

fn contains(rect: Rect, point: Point) -> bool {
    let origin = rect.origin();
    let Ok(size) = rect.size() else {
        return false;
    };
    let x = i64::from(point.x());
    let y = i64::from(point.y());
    let x1 = i64::from(origin.x());
    let y1 = i64::from(origin.y());
    x >= x1 && x < x1 + i64::from(size.width()) && y >= y1 && y < y1 + i64::from(size.height())
}
