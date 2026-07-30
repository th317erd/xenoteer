use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderValue, Request, StatusCode, header},
};
use tower::ServiceExt;
use xenoteer_protocol::{
    AccessibilityIdentityHash, AccessibilityQueryLimits, AccessibilityRevision, ApplicationRef,
    AtspiBusName, AtspiGeneration, AtspiObjectPath, DesktopGeneration, DesktopId,
    ElementCompleteness, ElementInterface, ElementListPage, ElementListRequest, ElementOrder,
    ElementPredicate, ElementQueryPage, ElementQueryRequest, ElementRef, ElementResolveRequest,
    ElementResolveResult, ElementRole, ElementRoleSnapshot, ElementScope, ElementSelector,
    ElementSnapshot, ElementSnapshotEntry, ElementSnapshotExpansion, ElementSnapshotRequest,
    ElementSnapshotResult, ElementState, ElementWaitPredicate, ElementWaitQuantifier,
    ElementWaitRequest, ElementWaitResult, ElementWaitStatus, ElementWaitTarget,
    ElementWindowCorrelation, ErrorCode, Problem, RetryAdvice, WindowCorrelationConfidence,
    WindowIdentityHash, WindowRef,
};

use crate::{
    AccessibilityFuture, AccessibilityPlane, AccessibilityPlaneError, AllowedOrigins, ApiServices,
    Authentication, ControlRequestContext, DesktopReadiness, Grant, Principal, ReadinessHandle,
    ReadinessSnapshot, StaticCapabilityProvider, StaticTokenProvider, TransportLimits,
    api_router_with_services, control::UnavailableControlPlane,
    observation::UnavailableObservationPlane,
};

const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

#[derive(Debug, Clone, Copy)]
enum FixtureMode {
    Success,
    Error(AccessibilityPlaneError),
    Wait(ElementWaitStatus),
    Resolve(ElementRole),
    Page(ElementRole),
    MatchedAtBoundary,
    PollFallback,
    ReferenceOvercount,
    ScopedPage(ScopedPageMode),
}

#[derive(Debug, Clone, Copy)]
enum ScopedPageMode {
    ExactWindow,
    CrossWindow,
    UncorrelatedWindowDescendant,
    SubtreeOrphan,
    SubtreeDirectChild,
    SubtreeDeepDescendant,
}

struct FixtureAccessibility {
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    calls: Arc<AtomicUsize>,
    mode: FixtureMode,
}

impl FixtureAccessibility {
    fn empty_page(&self) -> Result<ElementListPage, AccessibilityPlaneError> {
        Ok(ElementListPage {
            desktop_id: self.desktop_id,
            desktop_generation: self.generation,
            atspi_generation: AtspiGeneration::new(1)
                .map_err(|_| AccessibilityPlaneError::Internal)?,
            snapshot_revision: AccessibilityRevision::new(1)
                .map_err(|_| AccessibilityPlaneError::Internal)?,
            order: ElementOrder::Preorder,
            elements: Vec::new(),
            next_cursor: None,
            visited_nodes: 0,
            truncated: false,
            warnings: Vec::new(),
        })
    }

    fn element(&self, role: ElementRole) -> Result<ElementSnapshot, AccessibilityPlaneError> {
        let atspi_generation =
            AtspiGeneration::new(1).map_err(|_| AccessibilityPlaneError::Internal)?;
        let application = ApplicationRef {
            desktop_id: self.desktop_id,
            desktop_generation: self.generation,
            atspi_generation,
            unique_bus_name: AtspiBusName::new(":1.42")
                .map_err(|_| AccessibilityPlaneError::Internal)?,
            root_object_path: AtspiObjectPath::new("/org/example/App")
                .map_err(|_| AccessibilityPlaneError::Internal)?,
            app_instance_generation: 1,
            identity_hash: AccessibilityIdentityHash::new("a".repeat(64))
                .map_err(|_| AccessibilityPlaneError::Internal)?,
        };
        Ok(ElementSnapshot {
            element: ElementRef {
                desktop_id: self.desktop_id,
                desktop_generation: self.generation,
                atspi_generation,
                application,
                object_path: AtspiObjectPath::new("/org/example/App/target")
                    .map_err(|_| AccessibilityPlaneError::Internal)?,
                object_identity_hash: AccessibilityIdentityHash::new("b".repeat(64))
                    .map_err(|_| AccessibilityPlaneError::Internal)?,
                cache_sequence: 1,
            },
            parent: None,
            index_in_parent: Some(0),
            child_count: Some(0),
            role: ElementRoleSnapshot {
                role,
                raw_name: None,
                raw_numeric: None,
            },
            name: Some("Save".to_owned()),
            description: None,
            accessible_id: Some("save".to_owned()),
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
            revision: AccessibilityRevision::new(1)
                .map_err(|_| AccessibilityPlaneError::Internal)?,
            completeness: ElementCompleteness::Complete,
            truncated: false,
            warnings: Vec::new(),
        })
    }

    fn page(
        &self,
        role: ElementRole,
        order: ElementOrder,
    ) -> Result<ElementListPage, AccessibilityPlaneError> {
        Ok(ElementListPage {
            desktop_id: self.desktop_id,
            desktop_generation: self.generation,
            atspi_generation: AtspiGeneration::new(1)
                .map_err(|_| AccessibilityPlaneError::Internal)?,
            snapshot_revision: AccessibilityRevision::new(1)
                .map_err(|_| AccessibilityPlaneError::Internal)?,
            order,
            elements: vec![ElementSnapshotEntry {
                snapshot: self.element(role)?,
            }],
            next_cursor: None,
            visited_nodes: 1,
            truncated: false,
            warnings: Vec::new(),
        })
    }

    fn scoped_page(
        &self,
        request: &ElementListRequest,
        mode: ScopedPageMode,
    ) -> Result<ElementListPage, AccessibilityPlaneError> {
        let mut page = self.page(ElementRole::Button, request.order)?;
        let snapshot = &mut page.elements[0].snapshot;
        match (mode, &request.scope) {
            (ScopedPageMode::ExactWindow, ElementScope::Window { window }) => {
                snapshot.window_correlation.window = Some(window.clone());
                snapshot.window_correlation.confidence = WindowCorrelationConfidence::Strong;
            }
            (ScopedPageMode::CrossWindow, ElementScope::Window { window }) => {
                let mut other = window.clone();
                other.xid = other.xid.saturating_add(1).max(1);
                other.identity_hash = WindowIdentityHash::new("d".repeat(64))
                    .map_err(|_| AccessibilityPlaneError::Internal)?;
                snapshot.window_correlation.window = Some(other);
                snapshot.window_correlation.confidence = WindowCorrelationConfidence::Strong;
            }
            (ScopedPageMode::UncorrelatedWindowDescendant, ElementScope::Window { .. }) => {
                let mut parent = snapshot.element.clone();
                parent.object_path = AtspiObjectPath::new("/org/example/App/window")
                    .map_err(|_| AccessibilityPlaneError::Internal)?;
                parent.object_identity_hash = AccessibilityIdentityHash::new("c".repeat(64))
                    .map_err(|_| AccessibilityPlaneError::Internal)?;
                snapshot.parent = Some(parent);
            }
            (ScopedPageMode::SubtreeOrphan, ElementScope::Subtree { .. }) => {}
            (ScopedPageMode::SubtreeDirectChild, ElementScope::Subtree { root, .. }) => {
                snapshot.parent = Some(root.clone())
            }
            (ScopedPageMode::SubtreeDeepDescendant, ElementScope::Subtree { root, .. }) => {
                let mut parent = root.clone();
                parent.object_path = AtspiObjectPath::new("/org/example/App/intermediate")
                    .map_err(|_| AccessibilityPlaneError::Internal)?;
                parent.object_identity_hash = AccessibilityIdentityHash::new("c".repeat(64))
                    .map_err(|_| AccessibilityPlaneError::Internal)?;
                snapshot.parent = Some(parent);
            }
            _ => return Err(AccessibilityPlaneError::Internal),
        }
        Ok(page)
    }
}

impl AccessibilityPlane for FixtureAccessibility {
    fn list_elements<'a>(
        &'a self,
        _: ControlRequestContext,
        request: ElementListRequest,
    ) -> AccessibilityFuture<'a, Result<ElementListPage, AccessibilityPlaneError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if request.validate().is_err()
                || request.desktop_id != self.desktop_id
                || request.desktop_generation != self.generation
            {
                return Err(AccessibilityPlaneError::InvalidRequest);
            }
            match self.mode {
                FixtureMode::Error(error) => Err(error),
                FixtureMode::Page(role) => self.page(role, request.order),
                FixtureMode::ScopedPage(mode) => self.scoped_page(&request, mode),
                FixtureMode::Success
                | FixtureMode::Wait(_)
                | FixtureMode::Resolve(_)
                | FixtureMode::MatchedAtBoundary
                | FixtureMode::PollFallback
                | FixtureMode::ReferenceOvercount => self.empty_page(),
            }
        })
    }

    fn query_elements<'a>(
        &'a self,
        _: ControlRequestContext,
        request: ElementQueryRequest,
    ) -> AccessibilityFuture<'a, Result<ElementQueryPage, AccessibilityPlaneError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            match self.mode {
                FixtureMode::Page(role) => self.page(role, request.selector.order),
                FixtureMode::Error(error) => Err(error),
                FixtureMode::ScopedPage(_) => Err(AccessibilityPlaneError::Internal),
                _ => Err(AccessibilityPlaneError::NotFound),
            }
        })
    }

    fn resolve_element<'a>(
        &'a self,
        _: ControlRequestContext,
        request: ElementResolveRequest,
    ) -> AccessibilityFuture<'a, Result<ElementResolveResult, AccessibilityPlaneError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if request.validate().is_err()
                || request.desktop_id != self.desktop_id
                || request.desktop_generation != self.generation
            {
                return Err(AccessibilityPlaneError::InvalidRequest);
            }
            let role = match self.mode {
                FixtureMode::Resolve(role) => role,
                FixtureMode::Error(error) => return Err(error),
                FixtureMode::ScopedPage(_) => return Err(AccessibilityPlaneError::Internal),
                _ => return Err(AccessibilityPlaneError::NotFound),
            };
            Ok(ElementResolveResult {
                desktop_id: self.desktop_id,
                desktop_generation: self.generation,
                atspi_generation: AtspiGeneration::new(1)
                    .map_err(|_| AccessibilityPlaneError::Internal)?,
                snapshot_revision: AccessibilityRevision::new(1)
                    .map_err(|_| AccessibilityPlaneError::Internal)?,
                element: ElementSnapshotEntry {
                    snapshot: self.element(role)?,
                },
            })
        })
    }

    fn element_snapshot<'a>(
        &'a self,
        _: ControlRequestContext,
        _: ElementSnapshotRequest,
    ) -> AccessibilityFuture<'a, Result<ElementSnapshotResult, AccessibilityPlaneError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Err(AccessibilityPlaneError::NotFound) })
    }

    fn wait_element<'a>(
        &'a self,
        _: ControlRequestContext,
        request: ElementWaitRequest,
    ) -> AccessibilityFuture<'a, Result<ElementWaitResult, AccessibilityPlaneError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if request.validate().is_err()
                || request.desktop_id != self.desktop_id
                || request.desktop_generation != self.generation
            {
                return Err(AccessibilityPlaneError::InvalidRequest);
            }
            let (
                status,
                predicate_satisfied,
                matched_count,
                elements,
                poll_fallback_used,
                truncated,
            ) = match self.mode {
                FixtureMode::Wait(status) => (status, false, 0, Vec::new(), false, false),
                FixtureMode::Error(error) => return Err(error),
                FixtureMode::MatchedAtBoundary => {
                    let element = ElementSnapshotEntry {
                        snapshot: self.element(ElementRole::Button)?,
                    };
                    (
                        ElementWaitStatus::Matched,
                        true,
                        1,
                        vec![element],
                        false,
                        false,
                    )
                }
                FixtureMode::PollFallback => (
                    ElementWaitStatus::TimedOut,
                    false,
                    0,
                    Vec::new(),
                    true,
                    false,
                ),
                FixtureMode::ReferenceOvercount => {
                    (ElementWaitStatus::Matched, true, 2, Vec::new(), false, true)
                }
                FixtureMode::Success
                | FixtureMode::Resolve(_)
                | FixtureMode::Page(_)
                | FixtureMode::ScopedPage(_) => (
                    ElementWaitStatus::TimedOut,
                    false,
                    0,
                    Vec::new(),
                    false,
                    false,
                ),
            };
            Ok(ElementWaitResult {
                desktop_id: self.desktop_id,
                desktop_generation: self.generation,
                atspi_generation: AtspiGeneration::new(1)
                    .map_err(|_| AccessibilityPlaneError::Internal)?,
                status,
                evaluated_revision: request.after_revision.unwrap_or(
                    AccessibilityRevision::new(1).map_err(|_| AccessibilityPlaneError::Internal)?,
                ),
                predicate_satisfied,
                matched_count,
                elements,
                poll_fallback_used,
                truncated,
                warnings: Vec::new(),
            })
        })
    }
}

fn application(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    principal: Principal,
    accessibility: Option<Arc<dyn AccessibilityPlane>>,
) -> Result<Router, Box<dyn std::error::Error>> {
    let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
        DesktopReadiness::Ready,
        Some(generation),
        None::<String>,
    ));
    let provider = StaticTokenProvider::single(TOKEN, principal)?;
    let mut services = ApiServices::new(
        Arc::new(UnavailableControlPlane),
        Arc::new(UnavailableObservationPlane),
    );
    if let Some(accessibility) = accessibility {
        services = services.with_accessibility_plane(accessibility);
    }
    Ok(api_router_with_services(
        readiness,
        desktop_id,
        Authentication::bearer(provider),
        StaticCapabilityProvider::empty()?,
        TransportLimits::default(),
        AllowedOrigins::default(),
        services,
    ))
}

fn list_request(desktop_id: DesktopId, generation: DesktopGeneration) -> ElementListRequest {
    ElementListRequest {
        desktop_id,
        desktop_generation: generation,
        scope: ElementScope::Desktop,
        order: ElementOrder::Preorder,
        limit: 100,
        cursor: None,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    }
}

fn wait_request(desktop_id: DesktopId, generation: DesktopGeneration) -> ElementWaitRequest {
    ElementWaitRequest {
        desktop_id,
        desktop_generation: generation,
        target: ElementWaitTarget::Selector {
            selector: ElementSelector {
                scope: ElementScope::Desktop,
                predicates: Vec::new(),
                order: ElementOrder::Preorder,
                result_index: None,
            },
            quantifier: ElementWaitQuantifier::Any,
        },
        predicate: ElementWaitPredicate::Exists,
        after_revision: None,
        timeout_ms: 1_000,
        allow_poll_fallback: true,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    }
}

fn selector_for(role: ElementRole) -> ElementSelector {
    ElementSelector {
        scope: ElementScope::Desktop,
        predicates: vec![ElementPredicate::Role { roles: vec![role] }],
        order: ElementOrder::Preorder,
        result_index: None,
    }
}

fn query_request(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    role: ElementRole,
) -> ElementQueryRequest {
    ElementQueryRequest {
        desktop_id,
        desktop_generation: generation,
        selector: selector_for(role),
        limit: 100,
        cursor: None,
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    }
}

fn resolve_request(
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    role: ElementRole,
) -> ElementResolveRequest {
    ElementResolveRequest {
        desktop_id,
        desktop_generation: generation,
        selector: selector_for(role),
        expansion: ElementSnapshotExpansion::default(),
        limits: AccessibilityQueryLimits::default(),
    }
}

fn post_json<T: serde::Serialize>(
    desktop_id: DesktopId,
    operation: &str,
    body: &T,
) -> Result<Request<Body>, Box<dyn std::error::Error>> {
    Ok(Request::post(format!(
        "/v1/desktops/{desktop_id}/accessibility/elements/{operation}"
    ))
    .header(
        header::AUTHORIZATION,
        "Bearer 0123456789abcdef0123456789abcdef",
    )
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(serde_json::to_vec(body)?))?)
}

#[tokio::test]
async fn missing_grant_is_denied_before_accessibility_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
        desktop_id,
        generation,
        calls: Arc::clone(&calls),
        mode: FixtureMode::Success,
    });
    let principal = Principal::new("observer", [Grant::DesktopObserve])?;
    let response = application(desktop_id, generation, principal, Some(plane))?
        .oneshot(post_json(
            desktop_id,
            "list",
            &list_request(desktop_id, generation),
        )?)
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[tokio::test]
async fn unavailable_default_is_an_honest_capability_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
    let response = application(desktop_id, generation, principal, None)?
        .oneshot(post_json(
            desktop_id,
            "list",
            &list_request(desktop_id, generation),
        )?)
        .await?;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), 16 * 1_024).await?;
    let problem: Problem = serde_json::from_slice(&body)?;
    assert_eq!(problem.code(), ErrorCode::CapabilityUnavailable);
    Ok(())
}

#[tokio::test]
async fn invalid_request_is_rejected_before_accessibility_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
        desktop_id,
        generation,
        calls: Arc::clone(&calls),
        mode: FixtureMode::Success,
    });
    let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
    let mut request = list_request(desktop_id, generation);
    request.limit = 0;
    let response = application(desktop_id, generation, principal, Some(plane))?
        .oneshot(post_json(desktop_id, "list", &request)?)
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[tokio::test]
async fn successful_list_is_validated_and_never_cached() -> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
        desktop_id,
        generation,
        calls: Arc::clone(&calls),
        mode: FixtureMode::Success,
    });
    let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
    let response = application(desktop_id, generation, principal, Some(plane))?
        .oneshot(post_json(
            desktop_id,
            "list",
            &list_request(desktop_id, generation),
        )?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL),
        Some(&HeaderValue::from_static("private, no-store"))
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    Ok(())
}

#[tokio::test]
async fn exact_one_resolve_is_authenticated_and_binds_selector_output()
-> Result<(), Box<dyn std::error::Error>> {
    for (requested_role, expected_status) in [
        (ElementRole::Button, StatusCode::OK),
        (ElementRole::Label, StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
            desktop_id,
            generation,
            calls: Arc::new(AtomicUsize::new(0)),
            mode: FixtureMode::Resolve(ElementRole::Button),
        });
        let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
        let response = application(desktop_id, generation, principal, Some(plane))?
            .oneshot(post_json(
                desktop_id,
                "resolve",
                &resolve_request(desktop_id, generation, requested_role),
            )?)
            .await?;
        assert_eq!(response.status(), expected_status);
        if expected_status == StatusCode::OK {
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL),
                Some(&HeaderValue::from_static("private, no-store"))
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn invalid_exact_one_policy_is_rejected_before_dispatch()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
        desktop_id,
        generation,
        calls: Arc::clone(&calls),
        mode: FixtureMode::Resolve(ElementRole::Button),
    });
    let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
    let mut request = resolve_request(desktop_id, generation, ElementRole::Button);
    request.selector.result_index = Some(0);
    let response = application(desktop_id, generation, principal, Some(plane))?
        .oneshot(post_json(desktop_id, "resolve", &request)?)
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[tokio::test]
async fn adapter_pages_cannot_escape_scope_or_selector_predicates()
-> Result<(), Box<dyn std::error::Error>> {
    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let fixture = FixtureAccessibility {
        desktop_id,
        generation,
        calls: Arc::new(AtomicUsize::new(0)),
        mode: FixtureMode::Page(ElementRole::Button),
    };
    let mut list = list_request(desktop_id, generation);
    let mut other_application = fixture
        .element(ElementRole::Button)
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?
        .element
        .application;
    other_application.identity_hash = AccessibilityIdentityHash::new("c".repeat(64))?;
    list.scope = ElementScope::Application {
        application: other_application,
    };
    let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
    let response = application(
        desktop_id,
        generation,
        principal.clone(),
        Some(Arc::new(fixture)),
    )?
    .oneshot(post_json(desktop_id, "list", &list)?)
    .await?;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
        desktop_id,
        generation,
        calls: Arc::new(AtomicUsize::new(0)),
        mode: FixtureMode::Page(ElementRole::Button),
    });
    let response = application(desktop_id, generation, principal, Some(plane))?
        .oneshot(post_json(
            desktop_id,
            "query",
            &query_request(desktop_id, generation, ElementRole::Label),
        )?)
        .await?;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    Ok(())
}

#[tokio::test]
async fn window_scope_rejects_conflicting_direct_correlation()
-> Result<(), Box<dyn std::error::Error>> {
    for (mode, expected_status) in [
        (ScopedPageMode::ExactWindow, StatusCode::OK),
        (
            ScopedPageMode::CrossWindow,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (ScopedPageMode::UncorrelatedWindowDescendant, StatusCode::OK),
    ] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let window = WindowRef {
            desktop_id,
            desktop_generation: generation,
            xid: 100,
            observed_generation: 1,
            identity_hash: WindowIdentityHash::new("e".repeat(64))?,
        };
        let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
            desktop_id,
            generation,
            calls: Arc::new(AtomicUsize::new(0)),
            mode: FixtureMode::ScopedPage(mode),
        });
        let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
        let mut request = list_request(desktop_id, generation);
        request.scope = ElementScope::Window { window };
        let response = application(desktop_id, generation, principal, Some(plane))?
            .oneshot(post_json(desktop_id, "list", &request)?)
            .await?;
        assert_eq!(response.status(), expected_status, "mode: {mode:?}");
    }
    Ok(())
}

#[tokio::test]
async fn subtree_scope_checks_available_parent_evidence_without_rewalking_the_graph()
-> Result<(), Box<dyn std::error::Error>> {
    for (mode, expected_status) in [
        (
            ScopedPageMode::SubtreeOrphan,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (ScopedPageMode::SubtreeDirectChild, StatusCode::OK),
        (ScopedPageMode::SubtreeDeepDescendant, StatusCode::OK),
    ] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let fixture = FixtureAccessibility {
            desktop_id,
            generation,
            calls: Arc::new(AtomicUsize::new(0)),
            mode: FixtureMode::ScopedPage(mode),
        };
        let mut root = fixture
            .element(ElementRole::Panel)
            .map_err(|error| std::io::Error::other(format!("{error:?}")))?
            .element;
        root.object_path = AtspiObjectPath::new("/org/example/App/root")?;
        root.object_identity_hash = AccessibilityIdentityHash::new("f".repeat(64))?;
        let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
        let mut request = list_request(desktop_id, generation);
        request.scope = ElementScope::Subtree {
            root,
            include_root: false,
        };
        let response = application(desktop_id, generation, principal, Some(Arc::new(fixture)))?
            .oneshot(post_json(desktop_id, "list", &request)?)
            .await?;
        assert_eq!(response.status(), expected_status, "mode: {mode:?}");
    }
    Ok(())
}

#[tokio::test]
async fn wait_timeout_and_resync_are_typed_no_store_results()
-> Result<(), Box<dyn std::error::Error>> {
    for (mode, expected) in [
        (FixtureMode::Wait(ElementWaitStatus::TimedOut), "timed_out"),
        (
            FixtureMode::Wait(ElementWaitStatus::ResyncRequired),
            "resync_required",
        ),
    ] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
            desktop_id,
            generation,
            calls: Arc::new(AtomicUsize::new(0)),
            mode,
        });
        let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
        let response = application(desktop_id, generation, principal, Some(plane))?
            .oneshot(post_json(
                desktop_id,
                "wait",
                &wait_request(desktop_id, generation),
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static("private, no-store"))
        );
        let body = to_bytes(response.into_body(), 64 * 1_024).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(body["status"], expected);
    }
    Ok(())
}

#[tokio::test]
async fn wait_results_are_bound_to_revision_fallback_and_reference_cardinality()
-> Result<(), Box<dyn std::error::Error>> {
    for mode in [FixtureMode::MatchedAtBoundary, FixtureMode::PollFallback] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
            desktop_id,
            generation,
            calls: Arc::new(AtomicUsize::new(0)),
            mode,
        });
        let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
        let mut request = wait_request(desktop_id, generation);
        if matches!(mode, FixtureMode::MatchedAtBoundary) {
            request.after_revision = Some(AccessibilityRevision::new(1)?);
        } else {
            request.allow_poll_fallback = false;
        }
        let response = application(desktop_id, generation, principal, Some(plane))?
            .oneshot(post_json(desktop_id, "wait", &request)?)
            .await?;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    let desktop_id = DesktopId::new();
    let generation = DesktopGeneration::new();
    let fixture = FixtureAccessibility {
        desktop_id,
        generation,
        calls: Arc::new(AtomicUsize::new(0)),
        mode: FixtureMode::ReferenceOvercount,
    };
    let element = fixture
        .element(ElementRole::Button)
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?
        .element;
    let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
    let mut request = wait_request(desktop_id, generation);
    request.target = ElementWaitTarget::Reference { element };
    let response = application(desktop_id, generation, principal, Some(Arc::new(fixture)))?
        .oneshot(post_json(desktop_id, "wait", &request)?)
        .await?;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    Ok(())
}

#[tokio::test]
async fn resync_ambiguous_and_limit_errors_have_stable_safe_mappings()
-> Result<(), Box<dyn std::error::Error>> {
    for (error, status, code) in [
        (
            AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            },
            StatusCode::CONFLICT,
            ErrorCode::ToolkitProtocolError,
        ),
        (
            AccessibilityPlaneError::AmbiguousTarget,
            StatusCode::CONFLICT,
            ErrorCode::AmbiguousTarget,
        ),
        (
            AccessibilityPlaneError::QueryLimitExceeded,
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::QueryBudgetExceeded,
        ),
        (
            AccessibilityPlaneError::NotFound,
            StatusCode::NOT_FOUND,
            ErrorCode::ElementNotFound,
        ),
        (
            AccessibilityPlaneError::UnsupportedByTarget,
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InterfaceNotSupported,
        ),
    ] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
            desktop_id,
            generation,
            calls: Arc::new(AtomicUsize::new(0)),
            mode: FixtureMode::Error(error),
        });
        let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
        let response = application(desktop_id, generation, principal, Some(plane))?
            .oneshot(post_json(
                desktop_id,
                "list",
                &list_request(desktop_id, generation),
            )?)
            .await?;
        assert_eq!(response.status(), status);
        let body = to_bytes(response.into_body(), 16 * 1_024).await?;
        let problem: Problem = serde_json::from_slice(&body)?;
        problem.validate()?;
        assert_eq!(problem.code(), code);
        let encoded = String::from_utf8(body.to_vec())?;
        assert!(!encoded.contains("canary-secret"));
        assert!(!encoded.contains("Save Password"));
    }
    Ok(())
}

#[tokio::test]
async fn query_cursor_failures_have_retryable_safe_http_mappings()
-> Result<(), Box<dyn std::error::Error>> {
    for (error, status, code, retry, retry_after) in [
        (
            AccessibilityPlaneError::StaleReference {
                current_generation: None,
            },
            StatusCode::CONFLICT,
            ErrorCode::StaleReference,
            RetryAdvice::AfterResync,
            None,
        ),
        (
            AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            },
            StatusCode::CONFLICT,
            ErrorCode::ToolkitProtocolError,
            RetryAdvice::AfterResync,
            None,
        ),
        (
            AccessibilityPlaneError::ResourceExhausted,
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::ResourceExhausted,
            RetryAdvice::AfterBackoff,
            Some("1"),
        ),
        (
            AccessibilityPlaneError::CapabilityUnavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::CapabilityUnavailable,
            RetryAdvice::AfterBackoff,
            Some("1"),
        ),
    ] {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let plane: Arc<dyn AccessibilityPlane> = Arc::new(FixtureAccessibility {
            desktop_id,
            generation,
            calls: Arc::new(AtomicUsize::new(0)),
            mode: FixtureMode::Error(error),
        });
        let principal = Principal::new("semantic-reader", [Grant::AccessibilityRead])?;
        let response = application(desktop_id, generation, principal, Some(plane))?
            .oneshot(post_json(
                desktop_id,
                "query",
                &query_request(desktop_id, generation, ElementRole::Button),
            )?)
            .await?;
        assert_eq!(response.status(), status);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            retry_after
        );
        let body = to_bytes(response.into_body(), 16 * 1_024).await?;
        let problem: Problem = serde_json::from_slice(&body)?;
        problem.validate()?;
        assert_eq!(problem.code(), code);
        assert_eq!(problem.retry(), retry);
    }
    Ok(())
}
