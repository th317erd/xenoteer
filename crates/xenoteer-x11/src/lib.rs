//! X11 backend primitives and executable platform probes for Xenoteer.
//!
//! Connections are intentionally owned by one role. The crate exposes no
//! shared `Arc<Mutex<RustConnection>>` escape hatch.

#![forbid(unsafe_code)]

mod barrier;
pub mod capture;
mod clipboard;
mod connect;
mod desktop;
mod error;
pub mod input;
pub mod keyboard;
mod observe;
mod window_control;

pub use barrier::{fake_absolute_motion, query_pointer_barrier};
pub use clipboard::{
    CLIPBOARD_DIRECT_LIMIT_BYTES, CLIPBOARD_INCR_CHUNK_BYTES, CLIPBOARD_TRANSFER_TIMEOUT,
    ClipboardActorEvent, ClipboardActorEventReceiver, ClipboardActorExit, ClipboardActorFailure,
    ClipboardActorFailureKind, ClipboardActorHandle, ClipboardActorHealth, ClipboardActorJoin,
    ClipboardActorState, ClipboardContentDigest, ClipboardOwnershipEvidence,
    ClipboardOwnershipSource, ClipboardPasteObservation, ClipboardPasteObservationRequest,
    ClipboardPayload, ClipboardPayloadKind, ClipboardReadRawRequest, ClipboardReply,
    ClipboardRequestError, ClipboardSetRequest, ClipboardSubmitError,
    DEFAULT_CLIPBOARD_REQUEST_CAPACITY, MAX_INCR_TRANSFERS_GLOBAL,
    MAX_INCR_TRANSFERS_PER_REQUESTOR, RawClipboardPasteObservation, RawClipboardReadResult,
    RawClipboardTarget, RawSelectionTransferEvidence, spawn_clipboard_actor,
};
pub use connect::{
    ExtensionInfo, ExtensionInventory, ExtensionName, OpenedConnection, XConnectionInfo, connect,
};
pub use desktop::{
    DesktopProbeEvidence, DesktopProbeExpectation, probe_desktop, probe_desktop_steady_state,
};
pub use error::{Result, X11Error};
pub use observe::{
    DAMAGE_COALESCE_INTERVAL, DEFAULT_OBSERVATION_EVENT_CAPACITY,
    DEFAULT_OBSERVATION_REQUEST_CAPACITY, FocusAncestryInput, FocusAncestryStatus, InventorySource,
    InventoryWarning, KnownAtom, MAX_DAMAGE_REGIONS, MAX_FOCUS_ANCESTRY_DEPTH, MAX_ROOT_WINDOWS,
    MAX_SNAPSHOT_INPUT_WARNINGS, OBSERVATION_EVENT_CAPACITY, ObservationActorEvent,
    ObservationActorEventReceiver, ObservationActorExit, ObservationActorFailure,
    ObservationActorFailureKind, ObservationActorHandle, ObservationActorHealth,
    ObservationActorJoin, ObservationActorState, ObservationActorSubmitError,
    ObservationEventReceiver, ObservationPollHandle, ObservationPollThread, ObservationReply,
    ObservedAtom, ObservedPropertyWarning, PollThreadEvent, PropertyWarning, ReconcileDecision,
    RootDamageBatch, RootDamageCoverage, RootDamageHint, RootDamageRect, RootGeometryInput,
    RootInventory, RootWindowEvidenceInput, WindowAttributeInput, WindowPropertyInput,
    WindowRefresh, WindowSnapshotInput, spawn_observation_actor,
};
pub use window_control::{
    DEFAULT_WINDOW_CONTROL_REQUEST_CAPACITY, MAX_WINDOW_CONTROL_TIMEOUT,
    RawWindowBooleanObservation, RawWindowControlEvidence, RawWindowControlObservation,
    RawWindowControlOperation, RawWindowControlOutcome, RawWindowControlRequest,
    RawWindowControlRequestError, RawWindowGeometryObservation, RawWindowManagerCapabilities,
    RawWindowRevalidationError, WINDOW_CONTROL_POLL_INTERVAL, WindowControlActorExit,
    WindowControlActorFailure, WindowControlActorFailureKind, WindowControlActorHandle,
    WindowControlActorHealth, WindowControlActorJoin, WindowControlActorState, WindowControlReply,
    WindowControlSubmitError, spawn_window_control_actor,
};
