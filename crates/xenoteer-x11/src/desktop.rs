//! Bounded, side-effect-clean desktop readiness probe.

use std::{
    thread,
    time::{Duration, Instant},
};

use x11rb::{
    COPY_DEPTH_FROM_PARENT,
    connection::Connection,
    protocol::xproto::{
        Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Window,
        WindowClass,
    },
    wrapper::ConnectionExt as _,
};

use crate::{ExtensionName, Result, X11Error, capture::get_image_bgra8, connect};

const EWMH_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
const EWMH_SETTLE_INTERVAL: Duration = Duration::from_millis(20);

/// Fixed display properties required by the selected runtime profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopProbeExpectation {
    /// Root width in physical pixels.
    pub width_px: u16,
    /// Root height in physical pixels.
    pub height_px: u16,
    /// Root visual depth.
    pub depth: u8,
    /// Server-reported horizontal and vertical DPI.
    pub dpi: u16,
}

/// Non-secret evidence from a complete readiness probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopProbeEvidence {
    /// EWMH-supporting window advertised by the window manager.
    pub supporting_wm_window: Window,
    /// Number of atoms in `_NET_SUPPORTED`.
    pub supported_atom_count: usize,
    /// Advertised workspace count.
    pub workspace_count: u32,
    /// Current workspace index.
    pub current_workspace: u32,
    /// Rounded horizontal DPI derived from server physical dimensions.
    pub dpi_x: u16,
    /// Rounded vertical DPI derived from server physical dimensions.
    pub dpi_y: u16,
    /// Bytes returned by the one-pixel BGRA capture.
    pub capture_bytes: usize,
}

/// Prove the authenticated X server, XFCE/EWMH contract, compositor absence,
/// one-workspace profile, managed-window lifecycle, and one-pixel capture.
pub fn probe_desktop(
    display: &str,
    expected: DesktopProbeExpectation,
) -> Result<DesktopProbeEvidence> {
    probe_desktop_inner(display, expected, true)
}

/// Recheck persistent desktop capabilities without creating a managed window.
///
/// The startup probe performs the managed-window lifecycle proof with exact
/// focus preservation. Recurring health checks deliberately use this
/// side-effect-free form so a legitimate application focus transition cannot
/// be mistaken for damage caused by the probe itself.
pub fn probe_desktop_steady_state(
    display: &str,
    expected: DesktopProbeExpectation,
) -> Result<DesktopProbeEvidence> {
    probe_desktop_inner(display, expected, false)
}

fn probe_desktop_inner(
    display: &str,
    expected: DesktopProbeExpectation,
    prove_window_lifecycle: bool,
) -> Result<DesktopProbeEvidence> {
    let opened = connect(display)?;
    opened.core_roundtrip()?;
    opened.info.extensions.require(ExtensionName::XTest)?;
    validate_geometry(
        opened.info.width_px,
        opened.info.height_px,
        opened.info.root_depth,
        expected,
    )?;
    let dpi_x = rounded_dpi(opened.info.width_px, opened.info.width_mm)?;
    let dpi_y = rounded_dpi(opened.info.height_px, opened.info.height_mm)?;
    if dpi_x != expected.dpi || dpi_y != expected.dpi {
        return Err(X11Error::InvalidSetup(
            "X server DPI differs from the configured desktop profile",
        ));
    }

    let atoms = DesktopAtoms::intern(&opened.connection)?;
    let root = opened.info.root;
    let supporting_wm_window = one_window_property(
        &opened.connection,
        root,
        atoms.net_supporting_wm_check,
        "root _NET_SUPPORTING_WM_CHECK",
    )?;
    if supporting_wm_window == 0 {
        return Err(X11Error::InvalidSetup(
            "window manager support window is None",
        ));
    }
    let wm_self = one_window_property(
        &opened.connection,
        supporting_wm_window,
        atoms.net_supporting_wm_check,
        "window-manager _NET_SUPPORTING_WM_CHECK",
    )?;
    if wm_self != supporting_wm_window {
        return Err(X11Error::InvalidSetup(
            "window manager support window does not point to itself",
        ));
    }

    let supported = atom_list_property(
        &opened.connection,
        root,
        atoms.net_supported,
        AtomEnum::ATOM.into(),
        "root _NET_SUPPORTED",
        false,
    )?;
    for required in atoms.required_supported() {
        if !supported.contains(&required) {
            return Err(X11Error::InvalidSetup(
                "window manager omitted a required EWMH capability",
            ));
        }
    }

    let workspace_count = one_cardinal_property(
        &opened.connection,
        root,
        atoms.net_number_of_desktops,
        "root _NET_NUMBER_OF_DESKTOPS",
    )?;
    let current_workspace = one_cardinal_property(
        &opened.connection,
        root,
        atoms.net_current_desktop,
        "root _NET_CURRENT_DESKTOP",
    )?;
    validate_workspace(workspace_count, current_workspace)?;

    let compositor_owner = opened
        .connection
        .get_selection_owner(atoms.net_wm_cm_s0)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?
        .owner;
    if compositor_owner != 0 {
        return Err(X11Error::InvalidSetup(
            "an X11 compositor owns _NET_WM_CM_S0",
        ));
    }

    if prove_window_lifecycle {
        prove_managed_window_lifecycle(&opened.connection, root, &atoms)?;
    }
    let captured = get_image_bgra8(&opened.connection, &opened.info, root, 0, 0, 1, 1)?;
    if captured.len() != 4 {
        return Err(X11Error::InvalidSetup(
            "one-pixel capture did not return one BGRA pixel",
        ));
    }

    Ok(DesktopProbeEvidence {
        supporting_wm_window,
        supported_atom_count: supported.len(),
        workspace_count,
        current_workspace,
        dpi_x,
        dpi_y,
        capture_bytes: captured.len(),
    })
}

fn validate_geometry(
    width_px: u16,
    height_px: u16,
    depth: u8,
    expected: DesktopProbeExpectation,
) -> Result<()> {
    if width_px != expected.width_px || height_px != expected.height_px {
        return Err(X11Error::InvalidSetup(
            "root geometry differs from the configured desktop profile",
        ));
    }
    if depth != expected.depth {
        return Err(X11Error::InvalidSetup(
            "root depth differs from the configured desktop profile",
        ));
    }
    Ok(())
}

fn validate_workspace(workspace_count: u32, current_workspace: u32) -> Result<()> {
    if workspace_count != 1 {
        return Err(X11Error::InvalidSetup(
            "release-one desktop must advertise exactly one workspace",
        ));
    }
    if current_workspace != 0 {
        return Err(X11Error::InvalidSetup(
            "release-one current workspace must be zero",
        ));
    }
    Ok(())
}

fn rounded_dpi(pixels: u16, millimeters: u16) -> Result<u16> {
    if millimeters == 0 {
        return Err(X11Error::InvalidSetup(
            "X server physical screen dimension is zero",
        ));
    }
    let tenths = u32::from(pixels)
        .checked_mul(254)
        .ok_or(X11Error::InvalidSetup(
            "X server DPI calculation overflowed",
        ))?;
    let denominator = u32::from(millimeters) * 10;
    let rounded = tenths
        .checked_add(denominator / 2)
        .ok_or(X11Error::InvalidSetup(
            "X server DPI calculation overflowed",
        ))?
        / denominator;
    u16::try_from(rounded)
        .map_err(|_| X11Error::InvalidSetup("X server DPI is outside the supported range"))
}

struct DesktopAtoms {
    net_supported: Atom,
    net_supporting_wm_check: Atom,
    net_number_of_desktops: Atom,
    net_current_desktop: Atom,
    net_client_list: Atom,
    net_active_window: Atom,
    net_wm_name: Atom,
    net_wm_window_type: Atom,
    net_wm_window_type_utility: Atom,
    net_wm_cm_s0: Atom,
    utf8_string: Atom,
    wm_class: Atom,
    wm_hints: Atom,
}

impl DesktopAtoms {
    fn intern<C: Connection>(connection: &C) -> Result<Self> {
        Ok(Self {
            net_supported: intern(connection, b"_NET_SUPPORTED")?,
            net_supporting_wm_check: intern(connection, b"_NET_SUPPORTING_WM_CHECK")?,
            net_number_of_desktops: intern(connection, b"_NET_NUMBER_OF_DESKTOPS")?,
            net_current_desktop: intern(connection, b"_NET_CURRENT_DESKTOP")?,
            net_client_list: intern(connection, b"_NET_CLIENT_LIST")?,
            net_active_window: intern(connection, b"_NET_ACTIVE_WINDOW")?,
            net_wm_name: intern(connection, b"_NET_WM_NAME")?,
            net_wm_window_type: intern(connection, b"_NET_WM_WINDOW_TYPE")?,
            net_wm_window_type_utility: intern(connection, b"_NET_WM_WINDOW_TYPE_UTILITY")?,
            net_wm_cm_s0: intern(connection, b"_NET_WM_CM_S0")?,
            utf8_string: intern(connection, b"UTF8_STRING")?,
            wm_class: AtomEnum::WM_CLASS.into(),
            wm_hints: AtomEnum::WM_HINTS.into(),
        })
    }

    fn required_supported(&self) -> [Atom; 5] {
        [
            self.net_supporting_wm_check,
            self.net_number_of_desktops,
            self.net_current_desktop,
            self.net_client_list,
            self.net_active_window,
        ]
    }
}

fn intern<C: Connection>(connection: &C, name: &[u8]) -> Result<Atom> {
    connection
        .intern_atom(false, name)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| X11Error::Reply(error.to_string()))
}

fn one_window_property<C: Connection>(
    connection: &C,
    window: Window,
    property: Atom,
    label: &'static str,
) -> Result<Window> {
    one_u32_property(connection, window, property, AtomEnum::WINDOW.into(), label)
}

fn one_cardinal_property<C: Connection>(
    connection: &C,
    window: Window,
    property: Atom,
    label: &'static str,
) -> Result<u32> {
    one_u32_property(
        connection,
        window,
        property,
        AtomEnum::CARDINAL.into(),
        label,
    )
}

fn one_u32_property<C: Connection>(
    connection: &C,
    window: Window,
    property: Atom,
    property_type: Atom,
    label: &'static str,
) -> Result<u32> {
    let reply = connection
        .get_property(false, window, property, property_type, 0, 2)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    if reply.type_ != property_type || reply.format != 32 || reply.bytes_after != 0 {
        return Err(X11Error::InvalidSetup(label));
    }
    let values: Vec<_> = reply
        .value32()
        .ok_or(X11Error::InvalidSetup(label))?
        .collect();
    match values.as_slice() {
        [value] => Ok(*value),
        _ => Err(X11Error::InvalidSetup(label)),
    }
}

fn atom_list_property<C: Connection>(
    connection: &C,
    window: Window,
    property: Atom,
    property_type: Atom,
    label: &'static str,
    allow_empty: bool,
) -> Result<Vec<Atom>> {
    let reply = connection
        .get_property(false, window, property, property_type, 0, 4_096)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    if reply.type_ != property_type || reply.format != 32 || reply.bytes_after != 0 {
        return Err(X11Error::InvalidSetup(label));
    }
    let values: Vec<_> = reply
        .value32()
        .ok_or(X11Error::InvalidSetup(label))?
        .collect();
    if (!allow_empty && values.is_empty()) || values.len() > 4_096 {
        return Err(X11Error::InvalidSetup(label));
    }
    Ok(values)
}

fn prove_managed_window_lifecycle<C: Connection>(
    connection: &C,
    root: Window,
    atoms: &DesktopAtoms,
) -> Result<()> {
    let focused_before = input_focus_window(connection, "core focus before probe map")?;
    let active_before = optional_window_property(
        connection,
        root,
        atoms.net_active_window,
        "root _NET_ACTIVE_WINDOW before probe map",
    )?;
    let window = connection
        .generate_id()
        .map_err(|error| X11Error::Connection(error.to_string()))?;
    let guard = ProbeWindowGuard {
        connection,
        window,
        destroyed: false,
    };
    connection
        .create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .check()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    connection
        .change_property8(
            PropMode::REPLACE,
            window,
            atoms.net_wm_name,
            atoms.utf8_string,
            b"xenoteer-desktop-probe",
        )
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .check()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    connection
        .change_property8(
            PropMode::REPLACE,
            window,
            atoms.wm_class,
            AtomEnum::STRING,
            b"xenoteer-desktop-probe\0XenoteerDesktopProbe\0",
        )
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .check()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    connection
        .change_property32(
            PropMode::REPLACE,
            window,
            atoms.net_wm_window_type,
            AtomEnum::ATOM,
            &[atoms.net_wm_window_type_utility],
        )
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .check()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    // ICCCM InputHint with input=false keeps the readiness window from taking focus.
    connection
        .change_property32(
            PropMode::REPLACE,
            window,
            atoms.wm_hints,
            atoms.wm_hints,
            &[1, 0, 1, 0, 0, 0, 0, 0, 0],
        )
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .check()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    connection
        .map_window(window)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .check()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    connection
        .flush()
        .map_err(|error| X11Error::Connection(error.to_string()))?;

    wait_for_client_membership(connection, root, atoms.net_client_list, window, true)?;
    let focused_during = input_focus_window(connection, "core focus while probe was mapped")?;
    if focused_during == window {
        return Err(X11Error::InvalidSetup(
            "desktop readiness probe window stole input focus",
        ));
    }
    let active_during = optional_window_property(
        connection,
        root,
        atoms.net_active_window,
        "root _NET_ACTIVE_WINDOW while probe was mapped",
    )?;
    if active_during == Some(window) {
        return Err(X11Error::InvalidSetup(
            "desktop readiness probe window became active",
        ));
    }
    connection
        .unmap_window(window)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .check()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    connection
        .flush()
        .map_err(|error| X11Error::Connection(error.to_string()))?;
    wait_for_client_membership(connection, root, atoms.net_client_list, window, false)?;
    guard.destroy()?;
    wait_for_focus_restoration(connection, root, atoms, focused_before, active_before)?;
    Ok(())
}

fn wait_for_focus_restoration<C: Connection>(
    connection: &C,
    root: Window,
    atoms: &DesktopAtoms,
    focused_before: Window,
    active_before: Option<Window>,
) -> Result<()> {
    let deadline = Instant::now() + EWMH_SETTLE_TIMEOUT;
    loop {
        let focused_after = input_focus_window(connection, "core focus after probe destroy")?;
        let active_after = optional_window_property(
            connection,
            root,
            atoms.net_active_window,
            "root _NET_ACTIVE_WINDOW after probe destroy",
        )?;
        if focused_after == focused_before && active_after == active_before {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(X11Error::InvalidSetup(
                "desktop readiness probe did not preserve focus and active window",
            ));
        }
        thread::sleep(EWMH_SETTLE_INTERVAL);
    }
}

fn input_focus_window<C: Connection>(connection: &C, label: &'static str) -> Result<Window> {
    connection
        .get_input_focus()
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map(|reply| reply.focus)
        .map_err(|error| X11Error::Reply(format!("{label}: {error}")))
}

fn optional_window_property<C: Connection>(
    connection: &C,
    window: Window,
    property: Atom,
    label: &'static str,
) -> Result<Option<Window>> {
    let reply = connection
        .get_property(false, window, property, AtomEnum::WINDOW, 0, 2)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    if reply.type_ == 0 && reply.format == 0 && reply.bytes_after == 0 && reply.value.is_empty() {
        return Ok(None);
    }
    if reply.type_ != u32::from(AtomEnum::WINDOW) || reply.format != 32 || reply.bytes_after != 0 {
        return Err(X11Error::InvalidSetup(label));
    }
    let values: Vec<_> = reply
        .value32()
        .ok_or(X11Error::InvalidSetup(label))?
        .collect();
    normalize_optional_window_values(&values, label)
}

fn normalize_optional_window_values(
    values: &[Window],
    label: &'static str,
) -> Result<Option<Window>> {
    // EWMH specifies one WINDOW/32 value. XFWM4 4.20's
    // clientSetNetActiveWindow deliberately appends its last-focus timestamp,
    // so admit exactly that bounded two-field representation as well. The
    // timestamp is evidence about focus recency, not another window ID.
    let value = match values {
        [value] | [value, _] => *value,
        _ => return Err(X11Error::InvalidSetup(label)),
    };
    Ok((value != 0).then_some(value))
}

fn wait_for_client_membership<C: Connection>(
    connection: &C,
    root: Window,
    property: Atom,
    window: Window,
    expected: bool,
) -> Result<()> {
    let deadline = Instant::now() + EWMH_SETTLE_TIMEOUT;
    loop {
        let members = atom_list_property(
            connection,
            root,
            property,
            AtomEnum::WINDOW.into(),
            "root _NET_CLIENT_LIST",
            true,
        )?;
        if members.contains(&window) == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(X11Error::InvalidSetup(
                "probe window EWMH lifecycle did not settle",
            ));
        }
        thread::sleep(EWMH_SETTLE_INTERVAL);
    }
}

struct ProbeWindowGuard<'a, C: Connection> {
    connection: &'a C,
    window: Window,
    destroyed: bool,
}

impl<C: Connection> ProbeWindowGuard<'_, C> {
    fn destroy(mut self) -> Result<()> {
        self.connection
            .destroy_window(self.window)
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .check()
            .map_err(|error| X11Error::Reply(error.to_string()))?;
        self.destroyed = true;
        self.connection
            .get_input_focus()
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .reply()
            .map_err(|error| X11Error::Reply(error.to_string()))?;
        Ok(())
    }
}

impl<C: Connection> Drop for ProbeWindowGuard<'_, C> {
    fn drop(&mut self) {
        if !self.destroyed {
            let _ignored = self.connection.destroy_window(self.window);
            let _ignored = self.connection.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DesktopProbeExpectation, normalize_optional_window_values, rounded_dpi, validate_geometry,
        validate_workspace,
    };

    #[test]
    fn geometry_requires_exact_width_height_and_depth() {
        let expected = DesktopProbeExpectation {
            width_px: 1_920,
            height_px: 1_080,
            depth: 24,
            dpi: 96,
        };
        assert!(validate_geometry(1_920, 1_080, 24, expected).is_ok());
        assert!(validate_geometry(1_919, 1_080, 24, expected).is_err());
        assert!(validate_geometry(1_920, 1_079, 24, expected).is_err());
        assert!(validate_geometry(1_920, 1_080, 16, expected).is_err());
    }

    #[test]
    fn workspace_contract_is_exactly_index_zero_of_one() {
        assert!(validate_workspace(1, 0).is_ok());
        assert!(validate_workspace(2, 0).is_err());
        assert!(validate_workspace(1, 1).is_err());
    }

    #[test]
    fn dpi_uses_server_physical_dimensions_and_rejects_zero() {
        assert!(matches!(rounded_dpi(1_920, 508), Ok(96)));
        assert!(matches!(rounded_dpi(1_080, 286), Ok(96)));
        assert!(rounded_dpi(1_920, 0).is_err());
    }

    #[test]
    fn xfwm_active_window_timestamp_extension_is_exactly_bounded() {
        assert!(matches!(
            normalize_optional_window_values(&[42], "test"),
            Ok(Some(42))
        ));
        assert!(matches!(
            normalize_optional_window_values(&[42, 0], "test"),
            Ok(Some(42))
        ));
        assert!(matches!(
            normalize_optional_window_values(&[42, 123_456], "test"),
            Ok(Some(42))
        ));
        assert!(matches!(
            normalize_optional_window_values(&[0, 123_456], "test"),
            Ok(None)
        ));
        assert!(normalize_optional_window_values(&[], "test").is_err());
        assert!(normalize_optional_window_values(&[42, 0, 0], "test").is_err());
    }
}
