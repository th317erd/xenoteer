//! Fixed atom inventory for observation.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _};

use crate::{Result, X11Error};

/// Every atom the observation primitive is permitted to intern.
///
/// There is deliberately no string-taking interning API. Extending the
/// inventory requires a reviewed source change and keeps request sizes fixed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(usize)]
pub enum KnownAtom {
    /// `_NET_CLIENT_LIST_STACKING`.
    NetClientListStacking,
    /// `_NET_CLIENT_LIST`.
    NetClientList,
    /// `_NET_ACTIVE_WINDOW`.
    NetActiveWindow,
    /// `_NET_SUPPORTED`.
    NetSupported,
    /// `_NET_CLOSE_WINDOW`.
    NetCloseWindow,
    /// `_NET_MOVERESIZE_WINDOW`.
    NetMoveResizeWindow,
    /// `_NET_CURRENT_DESKTOP`.
    NetCurrentDesktop,
    /// `_NET_NUMBER_OF_DESKTOPS`.
    NetNumberOfDesktops,
    /// `_NET_RESTACK_WINDOW`.
    NetRestackWindow,
    /// `_NET_WM_NAME`.
    NetWmName,
    /// `_NET_WM_VISIBLE_NAME`.
    NetWmVisibleName,
    /// `_NET_WM_ICON_NAME`.
    NetWmIconName,
    /// `_NET_WM_VISIBLE_ICON_NAME`.
    NetWmVisibleIconName,
    /// `_NET_WM_PID`.
    NetWmPid,
    /// `_NET_WM_WINDOW_TYPE`.
    NetWmWindowType,
    /// `_NET_WM_WINDOW_TYPE_DESKTOP`.
    NetWmWindowTypeDesktop,
    /// `_NET_WM_WINDOW_TYPE_DOCK`.
    NetWmWindowTypeDock,
    /// `_NET_WM_WINDOW_TYPE_TOOLBAR`.
    NetWmWindowTypeToolbar,
    /// `_NET_WM_WINDOW_TYPE_MENU`.
    NetWmWindowTypeMenu,
    /// `_NET_WM_WINDOW_TYPE_UTILITY`.
    NetWmWindowTypeUtility,
    /// `_NET_WM_WINDOW_TYPE_SPLASH`.
    NetWmWindowTypeSplash,
    /// `_NET_WM_WINDOW_TYPE_DIALOG`.
    NetWmWindowTypeDialog,
    /// `_NET_WM_WINDOW_TYPE_DROPDOWN_MENU`.
    NetWmWindowTypeDropdownMenu,
    /// `_NET_WM_WINDOW_TYPE_POPUP_MENU`.
    NetWmWindowTypePopupMenu,
    /// `_NET_WM_WINDOW_TYPE_TOOLTIP`.
    NetWmWindowTypeTooltip,
    /// `_NET_WM_WINDOW_TYPE_NOTIFICATION`.
    NetWmWindowTypeNotification,
    /// `_NET_WM_WINDOW_TYPE_COMBO`.
    NetWmWindowTypeCombo,
    /// `_NET_WM_WINDOW_TYPE_DND`.
    NetWmWindowTypeDnd,
    /// `_NET_WM_WINDOW_TYPE_NORMAL`.
    NetWmWindowTypeNormal,
    /// `_NET_WM_STATE`.
    NetWmState,
    /// `_NET_WM_STATE_MAXIMIZED_VERT`.
    NetWmStateMaximizedVert,
    /// `_NET_WM_STATE_MAXIMIZED_HORZ`.
    NetWmStateMaximizedHorz,
    /// `_NET_WM_STATE_FULLSCREEN`.
    NetWmStateFullscreen,
    /// `_NET_WM_STATE_ABOVE`.
    NetWmStateAbove,
    /// `_NET_WM_STATE_STICKY`.
    NetWmStateSticky,
    /// `_NET_WM_STATE_HIDDEN`, observed but never requested directly.
    NetWmStateHidden,
    /// `_NET_WM_STATE_MODAL`.
    NetWmStateModal,
    /// `_NET_WM_STATE_SHADED`.
    NetWmStateShaded,
    /// `_NET_WM_STATE_SKIP_TASKBAR`.
    NetWmStateSkipTaskbar,
    /// `_NET_WM_STATE_SKIP_PAGER`.
    NetWmStateSkipPager,
    /// `_NET_WM_STATE_BELOW`.
    NetWmStateBelow,
    /// `_NET_WM_STATE_DEMANDS_ATTENTION`.
    NetWmStateDemandsAttention,
    /// `_NET_WM_STATE_FOCUSED`.
    NetWmStateFocused,
    /// `_NET_WM_ALLOWED_ACTIONS`.
    NetWmAllowedActions,
    /// `_NET_WM_ACTION_MOVE`.
    NetWmActionMove,
    /// `_NET_WM_ACTION_RESIZE`.
    NetWmActionResize,
    /// `_NET_WM_ACTION_MINIMIZE`.
    NetWmActionMinimize,
    /// `_NET_WM_ACTION_SHADE`.
    NetWmActionShade,
    /// `_NET_WM_ACTION_STICK`.
    NetWmActionStick,
    /// `_NET_WM_ACTION_MAXIMIZE_HORZ`.
    NetWmActionMaximizeHorz,
    /// `_NET_WM_ACTION_MAXIMIZE_VERT`.
    NetWmActionMaximizeVert,
    /// `_NET_WM_ACTION_CHANGE_DESKTOP`.
    NetWmActionChangeDesktop,
    /// `_NET_WM_ACTION_CLOSE`.
    NetWmActionClose,
    /// `_NET_WM_ACTION_ABOVE`.
    NetWmActionAbove,
    /// `_NET_WM_ACTION_BELOW`.
    NetWmActionBelow,
    /// `_NET_WM_ACTION_FULLSCREEN`.
    NetWmActionFullscreen,
    /// `_NET_WM_DESKTOP`.
    NetWmDesktop,
    /// `_NET_FRAME_EXTENTS`.
    NetFrameExtents,
    /// `WM_CLIENT_LEADER`.
    WmClientLeader,
    /// `WM_PROTOCOLS`.
    WmProtocols,
    /// `WM_DELETE_WINDOW`.
    WmDeleteWindow,
    /// `WM_TAKE_FOCUS`.
    WmTakeFocus,
    /// `_NET_WM_PING`.
    NetWmPing,
    /// `_NET_WM_SYNC_REQUEST`.
    NetWmSyncRequest,
    /// `WM_CHANGE_STATE`.
    WmChangeState,
    /// `UTF8_STRING`.
    Utf8String,
    /// Core `STRING` type.
    String,
    /// Core `ATOM` type.
    Atom,
    /// Core `CARDINAL` type.
    Cardinal,
    /// Core `WINDOW` type.
    Window,
    /// Core `WM_NAME` property.
    WmName,
    /// Core `WM_ICON_NAME` property.
    WmIconName,
    /// Core `WM_CLASS` property.
    WmClass,
    /// Core `WM_CLIENT_MACHINE` property.
    WmClientMachine,
    /// Core `WM_TRANSIENT_FOR` property.
    WmTransientFor,
    /// Core `WM_HINTS` property and type.
    WmHints,
}

impl KnownAtom {
    /// Complete, deterministic inventory.
    pub const ALL: [Self; 76] = [
        Self::NetClientListStacking,
        Self::NetClientList,
        Self::NetActiveWindow,
        Self::NetSupported,
        Self::NetCloseWindow,
        Self::NetMoveResizeWindow,
        Self::NetCurrentDesktop,
        Self::NetNumberOfDesktops,
        Self::NetRestackWindow,
        Self::NetWmName,
        Self::NetWmVisibleName,
        Self::NetWmIconName,
        Self::NetWmVisibleIconName,
        Self::NetWmPid,
        Self::NetWmWindowType,
        Self::NetWmWindowTypeDesktop,
        Self::NetWmWindowTypeDock,
        Self::NetWmWindowTypeToolbar,
        Self::NetWmWindowTypeMenu,
        Self::NetWmWindowTypeUtility,
        Self::NetWmWindowTypeSplash,
        Self::NetWmWindowTypeDialog,
        Self::NetWmWindowTypeDropdownMenu,
        Self::NetWmWindowTypePopupMenu,
        Self::NetWmWindowTypeTooltip,
        Self::NetWmWindowTypeNotification,
        Self::NetWmWindowTypeCombo,
        Self::NetWmWindowTypeDnd,
        Self::NetWmWindowTypeNormal,
        Self::NetWmState,
        Self::NetWmStateMaximizedVert,
        Self::NetWmStateMaximizedHorz,
        Self::NetWmStateFullscreen,
        Self::NetWmStateAbove,
        Self::NetWmStateSticky,
        Self::NetWmStateHidden,
        Self::NetWmStateModal,
        Self::NetWmStateShaded,
        Self::NetWmStateSkipTaskbar,
        Self::NetWmStateSkipPager,
        Self::NetWmStateBelow,
        Self::NetWmStateDemandsAttention,
        Self::NetWmStateFocused,
        Self::NetWmAllowedActions,
        Self::NetWmActionMove,
        Self::NetWmActionResize,
        Self::NetWmActionMinimize,
        Self::NetWmActionShade,
        Self::NetWmActionStick,
        Self::NetWmActionMaximizeHorz,
        Self::NetWmActionMaximizeVert,
        Self::NetWmActionChangeDesktop,
        Self::NetWmActionClose,
        Self::NetWmActionAbove,
        Self::NetWmActionBelow,
        Self::NetWmActionFullscreen,
        Self::NetWmDesktop,
        Self::NetFrameExtents,
        Self::WmClientLeader,
        Self::WmProtocols,
        Self::WmDeleteWindow,
        Self::WmTakeFocus,
        Self::NetWmPing,
        Self::NetWmSyncRequest,
        Self::WmChangeState,
        Self::Utf8String,
        Self::String,
        Self::Atom,
        Self::Cardinal,
        Self::Window,
        Self::WmName,
        Self::WmIconName,
        Self::WmClass,
        Self::WmClientMachine,
        Self::WmTransientFor,
        Self::WmHints,
    ];

    /// Static wire name used when an atom is not predefined by core X11.
    #[must_use]
    pub const fn name(self) -> &'static [u8] {
        match self {
            Self::NetClientListStacking => b"_NET_CLIENT_LIST_STACKING",
            Self::NetClientList => b"_NET_CLIENT_LIST",
            Self::NetActiveWindow => b"_NET_ACTIVE_WINDOW",
            Self::NetSupported => b"_NET_SUPPORTED",
            Self::NetCloseWindow => b"_NET_CLOSE_WINDOW",
            Self::NetMoveResizeWindow => b"_NET_MOVERESIZE_WINDOW",
            Self::NetCurrentDesktop => b"_NET_CURRENT_DESKTOP",
            Self::NetNumberOfDesktops => b"_NET_NUMBER_OF_DESKTOPS",
            Self::NetRestackWindow => b"_NET_RESTACK_WINDOW",
            Self::NetWmName => b"_NET_WM_NAME",
            Self::NetWmVisibleName => b"_NET_WM_VISIBLE_NAME",
            Self::NetWmIconName => b"_NET_WM_ICON_NAME",
            Self::NetWmVisibleIconName => b"_NET_WM_VISIBLE_ICON_NAME",
            Self::NetWmPid => b"_NET_WM_PID",
            Self::NetWmWindowType => b"_NET_WM_WINDOW_TYPE",
            Self::NetWmWindowTypeDesktop => b"_NET_WM_WINDOW_TYPE_DESKTOP",
            Self::NetWmWindowTypeDock => b"_NET_WM_WINDOW_TYPE_DOCK",
            Self::NetWmWindowTypeToolbar => b"_NET_WM_WINDOW_TYPE_TOOLBAR",
            Self::NetWmWindowTypeMenu => b"_NET_WM_WINDOW_TYPE_MENU",
            Self::NetWmWindowTypeUtility => b"_NET_WM_WINDOW_TYPE_UTILITY",
            Self::NetWmWindowTypeSplash => b"_NET_WM_WINDOW_TYPE_SPLASH",
            Self::NetWmWindowTypeDialog => b"_NET_WM_WINDOW_TYPE_DIALOG",
            Self::NetWmWindowTypeDropdownMenu => b"_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
            Self::NetWmWindowTypePopupMenu => b"_NET_WM_WINDOW_TYPE_POPUP_MENU",
            Self::NetWmWindowTypeTooltip => b"_NET_WM_WINDOW_TYPE_TOOLTIP",
            Self::NetWmWindowTypeNotification => b"_NET_WM_WINDOW_TYPE_NOTIFICATION",
            Self::NetWmWindowTypeCombo => b"_NET_WM_WINDOW_TYPE_COMBO",
            Self::NetWmWindowTypeDnd => b"_NET_WM_WINDOW_TYPE_DND",
            Self::NetWmWindowTypeNormal => b"_NET_WM_WINDOW_TYPE_NORMAL",
            Self::NetWmState => b"_NET_WM_STATE",
            Self::NetWmStateMaximizedVert => b"_NET_WM_STATE_MAXIMIZED_VERT",
            Self::NetWmStateMaximizedHorz => b"_NET_WM_STATE_MAXIMIZED_HORZ",
            Self::NetWmStateFullscreen => b"_NET_WM_STATE_FULLSCREEN",
            Self::NetWmStateAbove => b"_NET_WM_STATE_ABOVE",
            Self::NetWmStateSticky => b"_NET_WM_STATE_STICKY",
            Self::NetWmStateHidden => b"_NET_WM_STATE_HIDDEN",
            Self::NetWmStateModal => b"_NET_WM_STATE_MODAL",
            Self::NetWmStateShaded => b"_NET_WM_STATE_SHADED",
            Self::NetWmStateSkipTaskbar => b"_NET_WM_STATE_SKIP_TASKBAR",
            Self::NetWmStateSkipPager => b"_NET_WM_STATE_SKIP_PAGER",
            Self::NetWmStateBelow => b"_NET_WM_STATE_BELOW",
            Self::NetWmStateDemandsAttention => b"_NET_WM_STATE_DEMANDS_ATTENTION",
            Self::NetWmStateFocused => b"_NET_WM_STATE_FOCUSED",
            Self::NetWmAllowedActions => b"_NET_WM_ALLOWED_ACTIONS",
            Self::NetWmActionMove => b"_NET_WM_ACTION_MOVE",
            Self::NetWmActionResize => b"_NET_WM_ACTION_RESIZE",
            Self::NetWmActionMinimize => b"_NET_WM_ACTION_MINIMIZE",
            Self::NetWmActionShade => b"_NET_WM_ACTION_SHADE",
            Self::NetWmActionStick => b"_NET_WM_ACTION_STICK",
            Self::NetWmActionMaximizeHorz => b"_NET_WM_ACTION_MAXIMIZE_HORZ",
            Self::NetWmActionMaximizeVert => b"_NET_WM_ACTION_MAXIMIZE_VERT",
            Self::NetWmActionChangeDesktop => b"_NET_WM_ACTION_CHANGE_DESKTOP",
            Self::NetWmActionClose => b"_NET_WM_ACTION_CLOSE",
            Self::NetWmActionAbove => b"_NET_WM_ACTION_ABOVE",
            Self::NetWmActionBelow => b"_NET_WM_ACTION_BELOW",
            Self::NetWmActionFullscreen => b"_NET_WM_ACTION_FULLSCREEN",
            Self::NetWmDesktop => b"_NET_WM_DESKTOP",
            Self::NetFrameExtents => b"_NET_FRAME_EXTENTS",
            Self::WmClientLeader => b"WM_CLIENT_LEADER",
            Self::WmProtocols => b"WM_PROTOCOLS",
            Self::WmDeleteWindow => b"WM_DELETE_WINDOW",
            Self::WmTakeFocus => b"WM_TAKE_FOCUS",
            Self::NetWmPing => b"_NET_WM_PING",
            Self::NetWmSyncRequest => b"_NET_WM_SYNC_REQUEST",
            Self::WmChangeState => b"WM_CHANGE_STATE",
            Self::Utf8String => b"UTF8_STRING",
            Self::String => b"STRING",
            Self::Atom => b"ATOM",
            Self::Cardinal => b"CARDINAL",
            Self::Window => b"WINDOW",
            Self::WmName => b"WM_NAME",
            Self::WmIconName => b"WM_ICON_NAME",
            Self::WmClass => b"WM_CLASS",
            Self::WmClientMachine => b"WM_CLIENT_MACHINE",
            Self::WmTransientFor => b"WM_TRANSIENT_FOR",
            Self::WmHints => b"WM_HINTS",
        }
    }

    /// Canonical reviewed EWMH/ICCCM/core name.
    #[must_use]
    pub fn canonical_name(self) -> &'static str {
        // Every fixed inventory spelling above is an ASCII protocol token.
        std::str::from_utf8(self.name()).unwrap_or("")
    }

    fn predefined(self) -> Option<Atom> {
        match self {
            Self::String => Some(u32::from(AtomEnum::STRING)),
            Self::Atom => Some(u32::from(AtomEnum::ATOM)),
            Self::Cardinal => Some(u32::from(AtomEnum::CARDINAL)),
            Self::Window => Some(u32::from(AtomEnum::WINDOW)),
            Self::WmName => Some(u32::from(AtomEnum::WM_NAME)),
            Self::WmIconName => Some(u32::from(AtomEnum::WM_ICON_NAME)),
            Self::WmClass => Some(u32::from(AtomEnum::WM_CLASS)),
            Self::WmClientMachine => Some(u32::from(AtomEnum::WM_CLIENT_MACHINE)),
            Self::WmTransientFor => Some(u32::from(AtomEnum::WM_TRANSIENT_FOR)),
            Self::WmHints => Some(u32::from(AtomEnum::WM_HINTS)),
            _ => None,
        }
    }
}

/// Resolved IDs for the complete fixed observation atom inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownAtoms {
    values: [Atom; KnownAtom::ALL.len()],
}

impl KnownAtoms {
    /// Resolve the fixed inventory on the connection owner's thread.
    pub fn intern<C: Connection>(connection: &C) -> Result<Self> {
        let mut values = [0; KnownAtom::ALL.len()];
        for known in KnownAtom::ALL {
            values[known as usize] = if let Some(predefined) = known.predefined() {
                predefined
            } else {
                connection
                    .intern_atom(false, known.name())
                    .map_err(|error| X11Error::Connection(error.to_string()))?
                    .reply()
                    .map_err(|error| X11Error::Reply(error.to_string()))?
                    .atom
            };
        }
        Ok(Self { values })
    }

    /// Look up one reviewed atom ID.
    #[must_use]
    pub const fn get(&self, atom: KnownAtom) -> Atom {
        self.values[atom as usize]
    }

    /// Resolve an observed numeric atom ID back to the reviewed inventory.
    #[must_use]
    pub fn identify(&self, observed: Atom) -> Option<KnownAtom> {
        KnownAtom::ALL
            .into_iter()
            .find(|known| self.get(*known) == observed)
    }

    #[cfg(test)]
    pub(crate) fn for_test(mut resolve: impl FnMut(KnownAtom) -> Atom) -> Self {
        let mut values = [0; KnownAtom::ALL.len()];
        for atom in KnownAtom::ALL {
            values[atom as usize] = resolve(atom);
        }
        Self { values }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn fixed_atom_policy_is_unique_and_has_no_unbounded_names() {
        let names: HashSet<_> = KnownAtom::ALL.into_iter().map(KnownAtom::name).collect();
        assert_eq!(names.len(), KnownAtom::ALL.len());
        assert!(
            names
                .iter()
                .all(|name| !name.is_empty() && name.len() <= 255)
        );
        assert!(names.contains(b"_NET_CLIENT_LIST".as_slice()));
        assert!(names.contains(b"WM_CLASS".as_slice()));
        assert!(names.contains(b"_NET_WM_STATE_MODAL".as_slice()));
        assert!(names.contains(b"_NET_WM_WINDOW_TYPE_DIALOG".as_slice()));
        assert!(names.contains(b"_NET_WM_ACTION_CLOSE".as_slice()));
    }

    #[test]
    fn lookup_is_total_over_the_fixed_inventory() {
        let atoms = KnownAtoms::for_test(|atom| atom as u32 + 1);
        for atom in KnownAtom::ALL {
            assert_eq!(atoms.get(atom), atom as u32 + 1);
        }
    }

    #[test]
    fn reverse_lookup_and_canonical_name_preserve_reviewed_identity() {
        let atoms = KnownAtoms::for_test(|atom| atom as u32 + 1_000);
        let known = KnownAtom::NetWmStateFullscreen;
        assert_eq!(atoms.identify(atoms.get(known)), Some(known));
        assert_eq!(known.canonical_name(), "_NET_WM_STATE_FULLSCREEN");
        assert_eq!(atoms.identify(u32::MAX), None);
    }
}
