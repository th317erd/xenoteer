#![allow(clippy::unwrap_used)]

use xenoteer_protocol::{
    CoordinateSpace, Point, Rect, WindowFrameExtents, WindowGeometry, WindowGeometryRequest,
    WindowGeometryTarget, WindowRect, WindowScreenBoundsPolicy,
};

use crate::window_geometry::{WindowGeometryContext, WindowGeometryResolveError};

fn root(x: i32, y: i32, width: u32, height: u32) -> WindowRect {
    WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(x, y, width, height).unwrap(),
    )
    .unwrap()
}

fn decorated_window() -> WindowGeometry {
    WindowGeometry {
        client_rect: root(110, 130, 400, 300),
        frame_rect: Some(root(100, 100, 420, 340)),
        content_rect: root(110, 130, 400, 300),
        frame_extents: Some(WindowFrameExtents {
            left: 10,
            right: 10,
            top: 30,
            bottom: 10,
        }),
    }
}

#[test]
fn frame_request_uses_live_extents_and_static_client_coordinates() {
    let context = WindowGeometryContext::new(root(0, 0, 1_000, 800), decorated_window()).unwrap();
    let resolved = context
        .resolve_move_resize(
            WindowGeometryTarget::Frame,
            WindowGeometryRequest {
                x: Some(20),
                y: Some(40),
                width: Some(620),
                height: Some(540),
            },
            WindowScreenBoundsPolicy::AllowOffscreen,
        )
        .unwrap();

    assert_eq!(resolved.effective, root(20, 40, 620, 540));
    assert_eq!(resolved.client_rect, root(30, 70, 600, 500));
    assert_eq!(
        resolved.client_request,
        WindowGeometryRequest {
            x: Some(30),
            y: Some(70),
            width: Some(600),
            height: Some(500),
        }
    );
    assert!(!resolved.bounds_constrained);
}

#[test]
fn clamp_is_root_origin_aware_and_can_change_omitted_fields() {
    let context =
        WindowGeometryContext::new(root(-100, 150, 500, 400), decorated_window()).unwrap();
    let resolved = context
        .resolve_move_resize(
            WindowGeometryTarget::Client,
            WindowGeometryRequest {
                x: Some(350),
                y: None,
                width: Some(600),
                height: None,
            },
            WindowScreenBoundsPolicy::ClampToRoot,
        )
        .unwrap();

    assert_eq!(resolved.effective, root(-100, 150, 500, 300));
    assert_eq!(resolved.client_rect, resolved.effective);
    assert_eq!(
        resolved.client_request,
        WindowGeometryRequest {
            x: Some(-100),
            y: Some(150),
            width: Some(500),
            height: None,
        }
    );
    assert!(resolved.bounds_constrained);
}

#[test]
fn require_inside_rejects_and_allow_offscreen_preserves_the_same_candidate() {
    let context = WindowGeometryContext::new(root(0, 0, 500, 400), decorated_window()).unwrap();
    let desired = WindowGeometryRequest {
        x: Some(-1),
        y: None,
        width: None,
        height: None,
    };
    assert_eq!(
        context.resolve_move_resize(
            WindowGeometryTarget::Frame,
            desired,
            WindowScreenBoundsPolicy::RequireInsideRoot,
        ),
        Err(WindowGeometryResolveError::OutsideRootBounds)
    );
    let allowed = context
        .resolve_move_resize(
            WindowGeometryTarget::Frame,
            desired,
            WindowScreenBoundsPolicy::AllowOffscreen,
        )
        .unwrap();
    assert_eq!(allowed.effective, root(-1, 100, 420, 340));
}

#[test]
fn frame_resolution_fails_closed_without_consistent_live_extents() {
    let mut missing = decorated_window();
    missing.frame_rect = None;
    missing.frame_extents = None;
    let context = WindowGeometryContext::new(root(0, 0, 1_000, 800), missing).unwrap();
    assert_eq!(
        context.resolve_move_resize(
            WindowGeometryTarget::Frame,
            WindowGeometryRequest {
                x: Some(1),
                y: None,
                width: None,
                height: None,
            },
            WindowScreenBoundsPolicy::AllowOffscreen,
        ),
        Err(WindowGeometryResolveError::FrameGeometryUnavailable)
    );

    let mut inconsistent = decorated_window();
    inconsistent.frame_rect = Some(root(99, 100, 420, 340));
    assert_eq!(
        WindowGeometryContext::new(root(0, 0, 1_000, 800), inconsistent),
        Err(WindowGeometryResolveError::InconsistentFrameGeometry)
    );
}

#[test]
fn frame_smaller_than_decorations_is_rejected_without_underflow() {
    let context = WindowGeometryContext::new(root(0, 0, 1_000, 800), decorated_window()).unwrap();
    assert_eq!(
        context.resolve_move_resize(
            WindowGeometryTarget::Frame,
            WindowGeometryRequest {
                x: None,
                y: None,
                width: Some(20),
                height: Some(40),
            },
            WindowScreenBoundsPolicy::AllowOffscreen,
        ),
        Err(WindowGeometryResolveError::FrameTooSmall)
    );
}

#[test]
fn local_points_resolve_from_client_or_frame_and_obey_root_policy() {
    let context = WindowGeometryContext::new(root(0, 0, 500, 400), decorated_window()).unwrap();
    assert_eq!(
        context
            .resolve_local_point(
                CoordinateSpace::WindowClient,
                Point::new(5, 7),
                WindowScreenBoundsPolicy::RequireInsideRoot,
            )
            .unwrap()
            .root,
        Point::new(115, 137)
    );
    assert_eq!(
        context
            .resolve_local_point(
                CoordinateSpace::WindowFrame,
                Point::new(5, 7),
                WindowScreenBoundsPolicy::RequireInsideRoot,
            )
            .unwrap()
            .root,
        Point::new(105, 107)
    );

    let clamped = context
        .resolve_local_point(
            CoordinateSpace::WindowClient,
            Point::new(500, 500),
            WindowScreenBoundsPolicy::ClampToRoot,
        )
        .unwrap();
    assert_eq!(clamped.root, Point::new(499, 399));
    assert!(clamped.bounds_constrained);
    assert_eq!(
        context.resolve_local_point(
            CoordinateSpace::RootPhysical,
            Point::new(1, 1),
            WindowScreenBoundsPolicy::AllowOffscreen,
        ),
        Err(WindowGeometryResolveError::UnsupportedCoordinateSpace)
    );
}
