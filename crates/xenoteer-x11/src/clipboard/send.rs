//! Pure outgoing direct/INCR and MULTIPLE state machines.

use core::fmt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use x11rb::protocol::xproto::{Atom, Window};
use xenoteer_protocol::{
    MAX_INCR_CHUNKS, SelectionTransferFailureReason, SelectionTransferMode,
    SelectionTransferTerminal,
};

use super::{
    CLIPBOARD_DIRECT_LIMIT_BYTES, CLIPBOARD_INCR_CHUNK_BYTES, CLIPBOARD_TRANSFER_TIMEOUT,
    ClipboardContentDigest, MAX_INCR_TRANSFERS_GLOBAL, MAX_INCR_TRANSFERS_PER_REQUESTOR,
    RawClipboardTarget, RawSelectionTransferEvidence,
};

pub(super) const MAX_MULTIPLE_ITEMS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TransferWireMode {
    Direct,
    Incr,
}

pub(super) const fn transfer_wire_mode(bytes: usize) -> TransferWireMode {
    if bytes <= CLIPBOARD_DIRECT_LIMIT_BYTES {
        TransferWireMode::Direct
    } else {
        TransferWireMode::Incr
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct OutgoingKey {
    pub requestor: Window,
    pub property: Atom,
}

pub(super) struct OutgoingIncr {
    key: OutgoingKey,
    target: RawClipboardTarget,
    bytes: Arc<[u8]>,
    digest: ClipboardContentDigest,
    offset: usize,
    chunks: u32,
    last_delete_sequence: Option<u16>,
    deadline: Instant,
}

impl fmt::Debug for OutgoingIncr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutgoingIncr")
            .field("key", &self.key)
            .field("target", &self.target)
            .field("bytes", &self.bytes.len())
            .field("content", &"[REDACTED]")
            .field("offset", &self.offset)
            .field("chunks", &self.chunks)
            .finish_non_exhaustive()
    }
}

impl OutgoingIncr {
    pub fn key(&self) -> OutgoingKey {
        self.key
    }

    pub fn new(
        key: OutgoingKey,
        target: RawClipboardTarget,
        bytes: Arc<[u8]>,
        digest: ClipboardContentDigest,
        now: Instant,
    ) -> Option<Self> {
        (transfer_wire_mode(bytes.len()) == TransferWireMode::Incr).then_some(Self {
            key,
            target,
            bytes,
            digest,
            offset: 0,
            chunks: 0,
            last_delete_sequence: None,
            deadline: now + CLIPBOARD_TRANSFER_TIMEOUT,
        })
    }

    pub fn on_property_deleted(&mut self, sequence: u16, now: Instant) -> OutgoingAction {
        if now >= self.deadline {
            return OutgoingAction::Failed(self.failed(SelectionTransferFailureReason::Timeout));
        }
        if self.last_delete_sequence == Some(sequence) {
            return OutgoingAction::DuplicateIgnored;
        }
        self.last_delete_sequence = Some(sequence);
        if self.offset < self.bytes.len() {
            let end = self
                .offset
                .saturating_add(CLIPBOARD_INCR_CHUNK_BYTES)
                .min(self.bytes.len());
            let chunk: Arc<[u8]> = self.bytes[self.offset..end].to_vec().into();
            self.offset = end;
            self.chunks = self.chunks.saturating_add(1).min(MAX_INCR_CHUNKS);
            OutgoingAction::WriteChunk {
                target: self.target,
                bytes: chunk,
            }
        } else {
            OutgoingAction::WriteTerminator(self.completed())
        }
    }

    pub fn timed_out(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    pub fn fail(self, reason: SelectionTransferFailureReason) -> RawSelectionTransferEvidence {
        self.failed(reason)
    }

    fn completed(&self) -> RawSelectionTransferEvidence {
        RawSelectionTransferEvidence {
            target: self.target,
            transfer: SelectionTransferMode::Incr {
                announced_minimum_bytes: self.bytes.len() as u64,
                chunks: self.chunks,
            },
            content_length: self.bytes.len() as u64,
            sha256: self.digest,
            owner_changed: false,
            terminal_chunk_observed: true,
            terminal: SelectionTransferTerminal::Completed,
        }
    }

    fn failed(&self, reason: SelectionTransferFailureReason) -> RawSelectionTransferEvidence {
        RawSelectionTransferEvidence {
            target: self.target,
            transfer: SelectionTransferMode::Incr {
                announced_minimum_bytes: self.bytes.len() as u64,
                chunks: self.chunks,
            },
            content_length: self.offset as u64,
            sha256: self.digest,
            owner_changed: reason == SelectionTransferFailureReason::OwnerChanged,
            terminal_chunk_observed: false,
            terminal: SelectionTransferTerminal::Failed { reason },
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(super) enum OutgoingAction {
    WriteChunk {
        target: RawClipboardTarget,
        bytes: Arc<[u8]>,
    },
    WriteTerminator(RawSelectionTransferEvidence),
    DuplicateIgnored,
    Failed(RawSelectionTransferEvidence),
}

impl fmt::Debug for OutgoingAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WriteChunk { target, bytes } => formatter
                .debug_struct("WriteChunk")
                .field("target", target)
                .field("bytes", &bytes.len())
                .field("content", &"[REDACTED]")
                .finish(),
            Self::WriteTerminator(evidence) => formatter
                .debug_tuple("WriteTerminator")
                .field(evidence)
                .finish(),
            Self::DuplicateIgnored => formatter.write_str("DuplicateIgnored"),
            Self::Failed(evidence) => formatter.debug_tuple("Failed").field(evidence).finish(),
        }
    }
}

#[derive(Default)]
pub(super) struct OutgoingTransfers {
    values: HashMap<OutgoingKey, OutgoingIncr>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OutgoingAdmissionError {
    DuplicateProperty,
    PerRequestorLimit,
    GlobalLimit,
}

impl OutgoingTransfers {
    pub fn insert(&mut self, transfer: OutgoingIncr) -> Result<(), OutgoingAdmissionError> {
        if self.values.contains_key(&transfer.key) {
            return Err(OutgoingAdmissionError::DuplicateProperty);
        }
        if self.values.len() >= MAX_INCR_TRANSFERS_GLOBAL {
            return Err(OutgoingAdmissionError::GlobalLimit);
        }
        if self
            .values
            .keys()
            .filter(|key| key.requestor == transfer.key.requestor)
            .count()
            >= MAX_INCR_TRANSFERS_PER_REQUESTOR
        {
            return Err(OutgoingAdmissionError::PerRequestorLimit);
        }
        self.values.insert(transfer.key, transfer);
        Ok(())
    }

    pub fn get_mut(&mut self, key: OutgoingKey) -> Option<&mut OutgoingIncr> {
        self.values.get_mut(&key)
    }

    pub fn remove(&mut self, key: OutgoingKey) -> Option<OutgoingIncr> {
        self.values.remove(&key)
    }

    pub fn remove_requestor(&mut self, requestor: Window) -> Vec<OutgoingIncr> {
        let keys: Vec<_> = self
            .values
            .keys()
            .filter(|key| key.requestor == requestor)
            .copied()
            .collect();
        keys.into_iter()
            .filter_map(|key| self.values.remove(&key))
            .collect()
    }

    pub fn expired_keys(&self, now: Instant) -> Vec<OutgoingKey> {
        let mut keys: Vec<_> = self
            .values
            .iter()
            .filter_map(|(key, transfer)| transfer.timed_out(now).then_some(*key))
            .collect();
        keys.sort_unstable_by_key(|key| (key.requestor, key.property));
        keys
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MultiplePair {
    pub target: Atom,
    pub property: Atom,
}

pub(super) fn decode_multiple_pairs(
    actual_type: Atom,
    format: u8,
    values: &[u32],
    atom_pair: Atom,
) -> Option<Vec<MultiplePair>> {
    if actual_type != atom_pair
        || format != 32
        || !values.len().is_multiple_of(2)
        || values.len() / 2 > MAX_MULTIPLE_ITEMS
    {
        return None;
    }
    Some(
        values
            .chunks_exact(2)
            .map(|pair| MultiplePair {
                target: pair[0],
                property: pair[1],
            })
            .collect(),
    )
}

pub(super) fn encode_multiple_pairs(pairs: &[MultiplePair]) -> Vec<u32> {
    pairs
        .iter()
        .flat_map(|pair| [pair.target, pair.property])
        .collect()
}

#[cfg(test)]
#[path = "send_tests.rs"]
mod tests;
