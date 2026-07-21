//! Feature-gated libxkbcommon model construction.
//!
//! The default crate requires no native library. Enabling
//! `native-xkbcommon` proves the exact server-derived model through
//! libxkbcommon-x11 and its own XCB connection.

/// Why a keyboard model is unavailable in a portable build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardModelAvailability {
    /// Native model support was compiled in.
    Available,
    /// The optional native dependency was not compiled in.
    FeatureDisabled,
}

/// Return whether this crate was built with the native server model.
#[must_use]
pub const fn availability() -> KeyboardModelAvailability {
    if cfg!(feature = "native-xkbcommon") {
        KeyboardModelAvailability::Available
    } else {
        KeyboardModelAvailability::FeatureDisabled
    }
}

/// One concrete server keycode/layout/level to keysym mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SymbolMapping {
    /// X server keycode.
    pub keycode: u32,
    /// XKB layout index.
    pub layout: u32,
    /// XKB level index.
    pub level: u32,
    /// Raw X11 keysym.
    pub keysym: u32,
}

#[cfg(feature = "native-xkbcommon")]
mod native {
    use std::ffi::CString;

    use x11rb::xcb_ffi::XCBConnection;
    use xkbcommon::xkb;

    use super::SymbolMapping;
    use crate::{Result, X11Error};

    /// XKB model compiled from the core keyboard device on the live X server.
    pub struct NativeKeyboardModel {
        _connection: XCBConnection,
        keymap: xkb::Keymap,
        _state: xkb::State,
        server_major: u16,
        server_minor: u16,
    }

    impl NativeKeyboardModel {
        /// Connect to `display`, negotiate XKB, and compile keymap/state from the
        /// server's core keyboard device.
        pub fn connect(display: &str) -> Result<Self> {
            let display =
                CString::new(display).map_err(|error| X11Error::Keyboard(error.to_string()))?;
            let (connection, _screen_index) = XCBConnection::connect(Some(&display))
                .map_err(|error| X11Error::Keyboard(error.to_string()))?;
            let mut server_major = 0;
            let mut server_minor = 0;
            let mut base_event = 0;
            let mut base_error = 0;
            if !xkb::x11::setup_xkb_extension(
                &connection,
                xkb::x11::MIN_MAJOR_XKB_VERSION,
                xkb::x11::MIN_MINOR_XKB_VERSION,
                xkb::x11::SetupXkbExtensionFlags::NoFlags,
                &mut server_major,
                &mut server_minor,
                &mut base_event,
                &mut base_error,
            ) {
                return Err(X11Error::Keyboard(
                    "server rejected the minimum XKB extension version".to_owned(),
                ));
            }
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let device_id = xkb::x11::get_core_keyboard_device_id(&connection);
            if device_id < 0 {
                return Err(X11Error::Keyboard(
                    "server returned no core keyboard device".to_owned(),
                ));
            }
            let keymap = xkb::x11::keymap_new_from_device(
                &context,
                &connection,
                device_id,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            );
            let state = xkb::x11::state_new_from_device(&keymap, &connection, device_id);
            Ok(Self {
                _connection: connection,
                keymap,
                _state: state,
                server_major,
                server_minor,
            })
        }

        /// Minimum keycode in the server-derived keymap.
        #[must_use]
        pub fn min_keycode(&self) -> u32 {
            self.keymap.min_keycode().raw()
        }

        /// Maximum keycode in the server-derived keymap.
        #[must_use]
        pub fn max_keycode(&self) -> u32 {
            self.keymap.max_keycode().raw()
        }

        /// Negotiated XKB protocol version.
        #[must_use]
        pub const fn server_version(&self) -> (u16, u16) {
            (self.server_major, self.server_minor)
        }

        /// Return the first nonzero mapping in deterministic
        /// keycode/layout/level order.
        #[must_use]
        pub fn first_symbol_mapping(&self) -> Option<SymbolMapping> {
            for raw_keycode in self.min_keycode()..=self.max_keycode() {
                let keycode = xkb::Keycode::new(raw_keycode);
                for layout in 0..self.keymap.num_layouts_for_key(keycode) {
                    for level in 0..self.keymap.num_levels_for_key(keycode, layout) {
                        if let Some(keysym) = self
                            .keymap
                            .key_get_syms_by_level(keycode, layout, level)
                            .iter()
                            .find(|keysym| keysym.raw() != 0)
                        {
                            return Some(SymbolMapping {
                                keycode: raw_keycode,
                                layout,
                                level,
                                keysym: keysym.raw(),
                            });
                        }
                    }
                }
            }
            None
        }
    }
}

#[cfg(feature = "native-xkbcommon")]
pub use native::NativeKeyboardModel;
