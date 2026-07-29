//! Bounded XFIXES cursor validation and premultiplied-ARGB composition.

use xenoteer_protocol::{CursorCaptureEvidence, Rect};

use crate::{Result, X11Error};

const MAX_CURSOR_DIMENSION: u16 = 512;
const MAX_CURSOR_PIXELS: usize = 512 * 512;

#[derive(Clone, Eq, PartialEq)]
pub(super) struct CursorSnapshot {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub xhot: u16,
    pub yhot: u16,
    pub serial: u32,
    pub premultiplied_argb: Vec<u32>,
}

impl core::fmt::Debug for CursorSnapshot {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CursorSnapshot")
            .field("x", &self.x)
            .field("y", &self.y)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("xhot", &self.xhot)
            .field("yhot", &self.yhot)
            .field("serial", &self.serial)
            .field("pixels", &self.premultiplied_argb.len())
            .finish()
    }
}

impl CursorSnapshot {
    pub fn validate(&self) -> Result<()> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_CURSOR_DIMENSION
            || self.height > MAX_CURSOR_DIMENSION
            || self.xhot >= self.width
            || self.yhot >= self.height
        {
            return Err(X11Error::Pixel("invalid XFIXES cursor geometry".to_owned()));
        }
        let pixels = usize::from(self.width)
            .checked_mul(usize::from(self.height))
            .ok_or_else(|| X11Error::Pixel("XFIXES cursor pixel count overflow".to_owned()))?;
        if pixels > MAX_CURSOR_PIXELS || self.premultiplied_argb.len() != pixels {
            return Err(X11Error::Pixel(
                "invalid bounded XFIXES cursor image length".to_owned(),
            ));
        }
        if self.premultiplied_argb.iter().any(|pixel| {
            let alpha = pixel >> 24;
            ((pixel >> 16) & 0xff) > alpha
                || ((pixel >> 8) & 0xff) > alpha
                || (pixel & 0xff) > alpha
        }) {
            return Err(X11Error::Pixel(
                "XFIXES cursor channels are not premultiplied ARGB".to_owned(),
            ));
        }
        Ok(())
    }

    fn changed_from(&self, other: &Self) -> bool {
        self.serial != other.serial
            || self.x != other.x
            || self.y != other.y
            || self.width != other.width
            || self.height != other.height
            || self.xhot != other.xhot
            || self.yhot != other.yhot
    }
}

pub(super) fn compose_cursor(
    frame: &mut [u8],
    frame_region: Rect,
    before: &CursorSnapshot,
    after: &CursorSnapshot,
) -> Result<CursorCaptureEvidence> {
    before.validate()?;
    after.validate()?;
    let frame_size = frame_region
        .size()
        .map_err(|_| X11Error::Pixel("invalid cursor frame region".to_owned()))?;
    let expected = u64::from(frame_size.width())
        .checked_mul(u64::from(frame_size.height()))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| X11Error::Pixel("cursor frame length overflow".to_owned()))?;
    if frame.len() != expected {
        return Err(X11Error::Pixel(
            "cursor frame BGRA8 length mismatch".to_owned(),
        ));
    }

    let cursor_left = i32::from(before.x) - i32::from(before.xhot);
    let cursor_top = i32::from(before.y) - i32::from(before.yhot);
    let frame_left = frame_region.origin().x();
    let frame_top = frame_region.origin().y();
    let frame_right = i64::from(frame_left) + i64::from(frame_size.width());
    let frame_bottom = i64::from(frame_top) + i64::from(frame_size.height());
    let cursor_right = i64::from(cursor_left) + i64::from(before.width);
    let cursor_bottom = i64::from(cursor_top) + i64::from(before.height);
    let left = i64::from(frame_left).max(i64::from(cursor_left));
    let top = i64::from(frame_top).max(i64::from(cursor_top));
    let right = frame_right.min(cursor_right);
    let bottom = frame_bottom.min(cursor_bottom);
    let mut composited = false;
    if left < right && top < bottom {
        for root_y in top..bottom {
            for root_x in left..right {
                let source_x = usize::try_from(root_x - i64::from(cursor_left))
                    .map_err(|_| X11Error::Pixel("cursor source x overflow".to_owned()))?;
                let source_y = usize::try_from(root_y - i64::from(cursor_top))
                    .map_err(|_| X11Error::Pixel("cursor source y overflow".to_owned()))?;
                let source_index = source_y
                    .checked_mul(usize::from(before.width))
                    .and_then(|row| row.checked_add(source_x))
                    .ok_or_else(|| X11Error::Pixel("cursor source offset overflow".to_owned()))?;
                let pixel = before.premultiplied_argb[source_index];
                let alpha = (pixel >> 24) & 0xff;
                if alpha == 0 {
                    continue;
                }
                let destination_x = usize::try_from(root_x - i64::from(frame_left))
                    .map_err(|_| X11Error::Pixel("cursor destination x overflow".to_owned()))?;
                let destination_y = usize::try_from(root_y - i64::from(frame_top))
                    .map_err(|_| X11Error::Pixel("cursor destination y overflow".to_owned()))?;
                let destination = destination_y
                    .checked_mul(usize::try_from(frame_size.width()).map_err(|_| {
                        X11Error::Pixel("cursor frame width conversion failed".to_owned())
                    })?)
                    .and_then(|row| row.checked_add(destination_x))
                    .and_then(|offset| offset.checked_mul(4))
                    .ok_or_else(|| {
                        X11Error::Pixel("cursor destination offset overflow".to_owned())
                    })?;
                let inverse = 255 - alpha;
                let source_bgra = [pixel & 0xff, (pixel >> 8) & 0xff, (pixel >> 16) & 0xff];
                for (channel, source) in source_bgra.into_iter().enumerate() {
                    let destination_channel = u32::from(frame[destination + channel]);
                    let over = source
                        .checked_add((destination_channel * inverse + 127) / 255)
                        .unwrap_or(255)
                        .min(255);
                    frame[destination + channel] = u8::try_from(over).unwrap_or(255);
                }
                frame[destination + 3] = 255;
                composited = true;
            }
        }
    }
    Ok(CursorCaptureEvidence {
        requested: true,
        composited,
        serial_before: Some(before.serial),
        serial_after: Some(after.serial),
        moved_during_capture: before.changed_from(after),
    })
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
