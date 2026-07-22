//! Bounded, advisory X11 screen-damage event contracts.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CoordinateSpace, DesktopGeneration, DesktopId, Rect, WindowRect};

/// Public event topic for coalesced root-framebuffer damage hints.
pub const SCREEN_DAMAGED_TOPIC: &str = "screen.damaged";
/// Maximum dirty rectangles retained in one coalesced event.
pub const MAX_SCREEN_DAMAGE_RECTANGLES: usize = 64;

/// How aggressively the source had to collapse accumulated dirty rectangles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScreenDamageCoverage {
    /// The bounded rectangle list preserves the source regions.
    Regions,
    /// Excess source regions were replaced by one conservative bounding box.
    BoundingBox,
    /// Damage was conservatively promoted to the complete root framebuffer.
    FullScreen,
}

/// One low-volume, advisory root-framebuffer change hint.
///
/// DAMAGE does not contain pixels and is never a substitute for a screenshot
/// or a state postcondition. Rectangles conservatively include changed pixels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScreenDamageEvent {
    /// Desktop resource whose framebuffer changed.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime that produced the notification.
    pub desktop_generation: DesktopGeneration,
    /// Complete root framebuffer in root-physical coordinates.
    pub root_region: WindowRect,
    /// Non-empty, root-clipped dirty rectangles.
    #[schemars(length(min = 1, max = MAX_SCREEN_DAMAGE_RECTANGLES))]
    pub damaged_regions: Vec<WindowRect>,
    /// Conservative region-reduction evidence.
    pub coverage: ScreenDamageCoverage,
    /// Number of raw DAMAGE notifications represented by this event.
    #[schemars(range(min = 1))]
    pub coalesced_notifications: u32,
}

impl ScreenDamageEvent {
    /// Rejects invalid scope, coordinate spaces, regions, and collapse claims.
    pub fn validate(&self) -> Result<(), ScreenDamageValidationError> {
        if self.desktop_id.as_uuid().is_nil() || self.desktop_generation.as_uuid().is_nil() {
            return Err(ScreenDamageValidationError::NilIdentifier);
        }
        validate_root_rect(self.root_region)?;
        if self.damaged_regions.is_empty()
            || self.damaged_regions.len() > MAX_SCREEN_DAMAGE_RECTANGLES
            || self.coalesced_notifications == 0
        {
            return Err(ScreenDamageValidationError::Regions);
        }
        for (index, region) in self.damaged_regions.iter().copied().enumerate() {
            validate_root_rect(region)?;
            if !rect_contains(self.root_region.rect, region.rect)
                || self.damaged_regions[..index].contains(&region)
            {
                return Err(ScreenDamageValidationError::Regions);
            }
        }
        match self.coverage {
            ScreenDamageCoverage::Regions => {}
            ScreenDamageCoverage::BoundingBox => {
                if self.damaged_regions.len() != 1 || self.damaged_regions[0] == self.root_region {
                    return Err(ScreenDamageValidationError::Coverage);
                }
            }
            ScreenDamageCoverage::FullScreen => {
                if self.damaged_regions.as_slice() != [self.root_region] {
                    return Err(ScreenDamageValidationError::Coverage);
                }
            }
        }
        Ok(())
    }
}

fn validate_root_rect(rect: WindowRect) -> Result<(), ScreenDamageValidationError> {
    rect.validate()
        .map_err(|_| ScreenDamageValidationError::Geometry)?;
    if rect.coordinate_space != CoordinateSpace::RootPhysical {
        return Err(ScreenDamageValidationError::CoordinateSpace);
    }
    Ok(())
}

fn rect_contains(container: Rect, candidate: Rect) -> bool {
    let container_origin = container.origin();
    let candidate_origin = candidate.origin();
    let (Ok(container_size), Ok(candidate_size)) = (container.size(), candidate.size()) else {
        return false;
    };
    let container_end_x = i64::from(container_origin.x()) + i64::from(container_size.width());
    let container_end_y = i64::from(container_origin.y()) + i64::from(container_size.height());
    let candidate_end_x = i64::from(candidate_origin.x()) + i64::from(candidate_size.width());
    let candidate_end_y = i64::from(candidate_origin.y()) + i64::from(candidate_size.height());
    candidate_origin.x() >= container_origin.x()
        && candidate_origin.y() >= container_origin.y()
        && candidate_end_x <= container_end_x
        && candidate_end_y <= container_end_y
}

/// Invalid public screen-damage evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ScreenDamageValidationError {
    /// Desktop scope contains a nil identifier.
    #[error("screen-damage scope contains a nil identifier")]
    NilIdentifier,
    /// One rectangle is empty, overflowing, or exceeds the X11 ceiling.
    #[error("screen-damage geometry is invalid")]
    Geometry,
    /// A rectangle is not tagged as root-physical.
    #[error("screen-damage rectangles must use root-physical coordinates")]
    CoordinateSpace,
    /// Region count, containment, uniqueness, or notification count is invalid.
    #[error("screen-damage rectangle set is invalid")]
    Regions,
    /// Coverage classification contradicts the region payload.
    #[error("screen-damage collapse evidence is inconsistent")]
    Coverage,
}

#[cfg(test)]
#[path = "damage_tests.rs"]
mod tests;
