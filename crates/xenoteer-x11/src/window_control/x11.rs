//! Production EWMH/ICCCM adapter and pure wire encoders.

use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection as _;
use x11rb::errors::ReplyError;
use x11rb::protocol::ErrorKind;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _,
    EventMask, InputFocus, MapState, StackMode as XStackMode, Window,
};
use x11rb::rust_connection::RustConnection;
use xenoteer_core::window_geometry::{
    ResolvedWindowGeometry, WindowGeometryContext, WindowGeometryResolveError, derive_frame_rect,
};
use xenoteer_protocol::{
    CoordinateSpace, MAX_WINDOW_ATOMS, Rect, WindowCloseWaitPolicy, WindowControlWarning,
    WindowFrameExtents, WindowGeometry, WindowGeometryRequest, WindowGeometryTarget,
    WindowManagerCapability, WindowManagerState, WindowRect, WindowStackMode,
};

use super::{
    BackendFault, RawWindowBooleanObservation, RawWindowControlEvidence,
    RawWindowControlObservation, RawWindowControlOperation, RawWindowControlOutcome,
    RawWindowControlRequest, RawWindowGeometryObservation, RawWindowManagerCapabilities,
    WINDOW_CONTROL_POLL_INTERVAL, WindowControlBackend,
};
use crate::observe::atoms::{KnownAtom, KnownAtoms};
use crate::observe::focus::query_focus_ancestry;
use crate::observe::geometry::query_root_geometry;
use crate::observe::property::{
    PropertyWarning, decode_atom_list, decode_cardinals, decode_window_list, read_property_bounded,
};
use crate::{Result, X11Error, connect};

const EWMH_SOURCE_PAGER: u32 = 2;
const EWMH_STATE_REMOVE: u32 = 0;
const EWMH_STATE_ADD: u32 = 1;
const ICCCM_ICONIC_STATE: u32 = 3;
const STATIC_GRAVITY: u32 = 10;
const MAX_CONTROL_PROPERTY_BYTES: usize = MAX_WINDOW_ATOMS * 4;
const ACTIVE_WINDOW_PROPERTY_BYTES: usize = 8;
pub(super) const GEOMETRY_QUIET_WINDOW: Duration = Duration::from_millis(50);
pub(super) const MAX_GEOMETRY_SETTLE: Duration = Duration::from_secs(1);
const ACTIVATION_FALLBACK_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GeometryWaitState {
    Waiting,
    Settled,
    Expired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActivationRequestPath {
    Ewmh,
    SetInputFocus,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct GeometryQuietTracker {
    last_change: Duration,
    saw_change: bool,
}

impl GeometryQuietTracker {
    pub(super) const fn new() -> Self {
        Self {
            last_change: Duration::ZERO,
            saw_change: false,
        }
    }

    pub(super) fn observe(
        &mut self,
        elapsed: Duration,
        configured_or_changed: bool,
        matches: bool,
    ) -> GeometryWaitState {
        if configured_or_changed {
            self.last_change = elapsed;
            self.saw_change = true;
        }
        if (matches || self.saw_change)
            && elapsed.saturating_sub(self.last_change) >= GEOMETRY_QUIET_WINDOW
        {
            GeometryWaitState::Settled
        } else if elapsed >= MAX_GEOMETRY_SETTLE {
            GeometryWaitState::Expired
        } else {
            GeometryWaitState::Waiting
        }
    }
}

pub(super) struct X11WindowControlBackend {
    connection: RustConnection,
    root: Window,
    atoms: KnownAtoms,
}

impl X11WindowControlBackend {
    pub(super) fn open(display: &str) -> Result<Self> {
        let opened = connect(display)?;
        let atoms = KnownAtoms::intern(&opened.connection)?;
        Ok(Self {
            connection: opened.connection,
            root: opened.info.root,
            atoms,
        })
    }

    fn wm_support(&self) -> std::result::Result<WmSupport, BackendFault> {
        let supported = self.read_atoms(
            self.root,
            KnownAtom::NetSupported,
            MAX_CONTROL_PROPERTY_BYTES,
        )?;
        let atoms: HashSet<_> = supported.unwrap_or_default().into_iter().collect();
        let has = |atom| atoms.contains(&self.atoms.get(atom));
        let mut public = Vec::with_capacity(13);
        if has(KnownAtom::NetActiveWindow) {
            public.push(WindowManagerCapability::Activate);
        }
        if has(KnownAtom::NetCloseWindow) {
            public.push(WindowManagerCapability::Close);
        }
        if has(KnownAtom::NetWmState)
            && has(KnownAtom::NetWmStateMaximizedVert)
            && has(KnownAtom::NetWmStateMaximizedHorz)
        {
            public.push(WindowManagerCapability::StateMaximized);
        }
        if has(KnownAtom::NetWmState) && has(KnownAtom::NetWmStateFullscreen) {
            public.push(WindowManagerCapability::StateFullscreen);
        }
        if has(KnownAtom::NetWmState) && has(KnownAtom::NetWmStateAbove) {
            public.push(WindowManagerCapability::StateAbove);
        }
        if has(KnownAtom::NetWmState) && has(KnownAtom::NetWmStateSticky) {
            public.push(WindowManagerCapability::StateSticky);
        }
        if has(KnownAtom::NetMoveResizeWindow) {
            public.push(WindowManagerCapability::MoveResize);
        }
        if has(KnownAtom::NetClientList) {
            public.push(WindowManagerCapability::ClientList);
        }
        if has(KnownAtom::NetClientListStacking) {
            public.push(WindowManagerCapability::StackingList);
        }
        if has(KnownAtom::NetFrameExtents) {
            public.push(WindowManagerCapability::FrameExtents);
        }
        if has(KnownAtom::NetCurrentDesktop) {
            public.push(WindowManagerCapability::CurrentWorkspace);
        }
        if has(KnownAtom::NetWmDesktop) {
            public.push(WindowManagerCapability::WindowWorkspace);
        }
        if has(KnownAtom::NetWmDesktop) && has(KnownAtom::NetNumberOfDesktops) {
            public.push(WindowManagerCapability::MoveToWorkspace);
        }
        let restack = has(KnownAtom::NetRestackWindow);
        Ok(WmSupport {
            atoms,
            public: RawWindowManagerCapabilities {
                supported: public,
                restack,
            },
        })
    }

    fn operation_supported(
        &self,
        operation: RawWindowControlOperation,
        support: &WmSupport,
    ) -> bool {
        let has = |atom| support.atoms.contains(&self.atoms.get(atom));
        match operation {
            RawWindowControlOperation::Activate {
                switch_workspace,
                allow_set_input_focus,
                ..
            } => {
                activation_request_path(has(KnownAtom::NetActiveWindow), allow_set_input_focus)
                    != ActivationRequestPath::Unsupported
                    && (switch_workspace.is_none()
                        || (has(KnownAtom::NetCurrentDesktop)
                            && has(KnownAtom::NetNumberOfDesktops)))
            }
            RawWindowControlOperation::Close { .. } => true,
            RawWindowControlOperation::SetState { state, .. } => {
                has(KnownAtom::NetWmState) && state_atoms(state).into_iter().flatten().all(has)
            }
            RawWindowControlOperation::Minimize { desired: true, .. } => true,
            RawWindowControlOperation::Minimize { desired: false, .. } => {
                has(KnownAtom::NetActiveWindow)
            }
            RawWindowControlOperation::MoveResize { .. } => has(KnownAtom::NetMoveResizeWindow),
            RawWindowControlOperation::MoveToWorkspace { .. } => {
                has(KnownAtom::NetWmDesktop) && has(KnownAtom::NetNumberOfDesktops)
            }
            RawWindowControlOperation::Stack {
                allow_raw_fallback, ..
            } => has(KnownAtom::NetRestackWindow) || allow_raw_fallback,
        }
    }

    fn send_operation(
        &self,
        request: &RawWindowControlRequest,
        support: &WmSupport,
        current_active: Option<Window>,
        geometry: Option<&ResolvedWindowGeometry>,
    ) -> std::result::Result<Vec<WindowControlWarning>, BackendFault> {
        let target = request.target;
        let mut warnings = Vec::with_capacity(2);
        let wire = match request.operation {
            RawWindowControlOperation::Activate {
                timestamp,
                switch_workspace,
                allow_set_input_focus,
            } => {
                if timestamp == 0 {
                    warnings.push(WindowControlWarning::CurrentTimeFallback);
                }
                if let Some(workspace) = switch_workspace {
                    self.validate_workspace(workspace)?;
                    self.send_checked(encode_current_workspace(
                        self.root,
                        self.atoms.get(KnownAtom::NetCurrentDesktop),
                        workspace,
                        timestamp,
                    ))?;
                }
                match activation_request_path(
                    support
                        .atoms
                        .contains(&self.atoms.get(KnownAtom::NetActiveWindow)),
                    allow_set_input_focus,
                ) {
                    ActivationRequestPath::Ewmh => encode_activate(
                        self.root,
                        target,
                        self.atoms.get(KnownAtom::NetActiveWindow),
                        timestamp,
                        current_active,
                    ),
                    ActivationRequestPath::SetInputFocus => {
                        warnings.push(WindowControlWarning::UsedSetInputFocusFallback);
                        WireRequest::SetInputFocus { target, timestamp }
                    }
                    ActivationRequestPath::Unsupported => {
                        return Err(BackendFault::Unsupported);
                    }
                }
            }
            RawWindowControlOperation::Close {
                timestamp,
                wait_for: _,
            } => {
                if timestamp == 0 {
                    warnings.push(WindowControlWarning::CurrentTimeFallback);
                }
                if support
                    .atoms
                    .contains(&self.atoms.get(KnownAtom::NetCloseWindow))
                {
                    encode_close(
                        self.root,
                        target,
                        self.atoms.get(KnownAtom::NetCloseWindow),
                        timestamp,
                    )
                } else {
                    let protocols = self.read_atoms(
                        target,
                        KnownAtom::WmProtocols,
                        MAX_CONTROL_PROPERTY_BYTES,
                    )?;
                    if protocols.as_ref().is_none_or(|values| {
                        !values.contains(&self.atoms.get(KnownAtom::WmDeleteWindow))
                    }) {
                        return Err(BackendFault::Unsupported);
                    }
                    warnings.push(WindowControlWarning::UsedWmDeleteWindowFallback);
                    encode_wm_delete(
                        target,
                        self.atoms.get(KnownAtom::WmProtocols),
                        self.atoms.get(KnownAtom::WmDeleteWindow),
                        timestamp,
                    )
                }
            }
            RawWindowControlOperation::SetState { state, desired } => encode_state(
                self.root,
                target,
                self.atoms.get(KnownAtom::NetWmState),
                state,
                desired,
                &self.atoms,
            ),
            RawWindowControlOperation::Minimize {
                desired: true,
                timestamp: _,
            } => encode_minimize(self.root, target, self.atoms.get(KnownAtom::WmChangeState)),
            RawWindowControlOperation::Minimize {
                desired: false,
                timestamp,
            } => {
                if timestamp == 0 {
                    warnings.push(WindowControlWarning::CurrentTimeFallback);
                }
                encode_activate(
                    self.root,
                    target,
                    self.atoms.get(KnownAtom::NetActiveWindow),
                    timestamp,
                    current_active,
                )
            }
            RawWindowControlOperation::MoveResize { .. } => encode_move_resize(
                self.root,
                target,
                self.atoms.get(KnownAtom::NetMoveResizeWindow),
                geometry
                    .ok_or(BackendFault::MalformedWindowManagerData)?
                    .client_request,
            ),
            RawWindowControlOperation::MoveToWorkspace { workspace } => {
                self.validate_workspace(workspace)?;
                encode_workspace(
                    self.root,
                    target,
                    self.atoms.get(KnownAtom::NetWmDesktop),
                    workspace,
                )
            }
            RawWindowControlOperation::Stack {
                mode,
                sibling,
                allow_raw_fallback,
            } => {
                if support
                    .atoms
                    .contains(&self.atoms.get(KnownAtom::NetRestackWindow))
                {
                    encode_restack(
                        self.root,
                        target,
                        self.atoms.get(KnownAtom::NetRestackWindow),
                        mode,
                        sibling,
                    )
                } else if allow_raw_fallback {
                    warnings.push(WindowControlWarning::UsedRawStackFallback);
                    WireRequest::ConfigureStack {
                        target,
                        sibling,
                        mode: x_stack_mode(mode),
                    }
                } else {
                    return Err(BackendFault::Unsupported);
                }
            }
        };
        self.send_checked(wire)?;
        Ok(warnings)
    }

    fn send_checked(&self, wire: WireRequest) -> std::result::Result<(), BackendFault> {
        match wire {
            WireRequest::ClientMessage {
                destination,
                event_mask,
                window,
                message_type,
                data,
            } => self
                .connection
                .send_event(
                    false,
                    destination,
                    event_mask,
                    ClientMessageEvent::new(32, window, message_type, data),
                )
                .map_err(|_| BackendFault::BackendUnavailable)?
                .check()
                .map_err(classify_reply_error),
            WireRequest::ConfigureStack {
                target,
                sibling,
                mode,
            } => {
                let mut aux = ConfigureWindowAux::new().stack_mode(mode);
                if let Some(sibling) = sibling {
                    aux = aux.sibling(sibling);
                }
                self.connection
                    .configure_window(target, &aux)
                    .map_err(|_| BackendFault::BackendUnavailable)?
                    .check()
                    .map_err(classify_reply_error)
            }
            WireRequest::SetInputFocus { target, timestamp } => self
                .connection
                .set_input_focus(InputFocus::PARENT, target, timestamp)
                .map_err(|_| BackendFault::BackendUnavailable)?
                .check()
                .map_err(classify_reply_error),
        }
    }

    fn observe(
        &self,
        request: &RawWindowControlRequest,
        current_active_sent: Option<Window>,
    ) -> std::result::Result<RawWindowControlObservation, BackendFault> {
        let viewable = match self.target_viewable(request.target) {
            Ok(viewable) => viewable,
            Err(BackendFault::TargetVanished)
                if matches!(request.operation, RawWindowControlOperation::Close { .. }) =>
            {
                return Ok(RawWindowControlObservation::Close {
                    exists: false,
                    viewable: None,
                });
            }
            Err(fault) => return Err(fault),
        };
        match request.operation {
            RawWindowControlOperation::Activate { timestamp, .. } => {
                self.observe_activation(request.target, timestamp, current_active_sent)
            }
            RawWindowControlOperation::Close { .. } => Ok(RawWindowControlObservation::Close {
                exists: true,
                viewable: Some(viewable),
            }),
            RawWindowControlOperation::SetState { state, .. } => {
                Ok(RawWindowControlObservation::State(
                    self.observe_manager_state(request.target, state)?,
                ))
            }
            RawWindowControlOperation::Minimize { timestamp, .. } => {
                let states = self.window_states(request.target)?;
                let hidden = states.contains(&self.atoms.get(KnownAtom::NetWmStateHidden));
                let value = if hidden || !viewable {
                    RawWindowBooleanObservation::Enabled
                } else {
                    RawWindowBooleanObservation::Disabled
                };
                let _ = timestamp;
                Ok(RawWindowControlObservation::State(value))
            }
            RawWindowControlOperation::MoveResize { .. } => {
                Err(BackendFault::MalformedWindowManagerData)
            }
            RawWindowControlOperation::MoveToWorkspace { .. } => {
                Ok(RawWindowControlObservation::Workspace(
                    self.read_single_cardinal(request.target, KnownAtom::NetWmDesktop)?,
                ))
            }
            RawWindowControlOperation::Stack { sibling, .. } => {
                let stacking = self
                    .read_windows(
                        self.root,
                        KnownAtom::NetClientListStacking,
                        MAX_CONTROL_PROPERTY_BYTES,
                    )?
                    .unwrap_or_default();
                Ok(RawWindowControlObservation::Stacking {
                    target_index: position(&stacking, request.target),
                    sibling_index: sibling.and_then(|window| position(&stacking, window)),
                    window_count: u32::try_from(stacking.len()).unwrap_or(u32::MAX),
                })
            }
        }
    }

    fn observe_activation(
        &self,
        target: Window,
        timestamp: u32,
        current_active_sent: Option<Window>,
    ) -> std::result::Result<RawWindowControlObservation, BackendFault> {
        let active = self.read_single_window(self.root, KnownAtom::NetActiveWindow)?;
        let focus = query_focus_ancestry(&self.connection, self.root, target, &[target])
            .map_err(classify_x11_error)?;
        let current_workspace =
            self.read_single_cardinal(self.root, KnownAtom::NetCurrentDesktop)?;
        Ok(RawWindowControlObservation::Activation {
            current_active_sent,
            timestamp_sent: timestamp,
            active,
            focused: focus.raw_focus,
            focus_within_target: focus.target_contains_focus,
            focus_ancestry_status: focus.status,
            current_workspace,
        })
    }

    fn root_rect(&self) -> std::result::Result<WindowRect, BackendFault> {
        let geometry = self
            .connection
            .get_geometry(self.root)
            .map_err(|_| BackendFault::BackendUnavailable)?
            .reply()
            .map_err(classify_reply_error)?;
        if geometry.root != self.root {
            return Err(BackendFault::MalformedWindowManagerData);
        }
        WindowRect::new(
            CoordinateSpace::RootPhysical,
            Rect::new(0, 0, u32::from(geometry.width), u32::from(geometry.height))
                .map_err(|_| BackendFault::MalformedWindowManagerData)?,
        )
        .map_err(|_| BackendFault::MalformedWindowManagerData)
    }

    fn window_geometry(&self, target: Window) -> std::result::Result<WindowGeometry, BackendFault> {
        let client_rect = query_root_geometry(&self.connection, self.root, target)
            .map_err(classify_x11_error)?
            .client_rect;
        let frame_extents = self.frame_extents(target)?;
        let frame_rect = frame_extents
            .map(|extents| derive_frame_rect(client_rect, extents))
            .transpose()
            .map_err(|_| BackendFault::MalformedWindowManagerData)?;
        let geometry = WindowGeometry {
            client_rect,
            frame_rect,
            content_rect: client_rect,
            frame_extents,
        };
        geometry
            .validate()
            .map_err(|_| BackendFault::MalformedWindowManagerData)?;
        Ok(geometry)
    }

    fn geometry_context(
        &self,
        target: Window,
    ) -> std::result::Result<WindowGeometryContext, BackendFault> {
        WindowGeometryContext::new(self.root_rect()?, self.window_geometry(target)?)
            .map_err(|_| BackendFault::MalformedWindowManagerData)
    }

    fn frame_extents(
        &self,
        target: Window,
    ) -> std::result::Result<Option<WindowFrameExtents>, BackendFault> {
        let raw = read_property_bounded(
            &self.connection,
            target,
            self.atoms.get(KnownAtom::NetFrameExtents),
            self.atoms.get(KnownAtom::Cardinal),
            16,
        )
        .map_err(classify_x11_error)?;
        let decoded = decode_cardinals(&raw, self.atoms.get(KnownAtom::Cardinal));
        let Some(values) = validate_decoded(decoded.value, &decoded.warnings)? else {
            return Ok(None);
        };
        let [left, right, top, bottom] = values.as_slice() else {
            return Err(BackendFault::MalformedWindowManagerData);
        };
        let extents = WindowFrameExtents {
            left: *left,
            right: *right,
            top: *top,
            bottom: *bottom,
        };
        extents
            .validate()
            .map_err(|_| BackendFault::MalformedWindowManagerData)?;
        Ok(Some(extents))
    }

    fn select_geometry_events(&self, target: Window) -> std::result::Result<(), BackendFault> {
        self.connection
            .change_window_attributes(
                target,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
            )
            .map_err(|_| BackendFault::BackendUnavailable)?
            .check()
            .map_err(classify_reply_error)
    }

    fn drain_target_configures(&self, target: Window) -> std::result::Result<bool, BackendFault> {
        let mut configured = false;
        loop {
            let event = self
                .connection
                .poll_for_event()
                .map_err(|_| BackendFault::BackendUnavailable)?;
            let Some(event) = event else {
                return Ok(configured);
            };
            if matches!(event, Event::ConfigureNotify(event) if event.window == target) {
                configured = true;
            }
        }
    }

    fn validate_workspace(&self, workspace: u32) -> std::result::Result<(), BackendFault> {
        let count = self.read_single_cardinal(self.root, KnownAtom::NetNumberOfDesktops)?;
        if count.is_none_or(|count| count == 0 || workspace >= count) {
            Err(BackendFault::Refused)
        } else {
            Ok(())
        }
    }

    fn observe_manager_state(
        &self,
        window: Window,
        state: WindowManagerState,
    ) -> std::result::Result<RawWindowBooleanObservation, BackendFault> {
        let states = self.window_states(window)?;
        let required: Vec<_> = state_atoms(state)
            .into_iter()
            .flatten()
            .map(|atom| self.atoms.get(atom))
            .collect();
        let present = required.iter().filter(|atom| states.contains(atom)).count();
        Ok(if present == 0 {
            RawWindowBooleanObservation::Disabled
        } else if present == required.len() {
            RawWindowBooleanObservation::Enabled
        } else {
            RawWindowBooleanObservation::Partial
        })
    }

    fn window_states(&self, window: Window) -> std::result::Result<Vec<Atom>, BackendFault> {
        self.read_atoms(window, KnownAtom::NetWmState, MAX_CONTROL_PROPERTY_BYTES)
            .map(|states| states.unwrap_or_default())
    }

    fn target_viewable(&self, window: Window) -> std::result::Result<bool, BackendFault> {
        self.connection
            .get_window_attributes(window)
            .map_err(|_| BackendFault::BackendUnavailable)?
            .reply()
            .map(|reply| reply.map_state == MapState::VIEWABLE)
            .map_err(classify_reply_error)
    }

    fn read_atoms(
        &self,
        window: Window,
        property: KnownAtom,
        max_bytes: usize,
    ) -> std::result::Result<Option<Vec<Atom>>, BackendFault> {
        let raw = read_property_bounded(
            &self.connection,
            window,
            self.atoms.get(property),
            self.atoms.get(KnownAtom::Atom),
            max_bytes,
        )
        .map_err(classify_x11_error)?;
        let decoded = decode_atom_list(&raw, self.atoms.get(KnownAtom::Atom));
        validate_decoded(decoded.value, &decoded.warnings)
    }

    fn read_windows(
        &self,
        window: Window,
        property: KnownAtom,
        max_bytes: usize,
    ) -> std::result::Result<Option<Vec<Window>>, BackendFault> {
        let raw = read_property_bounded(
            &self.connection,
            window,
            self.atoms.get(property),
            self.atoms.get(KnownAtom::Window),
            max_bytes,
        )
        .map_err(classify_x11_error)?;
        let decoded = decode_window_list(&raw, self.atoms.get(KnownAtom::Window));
        validate_decoded(decoded.value, &decoded.warnings)
    }

    fn read_single_window(
        &self,
        window: Window,
        property: KnownAtom,
    ) -> std::result::Result<Option<Window>, BackendFault> {
        let values = self.read_windows(window, property, ACTIVE_WINDOW_PROPERTY_BYTES)?;
        normalize_active_window(values)
    }

    fn read_single_cardinal(
        &self,
        window: Window,
        property: KnownAtom,
    ) -> std::result::Result<Option<u32>, BackendFault> {
        let raw = read_property_bounded(
            &self.connection,
            window,
            self.atoms.get(property),
            self.atoms.get(KnownAtom::Cardinal),
            4,
        )
        .map_err(classify_x11_error)?;
        let decoded = decode_cardinals(&raw, self.atoms.get(KnownAtom::Cardinal));
        exact_single(validate_decoded(decoded.value, &decoded.warnings)?)
    }

    fn execute_geometry(
        &mut self,
        request: &RawWindowControlRequest,
        support: WmSupport,
    ) -> std::result::Result<RawWindowControlEvidence, BackendFault> {
        let RawWindowControlOperation::MoveResize {
            relative_to,
            geometry,
            bounds_policy,
        } = request.operation
        else {
            return Err(BackendFault::MalformedWindowManagerData);
        };
        self.select_geometry_events(request.target)?;
        let _discarded = self.drain_target_configures(request.target)?;
        let context = self.geometry_context(request.target)?;
        let initial_geometry = context.window().clone();
        let resolution = match context.resolve_move_resize(relative_to, geometry, bounds_policy) {
            Ok(resolution) => resolution,
            Err(WindowGeometryResolveError::FrameGeometryUnavailable) => {
                return Ok(evidence(
                    request,
                    RawWindowControlOutcome::Unsupported,
                    RawWindowControlObservation::NotObserved,
                    support.public,
                    Vec::new(),
                ));
            }
            Err(
                WindowGeometryResolveError::InvalidGeometry
                | WindowGeometryResolveError::FrameTooSmall
                | WindowGeometryResolveError::OutsideRootBounds
                | WindowGeometryResolveError::ArithmeticOverflow
                | WindowGeometryResolveError::UnsupportedCoordinateSpace,
            ) => {
                return Ok(evidence(
                    request,
                    RawWindowControlOutcome::InvalidGeometry,
                    RawWindowControlObservation::NotObserved,
                    support.public,
                    Vec::new(),
                ));
            }
            Err(WindowGeometryResolveError::InconsistentFrameGeometry) => {
                return Ok(evidence(
                    request,
                    RawWindowControlOutcome::MalformedWindowManagerData,
                    RawWindowControlObservation::NotObserved,
                    support.public,
                    Vec::new(),
                ));
            }
        };
        let mut warnings = self.send_operation(request, &support, None, Some(&resolution))?;
        if resolution.bounds_constrained {
            warnings.push(WindowControlWarning::GeometryConstrained);
        }

        let started = Instant::now();
        let mut tracker = GeometryQuietTracker::new();
        let mut previous = initial_geometry.clone();
        let initial_observation =
            raw_geometry_observation(initial_geometry.clone(), resolution, false);
        let observed = loop {
            let configured_before = self.drain_target_configures(request.target)?;
            let current = match self.window_geometry(request.target) {
                Ok(current) => current,
                Err(BackendFault::MalformedWindowManagerData) => {
                    return Ok(evidence(
                        request,
                        RawWindowControlOutcome::MalformedWindowManagerData,
                        initial_observation,
                        support.public,
                        warnings,
                    ));
                }
                Err(fault) => return Err(fault),
            };
            let configured_after = self.drain_target_configures(request.target)?;
            let elapsed = started.elapsed();
            let matches = geometry_matches_effective(relative_to, &current, resolution.effective);
            let state = tracker.observe(
                elapsed,
                configured_before || configured_after || current != previous,
                matches,
            );
            let observed = raw_geometry_observation(
                current.clone(),
                resolution,
                state == GeometryWaitState::Settled,
            );
            if state == GeometryWaitState::Settled {
                return Ok(evidence(
                    request,
                    if matches {
                        RawWindowControlOutcome::Converged
                    } else {
                        RawWindowControlOutcome::Partial
                    },
                    observed,
                    support.public,
                    warnings,
                ));
            }
            if state == GeometryWaitState::Expired || elapsed >= request.timeout {
                break observed;
            }
            previous = current;
            let remaining = request
                .timeout
                .min(MAX_GEOMETRY_SETTLE)
                .saturating_sub(elapsed);
            thread::sleep(remaining.min(WINDOW_CONTROL_POLL_INTERVAL));
        };
        let final_geometry = match &observed {
            RawWindowControlObservation::Geometry(observed) => &observed.observed,
            _ => unreachable!(),
        };
        let outcome = if final_geometry != &initial_geometry {
            RawWindowControlOutcome::Partial
        } else {
            RawWindowControlOutcome::TimedOut
        };
        Ok(evidence(
            request,
            outcome,
            observed,
            support.public,
            warnings,
        ))
    }
}

impl WindowControlBackend for X11WindowControlBackend {
    fn capabilities(&mut self) -> std::result::Result<RawWindowManagerCapabilities, BackendFault> {
        self.wm_support().map(|support| support.public)
    }

    fn execute(
        &mut self,
        request: &RawWindowControlRequest,
    ) -> std::result::Result<RawWindowControlEvidence, BackendFault> {
        // This capability read is intentionally the first X11 operation after
        // the actor's exact-reference revalidator.
        let support = self.wm_support()?;
        if !self.operation_supported(request.operation, &support) {
            return Ok(evidence(
                request,
                RawWindowControlOutcome::Unsupported,
                RawWindowControlObservation::NotObserved,
                support.public,
                Vec::new(),
            ));
        }

        if matches!(
            request.operation,
            RawWindowControlOperation::MoveResize { .. }
        ) {
            return self.execute_geometry(request, support);
        }

        let mut initial = self.observe(request, None)?;
        let current_active = match &initial {
            RawWindowControlObservation::Activation { active, .. } => *active,
            _ if matches!(
                request.operation,
                RawWindowControlOperation::Minimize { desired: false, .. }
            ) =>
            {
                self.read_single_window(self.root, KnownAtom::NetActiveWindow)?
            }
            _ => None,
        };
        if let RawWindowControlObservation::Activation {
            ref mut current_active_sent,
            ..
        } = initial
        {
            *current_active_sent = current_active;
        }
        let mut warnings = match self.send_operation(request, &support, current_active, None) {
            Ok(warnings) => warnings,
            Err(BackendFault::Unsupported) => {
                return Ok(evidence(
                    request,
                    RawWindowControlOutcome::Unsupported,
                    initial,
                    support.public,
                    Vec::new(),
                ));
            }
            Err(fault) => return Err(fault),
        };

        if matches!(
            request.operation,
            RawWindowControlOperation::Close {
                wait_for: WindowCloseWaitPolicy::RequestSent,
                ..
            }
        ) {
            return Ok(evidence(
                request,
                RawWindowControlOutcome::RequestSent,
                initial,
                support.public,
                warnings,
            ));
        }

        let deadline = Instant::now() + request.timeout;
        let mut observed = match self.observe(request, current_active) {
            Ok(observed) => observed,
            Err(BackendFault::MalformedWindowManagerData) => {
                return Ok(evidence(
                    request,
                    RawWindowControlOutcome::MalformedWindowManagerData,
                    initial,
                    support.public,
                    warnings,
                ));
            }
            Err(fault) => return Err(fault),
        };
        let fallback_at = (request.timeout / 2).min(ACTIVATION_FALLBACK_DELAY);
        let mut fallback_attempted =
            warnings.contains(&WindowControlWarning::UsedSetInputFocusFallback);
        loop {
            if observation_satisfies(request, &observed) {
                return Ok(evidence(
                    request,
                    RawWindowControlOutcome::Converged,
                    observed,
                    support.public,
                    warnings,
                ));
            }
            if !fallback_attempted
                && activation_allows_set_input_focus(request)
                && (activation_is_active_without_focus(request, &observed)
                    || deadline.saturating_duration_since(Instant::now())
                        <= request.timeout.saturating_sub(fallback_at))
            {
                fallback_attempted = true;
                warnings.push(WindowControlWarning::UsedSetInputFocusFallback);
                let timestamp = activation_timestamp(request).unwrap_or_default();
                match self.send_checked(WireRequest::SetInputFocus {
                    target: request.target,
                    timestamp,
                }) {
                    Ok(()) | Err(BackendFault::Refused) => {
                        observed = match self.observe(request, current_active) {
                            Ok(observed) => observed,
                            Err(BackendFault::MalformedWindowManagerData) => {
                                return Ok(evidence(
                                    request,
                                    RawWindowControlOutcome::MalformedWindowManagerData,
                                    initial,
                                    support.public,
                                    warnings,
                                ));
                            }
                            Err(fault) => return Err(fault),
                        };
                        continue;
                    }
                    Err(fault) => return Err(fault),
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(WINDOW_CONTROL_POLL_INTERVAL));
            observed = match self.observe(request, current_active) {
                Ok(observed) => observed,
                Err(BackendFault::MalformedWindowManagerData) => {
                    return Ok(evidence(
                        request,
                        RawWindowControlOutcome::MalformedWindowManagerData,
                        initial,
                        support.public,
                        warnings,
                    ));
                }
                Err(fault) => return Err(fault),
            };
        }
        let outcome = if observation_is_partial(request, &initial, &observed) {
            RawWindowControlOutcome::Partial
        } else {
            RawWindowControlOutcome::TimedOut
        };
        Ok(evidence(
            request,
            outcome,
            observed,
            support.public,
            warnings,
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WireRequest {
    ClientMessage {
        destination: Window,
        event_mask: EventMask,
        window: Window,
        message_type: Atom,
        data: [u32; 5],
    },
    ConfigureStack {
        target: Window,
        sibling: Option<Window>,
        mode: XStackMode,
    },
    SetInputFocus {
        target: Window,
        timestamp: u32,
    },
}

fn root_message(root: Window, window: Window, message_type: Atom, data: [u32; 5]) -> WireRequest {
    WireRequest::ClientMessage {
        destination: root,
        event_mask: EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        window,
        message_type,
        data,
    }
}

pub(super) fn encode_activate(
    root: Window,
    target: Window,
    message_type: Atom,
    timestamp: u32,
    current_active: Option<Window>,
) -> WireRequest {
    root_message(
        root,
        target,
        message_type,
        [
            EWMH_SOURCE_PAGER,
            timestamp,
            current_active.unwrap_or(0),
            0,
            0,
        ],
    )
}

pub(super) fn encode_close(
    root: Window,
    target: Window,
    message_type: Atom,
    timestamp: u32,
) -> WireRequest {
    root_message(
        root,
        target,
        message_type,
        [timestamp, EWMH_SOURCE_PAGER, 0, 0, 0],
    )
}

pub(super) fn encode_wm_delete(
    target: Window,
    wm_protocols: Atom,
    wm_delete: Atom,
    timestamp: u32,
) -> WireRequest {
    WireRequest::ClientMessage {
        destination: target,
        event_mask: EventMask::NO_EVENT,
        window: target,
        message_type: wm_protocols,
        data: [wm_delete, timestamp, 0, 0, 0],
    }
}

pub(super) fn encode_state(
    root: Window,
    target: Window,
    message_type: Atom,
    state: WindowManagerState,
    desired: bool,
    atoms: &KnownAtoms,
) -> WireRequest {
    let [first, second] = state_atoms(state).map(|atom| atom.map_or(0, |atom| atoms.get(atom)));
    root_message(
        root,
        target,
        message_type,
        [
            if desired {
                EWMH_STATE_ADD
            } else {
                EWMH_STATE_REMOVE
            },
            first,
            second,
            EWMH_SOURCE_PAGER,
            0,
        ],
    )
}

pub(super) fn encode_minimize(root: Window, target: Window, message_type: Atom) -> WireRequest {
    root_message(root, target, message_type, [ICCCM_ICONIC_STATE, 0, 0, 0, 0])
}

pub(super) fn encode_move_resize(
    root: Window,
    target: Window,
    message_type: Atom,
    geometry: WindowGeometryRequest,
) -> WireRequest {
    // StaticGravity makes x/y name the root-physical client origin. The
    // live frame conversion is performed before this encoder.
    let mut flags = STATIC_GRAVITY | (EWMH_SOURCE_PAGER << 12);
    if geometry.x.is_some() {
        flags |= 1 << 8;
    }
    if geometry.y.is_some() {
        flags |= 1 << 9;
    }
    if geometry.width.is_some() {
        flags |= 1 << 10;
    }
    if geometry.height.is_some() {
        flags |= 1 << 11;
    }
    root_message(
        root,
        target,
        message_type,
        [
            flags,
            geometry.x.unwrap_or_default() as u32,
            geometry.y.unwrap_or_default() as u32,
            geometry.width.unwrap_or_default(),
            geometry.height.unwrap_or_default(),
        ],
    )
}

pub(super) fn encode_workspace(
    root: Window,
    target: Window,
    message_type: Atom,
    workspace: u32,
) -> WireRequest {
    root_message(
        root,
        target,
        message_type,
        [workspace, EWMH_SOURCE_PAGER, 0, 0, 0],
    )
}

pub(super) fn encode_current_workspace(
    root: Window,
    message_type: Atom,
    workspace: u32,
    timestamp: u32,
) -> WireRequest {
    root_message(root, root, message_type, [workspace, timestamp, 0, 0, 0])
}

pub(super) fn encode_restack(
    root: Window,
    target: Window,
    message_type: Atom,
    mode: WindowStackMode,
    sibling: Option<Window>,
) -> WireRequest {
    root_message(
        root,
        target,
        message_type,
        [
            EWMH_SOURCE_PAGER,
            sibling.unwrap_or(0),
            u32::from(x_stack_mode(mode)),
            0,
            0,
        ],
    )
}

fn x_stack_mode(mode: WindowStackMode) -> XStackMode {
    match mode {
        WindowStackMode::Raise | WindowStackMode::Above => XStackMode::ABOVE,
        WindowStackMode::Lower | WindowStackMode::Below => XStackMode::BELOW,
    }
}

fn state_atoms(state: WindowManagerState) -> [Option<KnownAtom>; 2] {
    match state {
        WindowManagerState::Maximized => [
            Some(KnownAtom::NetWmStateMaximizedVert),
            Some(KnownAtom::NetWmStateMaximizedHorz),
        ],
        WindowManagerState::Fullscreen => [Some(KnownAtom::NetWmStateFullscreen), None],
        WindowManagerState::Above => [Some(KnownAtom::NetWmStateAbove), None],
        WindowManagerState::Sticky => [Some(KnownAtom::NetWmStateSticky), None],
    }
}

fn evidence(
    request: &RawWindowControlRequest,
    outcome: RawWindowControlOutcome,
    observed: RawWindowControlObservation,
    capabilities: RawWindowManagerCapabilities,
    warnings: Vec<WindowControlWarning>,
) -> RawWindowControlEvidence {
    RawWindowControlEvidence {
        requested: request.clone(),
        outcome,
        observed,
        capabilities: Some(capabilities),
        warnings,
    }
}

fn raw_geometry_observation(
    observed: WindowGeometry,
    resolution: ResolvedWindowGeometry,
    quiet: bool,
) -> RawWindowControlObservation {
    RawWindowControlObservation::Geometry(RawWindowGeometryObservation {
        observed,
        effective: resolution.effective,
        client_request: resolution.client_request,
        bounds_constrained: resolution.bounds_constrained,
        quiet,
    })
}

fn geometry_matches_effective(
    relative_to: WindowGeometryTarget,
    observed: &WindowGeometry,
    effective: WindowRect,
) -> bool {
    (match relative_to {
        WindowGeometryTarget::Frame => observed.frame_rect,
        WindowGeometryTarget::Client => Some(observed.client_rect),
    }) == Some(effective)
}

fn activation_allows_set_input_focus(request: &RawWindowControlRequest) -> bool {
    matches!(
        request.operation,
        RawWindowControlOperation::Activate {
            allow_set_input_focus: true,
            ..
        }
    )
}

pub(super) const fn activation_request_path(
    ewmh_supported: bool,
    allow_set_input_focus: bool,
) -> ActivationRequestPath {
    if ewmh_supported {
        ActivationRequestPath::Ewmh
    } else if allow_set_input_focus {
        ActivationRequestPath::SetInputFocus
    } else {
        ActivationRequestPath::Unsupported
    }
}

fn activation_timestamp(request: &RawWindowControlRequest) -> Option<u32> {
    match request.operation {
        RawWindowControlOperation::Activate { timestamp, .. } => Some(timestamp),
        _ => None,
    }
}

fn activation_is_active_without_focus(
    request: &RawWindowControlRequest,
    observed: &RawWindowControlObservation,
) -> bool {
    matches!(
        observed,
        RawWindowControlObservation::Activation {
            active: Some(active),
            focus_within_target: false,
            ..
        } if *active == request.target
    )
}

pub(super) fn observation_satisfies(
    request: &RawWindowControlRequest,
    observed: &RawWindowControlObservation,
) -> bool {
    match (request.operation, observed) {
        (
            RawWindowControlOperation::Activate {
                switch_workspace, ..
            },
            RawWindowControlObservation::Activation {
                active,
                focus_within_target,
                current_workspace,
                ..
            },
        ) => {
            *active == Some(request.target)
                && *focus_within_target
                && switch_workspace.is_none_or(|workspace| *current_workspace == Some(workspace))
        }
        (
            RawWindowControlOperation::Close { .. },
            RawWindowControlObservation::Close { exists, viewable },
        ) => !exists || *viewable == Some(false),
        (
            RawWindowControlOperation::SetState { desired, .. }
            | RawWindowControlOperation::Minimize { desired, .. },
            RawWindowControlObservation::State(state),
        ) => matches!(
            (desired, state),
            (true, RawWindowBooleanObservation::Enabled)
                | (false, RawWindowBooleanObservation::Disabled)
        ),
        (
            RawWindowControlOperation::MoveResize { relative_to, .. },
            RawWindowControlObservation::Geometry(observed),
        ) => {
            observed.quiet
                && geometry_matches_effective(relative_to, &observed.observed, observed.effective)
        }
        (
            RawWindowControlOperation::MoveToWorkspace { workspace },
            RawWindowControlObservation::Workspace(observed),
        ) => *observed == Some(workspace),
        (
            RawWindowControlOperation::Stack {
                mode,
                sibling,
                allow_raw_fallback: _,
            },
            RawWindowControlObservation::Stacking {
                target_index,
                sibling_index,
                window_count,
            },
        ) => match (mode, sibling, target_index, sibling_index) {
            (WindowStackMode::Above, Some(_), Some(target), Some(sibling)) => target > sibling,
            (WindowStackMode::Below, Some(_), Some(target), Some(sibling)) => target < sibling,
            (WindowStackMode::Raise, None, Some(target), _) => {
                *window_count > 0 && target.saturating_add(1) == *window_count
            }
            (WindowStackMode::Lower, None, Some(target), _) => *target == 0,
            _ => false,
        },
        _ => false,
    }
}

pub(super) fn observation_is_partial(
    request: &RawWindowControlRequest,
    initial: &RawWindowControlObservation,
    final_observation: &RawWindowControlObservation,
) -> bool {
    if let (
        RawWindowControlObservation::Geometry(initial_geometry),
        RawWindowControlObservation::Geometry(final_geometry),
    ) = (initial, final_observation)
    {
        return initial_geometry.observed != final_geometry.observed
            && !observation_satisfies(request, final_observation);
    }
    matches!(
        final_observation,
        RawWindowControlObservation::State(RawWindowBooleanObservation::Partial)
    ) || (initial != final_observation && !observation_satisfies(request, final_observation))
}

fn position(windows: &[Window], target: Window) -> Option<u32> {
    windows
        .iter()
        .position(|window| *window == target)
        .and_then(|index| u32::try_from(index).ok())
}

fn exact_single<T>(values: Option<Vec<T>>) -> std::result::Result<Option<T>, BackendFault> {
    match values {
        None => Ok(None),
        Some(mut values) if values.len() == 1 => Ok(values.pop()),
        Some(_) => Err(BackendFault::MalformedWindowManagerData),
    }
}

pub(super) fn normalize_active_window(
    values: Option<Vec<Window>>,
) -> std::result::Result<Option<Window>, BackendFault> {
    // EWMH specifies one WINDOW/32 value. xfwm4 4.20 appends its last-focus
    // timestamp, so accept exactly that bounded two-field representation too.
    // The second field is focus-recency evidence, not another window XID.
    match values.as_deref() {
        None => Ok(None),
        Some([value]) | Some([value, _]) if *value != 0 => Ok(Some(*value)),
        Some([0]) | Some([0, _]) => Ok(None),
        Some(_) => Err(BackendFault::MalformedWindowManagerData),
    }
}

fn validate_decoded<T>(
    value: Option<T>,
    warnings: &[PropertyWarning],
) -> std::result::Result<Option<T>, BackendFault> {
    if warnings.is_empty() {
        Ok(value)
    } else {
        Err(BackendFault::MalformedWindowManagerData)
    }
}

fn classify_x11_error(error: X11Error) -> BackendFault {
    match error {
        X11Error::Connect(_) | X11Error::Connection(_) | X11Error::Poll(_) => {
            BackendFault::BackendUnavailable
        }
        X11Error::WorkerPanicked => BackendFault::BackendUnavailable,
        X11Error::Reply(_) => BackendFault::Refused,
        _ => BackendFault::MalformedWindowManagerData,
    }
}

fn classify_reply_error(error: ReplyError) -> BackendFault {
    match error {
        ReplyError::ConnectionError(_) => BackendFault::BackendUnavailable,
        ReplyError::X11Error(error) if error.error_kind == ErrorKind::Window => {
            BackendFault::TargetVanished
        }
        ReplyError::X11Error(_) => BackendFault::Refused,
    }
}

struct WmSupport {
    atoms: HashSet<Atom>,
    public: RawWindowManagerCapabilities,
}
