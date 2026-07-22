#![allow(clippy::panic, clippy::unwrap_used)]

use super::*;
use crate::clipboard::{CLIPBOARD_DIRECT_LIMIT_BYTES, ClipboardPayloadKind};

fn incoming(now: Instant) -> IncomingTransfer {
    let mut state = IncomingTransfer::new(SelectionName::Clipboard, 50, 200, 3, false, now);
    assert!(state.select_target(RawClipboardTarget::Utf8String));
    state
}

#[test]
fn target_negotiation_is_ordered_and_unique_policy_is_external() {
    let advertised = [
        RawClipboardTarget::String,
        RawClipboardTarget::TextPlain,
        RawClipboardTarget::Utf8String,
    ];
    assert_eq!(
        choose_target(&advertised, &[]),
        Some(RawClipboardTarget::Utf8String)
    );
    assert_eq!(
        choose_target(
            &advertised,
            &[RawClipboardTarget::String, RawClipboardTarget::Utf8String]
        ),
        Some(RawClipboardTarget::String)
    );
}

#[test]
fn direct_and_incr_require_exact_target_and_zero_terminator() {
    let now = Instant::now();
    let mut direct = incoming(now);
    assert!(matches!(
        direct.finish_direct(
            RawClipboardTarget::Utf8String,
            RawClipboardTarget::Utf8String,
            8,
            b"hello"
        ),
        IncomingAction::Completed(_)
    ));

    let mut incr = incoming(now);
    assert_eq!(
        incr.begin_incr(RawClipboardTarget::Utf8String, 5),
        IncomingAction::DeleteForNextChunk
    );
    assert_eq!(
        incr.receive_incr_chunk(RawClipboardTarget::Utf8String, 8, b"hello", 50, now),
        IncomingAction::DeleteForNextChunk
    );
    let IncomingAction::Completed(result) =
        incr.receive_incr_chunk(RawClipboardTarget::Utf8String, 8, b"", 50, now)
    else {
        unreachable!()
    };
    assert_eq!(result.payload.expose_secret(), b"hello");
    assert!(result.evidence.terminal_chunk_observed);
}

#[test]
fn incoming_incr_accepts_chunks_larger_than_our_outbound_chunk_policy() {
    let now = Instant::now();
    let first = vec![b'a'; CLIPBOARD_DIRECT_LIMIT_BYTES];
    let second = vec![b'b'; 128 * 1_024];
    let mut incr = incoming(now);
    assert_eq!(
        incr.begin_incr(
            RawClipboardTarget::Utf8String,
            (first.len() + second.len()) as u64,
        ),
        IncomingAction::DeleteForNextChunk
    );
    assert_eq!(
        incr.receive_incr_chunk(RawClipboardTarget::Utf8String, 8, &first, 50, now),
        IncomingAction::DeleteForNextChunk
    );
    assert_eq!(
        incr.receive_incr_chunk(RawClipboardTarget::Utf8String, 8, &second, 50, now),
        IncomingAction::DeleteForNextChunk
    );
    let IncomingAction::Completed(result) =
        incr.receive_incr_chunk(RawClipboardTarget::Utf8String, 8, b"", 50, now)
    else {
        unreachable!()
    };
    assert_eq!(result.payload.byte_len(), first.len() + second.len());
    assert_eq!(
        result.evidence.transfer,
        SelectionTransferMode::Incr {
            announced_minimum_bytes: (first.len() + second.len()) as u64,
            chunks: 2,
        }
    );
}

#[test]
fn premature_wrong_property_owner_change_timeout_and_overflow_fail_closed() {
    let now = Instant::now();
    let mut premature = incoming(now);
    assert!(matches!(
        premature.receive_incr_chunk(RawClipboardTarget::Utf8String, 8, b"x", 50, now),
        IncomingAction::Failed(_)
    ));

    let mut wrong = incoming(now);
    wrong.begin_incr(RawClipboardTarget::Utf8String, 1);
    assert!(matches!(
        wrong.receive_incr_chunk(RawClipboardTarget::String, 8, b"x", 50, now),
        IncomingAction::Failed(_)
    ));

    let mut owner = incoming(now);
    owner.begin_incr(RawClipboardTarget::Utf8String, 1);
    let IncomingAction::Failed(evidence) =
        owner.receive_incr_chunk(RawClipboardTarget::Utf8String, 8, b"x", 51, now)
    else {
        unreachable!()
    };
    assert!(evidence.owner_changed);

    let mut timeout = incoming(now);
    assert!(matches!(
        timeout.expire(now + CLIPBOARD_TRANSFER_TIMEOUT),
        Some(IncomingAction::Failed(_))
    ));

    let mut overflow = incoming(now);
    assert!(matches!(
        overflow.begin_incr(RawClipboardTarget::Utf8String, MAX_SELECTION_BYTES + 1),
        IncomingAction::Failed(_)
    ));
}

#[test]
fn invalid_utf8_is_rejected_unless_binary_fallback_is_explicit() {
    let now = Instant::now();
    let mut strict = incoming(now);
    assert!(matches!(
        strict.finish_direct(
            RawClipboardTarget::Utf8String,
            RawClipboardTarget::Utf8String,
            8,
            &[0xff]
        ),
        IncomingAction::Failed(_)
    ));

    let mut binary = IncomingTransfer::new(SelectionName::Clipboard, 50, 200, 3, true, now);
    binary.select_target(RawClipboardTarget::Utf8String);
    let IncomingAction::Completed(result) = binary.finish_direct(
        RawClipboardTarget::Utf8String,
        RawClipboardTarget::Utf8String,
        8,
        &[0xff],
    ) else {
        unreachable!()
    };
    assert_eq!(
        result.payload.kind(),
        ClipboardPayloadKind::Binary(RawClipboardTarget::ApplicationOctetStream)
    );
}

#[test]
fn incoming_direct_ceiling_is_an_isolated_bounded_policy() {
    let now = Instant::now();
    let above_outbound_threshold = vec![b'x'; CLIPBOARD_DIRECT_LIMIT_BYTES + 1];
    let mut interoperable = incoming(now);
    assert!(matches!(
        interoperable.finish_direct(
            RawClipboardTarget::Utf8String,
            RawClipboardTarget::Utf8String,
            8,
            &above_outbound_threshold,
        ),
        IncomingAction::Completed(_)
    ));

    let at_limit = vec![b'x'; MAX_INCOMING_DIRECT_BYTES];
    let mut accepted = incoming(now);
    assert!(matches!(
        accepted.finish_direct(
            RawClipboardTarget::Utf8String,
            RawClipboardTarget::Utf8String,
            8,
            &at_limit,
        ),
        IncomingAction::Completed(_)
    ));

    let over_limit = vec![b'x'; MAX_INCOMING_DIRECT_BYTES + 1];
    let mut rejected = incoming(now);
    let IncomingAction::Failed(evidence) = rejected.finish_direct(
        RawClipboardTarget::Utf8String,
        RawClipboardTarget::Utf8String,
        8,
        &over_limit,
    ) else {
        unreachable!()
    };
    assert_eq!(
        evidence.terminal,
        SelectionTransferTerminal::Failed {
            reason: SelectionTransferFailureReason::SelectionTooLarge,
        }
    );
}

#[test]
fn incr_empty_first_chunk_and_chunk_count_overflow_fail_closed() {
    let now = Instant::now();
    let mut empty = incoming(now);
    assert_eq!(
        empty.begin_incr(RawClipboardTarget::Utf8String, 0),
        IncomingAction::DeleteForNextChunk
    );
    assert!(matches!(
        empty.receive_incr_chunk(RawClipboardTarget::Utf8String, 8, b"", 50, now),
        IncomingAction::Failed(_)
    ));

    let mut chunks = incoming(now);
    chunks.begin_incr(RawClipboardTarget::Utf8String, 1);
    chunks.chunks = MAX_INCR_CHUNKS;
    let IncomingAction::Failed(evidence) =
        chunks.receive_incr_chunk(RawClipboardTarget::Utf8String, 8, b"x", 50, now)
    else {
        unreachable!()
    };
    assert_eq!(
        evidence.terminal,
        SelectionTransferTerminal::Failed {
            reason: SelectionTransferFailureReason::SelectionTooLarge,
        }
    );
}
