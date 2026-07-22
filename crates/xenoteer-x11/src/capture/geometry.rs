//! Pure capture-region resolution with explicit coordinate spaces.

use x11rb::protocol::xproto::Drawable;
use xenoteer_protocol::{Rect, ScreenshotTarget, WindowCaptureSpace};

use super::{CaptureActorFailureKind, RawWindowCaptureGeometry};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ResolvedCaptureArea {
    pub drawable: Drawable,
    pub drawable_x: i16,
    pub drawable_y: i16,
    pub width: u16,
    pub height: u16,
    pub root_region: Rect,
}

pub(super) fn resolve_root_area(
    root: Drawable,
    root_width: u16,
    root_height: u16,
    region: Option<Rect>,
) -> Result<ResolvedCaptureArea, CaptureActorFailureKind> {
    let root_bounds = Rect::new(0, 0, u32::from(root_width), u32::from(root_height))
        .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?;
    let selected = region.unwrap_or(root_bounds);
    if !contains(root_bounds, selected) {
        return Err(CaptureActorFailureKind::RegionOutOfBounds);
    }
    area(root, selected.origin().x(), selected.origin().y(), selected)
}

pub(super) fn resolve_window_area(
    target: &ScreenshotTarget,
    region: Option<Rect>,
    root_width: u16,
    root_height: u16,
    geometry: RawWindowCaptureGeometry,
) -> Result<ResolvedCaptureArea, CaptureActorFailureKind> {
    if !geometry.viewable {
        return Err(CaptureActorFailureKind::WindowNotViewable);
    }
    match target {
        ScreenshotTarget::WindowVisible {
            coordinate_space, ..
        } => {
            let base = match coordinate_space {
                WindowCaptureSpace::Client => geometry.client_root,
                WindowCaptureSpace::Frame => geometry
                    .frame_root
                    .ok_or(CaptureActorFailureKind::RegionOutOfBounds)?,
            };
            let base_size = base
                .size()
                .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?;
            let local_bounds = Rect::new(0, 0, base_size.width(), base_size.height())
                .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?;
            let local = region.unwrap_or(local_bounds);
            if !contains(local_bounds, local) {
                return Err(CaptureActorFailureKind::RegionOutOfBounds);
            }
            let translated = translate(local, base.origin().x(), base.origin().y())?;
            let root_bounds = Rect::new(0, 0, u32::from(root_width), u32::from(root_height))
                .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?;
            let clipped = intersect(translated, root_bounds)
                .ok_or(CaptureActorFailureKind::RegionOutOfBounds)?;
            area(
                geometry.root,
                clipped.origin().x(),
                clipped.origin().y(),
                clipped,
            )
        }
        ScreenshotTarget::WindowDrawable { .. } => {
            let client_size = geometry
                .client_root
                .size()
                .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?;
            let local_bounds = Rect::new(0, 0, client_size.width(), client_size.height())
                .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?;
            let local = region.unwrap_or(local_bounds);
            if !contains(local_bounds, local) {
                return Err(CaptureActorFailureKind::RegionOutOfBounds);
            }
            let root_region = translate(
                local,
                geometry.client_root.origin().x(),
                geometry.client_root.origin().y(),
            )?;
            let root_bounds = Rect::new(0, 0, u32::from(root_width), u32::from(root_height))
                .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?;
            if !contains(root_bounds, root_region) {
                return Err(CaptureActorFailureKind::RegionOutOfBounds);
            }
            area(
                geometry.window,
                local.origin().x(),
                local.origin().y(),
                root_region,
            )
        }
        ScreenshotTarget::Root => Err(CaptureActorFailureKind::InvalidTarget),
    }
}

fn area(
    drawable: Drawable,
    x: i32,
    y: i32,
    root_region: Rect,
) -> Result<ResolvedCaptureArea, CaptureActorFailureKind> {
    let size = root_region
        .size()
        .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?;
    Ok(ResolvedCaptureArea {
        drawable,
        drawable_x: i16::try_from(x).map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?,
        drawable_y: i16::try_from(y).map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?,
        width: u16::try_from(size.width())
            .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?,
        height: u16::try_from(size.height())
            .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?,
        root_region,
    })
}

fn contains(outer: Rect, inner: Rect) -> bool {
    let outer_size = match outer.size() {
        Ok(size) => size,
        Err(_) => return false,
    };
    let inner_size = match inner.size() {
        Ok(size) => size,
        Err(_) => return false,
    };
    let outer_left = i64::from(outer.origin().x());
    let outer_top = i64::from(outer.origin().y());
    let inner_left = i64::from(inner.origin().x());
    let inner_top = i64::from(inner.origin().y());
    inner_left >= outer_left
        && inner_top >= outer_top
        && inner_left + i64::from(inner_size.width()) <= outer_left + i64::from(outer_size.width())
        && inner_top + i64::from(inner_size.height()) <= outer_top + i64::from(outer_size.height())
}

fn intersect(first: Rect, second: Rect) -> Option<Rect> {
    let first_size = first.size().ok()?;
    let second_size = second.size().ok()?;
    let left = i64::from(first.origin().x()).max(i64::from(second.origin().x()));
    let top = i64::from(first.origin().y()).max(i64::from(second.origin().y()));
    let right = (i64::from(first.origin().x()) + i64::from(first_size.width()))
        .min(i64::from(second.origin().x()) + i64::from(second_size.width()));
    let bottom = (i64::from(first.origin().y()) + i64::from(first_size.height()))
        .min(i64::from(second.origin().y()) + i64::from(second_size.height()));
    if left >= right || top >= bottom {
        return None;
    }
    Rect::new(
        i32::try_from(left).ok()?,
        i32::try_from(top).ok()?,
        u32::try_from(right - left).ok()?,
        u32::try_from(bottom - top).ok()?,
    )
    .ok()
}

fn translate(local: Rect, offset_x: i32, offset_y: i32) -> Result<Rect, CaptureActorFailureKind> {
    let size = local
        .size()
        .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)?;
    let x = local
        .origin()
        .x()
        .checked_add(offset_x)
        .ok_or(CaptureActorFailureKind::RegionOutOfBounds)?;
    let y = local
        .origin()
        .y()
        .checked_add(offset_y)
        .ok_or(CaptureActorFailureKind::RegionOutOfBounds)?;
    Rect::new(x, y, size.width(), size.height())
        .map_err(|_| CaptureActorFailureKind::RegionOutOfBounds)
}

#[cfg(test)]
#[path = "geometry_tests.rs"]
mod tests;
