//! Production X11 selection transport.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use x11rb::COPY_DEPTH_FROM_PARENT;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux,
    DestroyNotifyEvent, EventMask, PropMode, Property, PropertyNotifyEvent, SelectionClearEvent,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};
use xenoteer_protocol::{
    MAX_CLIPBOARD_TARGETS, SelectionName, SelectionTransferFailureReason, SelectionTransferMode,
    SelectionTransferTerminal,
};

use super::atoms::{ClipboardAtoms, PRIVATE_PROPERTY_COUNT};
use super::receive::{
    IncomingAction, IncomingPhase, IncomingTransfer, MAX_INCOMING_DIRECT_BYTES, choose_target,
};
use super::send::{
    MultiplePair, OutgoingAction, OutgoingIncr, OutgoingKey, OutgoingTransfers, TransferWireMode,
    decode_multiple_pairs, encode_multiple_pairs, transfer_wire_mode,
};
use super::{
    BackendFault, ClipboardActorEvent, ClipboardActorFailure, ClipboardActorFailureKind,
    ClipboardBackend, ClipboardCommand, ClipboardEventSender, ClipboardOwnershipEvidence,
    ClipboardPasteObservationRequest, ClipboardPayload, ClipboardReadRawRequest,
    ClipboardSetRequest, RawClipboardPasteObservation, RawClipboardReadResult, RawClipboardTarget,
    RawSelectionTransferEvidence, sha256_digest,
};
use crate::{Result, X11Error};

const MAX_PENDING_READS: usize = PRIVATE_PROPERTY_COUNT;
const MAX_PASTE_WATCHERS: usize = 64;
// Browser toolkits can probe one compatible target and issue the conversion
// used by the renderer on a later event-loop turn. Retain temporary ownership
// until a genuinely quiet interval has elapsed after the latest request or
// transfer; restoring after the first transfer can paste the preserved value.
const PASTE_QUIET_PERIOD: Duration = Duration::from_millis(250);
const MAX_DEFERRED_TIME_EVENTS: usize = 256;
const TARGET_PROPERTY_LONGS: u32 = (MAX_CLIPBOARD_TARGETS as u32).saturating_add(1);
const INCOMING_PROPERTY_LONGS: u32 =
    ((MAX_INCOMING_DIRECT_BYTES as u32).saturating_add(3) / 4).saturating_add(1);

#[derive(Clone)]
struct OwnedSelection {
    payload: ClipboardPayload,
    acquired_time: u32,
}

struct PendingRead {
    transfer: IncomingTransfer,
    preferred_targets: Vec<RawClipboardTarget>,
    reply: SyncSender<std::result::Result<RawClipboardReadResult, ClipboardActorFailure>>,
}

struct PasteWatcher {
    selection: SelectionName,
    requested_targets: Vec<RawClipboardTarget>,
    transfer: Option<RawSelectionTransferEvidence>,
    deadline: Instant,
    quiet_deadline: Option<Instant>,
    reply: SyncSender<std::result::Result<RawClipboardPasteObservation, ClipboardActorFailure>>,
}

impl PasteWatcher {
    fn observe_request(&mut self, target: RawClipboardTarget, now: Instant) {
        if !self.requested_targets.contains(&target) {
            self.requested_targets.push(target);
        }
        if self.transfer.is_some() {
            self.quiet_deadline = Some(now + PASTE_QUIET_PERIOD);
        }
    }

    fn observe_transfer(
        &mut self,
        target: RawClipboardTarget,
        evidence: RawSelectionTransferEvidence,
        now: Instant,
    ) {
        if !self.requested_targets.contains(&target) {
            self.requested_targets.push(target);
        }
        if self.transfer.is_none() {
            self.transfer = Some(evidence);
        }
        self.quiet_deadline = Some(now + PASTE_QUIET_PERIOD);
    }

    fn ready(&self, now: Instant) -> bool {
        self.quiet_deadline.is_some_and(|deadline| now >= deadline) || now >= self.deadline
    }
}

pub(super) struct X11ClipboardBackend {
    connection: RustConnection,
    root: Window,
    owner_window: Window,
    atoms: ClipboardAtoms,
    deferred_events: VecDeque<Event>,
    clipboard: Option<OwnedSelection>,
    primary: Option<OwnedSelection>,
    revisions: [u64; 2],
    known_owners: [Window; 2],
    outgoing: OutgoingTransfers,
    outgoing_selections: HashMap<OutgoingKey, SelectionName>,
    incoming: HashMap<Atom, PendingRead>,
    paste_watchers: Vec<PasteWatcher>,
    last_server_time: u32,
    xfixes_selection_events: bool,
}

impl X11ClipboardBackend {
    pub fn open(display: &str) -> Result<Self> {
        let (connection, screen_index) = RustConnection::connect(Some(display))
            .map_err(|error| X11Error::Connect(error.to_string()))?;
        let root = connection
            .setup()
            .roots
            .get(screen_index)
            .ok_or(X11Error::InvalidSetup("clipboard screen is absent"))?
            .root;
        let atoms = ClipboardAtoms::intern(&connection)?;
        let owner_window = connection
            .generate_id()
            .map_err(|error| X11Error::Connection(error.to_string()))?;
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                owner_window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new()
                    .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
            )
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .check()
            .map_err(|error| X11Error::Reply(error.to_string()))?;

        let xfixes_selection_events = connection
            .extension_information(xfixes::X11_EXTENSION_NAME)
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .is_some();
        if xfixes_selection_events {
            connection
                .xfixes_query_version(5, 0)
                .map_err(|error| X11Error::Connection(error.to_string()))?
                .reply()
                .map_err(|error| X11Error::Reply(error.to_string()))?;
            let mask = SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE;
            for selection in [atoms.clipboard, atoms.primary] {
                connection
                    .xfixes_select_selection_input(owner_window, selection, mask)
                    .map_err(|error| X11Error::Connection(error.to_string()))?
                    .check()
                    .map_err(|error| X11Error::Reply(error.to_string()))?;
            }
        }
        let clipboard_owner = connection
            .get_selection_owner(atoms.clipboard)
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .reply()
            .map_err(|error| X11Error::Reply(error.to_string()))?
            .owner;
        let primary_owner = connection
            .get_selection_owner(atoms.primary)
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .reply()
            .map_err(|error| X11Error::Reply(error.to_string()))?
            .owner;

        let mut backend = Self {
            connection,
            root,
            owner_window,
            atoms,
            deferred_events: VecDeque::new(),
            clipboard: None,
            primary: None,
            revisions: [0, 0],
            known_owners: [clipboard_owner, primary_owner],
            outgoing: OutgoingTransfers::default(),
            outgoing_selections: HashMap::new(),
            incoming: HashMap::new(),
            paste_watchers: Vec::new(),
            last_server_time: 0,
            xfixes_selection_events,
        };
        backend.last_server_time = backend
            .query_server_time()
            .map_err(|()| X11Error::InvalidSetup("cannot obtain X server timestamp"))?;
        Ok(backend)
    }

    fn selection_index(selection: SelectionName) -> usize {
        match selection {
            SelectionName::Clipboard => 0,
            SelectionName::Primary => 1,
        }
    }

    fn selection_from_atom(&self, atom: Atom) -> Option<SelectionName> {
        if atom == self.atoms.clipboard {
            Some(SelectionName::Clipboard)
        } else if atom == self.atoms.primary {
            Some(SelectionName::Primary)
        } else {
            None
        }
    }

    fn owned(&self, selection: SelectionName) -> Option<&OwnedSelection> {
        match selection {
            SelectionName::Clipboard => self.clipboard.as_ref(),
            SelectionName::Primary => self.primary.as_ref(),
        }
    }

    fn set_owned(&mut self, selection: SelectionName, value: Option<OwnedSelection>) {
        match selection {
            SelectionName::Clipboard => self.clipboard = value,
            SelectionName::Primary => self.primary = value,
        }
    }

    fn query_server_time(&mut self) -> std::result::Result<u32, ()> {
        self.connection
            .change_property8(
                PropMode::REPLACE,
                self.owner_window,
                self.atoms.time_probe,
                AtomEnum::STRING,
                &[0],
            )
            .map_err(|_| ())?
            .check()
            .map_err(|_| ())?;
        self.connection
            .get_input_focus()
            .map_err(|_| ())?
            .reply()
            .map_err(|_| ())?;
        for _ in 0..MAX_DEFERRED_TIME_EVENTS {
            let Some(event) = self.connection.poll_for_event().map_err(|_| ())? else {
                return Err(());
            };
            if let Event::PropertyNotify(property) = event {
                if property.window == self.owner_window
                    && property.atom == self.atoms.time_probe
                    && property.state == Property::NEW_VALUE
                {
                    return Ok(property.time);
                }
                self.deferred_events
                    .push_back(Event::PropertyNotify(property));
            } else {
                self.deferred_events.push_back(event);
            }
        }
        Err(())
    }

    fn current_owner(&self, selection: SelectionName) -> std::result::Result<Window, ()> {
        self.connection
            .get_selection_owner(self.atoms.selection(selection))
            .map_err(|_| ())?
            .reply()
            .map(|reply| reply.owner)
            .map_err(|_| ())
    }

    fn handle_set(
        &mut self,
        request: ClipboardSetRequest,
        reply: SyncSender<std::result::Result<ClipboardOwnershipEvidence, ClipboardActorFailure>>,
        events: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        let time = self
            .query_server_time()
            .map_err(|()| BackendFault::Unavailable)?;
        let selection_atom = self.atoms.selection(request.selection);
        self.connection
            .set_selection_owner(self.owner_window, selection_atom, time)
            .map_err(|_| BackendFault::Unavailable)?
            .check()
            .map_err(|_| BackendFault::Unavailable)?;
        let owner = self
            .current_owner(request.selection)
            .map_err(|()| BackendFault::Unavailable)?;
        if owner != self.owner_window {
            let _ignored = reply.send(Err(failure(ClipboardActorFailureKind::OwnershipRace)));
            return Ok(());
        }
        let index = Self::selection_index(request.selection);
        self.revisions[index] = self.revisions[index].wrapping_add(1);
        self.known_owners[index] = owner;
        self.last_server_time = time;
        self.set_owned(
            request.selection,
            Some(OwnedSelection {
                payload: request.payload,
                acquired_time: time,
            }),
        );
        let evidence = ClipboardOwnershipEvidence {
            selection: request.selection,
            revision: self.revisions[index],
            owner,
            server_time: time,
            verified: true,
        };
        let _ignored = reply.send(Ok(evidence));
        emit_event(
            events,
            ClipboardActorEvent::OwnershipChanged {
                selection: request.selection,
                revision: self.revisions[index],
                owned: true,
            },
        );
        Ok(())
    }

    fn handle_clear(
        &mut self,
        selection: SelectionName,
        reply: SyncSender<std::result::Result<ClipboardOwnershipEvidence, ClipboardActorFailure>>,
        events: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        let current = self
            .current_owner(selection)
            .map_err(|()| BackendFault::Unavailable)?;
        if current != self.owner_window {
            self.set_owned(selection, None);
            let _ignored = reply.send(Err(failure(ClipboardActorFailureKind::OwnershipRace)));
            return Ok(());
        }
        let time = self
            .query_server_time()
            .map_err(|()| BackendFault::Unavailable)?;
        self.connection
            .set_selection_owner(NONE, self.atoms.selection(selection), time)
            .map_err(|_| BackendFault::Unavailable)?
            .check()
            .map_err(|_| BackendFault::Unavailable)?;
        let owner = self
            .current_owner(selection)
            .map_err(|()| BackendFault::Unavailable)?;
        if owner != NONE {
            let _ignored = reply.send(Err(failure(ClipboardActorFailureKind::OwnershipRace)));
            return Ok(());
        }
        let index = Self::selection_index(selection);
        self.revisions[index] = self.revisions[index].wrapping_add(1);
        self.known_owners[index] = NONE;
        self.last_server_time = time;
        self.set_owned(selection, None);
        let _ignored = reply.send(Ok(ClipboardOwnershipEvidence {
            selection,
            revision: self.revisions[index],
            owner,
            server_time: time,
            verified: true,
        }));
        emit_event(
            events,
            ClipboardActorEvent::OwnershipChanged {
                selection,
                revision: self.revisions[index],
                owned: false,
            },
        );
        Ok(())
    }

    fn handle_read(
        &mut self,
        request: ClipboardReadRawRequest,
        reply: SyncSender<std::result::Result<RawClipboardReadResult, ClipboardActorFailure>>,
    ) -> std::result::Result<(), BackendFault> {
        let owner = self
            .current_owner(request.selection)
            .map_err(|()| BackendFault::Unavailable)?;
        if owner == NONE {
            let _ignored = reply.send(Err(failure(ClipboardActorFailureKind::SelectionHasNoOwner)));
            return Ok(());
        }
        if owner == self.owner_window {
            return self.read_owned(request, reply);
        }
        if self.incoming.len() >= MAX_PENDING_READS {
            let _ignored = reply.send(Err(failure(ClipboardActorFailureKind::BackendUnavailable)));
            return Ok(());
        }
        let Some(property) = self
            .atoms
            .private_properties
            .iter()
            .copied()
            .find(|property| !self.incoming.contains_key(property))
        else {
            let _ignored = reply.send(Err(failure(ClipboardActorFailureKind::BackendUnavailable)));
            return Ok(());
        };
        let time = self
            .query_server_time()
            .map_err(|()| BackendFault::Unavailable)?;
        self.connection
            .delete_property(self.owner_window, property)
            .map_err(|_| BackendFault::Unavailable)?
            .check()
            .map_err(|_| BackendFault::Unavailable)?;
        self.connection
            .convert_selection(
                self.owner_window,
                self.atoms.selection(request.selection),
                self.atoms.targets,
                property,
                time,
            )
            .map_err(|_| BackendFault::Unavailable)?
            .check()
            .map_err(|_| BackendFault::Unavailable)?;
        let index = Self::selection_index(request.selection);
        self.incoming.insert(
            property,
            PendingRead {
                transfer: IncomingTransfer::new(
                    request.selection,
                    owner,
                    property,
                    self.revisions[index],
                    request.allow_binary_fallback,
                    Instant::now(),
                ),
                preferred_targets: request.preferred_targets,
                reply,
            },
        );
        Ok(())
    }

    fn read_owned(
        &self,
        request: ClipboardReadRawRequest,
        reply: SyncSender<std::result::Result<RawClipboardReadResult, ClipboardActorFailure>>,
    ) -> std::result::Result<(), BackendFault> {
        let Some(owned) = self.owned(request.selection) else {
            let _ignored = reply.send(Err(failure(ClipboardActorFailureKind::OwnershipRace)));
            return Ok(());
        };
        let advertised = owned.payload.advertised_targets();
        let Some(target) = choose_target(&advertised, &request.preferred_targets) else {
            let _ignored = reply.send(Err(failure(ClipboardActorFailureKind::TargetUnsupported)));
            return Ok(());
        };
        let Some(bytes) = owned.payload.representation(target) else {
            let _ignored = reply.send(Err(failure(ClipboardActorFailureKind::TargetUnsupported)));
            return Ok(());
        };
        let index = Self::selection_index(request.selection);
        let payload = owned.payload.clone();
        let _ignored = reply.send(Ok(RawClipboardReadResult {
            selection: request.selection,
            revision: self.revisions[index],
            payload,
            evidence: RawSelectionTransferEvidence {
                target,
                transfer: SelectionTransferMode::Direct,
                content_length: bytes.len() as u64,
                sha256: sha256_digest(&bytes),
                owner_changed: false,
                terminal_chunk_observed: false,
                terminal: SelectionTransferTerminal::Completed,
            },
        }));
        Ok(())
    }

    fn handle_observe_paste(
        &mut self,
        request: ClipboardPasteObservationRequest,
        ready: SyncSender<std::result::Result<(), ClipboardActorFailure>>,
        reply: SyncSender<std::result::Result<RawClipboardPasteObservation, ClipboardActorFailure>>,
    ) {
        if self.paste_watchers.len() >= MAX_PASTE_WATCHERS {
            let failure = failure(ClipboardActorFailureKind::ControlQueueFull);
            let _ignored = ready.send(Err(failure));
            let _ignored = reply.send(Err(failure));
            return;
        }
        self.paste_watchers.push(PasteWatcher {
            selection: request.selection,
            requested_targets: Vec::new(),
            transfer: None,
            deadline: Instant::now() + request.timeout,
            quiet_deadline: None,
            reply,
        });
        let _ignored = ready.send(Ok(()));
    }

    fn handle_event(
        &mut self,
        event: Event,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        match event {
            Event::SelectionRequest(event) => self.handle_selection_request(event, sender),
            Event::SelectionNotify(event) => self.handle_selection_notify(event),
            Event::SelectionClear(event) => self.handle_selection_clear(event, sender),
            Event::PropertyNotify(event) => self.handle_property_notify(event, sender),
            Event::DestroyNotify(event) => self.handle_destroy_notify(event, sender),
            Event::XfixesSelectionNotify(event) if self.xfixes_selection_events => {
                self.handle_xfixes_selection_notify(event.selection, event.owner, sender)
            }
            _ => Ok(()),
        }
    }

    fn handle_selection_request(
        &mut self,
        event: SelectionRequestEvent,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        let Some(selection) = self.selection_from_atom(event.selection) else {
            return self.send_selection_notify(event, NONE);
        };
        if event.owner != self.owner_window || event.property == NONE {
            return self.send_selection_notify(event, NONE);
        }
        let Some(owned) = self.owned(selection).cloned() else {
            return self.send_selection_notify(event, NONE);
        };
        if event.time != CURRENT_TIME && time_before(event.time, owned.acquired_time) {
            return self.send_selection_notify(event, NONE);
        }
        if event.target == self.atoms.multiple {
            return self.serve_multiple(event, selection, &owned, sender);
        }
        let property = match self.serve_one(
            event.requestor,
            event.target,
            event.property,
            selection,
            &owned,
            sender,
        )? {
            true => event.property,
            false => NONE,
        };
        self.send_selection_notify(event, property)
    }

    fn serve_multiple(
        &mut self,
        event: SelectionRequestEvent,
        selection: SelectionName,
        owned: &OwnedSelection,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        let reply = self
            .connection
            .get_property(
                false,
                event.requestor,
                event.property,
                self.atoms.atom_pair,
                0,
                (MAX_CLIPBOARD_TARGETS as u32).saturating_mul(2),
            )
            .map_err(|_| BackendFault::Unavailable)?
            .reply()
            .map_err(|_| BackendFault::Unavailable)?;
        let values: Vec<u32> = reply.value32().map(Iterator::collect).unwrap_or_default();
        let Some(mut pairs) = (reply.bytes_after == 0)
            .then(|| {
                decode_multiple_pairs(reply.type_, reply.format, &values, self.atoms.atom_pair)
            })
            .flatten()
        else {
            return self.send_selection_notify(event, NONE);
        };
        for MultiplePair { target, property } in &mut pairs {
            if *property == NONE
                || *property == event.property
                || *target == self.atoms.multiple
                || !self.serve_one(
                    event.requestor,
                    *target,
                    *property,
                    selection,
                    owned,
                    sender,
                )?
            {
                *property = NONE;
            }
        }
        self.connection
            .change_property32(
                PropMode::REPLACE,
                event.requestor,
                event.property,
                self.atoms.atom_pair,
                &encode_multiple_pairs(&pairs),
            )
            .map_err(|_| BackendFault::Unavailable)?
            .check()
            .map_err(|_| BackendFault::Unavailable)?;
        self.send_selection_notify(event, event.property)
    }

    fn serve_one(
        &mut self,
        requestor: Window,
        target_atom: Atom,
        property: Atom,
        selection: SelectionName,
        owned: &OwnedSelection,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<bool, BackendFault> {
        let Some(target) = self.atoms.identify_target(target_atom) else {
            return Ok(false);
        };
        match target {
            RawClipboardTarget::Targets => {
                let targets: Vec<_> = owned
                    .payload
                    .advertised_targets()
                    .into_iter()
                    .map(|target| self.atoms.target(target))
                    .collect();
                self.connection
                    .change_property32(
                        PropMode::REPLACE,
                        requestor,
                        property,
                        self.atoms.atom,
                        &targets,
                    )
                    .map_err(|_| BackendFault::Unavailable)?
                    .check()
                    .map_err(|_| BackendFault::Unavailable)?;
                Ok(true)
            }
            RawClipboardTarget::Timestamp => {
                self.connection
                    .change_property32(
                        PropMode::REPLACE,
                        requestor,
                        property,
                        self.atoms.cardinal,
                        &[owned.acquired_time],
                    )
                    .map_err(|_| BackendFault::Unavailable)?
                    .check()
                    .map_err(|_| BackendFault::Unavailable)?;
                Ok(true)
            }
            RawClipboardTarget::Multiple => Ok(false),
            content => {
                let Some(bytes) = owned.payload.representation(content) else {
                    return Ok(false);
                };
                match transfer_wire_mode(bytes.len()) {
                    TransferWireMode::Direct => {
                        self.connection
                            .change_property8(
                                PropMode::REPLACE,
                                requestor,
                                property,
                                target_atom,
                                &bytes,
                            )
                            .map_err(|_| BackendFault::Unavailable)?
                            .check()
                            .map_err(|_| BackendFault::Unavailable)?;
                        let evidence = RawSelectionTransferEvidence {
                            target: content,
                            transfer: SelectionTransferMode::Direct,
                            content_length: bytes.len() as u64,
                            sha256: sha256_digest(&bytes),
                            owner_changed: false,
                            terminal_chunk_observed: false,
                            terminal: SelectionTransferTerminal::Completed,
                        };
                        self.observe_transfer(selection, content, evidence, sender);
                        Ok(true)
                    }
                    TransferWireMode::Incr => {
                        let key = OutgoingKey {
                            requestor,
                            property,
                        };
                        let Some(transfer) = OutgoingIncr::new(
                            key,
                            content,
                            Arc::clone(&bytes),
                            sha256_digest(&bytes),
                            Instant::now(),
                        ) else {
                            return Ok(false);
                        };
                        if self.outgoing.insert(transfer).is_err() {
                            return Ok(false);
                        }
                        if self
                            .connection
                            .change_window_attributes(
                                requestor,
                                &ChangeWindowAttributesAux::new().event_mask(
                                    EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY,
                                ),
                            )
                            .map_err(|_| BackendFault::Unavailable)?
                            .check()
                            .is_err()
                        {
                            let _removed = self.outgoing.remove(key);
                            return Ok(false);
                        }
                        if self
                            .connection
                            .change_property32(
                                PropMode::REPLACE,
                                requestor,
                                property,
                                self.atoms.incr,
                                &[u32::try_from(bytes.len()).unwrap_or(u32::MAX)],
                            )
                            .map_err(|_| BackendFault::Unavailable)?
                            .check()
                            .is_err()
                        {
                            let _removed = self.outgoing.remove(key);
                            return Ok(false);
                        }
                        self.outgoing_selections.insert(key, selection);
                        self.observe_request(selection, content);
                        Ok(true)
                    }
                }
            }
        }
    }

    fn send_selection_notify(
        &self,
        request: SelectionRequestEvent,
        property: Atom,
    ) -> std::result::Result<(), BackendFault> {
        let event = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property,
        };
        self.connection
            .send_event(false, request.requestor, EventMask::NO_EVENT, event)
            .map_err(|_| BackendFault::Unavailable)?
            .check()
            .map_err(|_| BackendFault::Unavailable)
    }

    fn handle_selection_notify(
        &mut self,
        event: SelectionNotifyEvent,
    ) -> std::result::Result<(), BackendFault> {
        if event.requestor != self.owner_window {
            return Ok(());
        }
        let property = if event.property != NONE {
            event.property
        } else {
            let matching = self.incoming.iter().find_map(|(property, pending)| {
                (self.atoms.selection(pending.transfer.selection) == event.selection
                    && self.expected_target_atom(&pending.transfer) == Some(event.target))
                .then_some(*property)
            });
            let Some(property) = matching else {
                return Ok(());
            };
            property
        };
        let Some(mut pending) = self.incoming.remove(&property) else {
            return Ok(());
        };
        if event.property == NONE
            || event.selection != self.atoms.selection(pending.transfer.selection)
            || self.expected_target_atom(&pending.transfer) != Some(event.target)
        {
            tracing::debug!(
                phase = ?pending.transfer.phase,
                property_none = event.property == NONE,
                selection_matches = event.selection
                    == self.atoms.selection(pending.transfer.selection),
                target_matches = self.expected_target_atom(&pending.transfer)
                    == Some(event.target),
                "external clipboard owner rejected or mismatched a selection conversion"
            );
            let _ignored = pending.reply.send(Err(failure(if event.property == NONE {
                ClipboardActorFailureKind::TargetUnsupported
            } else {
                ClipboardActorFailureKind::ProtocolViolation
            })));
            self.delete_private_property(property)?;
            return Ok(());
        }
        let owner_now = self
            .current_owner(pending.transfer.selection)
            .map_err(|()| BackendFault::Unavailable)?;
        if owner_now != pending.transfer.owner {
            let evidence = pending.transfer.owner_changed();
            complete_incoming(pending, evidence);
            self.delete_private_property(property)?;
            return Ok(());
        }
        match pending.transfer.phase {
            IncomingPhase::AwaitingTargets => {
                let reply = self
                    .connection
                    .get_property(
                        true,
                        self.owner_window,
                        property,
                        self.atoms.atom,
                        0,
                        TARGET_PROPERTY_LONGS,
                    )
                    .map_err(|_| BackendFault::Unavailable)?
                    .reply()
                    .map_err(|_| BackendFault::Unavailable)?;
                let advertised: Vec<_> = reply
                    .value32()
                    .map(|values| {
                        values
                            .filter_map(|atom| self.atoms.identify_target(atom))
                            .filter(|target| target.is_content())
                            .collect()
                    })
                    .unwrap_or_default();
                if reply.type_ != self.atoms.atom
                    || reply.format != 32
                    || reply.bytes_after != 0
                    || reply.value_len as usize > MAX_CLIPBOARD_TARGETS
                {
                    let _ignored = pending
                        .reply
                        .send(Err(failure(ClipboardActorFailureKind::ProtocolViolation)));
                    return Ok(());
                }
                let Some(target) = choose_target(&advertised, &pending.preferred_targets) else {
                    tracing::debug!(
                        advertised = ?advertised,
                        preferred = ?pending.preferred_targets,
                        "external clipboard owner advertised no compatible content target"
                    );
                    let _ignored = pending
                        .reply
                        .send(Err(failure(ClipboardActorFailureKind::TargetUnsupported)));
                    return Ok(());
                };
                if !pending.transfer.select_target(target) {
                    let _ignored = pending
                        .reply
                        .send(Err(failure(ClipboardActorFailureKind::ProtocolViolation)));
                    return Ok(());
                }
                self.connection
                    .convert_selection(
                        self.owner_window,
                        event.selection,
                        self.atoms.target(target),
                        property,
                        event.time,
                    )
                    .map_err(|_| BackendFault::Unavailable)?
                    .check()
                    .map_err(|_| BackendFault::Unavailable)?;
                self.incoming.insert(property, pending);
            }
            IncomingPhase::AwaitingData { target } => {
                let reply = self
                    .connection
                    .get_property(
                        false,
                        self.owner_window,
                        property,
                        AtomEnum::ANY,
                        0,
                        INCOMING_PROPERTY_LONGS,
                    )
                    .map_err(|_| BackendFault::Unavailable)?
                    .reply()
                    .map_err(|_| BackendFault::Unavailable)?;
                if reply.type_ == self.atoms.incr {
                    let announced = reply.value32().and_then(|mut values| values.next());
                    let action = if reply.format == 32
                        && reply.bytes_after == 0
                        && reply.value_len == 1
                    {
                        match announced {
                            Some(bytes) => pending.transfer.begin_incr(target, u64::from(bytes)),
                            None => pending.transfer.protocol_violation(),
                        }
                    } else {
                        pending.transfer.protocol_violation()
                    };
                    match action {
                        IncomingAction::DeleteForNextChunk => {
                            self.delete_private_property(property)?;
                            self.incoming.insert(property, pending);
                        }
                        IncomingAction::Failed(evidence) => {
                            let _ignored =
                                pending.reply.send(Err(failure_from_evidence(&evidence)));
                            self.delete_private_property(property)?;
                        }
                        IncomingAction::Completed(_) => {
                            let _ignored = pending
                                .reply
                                .send(Err(failure(ClipboardActorFailureKind::ProtocolViolation)));
                        }
                    }
                } else {
                    let actual = self
                        .atoms
                        .identify_target(reply.type_)
                        .unwrap_or(RawClipboardTarget::ApplicationOctetStream);
                    let action = if reply.bytes_after == 0 {
                        pending
                            .transfer
                            .finish_direct(target, actual, reply.format, &reply.value)
                    } else {
                        pending.transfer.protocol_violation()
                    };
                    self.delete_private_property(property)?;
                    complete_incoming(pending, action);
                }
            }
            IncomingPhase::ReceivingIncr { .. } | IncomingPhase::Terminal => {
                let _ignored = pending
                    .reply
                    .send(Err(failure(ClipboardActorFailureKind::ProtocolViolation)));
                self.delete_private_property(property)?;
            }
        }
        Ok(())
    }

    fn expected_target_atom(&self, transfer: &IncomingTransfer) -> Option<Atom> {
        match transfer.phase {
            IncomingPhase::AwaitingTargets => Some(self.atoms.targets),
            IncomingPhase::AwaitingData { target }
            | IncomingPhase::ReceivingIncr { target, .. } => Some(self.atoms.target(target)),
            IncomingPhase::Terminal => None,
        }
    }

    fn handle_property_notify(
        &mut self,
        event: PropertyNotifyEvent,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        if event.window == self.owner_window && event.state == Property::NEW_VALUE {
            if event.atom == self.atoms.time_probe {
                self.last_server_time = event.time;
                return Ok(());
            }
            if let Some(mut pending) = self.incoming.remove(&event.atom) {
                if !matches!(pending.transfer.phase, IncomingPhase::ReceivingIncr { .. }) {
                    self.incoming.insert(event.atom, pending);
                    return Ok(());
                }
                let owner = self
                    .current_owner(pending.transfer.selection)
                    .map_err(|()| BackendFault::Unavailable)?;
                let reply = self
                    .connection
                    .get_property(
                        true,
                        self.owner_window,
                        event.atom,
                        AtomEnum::ANY,
                        0,
                        INCOMING_PROPERTY_LONGS,
                    )
                    .map_err(|_| BackendFault::Unavailable)?
                    .reply()
                    .map_err(|_| BackendFault::Unavailable)?;
                let actual = self
                    .atoms
                    .identify_target(reply.type_)
                    .unwrap_or(RawClipboardTarget::ApplicationOctetStream);
                let action = if reply.bytes_after == 0 {
                    pending.transfer.receive_incr_chunk(
                        actual,
                        reply.format,
                        &reply.value,
                        owner,
                        Instant::now(),
                    )
                } else {
                    pending.transfer.protocol_violation()
                };
                match action {
                    IncomingAction::DeleteForNextChunk => {
                        self.incoming.insert(event.atom, pending);
                    }
                    terminal => complete_incoming(pending, terminal),
                }
                return Ok(());
            }
        }
        if event.state == Property::DELETE {
            let key = OutgoingKey {
                requestor: event.window,
                property: event.atom,
            };
            let action = self
                .outgoing
                .get_mut(key)
                .map(|transfer| transfer.on_property_deleted(event.sequence, Instant::now()));
            if let Some(action) = action {
                self.apply_outgoing_action(key, action, sender)?;
            }
        }
        Ok(())
    }

    fn apply_outgoing_action(
        &mut self,
        key: OutgoingKey,
        action: OutgoingAction,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        match action {
            OutgoingAction::WriteChunk { target, bytes } => self
                .connection
                .change_property8(
                    PropMode::REPLACE,
                    key.requestor,
                    key.property,
                    self.atoms.target(target),
                    &bytes,
                )
                .map_err(|_| BackendFault::Unavailable)?
                .check()
                .map_err(|_| BackendFault::Unavailable),
            OutgoingAction::WriteTerminator(evidence) => {
                self.connection
                    .change_property8(
                        PropMode::REPLACE,
                        key.requestor,
                        key.property,
                        self.atoms.target(evidence.target),
                        &[],
                    )
                    .map_err(|_| BackendFault::Unavailable)?
                    .check()
                    .map_err(|_| BackendFault::Unavailable)?;
                let _removed = self.outgoing.remove(key);
                if let Some(selection) = self.outgoing_selections.remove(&key) {
                    self.observe_transfer(selection, evidence.target, evidence, sender);
                }
                Ok(())
            }
            OutgoingAction::DuplicateIgnored => Ok(()),
            OutgoingAction::Failed(evidence) => {
                let _removed = self.outgoing.remove(key);
                if let Some(selection) = self.outgoing_selections.remove(&key) {
                    self.observe_transfer(selection, evidence.target, evidence, sender);
                }
                Ok(())
            }
        }
    }

    fn handle_selection_clear(
        &mut self,
        event: SelectionClearEvent,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        self.last_server_time = event.time;
        let Some(selection) = self.selection_from_atom(event.selection) else {
            return Ok(());
        };
        if self.owned(selection).is_some() {
            self.set_owned(selection, None);
            self.note_owner_change(selection, NONE, sender);
            self.fail_outgoing_for_selection(
                selection,
                SelectionTransferFailureReason::OwnerChanged,
                sender,
            );
        }
        Ok(())
    }

    fn handle_xfixes_selection_notify(
        &mut self,
        selection_atom: Atom,
        owner: Window,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        let Some(selection) = self.selection_from_atom(selection_atom) else {
            return Ok(());
        };
        let index = Self::selection_index(selection);
        if owner == self.known_owners[index] {
            return Ok(());
        }
        if owner != self.owner_window {
            self.set_owned(selection, None);
            self.fail_outgoing_for_selection(
                selection,
                SelectionTransferFailureReason::OwnerChanged,
                sender,
            );
        }
        self.note_owner_change(selection, owner, sender);
        let keys: Vec<_> = self
            .incoming
            .iter()
            .filter_map(|(property, pending)| {
                (pending.transfer.selection == selection && pending.transfer.owner != owner)
                    .then_some(*property)
            })
            .collect();
        for property in keys {
            if let Some(mut pending) = self.incoming.remove(&property) {
                let evidence = pending.transfer.owner_changed();
                complete_incoming(pending, evidence);
                self.delete_private_property(property)?;
            }
        }
        Ok(())
    }

    fn note_owner_change(
        &mut self,
        selection: SelectionName,
        owner: Window,
        sender: &ClipboardEventSender,
    ) {
        let index = Self::selection_index(selection);
        if self.known_owners[index] == owner {
            return;
        }
        self.known_owners[index] = owner;
        self.revisions[index] = self.revisions[index].wrapping_add(1);
        emit_event(
            sender,
            ClipboardActorEvent::OwnershipChanged {
                selection,
                revision: self.revisions[index],
                owned: owner == self.owner_window,
            },
        );
    }

    fn handle_destroy_notify(
        &mut self,
        event: DestroyNotifyEvent,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        for transfer in self.outgoing.remove_requestor(event.window) {
            let key = transfer.key();
            let evidence = transfer.fail(SelectionTransferFailureReason::RequestorDestroyed);
            if let Some(selection) = self.outgoing_selections.remove(&key) {
                self.observe_transfer(selection, evidence.target, evidence, sender);
            }
        }
        Ok(())
    }

    fn fail_outgoing_for_selection(
        &mut self,
        selection: SelectionName,
        reason: SelectionTransferFailureReason,
        sender: &ClipboardEventSender,
    ) {
        let keys: Vec<_> = self
            .outgoing_selections
            .iter()
            .filter_map(|(key, value)| (*value == selection).then_some(*key))
            .collect();
        for key in keys {
            if let Some(transfer) = self.outgoing.remove(key) {
                self.outgoing_selections.remove(&key);
                let evidence = transfer.fail(reason);
                self.observe_transfer(selection, evidence.target, evidence, sender);
            }
        }
    }

    fn observe_request(&mut self, selection: SelectionName, target: RawClipboardTarget) {
        let now = Instant::now();
        for watcher in &mut self.paste_watchers {
            if watcher.selection == selection {
                watcher.observe_request(target, now);
            }
        }
    }

    fn observe_transfer(
        &mut self,
        selection: SelectionName,
        target: RawClipboardTarget,
        evidence: RawSelectionTransferEvidence,
        sender: &ClipboardEventSender,
    ) {
        if matches!(evidence.terminal, SelectionTransferTerminal::Failed { .. }) {
            emit_event(
                sender,
                ClipboardActorEvent::TransferFailed {
                    selection,
                    failure: failure_from_evidence(&evidence).kind,
                },
            );
        }
        let now = Instant::now();
        for watcher in &mut self.paste_watchers {
            if watcher.selection != selection {
                continue;
            }
            watcher.observe_transfer(target, evidence.clone(), now);
        }
    }

    fn expire_transfers(
        &mut self,
        now: Instant,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        for key in self.outgoing.expired_keys(now) {
            if let Some(transfer) = self.outgoing.remove(key) {
                let evidence = transfer.fail(SelectionTransferFailureReason::Timeout);
                if let Some(selection) = self.outgoing_selections.remove(&key) {
                    self.observe_transfer(selection, evidence.target, evidence, sender);
                }
            }
        }
        let properties: Vec<_> = self
            .incoming
            .iter_mut()
            .filter_map(|(property, pending)| {
                pending
                    .transfer
                    .expire(now)
                    .map(|action| (*property, action))
            })
            .collect();
        for (property, action) in properties {
            if let Some(pending) = self.incoming.remove(&property) {
                complete_incoming(pending, action);
                self.delete_private_property(property)?;
            }
        }
        let mut index = 0;
        while index < self.paste_watchers.len() {
            let ready = self.paste_watchers[index].ready(now);
            if ready {
                let watcher = self.paste_watchers.swap_remove(index);
                let result = RawClipboardPasteObservation {
                    selection: watcher.selection,
                    request_observed: !watcher.requested_targets.is_empty(),
                    requested_targets: watcher.requested_targets,
                    transfer: watcher.transfer,
                };
                let _ignored = watcher.reply.send(Ok(result.clone()));
                emit_event(sender, ClipboardActorEvent::PasteObserved(result));
            } else {
                index += 1;
            }
        }
        Ok(())
    }

    fn delete_private_property(&self, property: Atom) -> std::result::Result<(), BackendFault> {
        self.connection
            .delete_property(self.owner_window, property)
            .map_err(|_| BackendFault::Unavailable)?
            .check()
            .map_err(|_| BackendFault::Unavailable)
    }
}

impl ClipboardBackend for X11ClipboardBackend {
    fn poll_event(
        &mut self,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<bool, BackendFault> {
        let event = if let Some(event) = self.deferred_events.pop_front() {
            Some(event)
        } else {
            self.connection
                .poll_for_event()
                .map_err(|_| BackendFault::Unavailable)?
        };
        let Some(event) = event else {
            return Ok(false);
        };
        self.handle_event(event, sender)?;
        Ok(true)
    }

    fn handle_command(
        &mut self,
        command: ClipboardCommand,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        match command {
            ClipboardCommand::Set { request, reply } => self.handle_set(request, reply, sender),
            ClipboardCommand::Clear { selection, reply } => {
                self.handle_clear(selection, reply, sender)
            }
            ClipboardCommand::Read { request, reply } => self.handle_read(request, reply),
            ClipboardCommand::ObservePaste {
                request,
                ready,
                reply,
            } => {
                self.handle_observe_paste(request, ready, reply);
                Ok(())
            }
        }
    }

    fn expire(
        &mut self,
        now: Instant,
        sender: &ClipboardEventSender,
    ) -> std::result::Result<(), BackendFault> {
        self.expire_transfers(now, sender)
    }

    fn counters(&self) -> (u8, u8) {
        (
            u8::try_from(self.outgoing.len()).unwrap_or(u8::MAX),
            u8::try_from(self.incoming.len()).unwrap_or(u8::MAX),
        )
    }

    fn shutdown(&mut self, kind: ClipboardActorFailureKind) {
        for (_, pending) in self.incoming.drain() {
            let _ignored = pending.reply.send(Err(failure(kind)));
        }
        for watcher in self.paste_watchers.drain(..) {
            let _ignored = watcher.reply.send(Err(failure(kind)));
        }
        self.outgoing = OutgoingTransfers::default();
        self.outgoing_selections.clear();
        for selection in [SelectionName::Clipboard, SelectionName::Primary] {
            if self.owned(selection).is_some()
                && let Ok(cookie) = self.connection.set_selection_owner(
                    NONE,
                    self.atoms.selection(selection),
                    self.last_server_time,
                )
            {
                let _ignored = cookie.check();
            }
        }
        let _ignored = self.connection.destroy_window(self.owner_window);
        let _ignored = self.connection.flush();
        let _root = self.root;
    }
}

fn complete_incoming(pending: PendingRead, action: IncomingAction) {
    match action {
        IncomingAction::Completed(result) => {
            let _ignored = pending.reply.send(Ok(result));
        }
        IncomingAction::Failed(evidence) => {
            let _ignored = pending.reply.send(Err(failure_from_evidence(&evidence)));
        }
        IncomingAction::DeleteForNextChunk => {
            let _ignored = pending
                .reply
                .send(Err(failure(ClipboardActorFailureKind::ProtocolViolation)));
        }
    }
}

fn failure_from_evidence(evidence: &RawSelectionTransferEvidence) -> ClipboardActorFailure {
    let kind = match evidence.terminal {
        SelectionTransferTerminal::Failed { reason } => match reason {
            SelectionTransferFailureReason::OwnerChanged => ClipboardActorFailureKind::OwnerChanged,
            SelectionTransferFailureReason::SelectionTooLarge => {
                ClipboardActorFailureKind::SelectionTooLarge
            }
            SelectionTransferFailureReason::Timeout => ClipboardActorFailureKind::TransferTimeout,
            SelectionTransferFailureReason::ProtocolViolation => {
                ClipboardActorFailureKind::ProtocolViolation
            }
            SelectionTransferFailureReason::RequestorDestroyed => {
                ClipboardActorFailureKind::RequestorDestroyed
            }
            SelectionTransferFailureReason::Cancelled => ClipboardActorFailureKind::ActorStopped,
        },
        SelectionTransferTerminal::Completed => ClipboardActorFailureKind::ProtocolViolation,
    };
    failure(kind)
}

fn failure(kind: ClipboardActorFailureKind) -> ClipboardActorFailure {
    ClipboardActorFailure { kind }
}

fn emit_event(sender: &ClipboardEventSender, event: ClipboardActorEvent) {
    sender.emit(event);
}

fn time_before(candidate: u32, reference: u32) -> bool {
    (candidate.wrapping_sub(reference) as i32).is_negative()
}

#[cfg(test)]
mod live_tests {
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    use xenoteer_protocol::{SelectionName, SelectionTransferMode, SelectionTransferTerminal};

    use super::{
        PASTE_QUIET_PERIOD, PasteWatcher, RawClipboardTarget, RawSelectionTransferEvidence,
        X11ClipboardBackend,
    };
    use crate::{
        ClipboardActorExit, ClipboardOwnershipSource, ClipboardPayload, ClipboardSetRequest,
        spawn_clipboard_actor,
    };

    #[test]
    fn paste_watcher_requires_quiet_after_the_latest_request_and_transfer() {
        let now = std::time::Instant::now();
        let (reply, _receiver) = sync_channel(1);
        let mut watcher = PasteWatcher {
            selection: SelectionName::Clipboard,
            requested_targets: Vec::new(),
            transfer: None,
            deadline: now + Duration::from_secs(2),
            quiet_deadline: None,
            reply,
        };
        let evidence = RawSelectionTransferEvidence {
            target: RawClipboardTarget::Utf8String,
            transfer: SelectionTransferMode::Direct,
            content_length: 1,
            sha256: super::sha256_digest(b"x"),
            owner_changed: false,
            terminal_chunk_observed: false,
            terminal: SelectionTransferTerminal::Completed,
        };

        watcher.observe_transfer(RawClipboardTarget::Utf8String, evidence.clone(), now);
        assert!(!watcher.ready(now + PASTE_QUIET_PERIOD - Duration::from_nanos(1)));

        let later_request = now + Duration::from_millis(200);
        watcher.observe_request(RawClipboardTarget::TextPlainUtf8, later_request);
        assert!(!watcher.ready(later_request + Duration::from_millis(200)));

        let later_transfer = later_request + Duration::from_millis(225);
        watcher.observe_transfer(RawClipboardTarget::TextPlainUtf8, evidence, later_transfer);
        assert!(!watcher.ready(later_transfer + PASTE_QUIET_PERIOD - Duration::from_nanos(1)));
        assert!(watcher.ready(later_transfer + PASTE_QUIET_PERIOD));
    }

    #[test]
    #[ignore = "requires an explicitly provisioned Xvfb display"]
    fn opens_dedicated_connection_and_hidden_owner_window() -> crate::Result<()> {
        let display = std::env::var("XENOTEER_TEST_DISPLAY").unwrap_or_else(|_| ":99".to_owned());
        let mut backend = X11ClipboardBackend::open(&display)?;
        assert_ne!(backend.owner_window, 0);
        assert!(backend.last_server_time > 0);
        <X11ClipboardBackend as super::ClipboardBackend>::shutdown(
            &mut backend,
            super::ClipboardActorFailureKind::ActorStopped,
        );
        std::thread::sleep(Duration::from_millis(1));
        Ok(())
    }

    #[test]
    #[ignore = "requires an explicitly provisioned Xvfb display"]
    fn actor_sets_verifies_and_clears_both_independent_selections()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let display = std::env::var("XENOTEER_TEST_DISPLAY").unwrap_or_else(|_| ":99".to_owned());
        let (handle, _events, join) = spawn_clipboard_actor(&display)?;
        for selection in [SelectionName::Clipboard, SelectionName::Primary] {
            let set = handle.try_set(ClipboardSetRequest {
                selection,
                payload: ClipboardPayload::utf8_text("xvfb-clipboard-probe")?,
                source: ClipboardOwnershipSource::Api,
            })?;
            let set = set.recv_timeout(Duration::from_secs(1))??;
            assert!(set.verified);
            assert_ne!(set.owner, 0);
            let clear = handle.try_clear(selection)?;
            let clear = clear.recv_timeout(Duration::from_secs(1))??;
            assert!(clear.verified);
            assert_eq!(clear.owner, 0);
        }
        assert_eq!(join.join(), ClipboardActorExit::Stopped);
        Ok(())
    }
}
