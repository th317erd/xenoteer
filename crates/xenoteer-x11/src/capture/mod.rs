//! Correctness-first core `GetImage` decoding.

mod image;
mod pixel;

pub use image::{CaptureImageLimits, ResizeFilter, encode_png_bgra8, resize_bgra8};
pub use pixel::{ByteOrder, PixelFormat, PixelVisualClass, RawImage, decode_bgra8};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, Drawable, ImageFormat};
use x11rb::rust_connection::RustConnection;

use crate::{Result, X11Error, XConnectionInfo};

fn with_capture_preflight<T>(
    width: u16,
    height: u16,
    request: impl FnOnce() -> Result<T>,
) -> Result<T> {
    image::validate_hard_capture_dimensions(u32::from(width), u32::from(height), "capture")?;
    request()
}

/// Capture a drawable with core `GetImage` and normalize it to opaque BGRA8.
///
/// This is a request/reply spike, not the future bounded capture actor. It
/// deliberately derives storage format, byte order, and masks from server setup.
pub fn get_image_bgra8(
    connection: &RustConnection,
    info: &XConnectionInfo,
    drawable: Drawable,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> Result<Vec<u8>> {
    let reply = with_capture_preflight(width, height, || {
        connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                drawable,
                x,
                y,
                width,
                height,
                u32::MAX,
            )
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .reply()
            .map_err(|error| X11Error::Reply(error.to_string()))
    })?;
    let setup = connection.setup();
    let pixmap_format = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == reply.depth)
        .ok_or_else(|| X11Error::Pixel("reply depth has no setup pixmap format".to_owned()))?;
    let screen = setup
        .roots
        .get(info.screen_index)
        .ok_or(X11Error::InvalidSetup("selected screen index is absent"))?;
    let visual_id = if reply.visual == 0 {
        info.root_visual
    } else {
        reply.visual
    };
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual_id == visual_id)
        .ok_or_else(|| X11Error::Pixel("GetImage visual is absent from setup".to_owned()))?;
    let byte_order = if setup.image_byte_order == x11rb::protocol::xproto::ImageOrder::LSB_FIRST {
        ByteOrder::LeastSignificantFirst
    } else if setup.image_byte_order == x11rb::protocol::xproto::ImageOrder::MSB_FIRST {
        ByteOrder::MostSignificantFirst
    } else {
        return Err(X11Error::Pixel(
            "unknown server image byte order".to_owned(),
        ));
    };
    let format = PixelFormat {
        visual_class: PixelVisualClass::from_wire_value(visual.class.into()),
        depth: reply.depth,
        bits_per_pixel: pixmap_format.bits_per_pixel,
        scanline_pad: pixmap_format.scanline_pad,
        byte_order,
        red_mask: visual.red_mask,
        green_mask: visual.green_mask,
        blue_mask: visual.blue_mask,
    };
    let image = RawImage::new(u32::from(width), u32::from(height), format, reply.data)?;
    decode_bgra8(&image)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::with_capture_preflight;

    #[test]
    fn oversized_capture_is_rejected_before_request_closure_runs() {
        let request_ran = Cell::new(false);
        let result = with_capture_preflight(8_193, 1, || {
            request_ran.set(true);
            Ok(())
        });
        assert!(result.is_err());
        assert!(!request_ran.get());
    }

    #[test]
    fn excessive_capture_pixels_are_rejected_before_request_closure_runs() {
        let request_ran = Cell::new(false);
        let result = with_capture_preflight(5_000, 5_000, || {
            request_ran.set(true);
            Ok(())
        });
        assert!(result.is_err());
        assert!(!request_ran.get());
    }
}
