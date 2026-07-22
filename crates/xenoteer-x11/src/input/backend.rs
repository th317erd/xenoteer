//! Narrow single-owner backend seam used by the actor and failure-injection tests.

use std::time::Duration;

use x11rb::connection::Connection as _;
use x11rb::cookie::VoidCookie;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, Mapping,
};
use x11rb::protocol::xtest::ConnectionExt as _;

use xenoteer_core::domain::RootPoint;
use xenoteer_core::input::{ButtonMapping, PhysicalButton, PhysicalKey};
use xenoteer_core::window_geometry::WindowGeometryContext;
use xenoteer_protocol::{CoordinateSpace, Rect, WindowFrameExtents, WindowGeometry, WindowRect};

use crate::error::classify_reply_error;
use crate::{ExtensionName, OpenedConnection, X11Error, connect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackendFaultKind {
    Connection,
    Request,
    Capability,
}

#[derive(Debug, thiserror::Error)]
#[error("input backend {kind:?} failure: {detail}")]
pub(super) struct BackendFault {
    pub(super) kind: BackendFaultKind,
    #[allow(dead_code)]
    detail: String,
}

impl BackendFault {
    pub(super) fn new(kind: BackendFaultKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct BackendStartup {
    pub(super) button_mapping: ButtonMapping,
    pub(super) min_keycode: u8,
    pub(super) max_keycode: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PointerObservation {
    pub(super) pointer: RootPoint,
    pub(super) logical_buttons_1_to_5: [bool; 5],
}

#[derive(Debug, Clone, Default)]
pub(super) struct DrainedEvents {
    pub(super) pointer_mapping: Option<ButtonMapping>,
    pub(super) keyboard_mapping_changed: bool,
    pub(super) xkb_model_changed: bool,
    pub(super) xkb_state_changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackendEvent {
    Motion {
        point: RootPoint,
        delay_ms: u32,
    },
    Button {
        button: PhysicalButton,
        pressed: bool,
        delay_ms: u32,
    },
    Key {
        key: PhysicalKey,
        pressed: bool,
        delay_ms: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CoreKeyboardMapping {
    pub(super) keysyms_per_keycode: u8,
    pub(super) keysyms: Vec<u32>,
}

pub(super) trait InputBackend: Send + 'static {
    type Cookie<'a>
    where
        Self: 'a;

    fn startup(&self) -> Result<BackendStartup, BackendFault>;
    fn drain_events(&self) -> Result<DrainedEvents, BackendFault>;
    fn send_event(&self, event: BackendEvent) -> Result<Self::Cookie<'_>, BackendFault>;
    fn check_cookie(cookie: Self::Cookie<'_>) -> Result<(), BackendFault>;
    fn observe_pointer(&self) -> Result<PointerObservation, BackendFault>;
    fn observe_keys(&self) -> Result<Vec<PhysicalKey>, BackendFault>;

    fn wait_for_input_delay(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn observe_window_geometry(&self, _window: u32) -> Result<WindowGeometryContext, BackendFault> {
        Err(BackendFault::new(
            BackendFaultKind::Capability,
            "window geometry observation is unavailable",
        ))
    }

    fn read_keyboard_mapping(
        &self,
        _key: PhysicalKey,
    ) -> Result<CoreKeyboardMapping, BackendFault> {
        Err(BackendFault::new(
            BackendFaultKind::Capability,
            "core keyboard mapping mutation is unavailable",
        ))
    }

    fn write_keyboard_mapping(
        &self,
        _key: PhysicalKey,
        _mapping: &CoreKeyboardMapping,
    ) -> Result<(), BackendFault> {
        Err(BackendFault::new(
            BackendFaultKind::Capability,
            "core keyboard mapping mutation is unavailable",
        ))
    }
}

pub(super) struct X11InputBackend {
    opened: OpenedConnection,
    net_frame_extents: Atom,
}

impl X11InputBackend {
    pub(super) fn open(display: &str) -> Result<Self, BackendFault> {
        let opened = connect(display)
            .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?;
        let net_frame_extents = opened
            .connection
            .intern_atom(false, b"_NET_FRAME_EXTENTS")
            .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?
            .reply()
            .map_err(classify_backend_reply)?
            .atom;
        Ok(Self {
            opened,
            net_frame_extents,
        })
    }

    fn pointer_mapping(&self) -> Result<ButtonMapping, BackendFault> {
        let reply = self
            .opened
            .connection
            .get_pointer_mapping()
            .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?
            .reply()
            .map_err(classify_backend_reply)?;
        ButtonMapping::from_server(&reply.map)
            .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))
    }
}

fn classify_backend_reply(error: x11rb::errors::ReplyError) -> BackendFault {
    let error = classify_reply_error(error);
    match error {
        X11Error::Connection(detail) => BackendFault::new(BackendFaultKind::Connection, detail),
        other => BackendFault::new(BackendFaultKind::Request, other.to_string()),
    }
}

impl InputBackend for X11InputBackend {
    type Cookie<'a> = VoidCookie<'a, x11rb::rust_connection::RustConnection>;

    fn startup(&self) -> Result<BackendStartup, BackendFault> {
        self.opened
            .info
            .extensions
            .require(ExtensionName::XTest)
            .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))?;
        let version = self
            .opened
            .connection
            .xtest_get_version(2, 2)
            .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?
            .reply()
            .map_err(classify_backend_reply)?;
        if !xtest_version_supported(version.major_version, version.minor_version) {
            return Err(BackendFault::new(
                BackendFaultKind::Capability,
                "XTEST 2.2 or newer is required",
            ));
        }
        Ok(BackendStartup {
            button_mapping: self.pointer_mapping()?,
            min_keycode: self.opened.info.min_keycode,
            max_keycode: self.opened.info.max_keycode,
        })
    }

    fn drain_events(&self) -> Result<DrainedEvents, BackendFault> {
        let mut pointer_mapping_changed = false;
        let mut drained = DrainedEvents::default();
        loop {
            let event = self.opened.connection.poll_for_event().map_err(|error| {
                BackendFault::new(BackendFaultKind::Connection, error.to_string())
            })?;
            match event {
                Some(Event::Error(error)) => {
                    return Err(BackendFault::new(
                        BackendFaultKind::Request,
                        format!("{error:?}"),
                    ));
                }
                Some(Event::MappingNotify(event)) if event.request == Mapping::POINTER => {
                    pointer_mapping_changed = true;
                }
                Some(Event::MappingNotify(_)) => drained.keyboard_mapping_changed = true,
                Some(Event::XkbNewKeyboardNotify(_) | Event::XkbMapNotify(_)) => {
                    drained.xkb_model_changed = true;
                }
                Some(Event::XkbStateNotify(_)) => drained.xkb_state_changed = true,
                Some(_) => {}
                None => break,
            }
        }
        if pointer_mapping_changed {
            drained.pointer_mapping = Some(self.pointer_mapping()?);
        }
        Ok(drained)
    }

    fn send_event(&self, event: BackendEvent) -> Result<Self::Cookie<'_>, BackendFault> {
        let (event_type, detail, delay_ms, x, y) = match event {
            BackendEvent::Motion { point, delay_ms } => (
                MOTION_NOTIFY_EVENT,
                0,
                delay_ms,
                i16::try_from(point.x()).map_err(|error| {
                    BackendFault::new(BackendFaultKind::Capability, error.to_string())
                })?,
                i16::try_from(point.y()).map_err(|error| {
                    BackendFault::new(BackendFaultKind::Capability, error.to_string())
                })?,
            ),
            BackendEvent::Button {
                button,
                pressed,
                delay_ms,
            } => (
                if pressed {
                    BUTTON_PRESS_EVENT
                } else {
                    BUTTON_RELEASE_EVENT
                },
                button.detail(),
                delay_ms,
                0,
                0,
            ),
            BackendEvent::Key {
                key,
                pressed,
                delay_ms,
            } => (
                if pressed {
                    KEY_PRESS_EVENT
                } else {
                    KEY_RELEASE_EVENT
                },
                key.keycode(),
                delay_ms,
                0,
                0,
            ),
        };
        self.opened
            .connection
            .xtest_fake_input(event_type, detail, delay_ms, self.opened.info.root, x, y, 0)
            .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))
    }

    fn check_cookie(cookie: Self::Cookie<'_>) -> Result<(), BackendFault> {
        cookie.check().map_err(classify_backend_reply)
    }

    fn observe_pointer(&self) -> Result<PointerObservation, BackendFault> {
        let pointer = self
            .opened
            .connection
            .query_pointer(self.opened.info.root)
            .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?
            .reply()
            .map_err(classify_backend_reply)?;
        let point = RootPoint::new(i32::from(pointer.root_x), i32::from(pointer.root_y))
            .map_err(|error| BackendFault::new(BackendFaultKind::Request, error.to_string()))?;
        let mask = u16::from(pointer.mask);
        Ok(PointerObservation {
            pointer: point,
            logical_buttons_1_to_5: [
                mask & (1 << 8) != 0,
                mask & (1 << 9) != 0,
                mask & (1 << 10) != 0,
                mask & (1 << 11) != 0,
                mask & (1 << 12) != 0,
            ],
        })
    }

    fn observe_window_geometry(&self, window: u32) -> Result<WindowGeometryContext, BackendFault> {
        let root = query_window_rect(
            &self.opened.connection,
            self.opened.info.root,
            self.opened.info.root,
        )?;
        let client_rect =
            query_window_rect(&self.opened.connection, self.opened.info.root, window)?;
        let frame_extents = self
            .opened
            .connection
            .get_property(
                false,
                window,
                self.net_frame_extents,
                AtomEnum::CARDINAL,
                0,
                4,
            )
            .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?
            .reply()
            .map_err(classify_backend_reply)
            .map(|reply| {
                let values = reply.value32().map(Iterator::collect::<Vec<_>>);
                match values.as_deref() {
                    Some([left, right, top, bottom]) => {
                        let extents = WindowFrameExtents {
                            left: *left,
                            right: *right,
                            top: *top,
                            bottom: *bottom,
                        };
                        extents.validate().is_ok().then_some(extents)
                    }
                    _ => None,
                }
            })?;
        let frame_rect = frame_extents
            .map(|extents| derive_frame_rect(client_rect, extents))
            .transpose()?;
        let geometry = WindowGeometry {
            client_rect,
            frame_rect,
            content_rect: client_rect,
            frame_extents,
        };
        WindowGeometryContext::new(root, geometry)
            .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))
    }

    fn observe_keys(&self) -> Result<Vec<PhysicalKey>, BackendFault> {
        let keymap = self
            .opened
            .connection
            .query_keymap()
            .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?
            .reply()
            .map_err(classify_backend_reply)?;
        let mut pressed_keys = Vec::new();
        for keycode in u8::MIN..=u8::MAX {
            let index = usize::from(keycode / 8);
            let bit = keycode % 8;
            if keymap.keys[index] & (1_u8 << bit) != 0
                && let Ok(key) = PhysicalKey::new(keycode)
            {
                pressed_keys.push(key);
            }
        }
        Ok(pressed_keys)
    }

    fn read_keyboard_mapping(&self, key: PhysicalKey) -> Result<CoreKeyboardMapping, BackendFault> {
        let reply = self
            .opened
            .connection
            .get_keyboard_mapping(key.keycode(), 1)
            .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?
            .reply()
            .map_err(classify_backend_reply)?;
        Ok(CoreKeyboardMapping {
            keysyms_per_keycode: reply.keysyms_per_keycode,
            keysyms: reply.keysyms,
        })
    }

    fn write_keyboard_mapping(
        &self,
        key: PhysicalKey,
        mapping: &CoreKeyboardMapping,
    ) -> Result<(), BackendFault> {
        if mapping.keysyms_per_keycode == 0
            || mapping.keysyms.len() != usize::from(mapping.keysyms_per_keycode)
        {
            return Err(BackendFault::new(
                BackendFaultKind::Capability,
                "one-key core mapping shape is invalid",
            ));
        }
        self.opened
            .connection
            .change_keyboard_mapping(
                1,
                key.keycode(),
                mapping.keysyms_per_keycode,
                &mapping.keysyms,
            )
            .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?
            .check()
            .map_err(classify_backend_reply)
    }
}

fn query_window_rect<C: x11rb::connection::Connection>(
    connection: &C,
    root: u32,
    window: u32,
) -> Result<WindowRect, BackendFault> {
    let geometry = connection
        .get_geometry(window)
        .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?
        .reply()
        .map_err(classify_backend_reply)?;
    if geometry.root != root {
        return Err(BackendFault::new(
            BackendFaultKind::Request,
            "window belongs to another root",
        ));
    }
    let translated = connection
        .translate_coordinates(window, root, 0, 0)
        .map_err(|error| BackendFault::new(BackendFaultKind::Connection, error.to_string()))?
        .reply()
        .map_err(classify_backend_reply)?;
    if !translated.same_screen {
        return Err(BackendFault::new(
            BackendFaultKind::Request,
            "window coordinates are not on the actor root",
        ));
    }
    let rect = Rect::new(
        i32::from(translated.dst_x),
        i32::from(translated.dst_y),
        u32::from(geometry.width),
        u32::from(geometry.height),
    )
    .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))?;
    WindowRect::new(CoordinateSpace::RootPhysical, rect)
        .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))
}

fn derive_frame_rect(
    client: WindowRect,
    extents: WindowFrameExtents,
) -> Result<WindowRect, BackendFault> {
    let origin = client.rect.origin();
    let size = client
        .rect
        .size()
        .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))?;
    let x = i64::from(origin.x()) - i64::from(extents.left);
    let y = i64::from(origin.y()) - i64::from(extents.top);
    let width = u64::from(size.width()) + u64::from(extents.left) + u64::from(extents.right);
    let height = u64::from(size.height()) + u64::from(extents.top) + u64::from(extents.bottom);
    let rect = Rect::new(
        i32::try_from(x)
            .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))?,
        i32::try_from(y)
            .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))?,
        u32::try_from(width)
            .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))?,
        u32::try_from(height)
            .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))?,
    )
    .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))?;
    WindowRect::new(CoordinateSpace::RootPhysical, rect)
        .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))
}

pub(super) const fn xtest_version_supported(major: u8, minor: u16) -> bool {
    major > 2 || (major == 2 && minor >= 2)
}

#[cfg(test)]
mod tests {
    use super::xtest_version_supported;

    #[test]
    fn xtest_version_boundary_is_lexicographic() {
        assert!(!xtest_version_supported(2, 1));
        assert!(xtest_version_supported(2, 2));
        assert!(xtest_version_supported(3, 0));
    }
}
