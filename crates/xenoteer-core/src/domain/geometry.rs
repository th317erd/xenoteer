//! Checked root-window geometry used by physical input planning.

use thiserror::Error;
use xenoteer_protocol::{Point, Rect};

/// The smallest absolute coordinate representable by core XTEST motion.
pub const MIN_XTEST_COORDINATE: i32 = i16::MIN as i32;

/// The largest absolute coordinate representable by core XTEST motion.
pub const MAX_XTEST_COORDINATE: i32 = i16::MAX as i32;

/// A point in root-window physical pixels that fits an XTEST request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootPoint(Point);

impl RootPoint {
    /// Creates a checked root point.
    pub fn new(x: i32, y: i32) -> Result<Self, GeometryError> {
        if !(MIN_XTEST_COORDINATE..=MAX_XTEST_COORDINATE).contains(&x)
            || !(MIN_XTEST_COORDINATE..=MAX_XTEST_COORDINATE).contains(&y)
        {
            return Err(GeometryError::OutsideXtestRange { x, y });
        }
        Ok(Self(Point::new(x, y)))
    }

    /// Checks and converts an unchecked protocol point.
    pub fn try_from_protocol(point: Point) -> Result<Self, GeometryError> {
        Self::new(point.x(), point.y())
    }

    /// Returns the wire-compatible point.
    #[must_use]
    pub const fn as_protocol(self) -> Point {
        self.0
    }

    /// Returns the horizontal coordinate.
    #[must_use]
    pub const fn x(self) -> i32 {
        self.0.x()
    }

    /// Returns the vertical coordinate.
    #[must_use]
    pub const fn y(self) -> i32 {
        self.0.y()
    }

    /// Applies a relative delta with overflow and XTEST-range checks.
    pub fn checked_add(self, delta: PointerDelta) -> Result<Self, GeometryError> {
        let x = self
            .x()
            .checked_add(delta.dx())
            .ok_or(GeometryError::CoordinateOverflow)?;
        let y = self
            .y()
            .checked_add(delta.dy())
            .ok_or(GeometryError::CoordinateOverflow)?;
        Self::new(x, y)
    }
}

/// A checked relative pointer displacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PointerDelta {
    dx: i32,
    dy: i32,
}

impl PointerDelta {
    /// Creates a displacement from a wider representation without truncation.
    pub fn new(dx: i64, dy: i64) -> Result<Self, GeometryError> {
        let dx = i32::try_from(dx).map_err(|_| GeometryError::DeltaOverflow)?;
        let dy = i32::try_from(dy).map_err(|_| GeometryError::DeltaOverflow)?;
        Ok(Self { dx, dy })
    }

    /// Returns the horizontal displacement.
    #[must_use]
    pub const fn dx(self) -> i32 {
        self.dx
    }

    /// Returns the vertical displacement.
    #[must_use]
    pub const fn dy(self) -> i32 {
        self.dy
    }
}

/// A non-empty root-window rectangle whose complete extent fits XTEST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect(Rect);

impl ScreenRect {
    /// Creates a checked screen rectangle.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, GeometryError> {
        if width == 0 || height == 0 {
            return Err(GeometryError::EmptyScreen);
        }
        let rect = Rect::new(x, y, width, height).map_err(|_| GeometryError::CoordinateOverflow)?;
        let max_x = i64::from(x) + i64::from(width) - 1;
        let max_y = i64::from(y) + i64::from(height) - 1;
        let max_x = i32::try_from(max_x).map_err(|_| GeometryError::CoordinateOverflow)?;
        let max_y = i32::try_from(max_y).map_err(|_| GeometryError::CoordinateOverflow)?;
        RootPoint::new(x, y)?;
        RootPoint::new(max_x, max_y)?;
        Ok(Self(rect))
    }

    /// Checks and converts an unchecked protocol rectangle.
    pub fn try_from_protocol(rect: Rect) -> Result<Self, GeometryError> {
        let origin = rect.origin();
        let size = rect.size().map_err(|_| GeometryError::CoordinateOverflow)?;
        Self::new(origin.x(), origin.y(), size.width(), size.height())
    }

    /// Returns the wire-compatible rectangle.
    #[must_use]
    pub const fn as_protocol(self) -> Rect {
        self.0
    }

    /// Returns the checked top-left point.
    #[must_use]
    pub fn origin(self) -> RootPoint {
        let origin = self.0.origin();
        RootPoint(origin)
    }

    /// Returns the inclusive bottom-right point.
    #[must_use]
    pub fn inclusive_end(self) -> RootPoint {
        let origin = self.0.origin();
        let size = match self.0.size() {
            Ok(size) => size,
            Err(_) => return RootPoint(origin),
        };
        let x = i64::from(origin.x()) + i64::from(size.width()) - 1;
        let y = i64::from(origin.y()) + i64::from(size.height()) - 1;
        let x = match i32::try_from(x) {
            Ok(value) => value,
            Err(_) => return RootPoint(origin),
        };
        let y = match i32::try_from(y) {
            Ok(value) => value,
            Err(_) => return RootPoint(origin),
        };
        RootPoint(Point::new(x, y))
    }

    /// Returns whether the rectangle contains the point, including its edges.
    #[must_use]
    pub fn contains(self, point: RootPoint) -> bool {
        let origin = self.origin();
        let end = self.inclusive_end();
        (origin.x()..=end.x()).contains(&point.x()) && (origin.y()..=end.y()).contains(&point.y())
    }

    /// Returns the point clamped to the nearest location inside the rectangle.
    #[must_use]
    pub fn clamp(self, point: RootPoint) -> RootPoint {
        let origin = self.origin();
        let end = self.inclusive_end();
        RootPoint(Point::new(
            point.x().clamp(origin.x(), end.x()),
            point.y().clamp(origin.y(), end.y()),
        ))
    }

    /// Returns the intersection of two screen rectangles.
    #[must_use]
    pub fn intersect(self, other: Self) -> Option<Self> {
        let origin = self.origin();
        let end = self.inclusive_end();
        let other_origin = other.origin();
        let other_end = other.inclusive_end();
        let x = origin.x().max(other_origin.x());
        let y = origin.y().max(other_origin.y());
        let max_x = end.x().min(other_end.x());
        let max_y = end.y().min(other_end.y());
        if x > max_x || y > max_y {
            return None;
        }
        let width = u32::try_from(i64::from(max_x) - i64::from(x) + 1).ok()?;
        let height = u32::try_from(i64::from(max_y) - i64::from(y) + 1).ok()?;
        Self::new(x, y, width, height).ok()
    }

    /// Rejects a point outside this screen unless explicit clamping is enabled.
    pub fn admit(self, point: RootPoint, clamp: bool) -> Result<RootPoint, GeometryError> {
        if self.contains(point) {
            Ok(point)
        } else if clamp {
            Ok(self.clamp(point))
        } else {
            Err(GeometryError::OutsideScreen { point })
        }
    }
}

/// Failure to construct or apply checked input geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum GeometryError {
    /// A screen rectangle has a zero extent.
    #[error("screen extent must be non-zero")]
    EmptyScreen,
    /// A coordinate calculation overflowed its representation.
    #[error("coordinate calculation overflowed")]
    CoordinateOverflow,
    /// A caller supplied a displacement outside `i32`.
    #[error("pointer delta is outside the supported range")]
    DeltaOverflow,
    /// A point cannot be represented by core XTEST motion.
    #[error("root point ({x}, {y}) is outside the XTEST i16 range")]
    OutsideXtestRange {
        /// Horizontal coordinate.
        x: i32,
        /// Vertical coordinate.
        y: i32,
    },
    /// A point lies outside the configured root screen.
    #[error("root point is outside the configured screen")]
    OutsideScreen {
        /// Rejected point.
        point: RootPoint,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_addition_checks_both_integer_and_xtest_ranges() -> Result<(), GeometryError> {
        let origin = RootPoint::new(10, 10)?;
        assert_eq!(
            origin.checked_add(PointerDelta::new(5, -7)?),
            RootPoint::new(15, 3)
        );
        assert!(
            RootPoint::new(i32::from(i16::MAX), 0)?
                .checked_add(PointerDelta::new(1, 0)?)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn rectangle_contains_clamps_and_intersects() -> Result<(), GeometryError> {
        let screen = ScreenRect::new(0, 0, 100, 50)?;
        assert!(screen.contains(RootPoint::new(99, 49)?));
        assert!(!screen.contains(RootPoint::new(100, 49)?));
        assert_eq!(
            screen.clamp(RootPoint::new(150, -10)?),
            RootPoint::new(99, 0)?
        );
        assert_eq!(
            screen.intersect(ScreenRect::new(90, 40, 20, 20)?),
            Some(ScreenRect::new(90, 40, 10, 10)?)
        );
        Ok(())
    }
}
