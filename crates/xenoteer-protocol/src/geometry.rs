//! Checked protocol geometry and coordinate-space declarations.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// A signed two-dimensional point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Point {
    x: i32,
    y: i32,
}

/// Closed request-direction point object.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Point")]
pub(crate) struct StrictPoint {
    x: i32,
    y: i32,
}

pub(crate) fn deserialize_strict_point<'de, D>(deserializer: D) -> Result<Point, D::Error>
where
    D: Deserializer<'de>,
{
    let value = StrictPoint::deserialize(deserializer)?;
    Ok(Point::new(value.x, value.y))
}

impl Point {
    /// Creates a point.
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Returns the x coordinate.
    #[must_use]
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Returns the y coordinate.
    #[must_use]
    pub const fn y(self) -> i32 {
        self.y
    }
}

/// A non-empty unsigned size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Size {
    #[schemars(range(min = 1))]
    width: u32,
    #[schemars(range(min = 1))]
    height: u32,
}

impl Size {
    /// Creates a non-empty size.
    pub fn new(width: u32, height: u32) -> Result<Self, GeometryError> {
        let value = Self { width, height };
        value.validate()?;
        Ok(value)
    }

    /// Validates a deserialized size.
    pub fn validate(self) -> Result<(), GeometryError> {
        if self.width == 0 || self.height == 0 {
            return Err(GeometryError::EmptySize);
        }
        Ok(())
    }

    /// Returns the width.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Returns the height.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }
}

/// A signed origin and non-empty unsigned extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Rect {
    x: i32,
    y: i32,
    #[schemars(range(min = 1))]
    width: u32,
    #[schemars(range(min = 1))]
    height: u32,
}

/// Closed request-direction rectangle object.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Rect")]
pub(crate) struct StrictRect {
    x: i32,
    y: i32,
    #[schemars(range(min = 1))]
    width: u32,
    #[schemars(range(min = 1))]
    height: u32,
}

impl From<StrictRect> for Rect {
    fn from(value: StrictRect) -> Self {
        Self {
            x: value.x,
            y: value.y,
            width: value.width,
            height: value.height,
        }
    }
}

pub(crate) fn deserialize_optional_strict_rect<'de, D>(
    deserializer: D,
) -> Result<Option<Rect>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<StrictRect>::deserialize(deserializer).map(|value| value.map(Into::into))
}

impl Rect {
    /// Creates a non-empty rectangle whose inclusive end coordinates fit `i32`.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, GeometryError> {
        let value = Self {
            x,
            y,
            width,
            height,
        };
        value.validate()?;
        Ok(value)
    }

    /// Validates a rectangle obtained through deserialization.
    pub fn validate(self) -> Result<(), GeometryError> {
        Size::new(self.width, self.height)?;
        let end_x = i64::from(self.x) + i64::from(self.width) - 1;
        let end_y = i64::from(self.y) + i64::from(self.height) - 1;
        if end_x > i64::from(i32::MAX) || end_y > i64::from(i32::MAX) {
            return Err(GeometryError::CoordinateOverflow);
        }
        Ok(())
    }

    /// Returns the origin.
    #[must_use]
    pub const fn origin(self) -> Point {
        Point::new(self.x, self.y)
    }

    /// Returns the extent.
    pub fn size(self) -> Result<Size, GeometryError> {
        Size::new(self.width, self.height)
    }
}

/// The reference frame used by a geometry value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateSpace {
    /// Root-window physical pixels.
    RootPhysical,
    /// Coordinates relative to a client window.
    WindowClient,
    /// Coordinates relative to a window-manager frame.
    WindowFrame,
    /// AT-SPI screen coordinates after profile correlation.
    AtspiScreen,
}

/// Geometry validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum GeometryError {
    /// At least one extent is zero.
    #[error("geometry extent must be non-zero")]
    EmptySize,
    /// The inclusive end coordinate is not representable.
    #[error("geometry end coordinate overflows i32")]
    CoordinateOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_or_overflowing_rectangles() {
        assert_eq!(Rect::new(0, 0, 0, 1), Err(GeometryError::EmptySize));
        assert_eq!(
            Rect::new(i32::MAX, 0, 2, 1),
            Err(GeometryError::CoordinateOverflow)
        );
    }

    #[test]
    fn deserialized_geometry_still_requires_admission_validation() -> Result<(), serde_json::Error>
    {
        let parsed: Rect = serde_json::from_str(r#"{"x":0,"y":0,"width":0,"height":1}"#)?;
        assert_eq!(parsed.validate(), Err(GeometryError::EmptySize));
        Ok(())
    }
}
