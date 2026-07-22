//! Bounded core-focus ancestry normalization.

use std::collections::HashSet;

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::ErrorKind;
use x11rb::protocol::xproto::{ConnectionExt as _, Window};

use crate::{Result, X11Error};

/// Maximum parent links followed while correlating core focus to a client.
pub const MAX_FOCUS_ANCESTRY_DEPTH: usize = 64;

/// Why focus ancestry traversal stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusAncestryStatus {
    /// No ordinary focus XID was reported.
    NoFocus,
    /// Parent traversal reached the nominated root.
    Resolved,
    /// Focus window vanished during the bounded traversal.
    Vanished,
    /// A malformed parent cycle was detected.
    Cycle,
    /// The fixed parent-depth ceiling was reached.
    DepthExceeded,
    /// QueryTree reported a different root.
    DifferentRoot,
}

/// Raw and normalized focus evidence for one target snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FocusAncestryInput {
    /// Raw core focus XID, excluding `None` and `PointerRoot` sentinels.
    pub raw_focus: Option<Window>,
    /// Nearest ancestor present in the bounded known-client inventory.
    pub normalized_top_level: Option<Window>,
    /// Whether the raw focus XID is the target or one of its descendants.
    pub target_contains_focus: bool,
    /// Bounded traversal terminal status.
    pub status: FocusAncestryStatus,
}

/// Query core focus and prove bounded ancestry to the target/known clients.
pub fn query_focus_ancestry<C: Connection>(
    connection: &C,
    root: Window,
    target: Window,
    known_clients: &[Window],
) -> Result<FocusAncestryInput> {
    let raw = connection
        .get_input_focus()
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?
        .focus;
    resolve_focus_ancestry(raw, root, target, known_clients, |window| {
        let reply = connection
            .query_tree(window)
            .map_err(|error| ParentLookupError::Terminal(X11Error::Connection(error.to_string())))?
            .reply()
            .map_err(classify_query_tree_error)?;
        Ok((reply.root, reply.parent))
    })
}

#[derive(Debug)]
enum ParentLookupError {
    Vanished,
    Terminal(X11Error),
}

impl From<ParentLookupError> for X11Error {
    fn from(error: ParentLookupError) -> Self {
        match error {
            ParentLookupError::Vanished => X11Error::Reply("focus window vanished".to_owned()),
            ParentLookupError::Terminal(error) => error,
        }
    }
}

fn classify_query_tree_error(error: ReplyError) -> ParentLookupError {
    match error {
        ReplyError::ConnectionError(error) => {
            ParentLookupError::Terminal(X11Error::Connection(error.to_string()))
        }
        ReplyError::X11Error(error) if error.error_kind == ErrorKind::Window => {
            ParentLookupError::Vanished
        }
        ReplyError::X11Error(error) => {
            ParentLookupError::Terminal(X11Error::Reply(format!("{error:?}")))
        }
    }
}

fn resolve_focus_ancestry<F>(
    raw_focus: Window,
    root: Window,
    target: Window,
    known_clients: &[Window],
    mut parent_of: F,
) -> std::result::Result<FocusAncestryInput, X11Error>
where
    F: FnMut(Window) -> std::result::Result<(Window, Window), ParentLookupError>,
{
    if raw_focus <= 1 || raw_focus == root {
        return Ok(FocusAncestryInput {
            raw_focus: None,
            normalized_top_level: None,
            target_contains_focus: false,
            status: FocusAncestryStatus::NoFocus,
        });
    }

    let mut current = raw_focus;
    let mut seen = HashSet::with_capacity(MAX_FOCUS_ANCESTRY_DEPTH);
    let mut normalized = None;
    let mut target_contains_focus = false;
    for _ in 0..MAX_FOCUS_ANCESTRY_DEPTH {
        if !seen.insert(current) {
            return Ok(focus_result(
                raw_focus,
                normalized,
                target_contains_focus,
                FocusAncestryStatus::Cycle,
            ));
        }
        if current == target {
            target_contains_focus = true;
            normalized.get_or_insert(target);
        } else if normalized.is_none() && known_clients.contains(&current) {
            normalized = Some(current);
        }
        let (observed_root, parent) = match parent_of(current) {
            Ok(value) => value,
            Err(ParentLookupError::Vanished) => {
                return Ok(focus_result(
                    raw_focus,
                    normalized,
                    target_contains_focus,
                    FocusAncestryStatus::Vanished,
                ));
            }
            Err(ParentLookupError::Terminal(error)) => return Err(error),
        };
        if observed_root != root {
            return Ok(focus_result(
                raw_focus,
                normalized,
                target_contains_focus,
                FocusAncestryStatus::DifferentRoot,
            ));
        }
        if parent == root || parent == 0 {
            return Ok(focus_result(
                raw_focus,
                normalized,
                target_contains_focus,
                FocusAncestryStatus::Resolved,
            ));
        }
        current = parent;
    }
    Ok(focus_result(
        raw_focus,
        normalized,
        target_contains_focus,
        FocusAncestryStatus::DepthExceeded,
    ))
}

const fn focus_result(
    raw_focus: Window,
    normalized_top_level: Option<Window>,
    target_contains_focus: bool,
    status: FocusAncestryStatus,
) -> FocusAncestryInput {
    FocusAncestryInput {
        raw_focus: Some(raw_focus),
        normalized_top_level,
        target_contains_focus,
        status,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn child_focus_normalizes_through_reparenting_frame_to_known_client() {
        let evidence = resolve_focus_ancestry(40, 1, 20, &[], |window| {
            Ok(match window {
                40 => (1, 20),
                20 => (1, 10),
                10 => (1, 1),
                _ => unreachable!(),
            })
        })
        .unwrap();
        assert_eq!(evidence.raw_focus, Some(40));
        assert_eq!(evidence.normalized_top_level, Some(20));
        assert!(evidence.target_contains_focus);
        assert_eq!(evidence.status, FocusAncestryStatus::Resolved);
    }

    #[test]
    fn cycle_depth_and_vanish_are_bounded_non_success_evidence() {
        let cycle = resolve_focus_ancestry(4, 1, 8, &[], |window| {
            Ok((1, if window == 4 { 5 } else { 4 }))
        })
        .unwrap();
        assert_eq!(cycle.status, FocusAncestryStatus::Cycle);
        assert!(!cycle.target_contains_focus);

        let depth = resolve_focus_ancestry(100, 1, 8, &[], |window| Ok((1, window + 1))).unwrap();
        assert_eq!(depth.status, FocusAncestryStatus::DepthExceeded);

        let vanished =
            resolve_focus_ancestry(4, 1, 8, &[], |_| Err(ParentLookupError::Vanished)).unwrap();
        assert_eq!(vanished.status, FocusAncestryStatus::Vanished);
    }
}
