//! Explicit X display connection and capability inventory.

use std::collections::BTreeMap;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, EventMask, GetInputFocusReply, Window,
};
use x11rb::rust_connection::RustConnection;

use crate::{Result, X11Error};

/// X11 extensions whose presence affects a declared Xenoteer capability.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ExtensionName {
    /// XTEST synthetic device input.
    XTest,
    /// X Keyboard Extension.
    XKeyboard,
    /// XFIXES cursor and selection notifications.
    XFixes,
    /// X DAMAGE dirty-region events.
    Damage,
    /// MIT shared-memory image transport.
    MitShm,
    /// Composite redirected window surfaces.
    Composite,
    /// RandR display configuration.
    RandR,
}

impl ExtensionName {
    /// Protocol extension name sent in `QueryExtension`.
    #[must_use]
    pub const fn wire_name(self) -> &'static [u8] {
        match self {
            Self::XTest => b"XTEST",
            Self::XKeyboard => b"XKEYBOARD",
            Self::XFixes => b"XFIXES",
            Self::Damage => b"DAMAGE",
            Self::MitShm => b"MIT-SHM",
            Self::Composite => b"Composite",
            Self::RandR => b"RANDR",
        }
    }

    /// Stable diagnostic name.
    #[must_use]
    pub const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::XTest => "XTEST",
            Self::XKeyboard => "XKEYBOARD",
            Self::XFixes => "XFIXES",
            Self::Damage => "DAMAGE",
            Self::MitShm => "MIT-SHM",
            Self::Composite => "Composite",
            Self::RandR => "RANDR",
        }
    }
}

/// Server metadata returned by `QueryExtension`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtensionInfo {
    /// Whether the server advertises the extension.
    pub present: bool,
    /// Major opcode allocated by the server.
    pub major_opcode: u8,
    /// First extension event number.
    pub first_event: u8,
    /// First extension error number.
    pub first_error: u8,
}

/// Complete extension probe performed for every new role connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionInventory {
    values: BTreeMap<ExtensionName, ExtensionInfo>,
}

impl ExtensionInventory {
    /// Return inventory information for a known extension.
    #[must_use]
    pub fn get(&self, name: ExtensionName) -> Option<&ExtensionInfo> {
        self.values.get(&name)
    }

    /// Require an extension to be represented and advertised as present.
    pub fn require(&self, name: ExtensionName) -> Result<&ExtensionInfo> {
        match self.get(name) {
            Some(info) if info.present => Ok(info),
            Some(_) => Err(X11Error::MissingExtension(name.diagnostic_name())),
            None => Err(X11Error::InvalidSetup("extension inventory is incomplete")),
        }
    }

    /// Iterate in stable enum order.
    pub fn iter(&self) -> impl Iterator<Item = (ExtensionName, &ExtensionInfo)> {
        self.values.iter().map(|(name, info)| (*name, info))
    }
}

/// Immutable setup information associated with one role connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XConnectionInfo {
    /// Explicit display string used to connect.
    pub display: String,
    /// Selected screen index.
    pub screen_index: usize,
    /// Root window for the selected screen.
    pub root: Window,
    /// Root visual identifier.
    pub root_visual: u32,
    /// Root depth.
    pub root_depth: u8,
    /// Root width in physical pixels.
    pub width_px: u16,
    /// Root height in physical pixels.
    pub height_px: u16,
    /// Core minimum keycode.
    pub min_keycode: u8,
    /// Core maximum keycode.
    pub max_keycode: u8,
    /// Extension capability inventory.
    pub extensions: ExtensionInventory,
}

/// An owned role connection plus the setup snapshot gathered at connection time.
pub struct OpenedConnection {
    /// Single-owner x11rb connection.
    pub connection: RustConnection,
    /// Immutable setup and capability evidence.
    pub info: XConnectionInfo,
}

impl OpenedConnection {
    /// Perform a reply-producing core request. This proves more than `flush`,
    /// which only proves buffered bytes were written.
    pub fn core_roundtrip(&self) -> Result<Window> {
        let GetInputFocusReply { focus, .. } = self
            .connection
            .get_input_focus()
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .reply()
            .map_err(|error| X11Error::Reply(error.to_string()))?;
        Ok(focus)
    }

    /// Select root events for this connection only and flush the request.
    pub fn select_root_events(&self, event_mask: EventMask) -> Result<()> {
        self.connection
            .change_window_attributes(
                self.info.root,
                &ChangeWindowAttributesAux::new().event_mask(event_mask),
            )
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .check()
            .map_err(|error| X11Error::Reply(error.to_string()))?;
        self.connection
            .flush()
            .map_err(|error| X11Error::Connection(error.to_string()))?;
        Ok(())
    }
}

/// Open an explicit display and inventory all release-one extension candidates.
pub fn connect(display: &str) -> Result<OpenedConnection> {
    if display.is_empty() {
        return Err(X11Error::Connect("display string is empty".to_owned()));
    }
    let (connection, screen_index) =
        x11rb::connect(Some(display)).map_err(|error| X11Error::Connect(error.to_string()))?;
    let setup = connection.setup();
    let screen = setup
        .roots
        .get(screen_index)
        .ok_or(X11Error::InvalidSetup("selected screen index is absent"))?;
    let extensions = inventory_extensions(&connection)?;
    let info = XConnectionInfo {
        display: display.to_owned(),
        screen_index,
        root: screen.root,
        root_visual: screen.root_visual,
        root_depth: screen.root_depth,
        width_px: screen.width_in_pixels,
        height_px: screen.height_in_pixels,
        min_keycode: setup.min_keycode,
        max_keycode: setup.max_keycode,
        extensions,
    };
    Ok(OpenedConnection { connection, info })
}

fn inventory_extensions(connection: &RustConnection) -> Result<ExtensionInventory> {
    const NAMES: [ExtensionName; 7] = [
        ExtensionName::XTest,
        ExtensionName::XKeyboard,
        ExtensionName::XFixes,
        ExtensionName::Damage,
        ExtensionName::MitShm,
        ExtensionName::Composite,
        ExtensionName::RandR,
    ];
    let mut values = BTreeMap::new();
    for name in NAMES {
        let reply = connection
            .query_extension(name.wire_name())
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .reply()
            .map_err(|error| X11Error::Reply(error.to_string()))?;
        values.insert(
            name,
            ExtensionInfo {
                present: reply.present,
                major_opcode: reply.major_opcode,
                first_event: reply.first_event,
                first_error: reply.first_error,
            },
        );
    }
    Ok(ExtensionInventory { values })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{ExtensionInfo, ExtensionInventory, ExtensionName};
    use crate::X11Error;

    fn inventory_with_xtest(present: bool) -> ExtensionInventory {
        ExtensionInventory {
            values: BTreeMap::from([(
                ExtensionName::XTest,
                ExtensionInfo {
                    present,
                    major_opcode: 132,
                    first_event: 0,
                    first_error: 0,
                },
            )]),
        }
    }

    #[test]
    fn require_rejects_known_but_absent_extension() {
        let inventory = inventory_with_xtest(false);
        assert!(matches!(
            inventory.require(ExtensionName::XTest),
            Err(X11Error::MissingExtension("XTEST"))
        ));
    }

    #[test]
    fn require_returns_known_present_extension() -> Result<(), Box<dyn std::error::Error>> {
        let inventory = inventory_with_xtest(true);
        let extension = inventory.require(ExtensionName::XTest)?;
        assert!(extension.present);
        assert_eq!(extension.major_opcode, 132);
        Ok(())
    }
}
