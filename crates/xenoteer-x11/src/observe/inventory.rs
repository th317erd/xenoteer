//! Bounded initial root-window inventory.

use std::collections::HashSet;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, Window};

use super::atoms::{KnownAtom, KnownAtoms};
use super::property::{PropertyWarning, decode_u32_list, read_property_bounded};
use crate::{Result, X11Error};

/// Maximum root children retained by the observation model.
pub const MAX_ROOT_WINDOWS: usize = 4_096;

/// Authority and ordering semantics of an initial inventory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InventorySource {
    /// EWMH bottom-to-top client stacking order.
    NetClientListStacking,
    /// EWMH managed-client list without stacking semantics.
    NetClientList,
    /// Raw root children, including possible frames and override-redirect windows.
    QueryTreeFallback,
}

/// Non-fatal inventory evidence retained for reconciliation diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InventoryWarning {
    /// An EWMH property could not be decoded authoritatively.
    Property {
        /// Property that failed decoding.
        atom: KnownAtom,
        /// Decode failure class.
        warning: PropertyWarning,
    },
    /// The source exceeded the retained model ceiling.
    Truncated,
    /// A zero or root XID was discarded.
    InvalidMember,
    /// A repeated XID was discarded while preserving first occurrence.
    DuplicateMember,
    /// A listed window disappeared before its event subscription or snapshot.
    VanishedMember,
    /// `QueryTree` did not identify the requested root as its root.
    RootMismatch,
}

/// Bounded initial inventory of candidate client windows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootInventory {
    /// Candidate XIDs in source order.
    pub windows: Vec<Window>,
    /// Source and its ordering guarantees.
    pub source: InventorySource,
    /// Deduplicated non-fatal warnings.
    pub warnings: Vec<InventoryWarning>,
}

/// Read the authoritative EWMH client list when available, then fall back to
/// raw root children. The connection remains borrowed by its single owner.
pub fn initial_root_inventory<C: Connection>(
    connection: &C,
    root: Window,
    atoms: &KnownAtoms,
) -> Result<RootInventory> {
    let mut warnings = Vec::new();
    for (property, source) in [
        (
            KnownAtom::NetClientListStacking,
            InventorySource::NetClientListStacking,
        ),
        (KnownAtom::NetClientList, InventorySource::NetClientList),
    ] {
        let raw = read_property_bounded(
            connection,
            root,
            atoms.get(property),
            atoms.get(KnownAtom::Window),
            MAX_ROOT_WINDOWS * 4,
        )?;
        if raw.is_absent() {
            continue;
        }
        let decoded = decode_u32_list(&raw, atoms.get(KnownAtom::Window), MAX_ROOT_WINDOWS);
        for warning in decoded.warnings.iter().copied() {
            push_warning(
                &mut warnings,
                InventoryWarning::Property {
                    atom: property,
                    warning,
                },
            );
        }
        let complete = !decoded.warnings.iter().any(|warning| {
            matches!(
                warning,
                PropertyWarning::Truncated
                    | PropertyWarning::UnexpectedType
                    | PropertyWarning::UnexpectedFormat
                    | PropertyWarning::Malformed
            )
        });
        if complete {
            let mut windows = decoded.value.unwrap_or_default();
            normalize_members(&mut windows, root, &mut warnings);
            return Ok(RootInventory {
                windows,
                source,
                warnings,
            });
        }
    }

    let reply = connection
        .query_tree(root)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    if reply.root != root {
        push_warning(&mut warnings, InventoryWarning::RootMismatch);
    }
    let mut windows = reply.children;
    if windows.len() > MAX_ROOT_WINDOWS {
        windows.truncate(MAX_ROOT_WINDOWS);
        push_warning(&mut warnings, InventoryWarning::Truncated);
    }
    normalize_members(&mut windows, root, &mut warnings);
    Ok(RootInventory {
        windows,
        source: InventorySource::QueryTreeFallback,
        warnings,
    })
}

fn normalize_members(
    windows: &mut Vec<Window>,
    root: Window,
    warnings: &mut Vec<InventoryWarning>,
) {
    let mut seen = HashSet::with_capacity(windows.len());
    windows.retain(|window| {
        if *window == 0 || *window == root {
            push_warning(warnings, InventoryWarning::InvalidMember);
            false
        } else if !seen.insert(*window) {
            push_warning(warnings, InventoryWarning::DuplicateMember);
            false
        } else {
            true
        }
    });
}

fn push_warning(warnings: &mut Vec<InventoryWarning>, warning: InventoryWarning) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_discards_invalid_and_duplicate_members_in_order() {
        let mut windows = vec![0, 10, 9, 10, 8, 9];
        let mut warnings = Vec::new();
        normalize_members(&mut windows, 9, &mut warnings);
        assert_eq!(windows, vec![10, 8]);
        assert!(warnings.contains(&InventoryWarning::InvalidMember));
        assert!(warnings.contains(&InventoryWarning::DuplicateMember));
    }
}
