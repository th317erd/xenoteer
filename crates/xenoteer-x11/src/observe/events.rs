//! Pure event-to-reconciliation classification.

use x11rb::protocol::xproto::Window;

use super::PollThreadEvent;
use super::atoms::{KnownAtom, KnownAtoms};

/// Narrow portion of a window snapshot invalidated by an event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowRefresh {
    /// Core window attributes, especially map state.
    Attributes,
    /// Root-physical geometry and frame extents.
    Geometry,
    /// Titles, class, machine, atom sets, or protocols.
    Metadata,
    /// Window-manager state and ICCCM hints.
    State,
    /// Advisory PID or client-leader evidence.
    ProcessEvidence,
    /// Transient or group relationships.
    Relations,
    /// Reported EWMH workspace.
    Workspace,
}

/// Side-effect-free instruction for the future observation actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileDecision {
    /// Discover and snapshot a newly plausible client.
    ObserveWindow {
        /// Candidate child XID.
        window: Window,
    },
    /// Remove one XID birth from the live model.
    RemoveWindow {
        /// Destroyed XID.
        window: Window,
    },
    /// Refresh one bounded slice of a live window.
    RefreshWindow {
        /// Window whose bounded slice is stale.
        window: Window,
        /// Narrow snapshot slice to refresh.
        refresh: WindowRefresh,
    },
    /// Re-read focus and root `_NET_ACTIVE_WINDOW` evidence.
    RefreshFocus,
    /// Re-read an EWMH list or the root tree and reconcile membership/order.
    RebuildInventory,
    /// Queue overflow invalidated all incremental assumptions.
    FullResync,
    /// The actor must stop or reconnect rather than mutate stale state.
    ConnectionFailed,
    /// Event is irrelevant to the window model.
    Ignore,
}

/// Classify one normalized event without performing I/O or granting authority.
#[must_use]
pub fn classify_reconcile(
    event: &PollThreadEvent,
    root: Window,
    atoms: &KnownAtoms,
) -> ReconcileDecision {
    match *event {
        PollThreadEvent::Create { window } | PollThreadEvent::Map { window } => {
            ReconcileDecision::ObserveWindow { window }
        }
        PollThreadEvent::Unmap { window } => ReconcileDecision::RefreshWindow {
            window,
            refresh: WindowRefresh::Attributes,
        },
        PollThreadEvent::Destroy { window } => ReconcileDecision::RemoveWindow { window },
        PollThreadEvent::Configure { window, .. } if window == root => {
            ReconcileDecision::FullResync
        }
        PollThreadEvent::Configure { window, .. } => ReconcileDecision::RefreshWindow {
            window,
            refresh: WindowRefresh::Geometry,
        },
        PollThreadEvent::Focus { .. } => ReconcileDecision::RefreshFocus,
        PollThreadEvent::Property { window, atom, .. } => {
            classify_property(window, atom, root, atoms)
        }
        PollThreadEvent::ResyncRequired => ReconcileDecision::FullResync,
        PollThreadEvent::Failed { .. } => ReconcileDecision::ConnectionFailed,
        PollThreadEvent::Motion { .. }
        | PollThreadEvent::RootDamage { .. }
        | PollThreadEvent::Other { .. } => ReconcileDecision::Ignore,
    }
}

fn classify_property(
    window: Window,
    atom: u32,
    root: Window,
    atoms: &KnownAtoms,
) -> ReconcileDecision {
    if window == root {
        if [KnownAtom::NetClientListStacking, KnownAtom::NetClientList]
            .into_iter()
            .any(|known| atoms.get(known) == atom)
        {
            return ReconcileDecision::RebuildInventory;
        }
        if atom == atoms.get(KnownAtom::NetActiveWindow) {
            return ReconcileDecision::RefreshFocus;
        }
        return ReconcileDecision::Ignore;
    }

    let refresh = if [
        KnownAtom::NetWmName,
        KnownAtom::NetWmVisibleName,
        KnownAtom::NetWmIconName,
        KnownAtom::NetWmVisibleIconName,
        KnownAtom::NetWmWindowType,
        KnownAtom::NetWmAllowedActions,
        KnownAtom::WmName,
        KnownAtom::WmIconName,
        KnownAtom::WmClass,
        KnownAtom::WmClientMachine,
        KnownAtom::WmProtocols,
    ]
    .into_iter()
    .any(|known| atoms.get(known) == atom)
    {
        WindowRefresh::Metadata
    } else if [KnownAtom::NetWmState, KnownAtom::WmHints]
        .into_iter()
        .any(|known| atoms.get(known) == atom)
    {
        WindowRefresh::State
    } else if [KnownAtom::NetWmPid, KnownAtom::WmClientLeader]
        .into_iter()
        .any(|known| atoms.get(known) == atom)
    {
        WindowRefresh::ProcessEvidence
    } else if atom == atoms.get(KnownAtom::NetFrameExtents) {
        WindowRefresh::Geometry
    } else if atom == atoms.get(KnownAtom::WmTransientFor) {
        WindowRefresh::Relations
    } else if atom == atoms.get(KnownAtom::NetWmDesktop) {
        WindowRefresh::Workspace
    } else {
        return ReconcileDecision::Ignore;
    };
    ReconcileDecision::RefreshWindow { window, refresh }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atoms() -> KnownAtoms {
        KnownAtoms::for_test(|atom| 100 + atom as u32)
    }

    #[test]
    fn root_client_list_requires_inventory_rebuild() {
        let atoms = atoms();
        assert_eq!(
            classify_reconcile(
                &PollThreadEvent::Property {
                    window: 1,
                    atom: atoms.get(KnownAtom::NetClientList),
                    deleted: false,
                },
                1,
                &atoms,
            ),
            ReconcileDecision::RebuildInventory
        );
    }

    #[test]
    fn geometry_and_unknown_properties_have_narrow_decisions() {
        let atoms = atoms();
        assert_eq!(
            classify_reconcile(
                &PollThreadEvent::Property {
                    window: 9,
                    atom: atoms.get(KnownAtom::NetFrameExtents),
                    deleted: true,
                },
                1,
                &atoms,
            ),
            ReconcileDecision::RefreshWindow {
                window: 9,
                refresh: WindowRefresh::Geometry,
            }
        );
        assert_eq!(
            classify_reconcile(
                &PollThreadEvent::Property {
                    window: 9,
                    atom: 99_999,
                    deleted: false,
                },
                1,
                &atoms,
            ),
            ReconcileDecision::Ignore
        );
    }
}
