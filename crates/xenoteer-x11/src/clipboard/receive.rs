//! Pure external-owner TARGETS/direct/INCR receive state machine.

use core::fmt;
use std::time::Instant;

use x11rb::protocol::xproto::{Atom, Window};
use xenoteer_protocol::{
    MAX_INCR_CHUNKS, MAX_SELECTION_BYTES, SelectionName, SelectionTransferFailureReason,
    SelectionTransferMode, SelectionTransferTerminal,
};

use super::{
    CLIPBOARD_TRANSFER_TIMEOUT, ClipboardPayload, RawClipboardReadResult, RawClipboardTarget,
    RawSelectionTransferEvidence, sha256_digest,
};

pub(super) const DEFAULT_TEXT_PREFERENCE: [RawClipboardTarget; 4] = [
    RawClipboardTarget::Utf8String,
    RawClipboardTarget::TextPlainUtf8,
    RawClipboardTarget::TextPlain,
    RawClipboardTarget::String,
];

// Deliberately isolated interoperability policy: ICCCM peers may send direct
// properties larger than our outbound INCR threshold. Bound those replies by
// the public selection ceiling; the outbound threshold is our owner's policy,
// not a constraint that can be imposed on another selection owner.
pub(super) const MAX_INCOMING_DIRECT_BYTES: usize = MAX_SELECTION_BYTES as usize;

pub(super) fn choose_target(
    advertised: &[RawClipboardTarget],
    preferred: &[RawClipboardTarget],
) -> Option<RawClipboardTarget> {
    let order = if preferred.is_empty() {
        DEFAULT_TEXT_PREFERENCE.as_slice()
    } else {
        preferred
    };
    order
        .iter()
        .copied()
        .find(|target| advertised.contains(target))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IncomingPhase {
    AwaitingTargets,
    AwaitingData {
        target: RawClipboardTarget,
    },
    ReceivingIncr {
        target: RawClipboardTarget,
        announced_minimum_bytes: u64,
    },
    Terminal,
}

pub(super) struct IncomingTransfer {
    pub selection: SelectionName,
    pub owner: Window,
    pub property: Atom,
    pub revision: u64,
    pub allow_binary_fallback: bool,
    pub phase: IncomingPhase,
    bytes: Vec<u8>,
    chunks: u32,
    deadline: Instant,
}

impl fmt::Debug for IncomingTransfer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IncomingTransfer")
            .field("selection", &self.selection)
            .field("owner", &self.owner)
            .field("property", &self.property)
            .field("revision", &self.revision)
            .field("phase", &self.phase)
            .field("bytes", &self.bytes.len())
            .field("content", &"[REDACTED]")
            .field("chunks", &self.chunks)
            .finish_non_exhaustive()
    }
}

impl IncomingTransfer {
    pub fn new(
        selection: SelectionName,
        owner: Window,
        property: Atom,
        revision: u64,
        allow_binary_fallback: bool,
        now: Instant,
    ) -> Self {
        Self {
            selection,
            owner,
            property,
            revision,
            allow_binary_fallback,
            phase: IncomingPhase::AwaitingTargets,
            bytes: Vec::new(),
            chunks: 0,
            deadline: now + CLIPBOARD_TRANSFER_TIMEOUT,
        }
    }

    pub fn select_target(&mut self, target: RawClipboardTarget) -> bool {
        if self.phase != IncomingPhase::AwaitingTargets || !target.is_content() {
            return false;
        }
        self.phase = IncomingPhase::AwaitingData { target };
        true
    }

    pub fn begin_incr(
        &mut self,
        target: RawClipboardTarget,
        announced_minimum_bytes: u64,
    ) -> IncomingAction {
        if self.phase != (IncomingPhase::AwaitingData { target }) {
            return self.protocol_failure();
        }
        if announced_minimum_bytes > MAX_SELECTION_BYTES {
            return self.failure(SelectionTransferFailureReason::SelectionTooLarge, false);
        }
        self.phase = IncomingPhase::ReceivingIncr {
            target,
            announced_minimum_bytes,
        };
        IncomingAction::DeleteForNextChunk
    }

    pub fn finish_direct(
        &mut self,
        target: RawClipboardTarget,
        actual_type: RawClipboardTarget,
        format: u8,
        bytes: &[u8],
    ) -> IncomingAction {
        if self.phase != (IncomingPhase::AwaitingData { target })
            || actual_type != target
            || format != 8
        {
            return self.protocol_failure();
        }
        if bytes.len() > MAX_INCOMING_DIRECT_BYTES {
            return self.failure(SelectionTransferFailureReason::SelectionTooLarge, false);
        }
        match payload_from_wire(target, bytes.to_vec(), self.allow_binary_fallback) {
            Some(payload) => {
                self.phase = IncomingPhase::Terminal;
                IncomingAction::Completed(RawClipboardReadResult {
                    selection: self.selection,
                    revision: self.revision,
                    evidence: RawSelectionTransferEvidence {
                        target,
                        transfer: SelectionTransferMode::Direct,
                        content_length: bytes.len() as u64,
                        sha256: sha256_digest(bytes),
                        owner_changed: false,
                        terminal_chunk_observed: false,
                        terminal: SelectionTransferTerminal::Completed,
                    },
                    payload,
                })
            }
            None => self.protocol_failure(),
        }
    }

    pub fn receive_incr_chunk(
        &mut self,
        actual_type: RawClipboardTarget,
        format: u8,
        bytes: &[u8],
        owner_now: Window,
        now: Instant,
    ) -> IncomingAction {
        let IncomingPhase::ReceivingIncr {
            target,
            announced_minimum_bytes,
        } = self.phase
        else {
            return self.protocol_failure();
        };
        if owner_now != self.owner {
            return self.failure(SelectionTransferFailureReason::OwnerChanged, true);
        }
        if now >= self.deadline {
            return self.failure(SelectionTransferFailureReason::Timeout, false);
        }
        if actual_type != target || format != 8 {
            return self.protocol_failure();
        }
        if bytes.is_empty() {
            if self.chunks == 0 {
                return self.protocol_failure();
            }
            let Some(payload) = payload_from_wire(
                target,
                std::mem::take(&mut self.bytes),
                self.allow_binary_fallback,
            ) else {
                return self.protocol_failure();
            };
            self.phase = IncomingPhase::Terminal;
            return IncomingAction::Completed(RawClipboardReadResult {
                selection: self.selection,
                revision: self.revision,
                evidence: RawSelectionTransferEvidence {
                    target,
                    transfer: SelectionTransferMode::Incr {
                        announced_minimum_bytes,
                        chunks: self.chunks,
                    },
                    content_length: payload.byte_len() as u64,
                    sha256: payload.digest(),
                    owner_changed: false,
                    terminal_chunk_observed: true,
                    terminal: SelectionTransferTerminal::Completed,
                },
                payload,
            });
        }
        let new_len = self.bytes.len().checked_add(bytes.len());
        if new_len.is_none_or(|len| len as u64 > MAX_SELECTION_BYTES)
            || self.chunks == MAX_INCR_CHUNKS
        {
            return self.failure(SelectionTransferFailureReason::SelectionTooLarge, false);
        }
        self.bytes.extend_from_slice(bytes);
        self.chunks += 1;
        IncomingAction::DeleteForNextChunk
    }

    pub fn expire(&mut self, now: Instant) -> Option<IncomingAction> {
        (self.phase != IncomingPhase::Terminal && now >= self.deadline)
            .then(|| self.failure(SelectionTransferFailureReason::Timeout, false))
    }

    pub fn owner_changed(&mut self) -> IncomingAction {
        self.failure(SelectionTransferFailureReason::OwnerChanged, true)
    }

    pub fn protocol_violation(&mut self) -> IncomingAction {
        self.protocol_failure()
    }

    fn protocol_failure(&mut self) -> IncomingAction {
        self.failure(SelectionTransferFailureReason::ProtocolViolation, false)
    }

    fn failure(
        &mut self,
        reason: SelectionTransferFailureReason,
        owner_changed: bool,
    ) -> IncomingAction {
        let (target, transfer) = match self.phase {
            IncomingPhase::AwaitingData { target } => (target, SelectionTransferMode::Direct),
            IncomingPhase::ReceivingIncr {
                target,
                announced_minimum_bytes,
            } => (
                target,
                SelectionTransferMode::Incr {
                    announced_minimum_bytes,
                    chunks: self.chunks,
                },
            ),
            IncomingPhase::AwaitingTargets | IncomingPhase::Terminal => (
                RawClipboardTarget::ApplicationOctetStream,
                SelectionTransferMode::Direct,
            ),
        };
        self.phase = IncomingPhase::Terminal;
        IncomingAction::Failed(RawSelectionTransferEvidence {
            target,
            transfer,
            content_length: self.bytes.len() as u64,
            sha256: sha256_digest(&self.bytes),
            owner_changed,
            terminal_chunk_observed: false,
            terminal: SelectionTransferTerminal::Failed { reason },
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum IncomingAction {
    DeleteForNextChunk,
    Completed(RawClipboardReadResult),
    Failed(RawSelectionTransferEvidence),
}

fn payload_from_wire(
    target: RawClipboardTarget,
    bytes: Vec<u8>,
    allow_binary_fallback: bool,
) -> Option<ClipboardPayload> {
    match target {
        RawClipboardTarget::Utf8String
        | RawClipboardTarget::TextPlainUtf8
        | RawClipboardTarget::TextPlain => match String::from_utf8(bytes) {
            Ok(text) => ClipboardPayload::utf8_text(text).ok(),
            Err(error) if allow_binary_fallback => ClipboardPayload::binary(
                RawClipboardTarget::ApplicationOctetStream,
                error.into_bytes(),
            )
            .ok(),
            Err(_) => None,
        },
        RawClipboardTarget::String => {
            let text: String = bytes.iter().map(|byte| char::from(*byte)).collect();
            ClipboardPayload::utf8_text(text).ok()
        }
        RawClipboardTarget::ImagePng | RawClipboardTarget::ApplicationOctetStream => {
            ClipboardPayload::binary(target, bytes).ok()
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "receive_tests.rs"]
mod tests;
