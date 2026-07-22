//! Root-physical geometry translation.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use xenoteer_protocol::{CoordinateSpace, Rect, WindowRect};

use crate::{Result, X11Error};

/// Client geometry translated to the root coordinate space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootGeometryInput {
    /// Checked client rectangle in root-physical pixels.
    pub client_rect: WindowRect,
    /// Core X border width, not included in the client rectangle.
    pub border_width: u16,
    /// Root returned by `GetGeometry`.
    pub geometry_root: Window,
    /// Immediate child beneath the destination found during translation.
    pub root_child: Option<Window>,
}

/// Query client dimensions and translate `(0, 0)` from the window into the
/// nominated root. Parent-relative `GetGeometry.x/y` are intentionally not
/// exposed as root coordinates.
pub fn query_root_geometry<C: Connection>(
    connection: &C,
    root: Window,
    window: Window,
) -> Result<RootGeometryInput> {
    let geometry = connection
        .get_geometry(window)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    if geometry.root != root {
        return Err(X11Error::InvalidSetup(
            "window geometry belongs to a different root",
        ));
    }
    let translated = connection
        .translate_coordinates(window, root, 0, 0)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    if !translated.same_screen {
        return Err(X11Error::InvalidSetup(
            "window coordinates cannot be translated to the nominated root",
        ));
    }
    let rect = Rect::new(
        i32::from(translated.dst_x),
        i32::from(translated.dst_y),
        u32::from(geometry.width),
        u32::from(geometry.height),
    )
    .map_err(|_| X11Error::InvalidSetup("window geometry is empty or overflows"))?;
    let client_rect = WindowRect::new(CoordinateSpace::RootPhysical, rect)
        .map_err(|_| X11Error::InvalidSetup("window geometry exceeds protocol bounds"))?;
    Ok(RootGeometryInput {
        client_rect,
        border_width: geometry.border_width,
        geometry_root: geometry.root,
        root_child: (translated.child != 0).then_some(translated.child),
    })
}
