#![allow(clippy::panic, clippy::unwrap_used)]

use super::*;
use crate::clipboard::sha256_digest;

fn transfer(requestor: Window, property: Atom, bytes: usize, now: Instant) -> OutgoingIncr {
    let body: Arc<[u8]> = vec![b'x'; bytes].into();
    OutgoingIncr::new(
        OutgoingKey {
            requestor,
            property,
        },
        RawClipboardTarget::Utf8String,
        Arc::clone(&body),
        sha256_digest(&body),
        now,
    )
    .unwrap()
}

#[test]
fn direct_boundary_is_inclusive_and_next_byte_is_incr() {
    assert_eq!(
        transfer_wire_mode(CLIPBOARD_DIRECT_LIMIT_BYTES),
        TransferWireMode::Direct
    );
    assert_eq!(
        transfer_wire_mode(CLIPBOARD_DIRECT_LIMIT_BYTES + 1),
        TransferWireMode::Incr
    );
}

#[test]
fn incr_chunks_exactly_and_finishes_only_after_next_delete() {
    let now = Instant::now();
    let total = CLIPBOARD_DIRECT_LIMIT_BYTES + 3;
    let mut state = transfer(1, 2, total, now);
    for (sequence, expected) in [
        (1, CLIPBOARD_INCR_CHUNK_BYTES),
        (2, CLIPBOARD_INCR_CHUNK_BYTES),
        (3, CLIPBOARD_INCR_CHUNK_BYTES),
        (4, CLIPBOARD_INCR_CHUNK_BYTES),
        (5, 3),
    ] {
        let OutgoingAction::WriteChunk { bytes, target } = state.on_property_deleted(sequence, now)
        else {
            unreachable!()
        };
        assert_eq!(target, RawClipboardTarget::Utf8String);
        assert_eq!(bytes.len(), expected);
    }
    let OutgoingAction::WriteTerminator(evidence) = state.on_property_deleted(6, now) else {
        unreachable!()
    };
    assert!(evidence.terminal_chunk_observed);
    assert_eq!(
        evidence.transfer,
        SelectionTransferMode::Incr {
            announced_minimum_bytes: total as u64,
            chunks: 5,
        }
    );
}

#[test]
fn duplicate_delete_sequence_cannot_skip_a_chunk() {
    let now = Instant::now();
    let mut state = transfer(1, 2, CLIPBOARD_DIRECT_LIMIT_BYTES + 10, now);
    assert!(matches!(
        state.on_property_deleted(8, now),
        OutgoingAction::WriteChunk { .. }
    ));
    assert_eq!(
        state.on_property_deleted(8, now),
        OutgoingAction::DuplicateIgnored
    );
}

#[test]
fn admission_enforces_duplicate_per_requestor_and_global_limits() {
    let now = Instant::now();
    let mut table = OutgoingTransfers::default();
    table
        .insert(transfer(1, 10, CLIPBOARD_DIRECT_LIMIT_BYTES + 1, now))
        .unwrap();
    assert_eq!(
        table.insert(transfer(1, 10, CLIPBOARD_DIRECT_LIMIT_BYTES + 1, now)),
        Err(OutgoingAdmissionError::DuplicateProperty)
    );
    table
        .insert(transfer(1, 11, CLIPBOARD_DIRECT_LIMIT_BYTES + 1, now))
        .unwrap();
    assert_eq!(
        table.insert(transfer(1, 12, CLIPBOARD_DIRECT_LIMIT_BYTES + 1, now)),
        Err(OutgoingAdmissionError::PerRequestorLimit)
    );
    for requestor in 2..=7 {
        table
            .insert(transfer(
                requestor,
                20 + requestor,
                CLIPBOARD_DIRECT_LIMIT_BYTES + 1,
                now,
            ))
            .unwrap();
    }
    assert_eq!(table.len(), MAX_INCR_TRANSFERS_GLOBAL);
    assert_eq!(
        table.insert(transfer(8, 40, CLIPBOARD_DIRECT_LIMIT_BYTES + 1, now)),
        Err(OutgoingAdmissionError::GlobalLimit)
    );
}

#[test]
fn timeout_and_requestor_cleanup_are_content_free_terminal_evidence() {
    let now = Instant::now();
    let mut table = OutgoingTransfers::default();
    table
        .insert(transfer(9, 1, CLIPBOARD_DIRECT_LIMIT_BYTES + 1, now))
        .unwrap();
    table
        .insert(transfer(9, 2, CLIPBOARD_DIRECT_LIMIT_BYTES + 1, now))
        .unwrap();
    assert_eq!(
        table.expired_keys(now + CLIPBOARD_TRANSFER_TIMEOUT),
        vec![
            OutgoingKey {
                requestor: 9,
                property: 1
            },
            OutgoingKey {
                requestor: 9,
                property: 2
            }
        ]
    );
    let removed = table.remove_requestor(9);
    assert_eq!(removed.len(), 2);
    assert!(removed.into_iter().all(|transfer| {
        matches!(
            transfer
                .fail(SelectionTransferFailureReason::RequestorDestroyed)
                .terminal,
            SelectionTransferTerminal::Failed {
                reason: SelectionTransferFailureReason::RequestorDestroyed
            }
        )
    }));
}

#[test]
fn multiple_requires_atom_pair_even_bounded_pairs_and_preserves_order() {
    assert!(decode_multiple_pairs(1, 32, &[10, 20, 11], 1).is_none());
    assert!(decode_multiple_pairs(2, 32, &[10, 20], 1).is_none());
    assert!(decode_multiple_pairs(1, 16, &[10, 20], 1).is_none());
    let mut pairs = decode_multiple_pairs(1, 32, &[10, 20, 11, 21, 12, 22], 1).unwrap();
    pairs[1].property = 0;
    assert_eq!(encode_multiple_pairs(&pairs), vec![10, 20, 11, 0, 12, 22]);

    let oversized: Vec<u32> = (0..=(MAX_MULTIPLE_ITEMS as u32 * 2)).collect();
    assert!(decode_multiple_pairs(1, 32, &oversized, 1).is_none());
}
