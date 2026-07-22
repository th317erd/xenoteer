//! Fixed-request per-window snapshot input.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, MapState, Window};
use xenoteer_protocol::{
    MAX_WINDOW_ATOMS, MAX_WINDOW_DIMENSION, MAX_WINDOW_TEXT_BYTES, WindowClass, WindowFrameExtents,
    WindowMapState, WindowText,
};

use super::atoms::{KnownAtom, KnownAtoms};
use super::focus::{FocusAncestryStatus, query_focus_ancestry};
use super::geometry::{RootGeometryInput, query_root_geometry};
use super::property::{
    DecodedProperty, PropertyWarning, RawProperty, decode_atom_list, decode_cardinals,
    decode_string, decode_utf8_string, decode_window_list, decode_wm_class, read_property_bounded,
};
use crate::{Result, X11Error};

/// Maximum property warnings retained for one snapshot input.
pub const MAX_SNAPSHOT_INPUT_WARNINGS: usize = 32;

/// Core attributes that do not require property interpretation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowAttributeInput {
    /// Core X server map/viewability state.
    pub map_state: WindowMapState,
    /// Whether this client bypasses window-manager redirection.
    pub override_redirect: bool,
    /// Whether the window is input-only rather than drawable.
    pub input_only: bool,
    /// Core visual identifier.
    pub visual: u32,
    /// Core colormap identifier, or zero.
    pub colormap: u32,
}

/// One bounded warning associated with its reviewed property name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservedPropertyWarning {
    /// Fixed property whose value produced the warning.
    pub property: KnownAtom,
    /// Bounded decoder warning.
    pub warning: PropertyWarning,
}

/// One bounded atom value with optional reviewed protocol identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservedAtom {
    /// Raw server atom ID retained for diagnostics and correlation.
    pub id: u32,
    /// Canonical fixed identity when the atom is in the reviewed inventory.
    pub known: Option<KnownAtom>,
}

impl ObservedAtom {
    /// Canonical name for reviewed atoms, otherwise a fixed-width hexadecimal
    /// diagnostic that never claims an invented protocol name.
    #[must_use]
    pub fn diagnostic_name(self) -> String {
        self.known.map_or_else(
            || format!("0x{:08x}", self.id),
            |known| known.canonical_name().to_owned(),
        )
    }
}

/// Bounded root/core evidence sampled with a target snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootWindowEvidenceInput {
    /// Root `_NET_ACTIVE_WINDOW`, when present and well formed.
    pub active_window: Option<Window>,
    /// Raw core `GetInputFocus` target; sentinels become `None`.
    pub raw_focused_window: Option<Window>,
    /// Nearest bounded `_NET_CLIENT_LIST` ancestor of core focus.
    pub focused_window: Option<Window>,
    /// Whether core focus is the snapshot target or a proven descendant.
    pub target_contains_focus: bool,
    /// Terminal status of the bounded ancestry walk.
    pub focus_ancestry_status: FocusAncestryStatus,
    /// Root `_NET_CURRENT_DESKTOP`, including zero.
    pub current_workspace: Option<u32>,
}

/// Property-derived input. Atom-valued metadata is correlated only against the
/// fixed reviewed inventory; this layer never performs caller-directed
/// interning or unbounded atom-name retrieval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowPropertyInput {
    /// Preferred EWMH title with ICCCM fallback.
    pub title: Option<WindowText>,
    /// Window-manager supplied visible title.
    pub visible_title: Option<WindowText>,
    /// Preferred visible/EWMH/ICCCM icon title.
    pub icon_title: Option<WindowText>,
    /// Separately decoded ICCCM instance and class.
    pub class: Option<WindowClass>,
    /// ICCCM client machine text.
    pub client_machine: Option<WindowText>,
    /// Bounded atoms from `_NET_WM_WINDOW_TYPE`.
    pub window_types: Vec<ObservedAtom>,
    /// Bounded atoms from `_NET_WM_STATE`.
    pub states: Vec<ObservedAtom>,
    /// Bounded atoms from `_NET_WM_ALLOWED_ACTIONS`.
    pub allowed_actions: Vec<ObservedAtom>,
    /// Bounded atoms from `WM_PROTOCOLS`.
    pub protocols: Vec<ObservedAtom>,
    /// Untrusted non-zero `_NET_WM_PID`, when well formed.
    pub reported_pid: Option<u32>,
    /// `_NET_WM_DESKTOP`, including its EWMH sentinel when reported.
    pub workspace: Option<u32>,
    /// Advisory EWMH frame borders.
    pub frame_extents: Option<WindowFrameExtents>,
    /// Untrusted ICCCM client-leader XID.
    pub client_leader: Option<Window>,
    /// Untrusted ICCCM transient parent XID.
    pub transient_for: Option<Window>,
    /// Untrusted group leader recovered from `WM_HINTS`.
    pub group_leader: Option<Window>,
    /// ICCCM urgency hint.
    pub urgent: bool,
    /// Deduplicated bounded property warnings.
    pub warnings: Vec<ObservedPropertyWarning>,
    /// Whether additional warnings exceeded the input warning ceiling.
    pub warnings_truncated: bool,
}

/// Complete raw input needed by the future actor to mint or refresh a model
/// entry. It contains no desktop identity, birth generation, or authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowSnapshotInput {
    /// Raw non-zero XID queried by the actor.
    pub window: Window,
    /// Core attribute input.
    pub attributes: WindowAttributeInput,
    /// Fixed bounded property input.
    pub properties: WindowPropertyInput,
    /// Client rectangle translated into root-physical coordinates.
    pub geometry: RootGeometryInput,
    /// Root/core truth sampled on the same owner connection.
    pub root: RootWindowEvidenceInput,
}

/// Query one fixed, bounded attribute/property set on the connection owner's
/// thread. Each property is fetched in one request with a precomputed ceiling.
pub fn query_window_snapshot_input<C: Connection>(
    connection: &C,
    root: Window,
    window: Window,
    atoms: &KnownAtoms,
) -> Result<WindowSnapshotInput> {
    if window == 0 || window == root {
        return Err(X11Error::InvalidSetup(
            "snapshot input requires a non-root window",
        ));
    }
    let attributes = query_attributes(connection, window)?;
    let geometry = query_root_geometry(connection, root, window)?;
    let (properties, root_evidence) = query_properties(connection, root, window, atoms)?;
    Ok(WindowSnapshotInput {
        window,
        attributes,
        properties,
        geometry,
        root: root_evidence,
    })
}

fn query_attributes<C: Connection>(connection: &C, window: Window) -> Result<WindowAttributeInput> {
    let reply = connection
        .get_window_attributes(window)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    let map_state = if reply.map_state == MapState::UNMAPPED {
        WindowMapState::Unmapped
    } else if reply.map_state == MapState::UNVIEWABLE {
        WindowMapState::Unviewable
    } else if reply.map_state == MapState::VIEWABLE {
        WindowMapState::Viewable
    } else {
        return Err(X11Error::InvalidSetup("unknown core X11 map state"));
    };
    Ok(WindowAttributeInput {
        map_state,
        override_redirect: reply.override_redirect,
        input_only: reply.class == x11rb::protocol::xproto::WindowClass::INPUT_ONLY,
        visual: reply.visual,
        colormap: reply.colormap,
    })
}

fn query_properties<C: Connection>(
    connection: &C,
    root: Window,
    window: Window,
    atoms: &KnownAtoms,
) -> Result<(WindowPropertyInput, RootWindowEvidenceInput)> {
    let mut collector = WarningCollector::default();

    let title = text_with_fallback(
        connection,
        window,
        atoms,
        KnownAtom::NetWmName,
        Some(KnownAtom::WmName),
        &mut collector,
    )?;
    let visible_title = text_with_fallback(
        connection,
        window,
        atoms,
        KnownAtom::NetWmVisibleName,
        None,
        &mut collector,
    )?;
    let icon_title = first_text(
        connection,
        window,
        atoms,
        &[
            KnownAtom::NetWmVisibleIconName,
            KnownAtom::NetWmIconName,
            KnownAtom::WmIconName,
        ],
        &mut collector,
    )?;

    let class = {
        let property = KnownAtom::WmClass;
        let raw = read(
            connection,
            window,
            atoms,
            property,
            KnownAtom::String,
            8_192,
        )?;
        let decoded = decode_wm_class(&raw, atoms.get(KnownAtom::String));
        collector.record(property, &decoded.warnings);
        decoded.value
    };
    let client_machine = {
        let property = KnownAtom::WmClientMachine;
        let raw = read(
            connection,
            window,
            atoms,
            property,
            KnownAtom::String,
            MAX_WINDOW_TEXT_BYTES,
        )?;
        let decoded = decode_string(&raw, atoms.get(KnownAtom::String));
        collector.record(property, &decoded.warnings);
        decoded.value
    };

    let window_types = atom_values(
        connection,
        window,
        atoms,
        KnownAtom::NetWmWindowType,
        &mut collector,
    )?;
    let states = atom_values(
        connection,
        window,
        atoms,
        KnownAtom::NetWmState,
        &mut collector,
    )?;
    let allowed_actions = atom_values(
        connection,
        window,
        atoms,
        KnownAtom::NetWmAllowedActions,
        &mut collector,
    )?;
    let protocols = atom_values(
        connection,
        window,
        atoms,
        KnownAtom::WmProtocols,
        &mut collector,
    )?;

    let reported_pid = cardinal_one(
        connection,
        window,
        atoms,
        KnownAtom::NetWmPid,
        false,
        &mut collector,
    )?;
    let workspace = cardinal_one(
        connection,
        window,
        atoms,
        KnownAtom::NetWmDesktop,
        true,
        &mut collector,
    )?;
    let frame_extents = frame_extents(connection, window, atoms, &mut collector)?;
    let client_leader = window_one(
        connection,
        window,
        atoms,
        KnownAtom::WmClientLeader,
        &mut collector,
    )?;
    let transient_for = window_one(
        connection,
        window,
        atoms,
        KnownAtom::WmTransientFor,
        &mut collector,
    )?;
    let (group_leader, urgent) = wm_hints(connection, window, atoms, &mut collector)?;

    let active_window = window_one(
        connection,
        root,
        atoms,
        KnownAtom::NetActiveWindow,
        &mut collector,
    )?;
    let current_workspace = cardinal_one(
        connection,
        root,
        atoms,
        KnownAtom::NetCurrentDesktop,
        true,
        &mut collector,
    )?;
    // Per-target ancestry is sufficient to prove this snapshot's focused
    // state. Re-reading `_NET_CLIENT_LIST` here would make full reconciliation
    // quadratic and would make normalization depend on that list's bound.
    let focus = query_focus_ancestry(connection, root, window, &[])?;
    let root_evidence = RootWindowEvidenceInput {
        active_window,
        raw_focused_window: focus.raw_focus,
        focused_window: focus.normalized_top_level,
        target_contains_focus: focus.target_contains_focus,
        focus_ancestry_status: focus.status,
        current_workspace,
    };

    Ok((
        WindowPropertyInput {
            title,
            visible_title,
            icon_title,
            class,
            client_machine,
            window_types,
            states,
            allowed_actions,
            protocols,
            reported_pid,
            workspace,
            frame_extents,
            client_leader,
            transient_for,
            group_leader,
            urgent,
            warnings: collector.warnings,
            warnings_truncated: collector.truncated,
        },
        root_evidence,
    ))
}

fn text_with_fallback<C: Connection>(
    connection: &C,
    window: Window,
    atoms: &KnownAtoms,
    utf8_property: KnownAtom,
    string_fallback: Option<KnownAtom>,
    collector: &mut WarningCollector,
) -> Result<Option<WindowText>> {
    let raw = read(
        connection,
        window,
        atoms,
        utf8_property,
        KnownAtom::Utf8String,
        MAX_WINDOW_TEXT_BYTES,
    )?;
    let decoded = decode_utf8_string(&raw, atoms.get(KnownAtom::Utf8String));
    collector.record(utf8_property, &decoded.warnings);
    if decoded.value.is_some() || string_fallback.is_none() {
        return Ok(decoded.value);
    }
    let Some(fallback) = string_fallback else {
        return Ok(None);
    };
    let raw = read(
        connection,
        window,
        atoms,
        fallback,
        KnownAtom::String,
        MAX_WINDOW_TEXT_BYTES,
    )?;
    let decoded = decode_string(&raw, atoms.get(KnownAtom::String));
    collector.record(fallback, &decoded.warnings);
    Ok(decoded.value)
}

fn first_text<C: Connection>(
    connection: &C,
    window: Window,
    atoms: &KnownAtoms,
    properties: &[KnownAtom],
    collector: &mut WarningCollector,
) -> Result<Option<WindowText>> {
    for property in properties {
        let expected = if *property == KnownAtom::WmIconName {
            KnownAtom::String
        } else {
            KnownAtom::Utf8String
        };
        let raw = read(
            connection,
            window,
            atoms,
            *property,
            expected,
            MAX_WINDOW_TEXT_BYTES,
        )?;
        let decoded = if expected == KnownAtom::String {
            decode_string(&raw, atoms.get(expected))
        } else {
            decode_utf8_string(&raw, atoms.get(expected))
        };
        collector.record(*property, &decoded.warnings);
        if decoded.value.is_some() {
            return Ok(decoded.value);
        }
    }
    Ok(None)
}

fn atom_values<C: Connection>(
    connection: &C,
    window: Window,
    atoms: &KnownAtoms,
    property: KnownAtom,
    collector: &mut WarningCollector,
) -> Result<Vec<ObservedAtom>> {
    let raw = read(
        connection,
        window,
        atoms,
        property,
        KnownAtom::Atom,
        MAX_WINDOW_ATOMS * 4,
    )?;
    let decoded = decode_atom_list(&raw, atoms.get(KnownAtom::Atom));
    collector.record(property, &decoded.warnings);
    Ok(observe_atom_identities(
        decoded.value.unwrap_or_default(),
        atoms,
        property,
        collector,
    ))
}

fn observe_atom_identities(
    values: Vec<u32>,
    atoms: &KnownAtoms,
    property: KnownAtom,
    collector: &mut WarningCollector,
) -> Vec<ObservedAtom> {
    values
        .into_iter()
        .map(|id| {
            let known = atoms.identify(id);
            if known.is_none() {
                collector.record(property, &[PropertyWarning::UnknownAtom]);
            }
            ObservedAtom { id, known }
        })
        .collect()
}

fn cardinal_one<C: Connection>(
    connection: &C,
    window: Window,
    atoms: &KnownAtoms,
    property: KnownAtom,
    allow_zero: bool,
    collector: &mut WarningCollector,
) -> Result<Option<u32>> {
    let raw = read(connection, window, atoms, property, KnownAtom::Cardinal, 4)?;
    let mut decoded = decode_cardinals(&raw, atoms.get(KnownAtom::Cardinal));
    let value = take_one(&mut decoded, allow_zero);
    collector.record(property, &decoded.warnings);
    Ok(value)
}

fn window_one<C: Connection>(
    connection: &C,
    window: Window,
    atoms: &KnownAtoms,
    property: KnownAtom,
    collector: &mut WarningCollector,
) -> Result<Option<Window>> {
    // xfwm4 4.20 appends its last-focus timestamp to `_NET_ACTIVE_WINDOW`.
    // Retain enough bytes to recognize that bounded non-canonical form without
    // weakening singleton parsing for client-owned properties.
    let allow_trailing_none = property == KnownAtom::NetActiveWindow;
    let max_bytes = if allow_trailing_none { 8 } else { 4 };
    let raw = read(
        connection,
        window,
        atoms,
        property,
        KnownAtom::Window,
        max_bytes,
    )?;
    let mut decoded = decode_window_list(&raw, atoms.get(KnownAtom::Window));
    let value = if allow_trailing_none {
        take_active_window(&mut decoded)
    } else {
        take_one(&mut decoded, false)
    };
    collector.record(property, &decoded.warnings);
    Ok(value)
}

fn frame_extents<C: Connection>(
    connection: &C,
    window: Window,
    atoms: &KnownAtoms,
    collector: &mut WarningCollector,
) -> Result<Option<WindowFrameExtents>> {
    let property = KnownAtom::NetFrameExtents;
    let raw = read(connection, window, atoms, property, KnownAtom::Cardinal, 16)?;
    let mut decoded = decode_cardinals(&raw, atoms.get(KnownAtom::Cardinal));
    let incomplete = decoded.warnings.contains(&PropertyWarning::Truncated);
    let value = match decoded.value.as_deref() {
        Some([left, right, top, bottom])
            if !incomplete
                && [*left, *right, *top, *bottom]
                    .into_iter()
                    .all(|extent| extent <= MAX_WINDOW_DIMENSION) =>
        {
            Some(WindowFrameExtents {
                left: *left,
                right: *right,
                top: *top,
                bottom: *bottom,
            })
        }
        None => None,
        Some(_) => {
            push_property_warning(&mut decoded.warnings, PropertyWarning::Malformed);
            None
        }
    };
    collector.record(property, &decoded.warnings);
    Ok(value)
}

fn wm_hints<C: Connection>(
    connection: &C,
    window: Window,
    atoms: &KnownAtoms,
    collector: &mut WarningCollector,
) -> Result<(Option<Window>, bool)> {
    const WINDOW_GROUP_HINT: u32 = 1 << 6;
    const URGENCY_HINT: u32 = 1 << 8;

    let property = KnownAtom::WmHints;
    let raw = read(
        connection,
        window,
        atoms,
        property,
        KnownAtom::WmHints,
        9 * 4,
    )?;
    let mut decoded = super::property::decode_u32_list(&raw, atoms.get(property), 9);
    let Some(values) = decoded.value.as_deref() else {
        collector.record(property, &decoded.warnings);
        return Ok((None, false));
    };
    if values.len() != 9 || decoded.warnings.contains(&PropertyWarning::Truncated) {
        push_property_warning(&mut decoded.warnings, PropertyWarning::Malformed);
        collector.record(property, &decoded.warnings);
        return Ok((None, false));
    }
    let flags = values[0];
    let group = ((flags & WINDOW_GROUP_HINT != 0) && values[8] != 0).then_some(values[8]);
    let urgent = flags & URGENCY_HINT != 0;
    collector.record(property, &decoded.warnings);
    Ok((group, urgent))
}

fn read<C: Connection>(
    connection: &C,
    window: Window,
    atoms: &KnownAtoms,
    property: KnownAtom,
    expected: KnownAtom,
    max_bytes: usize,
) -> Result<RawProperty> {
    read_property_bounded(
        connection,
        window,
        atoms.get(property),
        atoms.get(expected),
        max_bytes,
    )
}

fn take_one<T: Copy + PartialEq + From<u8>>(
    decoded: &mut DecodedProperty<Vec<T>>,
    allow_zero: bool,
) -> Option<T> {
    if decoded.warnings.contains(&PropertyWarning::Truncated) {
        return None;
    }
    match decoded.value.as_deref() {
        Some([value]) if allow_zero || *value != T::from(0) => Some(*value),
        None => None,
        Some(_) => {
            push_property_warning(&mut decoded.warnings, PropertyWarning::Malformed);
            None
        }
    }
}

fn take_active_window(decoded: &mut DecodedProperty<Vec<Window>>) -> Option<Window> {
    if decoded.warnings.contains(&PropertyWarning::Truncated) {
        return None;
    }
    match decoded.value.as_deref() {
        Some([value]) | Some([value, _]) if *value != 0 => Some(*value),
        Some([0]) | Some([0, _]) => None,
        None => None,
        Some(_) => {
            push_property_warning(&mut decoded.warnings, PropertyWarning::Malformed);
            None
        }
    }
}

#[derive(Default)]
struct WarningCollector {
    warnings: Vec<ObservedPropertyWarning>,
    truncated: bool,
}

impl WarningCollector {
    fn record(&mut self, property: KnownAtom, warnings: &[PropertyWarning]) {
        for warning in warnings {
            let observed = ObservedPropertyWarning {
                property,
                warning: *warning,
            };
            if self.warnings.contains(&observed) {
                continue;
            }
            if self.warnings.len() == MAX_SNAPSHOT_INPUT_WARNINGS {
                self.truncated = true;
                return;
            }
            self.warnings.push(observed);
        }
    }
}

fn push_property_warning(warnings: &mut Vec<PropertyWarning>, warning: PropertyWarning) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_value_contract_rejects_zero_pid_and_extra_cardinals() {
        let mut zero = DecodedProperty {
            value: Some(vec![0_u32]),
            warnings: Vec::new(),
        };
        assert_eq!(take_one(&mut zero, false), None);
        assert!(zero.warnings.contains(&PropertyWarning::Malformed));

        let mut extra = DecodedProperty {
            value: Some(vec![1_u32, 2]),
            warnings: Vec::new(),
        };
        assert_eq!(take_one(&mut extra, true), None);
        assert!(extra.warnings.contains(&PropertyWarning::Malformed));
    }

    #[test]
    fn active_window_accepts_only_the_bounded_xfwm_timestamp_extension() {
        let mut padded = DecodedProperty {
            value: Some(vec![42, 123_456]),
            warnings: Vec::new(),
        };
        assert_eq!(take_active_window(&mut padded), Some(42));
        assert!(padded.warnings.is_empty());

        let mut ambiguous = DecodedProperty {
            value: Some(vec![42, 7, 9]),
            warnings: Vec::new(),
        };
        assert_eq!(take_active_window(&mut ambiguous), None);
        assert!(ambiguous.warnings.contains(&PropertyWarning::Malformed));
    }

    #[test]
    fn warning_collector_is_deduplicated_and_bounded() {
        let mut collector = WarningCollector::default();
        for atom in KnownAtom::ALL {
            collector.record(
                atom,
                &[
                    PropertyWarning::Malformed,
                    PropertyWarning::Malformed,
                    PropertyWarning::UnexpectedType,
                ],
            );
        }
        assert_eq!(collector.warnings.len(), MAX_SNAPSHOT_INPUT_WARNINGS);
        assert!(collector.truncated);
    }

    #[test]
    fn observed_atoms_keep_canonical_identity_and_truthful_unknown_hex() {
        let atoms = KnownAtoms::for_test(|atom| atom as u32 + 100);
        let known = KnownAtom::NetWmWindowTypeDialog;
        let mut collector = WarningCollector::default();
        let observed = observe_atom_identities(
            vec![atoms.get(known), 0xfeed_beef],
            &atoms,
            KnownAtom::NetWmWindowType,
            &mut collector,
        );
        assert_eq!(observed[0].known, Some(known));
        assert_eq!(observed[0].diagnostic_name(), "_NET_WM_WINDOW_TYPE_DIALOG");
        assert_eq!(observed[1].known, None);
        assert_eq!(observed[1].diagnostic_name(), "0xfeedbeef");
        assert_eq!(
            collector.warnings,
            vec![ObservedPropertyWarning {
                property: KnownAtom::NetWmWindowType,
                warning: PropertyWarning::UnknownAtom,
            }]
        );
    }
}
