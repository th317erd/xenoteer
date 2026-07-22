//! Backend-independent window geometry normalization.
//!
//! The caller supplies one live root rectangle and one live window geometry
//! sample. The resolver performs no I/O and never guesses missing frame
//! decorations. Platform actors can therefore query immediately before an
//! effect and share exactly the same checked arithmetic and bounds policy.

use xenoteer_protocol::{
    CoordinateSpace, Point, Rect, WindowFrameExtents, WindowGeometry, WindowGeometryRequest,
    WindowGeometryTarget, WindowRect, WindowScreenBoundsPolicy,
};

/// Live geometry inputs used by move/resize and window-relative input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowGeometryContext {
    root: WindowRect,
    window: WindowGeometry,
}

/// Fully normalized geometry in both the public target space and EWMH client space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedWindowGeometry {
    /// Complete root-physical rectangle in the caller's requested frame/client space.
    pub effective: WindowRect,
    /// Complete client rectangle represented by the EWMH request.
    pub client_rect: WindowRect,
    /// Minimal static-gravity client fields which must be sent to realize `client_rect`.
    pub client_request: WindowGeometryRequest,
    /// Whether `ClampToRoot` changed the complete desired rectangle.
    pub bounds_constrained: bool,
}

/// One window-local point resolved to root-physical coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedWindowPoint {
    /// Root-physical point immediately usable by a pointer actor.
    pub root: Point,
    /// Whether `ClampToRoot` changed the translated point.
    pub bounds_constrained: bool,
}

/// Geometry normalization failure before any platform effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum WindowGeometryResolveError {
    /// Root/window geometry was malformed or used a non-root coordinate space.
    #[error("window geometry context is invalid")]
    InvalidGeometry,
    /// Frame-relative work requires both a live frame rectangle and live frame extents.
    #[error("live frame geometry and extents are unavailable")]
    FrameGeometryUnavailable,
    /// The frame rectangle disagreed with the client rectangle plus live extents.
    #[error("live frame rectangle and extents are inconsistent")]
    InconsistentFrameGeometry,
    /// The requested frame cannot contain a non-empty client after decorations.
    #[error("requested frame is too small for the live frame extents")]
    FrameTooSmall,
    /// `RequireInsideRoot` rejected a rectangle or point outside the live root.
    #[error("requested geometry is outside the live root bounds")]
    OutsideRootBounds,
    /// Checked root/client/frame translation exceeded representable coordinates.
    #[error("window geometry arithmetic overflowed")]
    ArithmeticOverflow,
    /// A local point named a coordinate space which is not window-relative.
    #[error("point coordinate space must be window_client or window_frame")]
    UnsupportedCoordinateSpace,
}

impl WindowGeometryContext {
    /// Validates one root rectangle and normalized window snapshot.
    pub fn new(
        root: WindowRect,
        window: WindowGeometry,
    ) -> Result<Self, WindowGeometryResolveError> {
        root.validate()
            .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
        window
            .validate()
            .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
        if root.coordinate_space != CoordinateSpace::RootPhysical {
            return Err(WindowGeometryResolveError::InvalidGeometry);
        }
        if let (Some(frame), Some(extents)) = (window.frame_rect, window.frame_extents)
            && derive_frame_rect(window.client_rect, extents)? != frame
        {
            return Err(WindowGeometryResolveError::InconsistentFrameGeometry);
        }
        Ok(Self { root, window })
    }

    /// Resolves partial public geometry against the live frame/client rectangle.
    pub fn resolve_move_resize(
        &self,
        relative_to: WindowGeometryTarget,
        desired: WindowGeometryRequest,
        bounds_policy: WindowScreenBoundsPolicy,
    ) -> Result<ResolvedWindowGeometry, WindowGeometryResolveError> {
        desired
            .validate()
            .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
        let current = match relative_to {
            WindowGeometryTarget::Client => self.window.client_rect,
            WindowGeometryTarget::Frame => {
                if self.window.frame_extents.is_none() {
                    return Err(WindowGeometryResolveError::FrameGeometryUnavailable);
                }
                self.window
                    .frame_rect
                    .ok_or(WindowGeometryResolveError::FrameGeometryUnavailable)?
            }
        };
        let candidate = apply_partial(current, desired)?;
        let effective = apply_bounds(candidate, self.root, bounds_policy)?;
        let client_rect = match relative_to {
            WindowGeometryTarget::Client => effective,
            WindowGeometryTarget::Frame => frame_to_client(
                effective,
                self.window
                    .frame_extents
                    .ok_or(WindowGeometryResolveError::FrameGeometryUnavailable)?,
            )?,
        };
        let client_request = changed_client_fields(self.window.client_rect, client_rect, desired)?;
        Ok(ResolvedWindowGeometry {
            effective,
            client_rect,
            client_request,
            bounds_constrained: candidate != effective,
        })
    }

    /// Translates one window-local point and applies the live root policy.
    pub fn resolve_local_point(
        &self,
        relative_to: CoordinateSpace,
        point: Point,
        bounds_policy: WindowScreenBoundsPolicy,
    ) -> Result<ResolvedWindowPoint, WindowGeometryResolveError> {
        let origin = match relative_to {
            CoordinateSpace::WindowClient => self.window.client_rect.rect.origin(),
            CoordinateSpace::WindowFrame => self
                .window
                .frame_rect
                .ok_or(WindowGeometryResolveError::FrameGeometryUnavailable)?
                .rect
                .origin(),
            CoordinateSpace::RootPhysical | CoordinateSpace::AtspiScreen => {
                return Err(WindowGeometryResolveError::UnsupportedCoordinateSpace);
            }
        };
        let translated = Point::new(
            add_i32(origin.x(), point.x())?,
            add_i32(origin.y(), point.y())?,
        );
        let root = match bounds_policy {
            WindowScreenBoundsPolicy::AllowOffscreen => translated,
            WindowScreenBoundsPolicy::RequireInsideRoot => {
                if !contains_point(self.root, translated)? {
                    return Err(WindowGeometryResolveError::OutsideRootBounds);
                }
                translated
            }
            WindowScreenBoundsPolicy::ClampToRoot => clamp_point(self.root, translated)?,
        };
        Ok(ResolvedWindowPoint {
            root,
            bounds_constrained: root != translated,
        })
    }

    /// Returns the validated live root rectangle.
    #[must_use]
    pub const fn root(&self) -> WindowRect {
        self.root
    }

    /// Returns the validated live window geometry.
    #[must_use]
    pub const fn window(&self) -> &WindowGeometry {
        &self.window
    }
}

fn apply_partial(
    current: WindowRect,
    desired: WindowGeometryRequest,
) -> Result<WindowRect, WindowGeometryResolveError> {
    let origin = current.rect.origin();
    let size = current
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    window_rect(
        desired.x.unwrap_or(origin.x()),
        desired.y.unwrap_or(origin.y()),
        desired.width.unwrap_or(size.width()),
        desired.height.unwrap_or(size.height()),
    )
}

fn apply_bounds(
    candidate: WindowRect,
    root: WindowRect,
    policy: WindowScreenBoundsPolicy,
) -> Result<WindowRect, WindowGeometryResolveError> {
    match policy {
        WindowScreenBoundsPolicy::AllowOffscreen => Ok(candidate),
        WindowScreenBoundsPolicy::RequireInsideRoot => {
            if contains_rect(root, candidate)? {
                Ok(candidate)
            } else {
                Err(WindowGeometryResolveError::OutsideRootBounds)
            }
        }
        WindowScreenBoundsPolicy::ClampToRoot => clamp_rect(root, candidate),
    }
}

fn clamp_rect(
    root: WindowRect,
    candidate: WindowRect,
) -> Result<WindowRect, WindowGeometryResolveError> {
    let root_origin = root.rect.origin();
    let root_size = root
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    let candidate_origin = candidate.rect.origin();
    let candidate_size = candidate
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    let width = candidate_size.width().min(root_size.width());
    let height = candidate_size.height().min(root_size.height());
    let max_x = i64::from(root_origin.x()) + i64::from(root_size.width() - width);
    let max_y = i64::from(root_origin.y()) + i64::from(root_size.height() - height);
    let x = i64::from(candidate_origin.x()).clamp(i64::from(root_origin.x()), max_x);
    let y = i64::from(candidate_origin.y()).clamp(i64::from(root_origin.y()), max_y);
    window_rect(i32_from(x)?, i32_from(y)?, width, height)
}

/// Derives a root-physical frame rectangle from live client geometry/extents.
pub fn derive_frame_rect(
    client: WindowRect,
    extents: WindowFrameExtents,
) -> Result<WindowRect, WindowGeometryResolveError> {
    let origin = client.rect.origin();
    let size = client
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    let x = i64::from(origin.x()) - i64::from(extents.left);
    let y = i64::from(origin.y()) - i64::from(extents.top);
    let width = u64::from(size.width()) + u64::from(extents.left) + u64::from(extents.right);
    let height = u64::from(size.height()) + u64::from(extents.top) + u64::from(extents.bottom);
    window_rect(
        i32_from(x)?,
        i32_from(y)?,
        u32_from(width)?,
        u32_from(height)?,
    )
}

fn frame_to_client(
    frame: WindowRect,
    extents: WindowFrameExtents,
) -> Result<WindowRect, WindowGeometryResolveError> {
    let origin = frame.rect.origin();
    let size = frame
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    let horizontal = extents
        .left
        .checked_add(extents.right)
        .ok_or(WindowGeometryResolveError::ArithmeticOverflow)?;
    let vertical = extents
        .top
        .checked_add(extents.bottom)
        .ok_or(WindowGeometryResolveError::ArithmeticOverflow)?;
    if size.width() <= horizontal || size.height() <= vertical {
        return Err(WindowGeometryResolveError::FrameTooSmall);
    }
    window_rect(
        add_u32_to_i32(origin.x(), extents.left)?,
        add_u32_to_i32(origin.y(), extents.top)?,
        size.width() - horizontal,
        size.height() - vertical,
    )
}

fn changed_client_fields(
    current: WindowRect,
    effective: WindowRect,
    desired: WindowGeometryRequest,
) -> Result<WindowGeometryRequest, WindowGeometryResolveError> {
    let current_origin = current.rect.origin();
    let current_size = current
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    let effective_origin = effective.rect.origin();
    let effective_size = effective
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    Ok(WindowGeometryRequest {
        x: (desired.x.is_some() || current_origin.x() != effective_origin.x())
            .then_some(effective_origin.x()),
        y: (desired.y.is_some() || current_origin.y() != effective_origin.y())
            .then_some(effective_origin.y()),
        width: (desired.width.is_some() || current_size.width() != effective_size.width())
            .then_some(effective_size.width()),
        height: (desired.height.is_some() || current_size.height() != effective_size.height())
            .then_some(effective_size.height()),
    })
}

fn contains_rect(outer: WindowRect, inner: WindowRect) -> Result<bool, WindowGeometryResolveError> {
    let outer_origin = outer.rect.origin();
    let outer_size = outer
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    let inner_origin = inner.rect.origin();
    let inner_size = inner
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    Ok(i64::from(inner_origin.x()) >= i64::from(outer_origin.x())
        && i64::from(inner_origin.y()) >= i64::from(outer_origin.y())
        && exclusive_end(inner_origin.x(), inner_size.width())?
            <= exclusive_end(outer_origin.x(), outer_size.width())?
        && exclusive_end(inner_origin.y(), inner_size.height())?
            <= exclusive_end(outer_origin.y(), outer_size.height())?)
}

fn contains_point(root: WindowRect, point: Point) -> Result<bool, WindowGeometryResolveError> {
    let origin = root.rect.origin();
    let size = root
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    Ok(i64::from(point.x()) >= i64::from(origin.x())
        && i64::from(point.y()) >= i64::from(origin.y())
        && i64::from(point.x()) < exclusive_end(origin.x(), size.width())?
        && i64::from(point.y()) < exclusive_end(origin.y(), size.height())?)
}

fn clamp_point(root: WindowRect, point: Point) -> Result<Point, WindowGeometryResolveError> {
    let origin = root.rect.origin();
    let size = root
        .rect
        .size()
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)?;
    let max_x = exclusive_end(origin.x(), size.width())? - 1;
    let max_y = exclusive_end(origin.y(), size.height())? - 1;
    Ok(Point::new(
        i32_from(i64::from(point.x()).clamp(i64::from(origin.x()), max_x))?,
        i32_from(i64::from(point.y()).clamp(i64::from(origin.y()), max_y))?,
    ))
}

fn window_rect(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<WindowRect, WindowGeometryResolveError> {
    let rect = Rect::new(x, y, width, height)
        .map_err(|_| WindowGeometryResolveError::ArithmeticOverflow)?;
    WindowRect::new(CoordinateSpace::RootPhysical, rect)
        .map_err(|_| WindowGeometryResolveError::InvalidGeometry)
}

fn exclusive_end(origin: i32, extent: u32) -> Result<i64, WindowGeometryResolveError> {
    i64::from(origin)
        .checked_add(i64::from(extent))
        .ok_or(WindowGeometryResolveError::ArithmeticOverflow)
}

fn add_i32(left: i32, right: i32) -> Result<i32, WindowGeometryResolveError> {
    i32_from(i64::from(left) + i64::from(right))
}

fn add_u32_to_i32(left: i32, right: u32) -> Result<i32, WindowGeometryResolveError> {
    i32_from(i64::from(left) + i64::from(right))
}

fn i32_from(value: i64) -> Result<i32, WindowGeometryResolveError> {
    i32::try_from(value).map_err(|_| WindowGeometryResolveError::ArithmeticOverflow)
}

fn u32_from(value: u64) -> Result<u32, WindowGeometryResolveError> {
    u32::try_from(value).map_err(|_| WindowGeometryResolveError::ArithmeticOverflow)
}
