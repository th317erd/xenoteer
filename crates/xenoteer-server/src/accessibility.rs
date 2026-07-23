//! Authenticated, generation-fenced accessibility observation transport.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use thiserror::Error;
use xenoteer_protocol::{
    AtspiGeneration, DesktopGeneration, DesktopId, ElementListPage, ElementListRequest,
    ElementOrder, ElementPredicate, ElementQueryPage, ElementQueryRequest, ElementRef,
    ElementResolveRequest, ElementResolveResult, ElementScope, ElementSelector, ElementSnapshot,
    ElementSnapshotExpansion, ElementSnapshotRequest, ElementSnapshotResult, ElementStringMatch,
    ElementWaitPredicate, ElementWaitQuantifier, ElementWaitRequest, ElementWaitResult,
    ElementWaitStatus, ElementWaitTarget, Rect, RequestId,
};

use crate::{
    ApiState,
    auth::{Grant, Principal},
    control::ControlRequestContext,
    problem::ApiProblem,
};

/// Boxed future used by the object-safe accessibility boundary.
pub type AccessibilityFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Stable, content-free failures produced by an accessibility read adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AccessibilityPlaneError {
    /// The typed request failed adapter-level validation.
    #[error("invalid accessibility request")]
    InvalidRequest,
    /// The authenticated principal cannot read this resource.
    #[error("accessibility permission denied")]
    PermissionDenied,
    /// The exact application or element does not exist or may not be disclosed.
    #[error("accessibility target not found")]
    NotFound,
    /// A desktop, AT-SPI, application, or element generation fence no longer matches.
    #[error("accessibility reference is stale")]
    StaleReference {
        /// Current desktop generation when it can be disclosed safely.
        current_generation: Option<DesktopGeneration>,
    },
    /// Event/cache completeness was lost and authoritative state must be refreshed.
    #[error("accessibility state requires resynchronization")]
    ResyncRequired {
        /// Current desktop generation when it can be disclosed safely.
        current_generation: Option<DesktopGeneration>,
    },
    /// A single-target operation matched more than one element.
    #[error("accessibility target is ambiguous")]
    AmbiguousTarget,
    /// Caller-selected or server-enforced traversal/encoding limits were reached.
    #[error("accessibility query limit exceeded")]
    QueryLimitExceeded,
    /// A bounded queue or concurrent-query quota is currently full.
    #[error("accessibility resources are exhausted")]
    ResourceExhausted,
    /// AT-SPI is disabled, disconnected, rebuilding, or otherwise unavailable.
    #[error("accessibility capability is unavailable")]
    CapabilityUnavailable,
    /// The target application does not expose the requested semantic data.
    #[error("accessibility operation is unsupported by the target")]
    UnsupportedByTarget,
    /// A private invariant failed; details must remain server-side.
    #[error("internal accessibility failure")]
    Internal,
}

/// Read-only seam between HTTP handlers and the single-owner accessibility actor.
///
/// Implementations are a trusted topology boundary: every returned snapshot must
/// belong to the request's exact scope. This matters for `Window` and `Subtree`
/// scopes because a response snapshot carries only its immediate parent and its
/// own optional window correlation, not a complete ancestry proof. The HTTP
/// adapter defensively checks every scope fact present in the response, but it
/// deliberately does not chase toolkit-controlled parent references or repeat
/// the actor's bounded graph traversal.
pub trait AccessibilityPlane: Send + Sync + 'static {
    /// Returns one bounded page from an atomic accessibility revision.
    fn list_elements<'a>(
        &'a self,
        context: ControlRequestContext,
        request: ElementListRequest,
    ) -> AccessibilityFuture<'a, Result<ElementListPage, AccessibilityPlaneError>>;

    /// Evaluates one bounded deterministic selector.
    fn query_elements<'a>(
        &'a self,
        context: ControlRequestContext,
        request: ElementQueryRequest,
    ) -> AccessibilityFuture<'a, Result<ElementQueryPage, AccessibilityPlaneError>>;

    /// Resolves a selector only after proving that its bounded result is exact-one.
    fn resolve_element<'a>(
        &'a self,
        context: ControlRequestContext,
        request: ElementResolveRequest,
    ) -> AccessibilityFuture<'a, Result<ElementResolveResult, AccessibilityPlaneError>>;

    /// Refreshes one exact generation- and identity-fenced element reference.
    fn element_snapshot<'a>(
        &'a self,
        context: ControlRequestContext,
        request: ElementSnapshotRequest,
    ) -> AccessibilityFuture<'a, Result<ElementSnapshotResult, AccessibilityPlaneError>>;

    /// Atomically snapshots, subscribes, and rechecks one bounded wait.
    fn wait_element<'a>(
        &'a self,
        context: ControlRequestContext,
        request: ElementWaitRequest,
    ) -> AccessibilityFuture<'a, Result<ElementWaitResult, AccessibilityPlaneError>>;
}

pub(crate) type SharedAccessibilityPlane = Arc<dyn AccessibilityPlane>;

#[derive(Clone)]
struct AccessibilityPlaneState(SharedAccessibilityPlane);

#[derive(Debug)]
pub(crate) struct UnavailableAccessibilityPlane;

impl AccessibilityPlane for UnavailableAccessibilityPlane {
    fn list_elements<'a>(
        &'a self,
        _: ControlRequestContext,
        _: ElementListRequest,
    ) -> AccessibilityFuture<'a, Result<ElementListPage, AccessibilityPlaneError>> {
        unavailable()
    }

    fn query_elements<'a>(
        &'a self,
        _: ControlRequestContext,
        _: ElementQueryRequest,
    ) -> AccessibilityFuture<'a, Result<ElementQueryPage, AccessibilityPlaneError>> {
        unavailable()
    }

    fn resolve_element<'a>(
        &'a self,
        _: ControlRequestContext,
        _: ElementResolveRequest,
    ) -> AccessibilityFuture<'a, Result<ElementResolveResult, AccessibilityPlaneError>> {
        unavailable()
    }

    fn element_snapshot<'a>(
        &'a self,
        _: ControlRequestContext,
        _: ElementSnapshotRequest,
    ) -> AccessibilityFuture<'a, Result<ElementSnapshotResult, AccessibilityPlaneError>> {
        unavailable()
    }

    fn wait_element<'a>(
        &'a self,
        _: ControlRequestContext,
        _: ElementWaitRequest,
    ) -> AccessibilityFuture<'a, Result<ElementWaitResult, AccessibilityPlaneError>> {
        unavailable()
    }
}

fn unavailable<'a, T>() -> AccessibilityFuture<'a, Result<T, AccessibilityPlaneError>> {
    Box::pin(async { Err(AccessibilityPlaneError::CapabilityUnavailable) })
}

pub(crate) fn routes(plane: SharedAccessibilityPlane) -> Router<ApiState> {
    Router::new()
        .route(
            "/v1/desktops/{desktop_id}/accessibility/elements/list",
            post(list_elements),
        )
        .route(
            "/v1/desktops/{desktop_id}/accessibility/elements/query",
            post(query_elements),
        )
        .route(
            "/v1/desktops/{desktop_id}/accessibility/elements/resolve",
            post(resolve_element),
        )
        .route(
            "/v1/desktops/{desktop_id}/accessibility/elements/snapshot",
            post(element_snapshot),
        )
        .route(
            "/v1/desktops/{desktop_id}/accessibility/elements/wait",
            post(wait_element),
        )
        .layer(Extension(AccessibilityPlaneState(plane)))
}

async fn list_elements(
    State(state): State<ApiState>,
    Extension(plane): Extension<AccessibilityPlaneState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    body: Result<Json<ElementListRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::AccessibilityRead) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Json(request))) = (path, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate().is_ok(),
        request_id,
    ) {
        return problem.into_response();
    }
    let expected_generation = request.desktop_generation;
    let expected_atspi_generation = scope_atspi_generation(&request.scope);
    let expected_order = request.order;
    let expected_limit = usize::from(request.limit);
    let expected_visited = request.limits.max_visited_nodes;
    let expected_expansion = request.expansion;
    let expected_scope = request.scope.clone();
    match plane
        .0
        .list_elements(context(principal, request_id), request)
        .await
    {
        Ok(page)
            if page.validate().is_ok()
                && page.desktop_id == desktop_id
                && page.desktop_generation == expected_generation
                && page.order == expected_order
                && page.elements.len() <= expected_limit
                && page.visited_nodes <= expected_visited
                && page.visited_nodes >= page.elements.len() as u32
                && page_order_matches(&page, expected_order)
                && expected_atspi_generation
                    .is_none_or(|generation| page.atspi_generation == generation)
                && page.elements.iter().all(|entry| {
                    snapshot_matches_expansion(&entry.snapshot, expected_expansion)
                        && snapshot_matches_scope(&entry.snapshot, &expected_scope)
                }) =>
        {
            json_no_store(page)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => accessibility_problem(error, request_id).into_response(),
    }
}

async fn query_elements(
    State(state): State<ApiState>,
    Extension(plane): Extension<AccessibilityPlaneState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    body: Result<Json<ElementQueryRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::AccessibilityRead) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Json(request))) = (path, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate().is_ok(),
        request_id,
    ) {
        return problem.into_response();
    }
    let expected_generation = request.desktop_generation;
    let expected_order = request.selector.order;
    let expected_limit = usize::from(request.limit);
    let expected_visited = request.limits.max_visited_nodes;
    let expected_expansion = request.expansion;
    let expected_selector = request.selector.clone();
    match plane
        .0
        .query_elements(context(principal, request_id), request)
        .await
    {
        Ok(page)
            if page.validate().is_ok()
                && page.desktop_id == desktop_id
                && page.desktop_generation == expected_generation
                && page.order == expected_order
                && page.elements.len() <= expected_limit
                && page.visited_nodes <= expected_visited
                && page.visited_nodes >= page.elements.len() as u32
                && page_order_matches(&page, expected_order)
                && selector_atspi_generation_matches(page.atspi_generation, &expected_selector)
                && page.elements.iter().all(|entry| {
                    snapshot_matches_expansion(&entry.snapshot, expected_expansion)
                        && snapshot_matches_selector(
                            &entry.snapshot,
                            &expected_selector,
                            expected_expansion,
                        )
                }) =>
        {
            json_no_store(page)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => accessibility_problem(error, request_id).into_response(),
    }
}

async fn resolve_element(
    State(state): State<ApiState>,
    Extension(plane): Extension<AccessibilityPlaneState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    body: Result<Json<ElementResolveRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::AccessibilityRead) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Json(request))) = (path, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate().is_ok(),
        request_id,
    ) {
        return problem.into_response();
    }
    let expected_generation = request.desktop_generation;
    let expected_selector = request.selector.clone();
    let expected_expansion = request.expansion;
    match plane
        .0
        .resolve_element(context(principal, request_id), request)
        .await
    {
        Ok(result)
            if result.validate().is_ok()
                && result.desktop_id == desktop_id
                && result.desktop_generation == expected_generation
                && selector_atspi_generation_matches(
                    result.atspi_generation,
                    &expected_selector,
                )
                && snapshot_matches_expansion(&result.element.snapshot, expected_expansion)
                && snapshot_matches_selector(
                    &result.element.snapshot,
                    &expected_selector,
                    expected_expansion,
                ) =>
        {
            json_no_store(result)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => accessibility_problem(error, request_id).into_response(),
    }
}

async fn element_snapshot(
    State(state): State<ApiState>,
    Extension(plane): Extension<AccessibilityPlaneState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    body: Result<Json<ElementSnapshotRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::AccessibilityRead) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Json(request))) = (path, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate().is_ok(),
        request_id,
    ) {
        return problem.into_response();
    }
    let expected_generation = request.desktop_generation;
    let expected_atspi_generation = request.element.atspi_generation;
    let expected_element = request.element.clone();
    let expected_expansion = request.expansion;
    match plane
        .0
        .element_snapshot(context(principal, request_id), request)
        .await
    {
        Ok(result)
            if result.validate().is_ok()
                && result.element.snapshot.element.desktop_id == desktop_id
                && result.element.snapshot.element.desktop_generation == expected_generation
                && result.element.snapshot.element.atspi_generation
                    == expected_atspi_generation
                && same_element_identity(&result.element.snapshot.element, &expected_element)
                && snapshot_matches_expansion(&result.element.snapshot, expected_expansion) =>
        {
            json_no_store(result)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => accessibility_problem(error, request_id).into_response(),
    }
}

async fn wait_element(
    State(state): State<ApiState>,
    Extension(plane): Extension<AccessibilityPlaneState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<DesktopId>, axum::extract::rejection::PathRejection>,
    body: Result<Json<ElementWaitRequest>, JsonRejection>,
) -> Response {
    if !principal.has_grant(Grant::AccessibilityRead) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(desktop_id)), Ok(Json(request))) = (path, body) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if let Err(problem) = validate_request(
        &state,
        desktop_id,
        request.desktop_id,
        request.desktop_generation,
        request.validate().is_ok(),
        request_id,
    ) {
        return problem.into_response();
    }
    let expected_reference = match &request.target {
        ElementWaitTarget::Reference { element } => Some(element.clone()),
        ElementWaitTarget::Selector { .. } => None,
    };
    let expected_matches = u32::from(request.limits.max_matches);
    let expected_expansion = request.expansion;
    let expected_target = request.target.clone();
    let expected_predicate = request.predicate.clone();
    let expected_after_revision = request.after_revision;
    let poll_fallback_allowed = request.allow_poll_fallback;
    let Some(_permit) = state.long_polls.try_acquire(principal.id()) else {
        return ApiProblem::resource_exhausted(request_id).into_response();
    };
    let expected_generation = request.desktop_generation;
    match plane
        .0
        .wait_element(context(principal, request_id), request)
        .await
    {
        Ok(result)
            if result.validate().is_ok()
                && result.desktop_id == desktop_id
                && result.desktop_generation == expected_generation
                && result.matched_count <= expected_matches
                && wait_atspi_generation_matches(result.atspi_generation, &expected_target)
                && (!result.poll_fallback_used || poll_fallback_allowed)
                && wait_result_matches_request(
                    &result,
                    &expected_target,
                    &expected_predicate,
                    expected_after_revision,
                    expected_expansion,
                )
                && result.elements.iter().all(|entry| {
                    snapshot_matches_expansion(&entry.snapshot, expected_expansion)
                        && expected_reference.as_ref().is_none_or(|expected| {
                            same_element_identity(&entry.snapshot.element, expected)
                        })
                }) =>
        {
            json_no_store(result)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => accessibility_problem(error, request_id).into_response(),
    }
}

fn validate_request(
    state: &ApiState,
    path_desktop: DesktopId,
    body_desktop: DesktopId,
    generation: DesktopGeneration,
    shape_valid: bool,
    request_id: RequestId,
) -> Result<(), ApiProblem> {
    if !shape_valid || body_desktop != path_desktop {
        return Err(ApiProblem::invalid_request(request_id));
    }
    super::control::validate_generation(state, path_desktop, generation, request_id)
}

fn context(principal: Principal, request_id: RequestId) -> ControlRequestContext {
    ControlRequestContext::new(principal, request_id)
}

fn scope_atspi_generation(scope: &ElementScope) -> Option<AtspiGeneration> {
    match scope {
        ElementScope::Desktop | ElementScope::Window { .. } => None,
        ElementScope::Application { application } => Some(application.atspi_generation),
        ElementScope::Subtree { root, .. } | ElementScope::Children { parent: root } => {
            Some(root.atspi_generation)
        }
    }
}

fn same_element_identity(actual: &ElementRef, expected: &ElementRef) -> bool {
    actual.desktop_id == expected.desktop_id
        && actual.desktop_generation == expected.desktop_generation
        && actual.atspi_generation == expected.atspi_generation
        && actual.application == expected.application
        && actual.object_path == expected.object_path
        && actual.object_identity_hash == expected.object_identity_hash
        && actual.cache_sequence == expected.cache_sequence
}

fn selector_atspi_generation_matches(actual: AtspiGeneration, selector: &ElementSelector) -> bool {
    scope_atspi_generation(&selector.scope).is_none_or(|expected| actual == expected)
        && selector.predicates.iter().all(|predicate| {
            !matches!(
                predicate,
                ElementPredicate::Relation { target, .. } if target.atspi_generation != actual
            )
        })
}

fn wait_atspi_generation_matches(actual: AtspiGeneration, target: &ElementWaitTarget) -> bool {
    match target {
        ElementWaitTarget::Reference { element } => actual == element.atspi_generation,
        ElementWaitTarget::Selector { selector, .. } => {
            selector_atspi_generation_matches(actual, selector)
        }
    }
}

fn snapshot_matches_scope(snapshot: &ElementSnapshot, scope: &ElementScope) -> bool {
    match scope {
        ElementScope::Desktop => true,
        ElementScope::Application { application } => snapshot.element.application == *application,
        // A descendant need not repeat its correlated top-level window, so an
        // absent direct correlation is not negative evidence. A snapshot that
        // does carry direct evidence, however, must never name another window.
        ElementScope::Window { window } => snapshot
            .window_correlation
            .window
            .as_ref()
            .is_none_or(|actual| actual == window),
        ElementScope::Subtree { root, include_root } => {
            let is_root = same_element_identity(&snapshot.element, root);
            snapshot.element.application == root.application
                && snapshot.element.atspi_generation == root.atspi_generation
                && if is_root {
                    *include_root
                } else {
                    // Every non-root descendant must expose a distinct immediate
                    // parent. Deeper ancestry remains the plane's trusted,
                    // bounded-traversal invariant because it is not encoded in
                    // an individual snapshot.
                    snapshot
                        .parent
                        .as_ref()
                        .is_some_and(|parent| !same_element_identity(parent, &snapshot.element))
                }
        }
        ElementScope::Children { parent } => snapshot
            .parent
            .as_ref()
            .is_some_and(|actual| same_element_identity(actual, parent)),
    }
}

fn snapshot_matches_selector(
    snapshot: &ElementSnapshot,
    selector: &ElementSelector,
    expansion: ElementSnapshotExpansion,
) -> bool {
    snapshot_matches_scope(snapshot, &selector.scope)
        && selector
            .predicates
            .iter()
            .all(|predicate| snapshot_matches_predicate(snapshot, predicate, expansion))
}

fn snapshot_matches_predicate(
    snapshot: &ElementSnapshot,
    predicate: &ElementPredicate,
    expansion: ElementSnapshotExpansion,
) -> bool {
    match predicate {
        ElementPredicate::Role { roles } => roles.contains(&snapshot.role.role),
        ElementPredicate::Name { matcher } => {
            optional_string_matches(snapshot.name.as_deref(), matcher)
        }
        ElementPredicate::Description { matcher } => {
            optional_string_matches(snapshot.description.as_deref(), matcher)
        }
        ElementPredicate::AccessibleId { matcher } => {
            optional_string_matches(snapshot.accessible_id.as_deref(), matcher)
        }
        ElementPredicate::State { state, value } => snapshot.states.contains(state) == *value,
        ElementPredicate::Interface { interface } => snapshot.interfaces.contains(interface),
        ElementPredicate::Attribute { name, matcher } => {
            !expansion.attributes
                || snapshot.attributes.iter().any(|attribute| {
                    attribute.name == *name && string_match_is_possible(&attribute.value, matcher)
                })
        }
        ElementPredicate::Action { matcher } => {
            !expansion.actions
                || snapshot
                    .actions
                    .iter()
                    .any(|action| string_match_is_possible(&action.name, matcher))
        }
        ElementPredicate::ValueRange { minimum, maximum } => {
            !expansion.value
                || snapshot
                    .value
                    .as_ref()
                    .is_some_and(|value| in_f64_range(value.current, *minimum, *maximum))
        }
        ElementPredicate::IndexInParent { index } => snapshot.index_in_parent == Some(*index),
        ElementPredicate::ChildCount { minimum, maximum } => snapshot
            .child_count
            .is_some_and(|count| in_u32_range(count, *minimum, *maximum)),
        ElementPredicate::Relation { relation, target } => {
            !expansion.relations
                || snapshot.relations.iter().any(|candidate| {
                    candidate.relation == *relation && candidate.targets.contains(target)
                })
        }
        ElementPredicate::ComponentIntersects {
            coordinate_space,
            rect,
        } => {
            !expansion.component
                || snapshot
                    .component
                    .as_ref()
                    .filter(|component| component.coordinate_space == *coordinate_space)
                    .and_then(|component| component.extents)
                    .is_some_and(|candidate| rectangles_intersect(candidate, *rect))
        }
    }
}

fn optional_string_matches(candidate: Option<&str>, matcher: &ElementStringMatch) -> bool {
    candidate.is_some_and(|candidate| string_match_is_possible(candidate, matcher))
}

fn string_match_is_possible(candidate: &str, matcher: &ElementStringMatch) -> bool {
    match matcher {
        ElementStringMatch::Exact {
            value,
            case_sensitive,
        } => fold(candidate, *case_sensitive) == fold(value, *case_sensitive),
        ElementStringMatch::Contains {
            value,
            case_sensitive,
        } => fold(candidate, *case_sensitive).contains(&fold(value, *case_sensitive)),
        ElementStringMatch::Prefix {
            value,
            case_sensitive,
        } => fold(candidate, *case_sensitive).starts_with(&fold(value, *case_sensitive)),
        ElementStringMatch::Suffix {
            value,
            case_sensitive,
        } => fold(candidate, *case_sensitive).ends_with(&fold(value, *case_sensitive)),
        // Regex execution belongs to the bounded actor. The transport can prove
        // absence of a candidate field, but must not compile attacker patterns.
        ElementStringMatch::Regex { .. } => true,
    }
}

fn fold(value: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        value.to_owned()
    } else {
        value.chars().flat_map(char::to_lowercase).collect()
    }
}

fn in_f64_range(value: f64, minimum: Option<f64>, maximum: Option<f64>) -> bool {
    minimum.is_none_or(|minimum| value >= minimum) && maximum.is_none_or(|maximum| value <= maximum)
}

fn in_u32_range(value: u32, minimum: Option<u32>, maximum: Option<u32>) -> bool {
    minimum.is_none_or(|minimum| value >= minimum) && maximum.is_none_or(|maximum| value <= maximum)
}

fn rectangles_intersect(left: Rect, right: Rect) -> bool {
    let left_origin = left.origin();
    let right_origin = right.origin();
    let (Ok(left_size), Ok(right_size)) = (left.size(), right.size()) else {
        return false;
    };
    let left_x2 = i64::from(left_origin.x()) + i64::from(left_size.width());
    let left_y2 = i64::from(left_origin.y()) + i64::from(left_size.height());
    let right_x2 = i64::from(right_origin.x()) + i64::from(right_size.width());
    let right_y2 = i64::from(right_origin.y()) + i64::from(right_size.height());
    i64::from(left_origin.x()) < right_x2
        && i64::from(right_origin.x()) < left_x2
        && i64::from(left_origin.y()) < right_y2
        && i64::from(right_origin.y()) < left_y2
}

fn page_order_matches(page: &ElementListPage, order: ElementOrder) -> bool {
    page.elements.windows(2).all(|pair| {
        let left = &pair[0].snapshot;
        let right = &pair[1].snapshot;
        match order {
            ElementOrder::Preorder | ElementOrder::ReversePreorder => true,
            ElementOrder::NameAscending => compare_optional(&left.name, &right.name, false).is_le(),
            ElementOrder::NameDescending => compare_optional(&left.name, &right.name, true).is_le(),
            ElementOrder::RoleThenName => {
                left.role.role < right.role.role
                    || (left.role.role == right.role.role
                        && compare_optional(&left.name, &right.name, false).is_le())
            }
            ElementOrder::ObjectPathAscending => {
                left.element.object_path <= right.element.object_path
            }
        }
    })
}

fn compare_optional(
    left: &Option<String>,
    right: &Option<String>,
    reverse: bool,
) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) if reverse => right.cmp(left),
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn wait_result_matches_request(
    result: &ElementWaitResult,
    target: &ElementWaitTarget,
    predicate: &ElementWaitPredicate,
    after_revision: Option<xenoteer_protocol::AccessibilityRevision>,
    expansion: ElementSnapshotExpansion,
) -> bool {
    if matches!(result.status, ElementWaitStatus::Matched)
        && after_revision.is_some_and(|boundary| result.evaluated_revision <= boundary)
    {
        return false;
    }
    if !result.truncated && result.matched_count != result.elements.len() as u32 {
        return false;
    }
    if matches!(predicate, ElementWaitPredicate::SelectorCount { .. })
        && (result.matched_count != 0 || !result.elements.is_empty())
    {
        return false;
    }
    if !result.elements.iter().all(|entry| {
        snapshot_matches_wait_predicate(&entry.snapshot, predicate, expansion)
            && match target {
                ElementWaitTarget::Reference { element } => {
                    same_element_identity(&entry.snapshot.element, element)
                }
                ElementWaitTarget::Selector { selector, .. } => {
                    snapshot_matches_selector(&entry.snapshot, selector, expansion)
                }
            }
    }) {
        return false;
    }
    if !matches!(result.status, ElementWaitStatus::Matched) {
        return !matches!(target, ElementWaitTarget::Reference { .. }) || result.matched_count <= 1;
    }
    match target {
        ElementWaitTarget::Reference { .. } => match predicate {
            ElementWaitPredicate::Gone => result.matched_count == 0 && result.elements.is_empty(),
            ElementWaitPredicate::SelectorCount { .. } => false,
            _ => result.matched_count == 1 && result.elements.len() == 1,
        },
        ElementWaitTarget::Selector { quantifier, .. } => {
            if matches!(predicate, ElementWaitPredicate::SelectorCount { .. }) {
                return true;
            }
            match quantifier {
                ElementWaitQuantifier::Any | ElementWaitQuantifier::All => result.matched_count > 0,
                ElementWaitQuantifier::ExactlyOne => result.matched_count == 1,
                ElementWaitQuantifier::None => {
                    result.matched_count == 0 && result.elements.is_empty()
                }
            }
        }
    }
}

fn snapshot_matches_wait_predicate(
    snapshot: &ElementSnapshot,
    predicate: &ElementWaitPredicate,
    expansion: ElementSnapshotExpansion,
) -> bool {
    match predicate {
        ElementWaitPredicate::Exists => true,
        ElementWaitPredicate::Gone | ElementWaitPredicate::SelectorCount { .. } => false,
        ElementWaitPredicate::State { state, value } => snapshot.states.contains(state) == *value,
        ElementWaitPredicate::Name { matcher } => {
            optional_string_matches(snapshot.name.as_deref(), matcher)
        }
        ElementWaitPredicate::Value { minimum, maximum } => {
            !expansion.value
                || snapshot
                    .value
                    .as_ref()
                    .is_some_and(|value| in_f64_range(value.current, *minimum, *maximum))
        }
        ElementWaitPredicate::Text { matcher } => {
            !expansion.text_content
                || snapshot
                    .text
                    .as_ref()
                    .and_then(|text| text.content.as_ref())
                    .is_some_and(|text| string_match_is_possible(text.expose(), matcher))
        }
        ElementWaitPredicate::ChildCount { minimum, maximum } => snapshot
            .child_count
            .is_some_and(|count| in_u32_range(count, *minimum, *maximum)),
        ElementWaitPredicate::Geometry {
            coordinate_space,
            intersects,
        } => {
            !expansion.component
                || snapshot
                    .component
                    .as_ref()
                    .filter(|component| component.coordinate_space == *coordinate_space)
                    .and_then(|component| component.extents)
                    .is_some_and(|candidate| rectangles_intersect(candidate, *intersects))
        }
    }
}

fn snapshot_matches_expansion(
    snapshot: &ElementSnapshot,
    expansion: ElementSnapshotExpansion,
) -> bool {
    (expansion.actions || snapshot.actions.is_empty())
        && (expansion.value || snapshot.value.is_none())
        && (expansion.text_metadata || snapshot.text.is_none())
        && (expansion.text_content
            || snapshot
                .text
                .as_ref()
                .is_none_or(|text| text.content.is_none()))
        && (expansion.attributes || snapshot.attributes.is_empty())
        && (expansion.relations || snapshot.relations.is_empty())
        && (expansion.component || snapshot.component.is_none())
}

fn accessibility_problem(error: AccessibilityPlaneError, request_id: RequestId) -> ApiProblem {
    match error {
        AccessibilityPlaneError::InvalidRequest => ApiProblem::invalid_request(request_id),
        AccessibilityPlaneError::PermissionDenied => ApiProblem::permission_denied(request_id),
        AccessibilityPlaneError::NotFound => {
            ApiProblem::accessibility_element_not_found(request_id)
        }
        AccessibilityPlaneError::StaleReference { current_generation } => {
            ApiProblem::stale_reference(request_id, current_generation)
        }
        AccessibilityPlaneError::ResyncRequired { current_generation } => {
            ApiProblem::accessibility_resync_required(request_id, current_generation)
        }
        AccessibilityPlaneError::AmbiguousTarget => {
            ApiProblem::ambiguous_accessibility_target(request_id)
        }
        AccessibilityPlaneError::QueryLimitExceeded => {
            ApiProblem::accessibility_query_limit_exceeded(request_id)
        }
        AccessibilityPlaneError::ResourceExhausted => ApiProblem::resource_exhausted(request_id),
        AccessibilityPlaneError::CapabilityUnavailable => {
            ApiProblem::capability_unavailable(request_id)
        }
        AccessibilityPlaneError::UnsupportedByTarget => {
            ApiProblem::accessibility_interface_not_supported(request_id)
        }
        AccessibilityPlaneError::Internal => ApiProblem::internal(request_id),
    }
}

fn json_no_store<T: serde::Serialize>(body: T) -> Response {
    let mut response = (StatusCode::OK, Json(body)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}
