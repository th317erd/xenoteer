//! Dedicated input actor lifecycle and bounded Tokio queue handles.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc as std_mpsc};
use std::thread::{self, JoinHandle};

use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use xenoteer_core::input::{InputAction, InputHealth, PoisonReason};

use super::backend::{InputBackend, X11InputBackend};
use super::execute::{InputEngine, requested_pointer};
use super::keyboard_model::ActorKeyboardModel;
#[cfg(any(test, not(feature = "native-xkbcommon")))]
use super::keyboard_model::unavailable_keyboard_model;
use super::{
    ActionContext, ActorThreadState, ControlOutcome, InputCleanupEvidence, InputCommand,
    InputFailure, InputFailureKind, InputHealthSnapshot, InputOperation, InputOutcome,
    KeyboardAction, PointerMoveRequest,
};
use crate::{Result, X11Error};

/// Default number of admitted ordinary commands per desktop.
pub const DEFAULT_INPUT_QUEUE_CAPACITY: usize = 256;
const CONTROL_CHANNEL_CAPACITY: usize = 1;
const MAX_CONTROL_WAITERS: usize = 64;

/// Observable terminal state returned by joining the actor thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputActorExit {
    /// Explicit shutdown or final-handle drop stopped the actor.
    Stopped,
    /// The actor unwound and the thread boundary caught the panic.
    Panicked,
}

/// Failure to admit a command to the bounded actor queue.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InputSubmitError {
    /// The actor queue has no remaining capacity.
    #[error("input queue is full")]
    QueueFull,
    /// The actor is stopping or stopped.
    #[error("input actor is closed")]
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ControlKind {
    Probe,
    Reset,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlFlow {
    Continue,
    Stopped,
    Panicked,
}

type ControlReply = oneshot::Sender<std::result::Result<ControlOutcome, InputFailure>>;

struct ControlWaiter {
    requested: ControlKind,
    reply: ControlReply,
}

#[derive(Default)]
struct PendingControl {
    kind: Option<ControlKind>,
    waiters: Vec<ControlWaiter>,
    closed: bool,
}

struct SharedControl {
    pending: Mutex<PendingControl>,
    wake: mpsc::Sender<()>,
}

impl SharedControl {
    fn enqueue(&self, requested: ControlKind, reply: ControlReply) {
        let mut rejected = None;
        {
            let mut pending = lock_mutex(&self.pending);
            if pending.closed {
                rejected = Some((reply, InputFailureKind::ActorStopped));
            } else if pending.waiters.len() >= MAX_CONTROL_WAITERS {
                if let Some(index) = pending
                    .waiters
                    .iter()
                    .position(|waiter| waiter.requested < requested)
                {
                    let evicted = pending.waiters.remove(index);
                    let _ignored = evicted
                        .reply
                        .send(Err(actor_failure(InputFailureKind::ControlQueueFull)));
                    pending.kind = Some(pending.kind.map_or(requested, |kind| kind.max(requested)));
                    pending.waiters.push(ControlWaiter { requested, reply });
                } else {
                    rejected = Some((reply, InputFailureKind::ControlQueueFull));
                }
            } else {
                pending.kind = Some(pending.kind.map_or(requested, |kind| kind.max(requested)));
                pending.waiters.push(ControlWaiter { requested, reply });
            }
        }
        if let Some((reply, kind)) = rejected {
            let _ignored = reply.send(Err(actor_failure(kind)));
            return;
        }
        match self.wake.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
            Err(mpsc::error::TrySendError::Closed(())) => {
                let mut pending = lock_mutex(&self.pending);
                fail_control_waiters(&mut pending.waiters, InputFailureKind::ActorStopped);
                pending.kind = None;
            }
        }
    }

    fn take(&self) -> Option<(ControlKind, Vec<ControlWaiter>)> {
        let mut pending = lock_mutex(&self.pending);
        pending
            .kind
            .take()
            .map(|kind| (kind, std::mem::take(&mut pending.waiters)))
    }

    fn close(&self, kind: InputFailureKind) {
        let mut pending = lock_mutex(&self.pending);
        pending.closed = true;
        pending.kind = None;
        fail_control_waiters(&mut pending.waiters, kind);
    }
}

/// Cloneable admission and control handle for one actor.
#[derive(Clone)]
pub struct InputActorHandle {
    ordinary: mpsc::Sender<InputCommand>,
    control: Arc<SharedControl>,
    health: Arc<RwLock<InputHealthSnapshot>>,
    accepting: Arc<AtomicBool>,
}

impl InputActorHandle {
    /// Attempts immediate FIFO admission of an execution-time planned move.
    pub fn try_submit_pointer_move(
        &self,
        context: ActionContext,
        request: PointerMoveRequest,
        cancellation: CancellationToken,
    ) -> std::result::Result<
        oneshot::Receiver<std::result::Result<InputOutcome, InputFailure>>,
        InputSubmitError,
    > {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(InputSubmitError::Closed);
        }
        let (reply, receiver) = oneshot::channel();
        self.ordinary
            .try_send(InputCommand {
                context,
                operation: InputOperation::PointerMove(request),
                cancellation,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => InputSubmitError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => InputSubmitError::Closed,
            })?;
        Ok(receiver)
    }

    /// Attempts immediate FIFO admission without waiting for queue capacity.
    pub fn try_submit(
        &self,
        context: ActionContext,
        action: InputAction,
        cancellation: CancellationToken,
    ) -> std::result::Result<
        oneshot::Receiver<std::result::Result<InputOutcome, InputFailure>>,
        InputSubmitError,
    > {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(InputSubmitError::Closed);
        }
        let (reply, receiver) = oneshot::channel();
        let command = InputCommand {
            context,
            operation: InputOperation::Pointer(action),
            cancellation,
            reply,
        };
        self.ordinary
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => InputSubmitError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => InputSubmitError::Closed,
            })?;
        Ok(receiver)
    }

    /// Attempts immediate FIFO admission of an unresolved keyboard action.
    pub fn try_submit_keyboard(
        &self,
        context: ActionContext,
        action: KeyboardAction,
        cancellation: CancellationToken,
    ) -> std::result::Result<
        oneshot::Receiver<std::result::Result<InputOutcome, InputFailure>>,
        InputSubmitError,
    > {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(InputSubmitError::Closed);
        }
        let (reply, receiver) = oneshot::channel();
        self.ordinary
            .try_send(InputCommand {
                context,
                operation: InputOperation::Keyboard(action),
                cancellation,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => InputSubmitError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => InputSubmitError::Closed,
            })?;
        Ok(receiver)
    }

    /// Coalesces a liveness probe on the independently bounded control lane.
    #[must_use]
    pub fn probe(&self) -> oneshot::Receiver<std::result::Result<ControlOutcome, InputFailure>> {
        self.enqueue_control(ControlKind::Probe)
    }

    /// Coalesces conservative owned-input reset on the control lane.
    #[must_use]
    pub fn reset(&self) -> oneshot::Receiver<std::result::Result<ControlOutcome, InputFailure>> {
        self.enqueue_control(ControlKind::Reset)
    }

    /// Stops ordinary admission immediately and delivers shutdown even when full.
    #[must_use]
    pub fn shutdown(&self) -> oneshot::Receiver<std::result::Result<ControlOutcome, InputFailure>> {
        self.accepting.store(false, Ordering::Release);
        self.enqueue_control(ControlKind::Shutdown)
    }

    /// Returns the latest actor-owned health snapshot.
    #[must_use]
    pub fn health(&self) -> InputHealthSnapshot {
        read_lock(&self.health).clone()
    }

    /// Returns remaining ordinary queue capacity at this instant.
    #[must_use]
    pub fn remaining_capacity(&self) -> usize {
        self.ordinary.capacity()
    }

    fn enqueue_control(
        &self,
        kind: ControlKind,
    ) -> oneshot::Receiver<std::result::Result<ControlOutcome, InputFailure>> {
        let (reply, receiver) = oneshot::channel();
        if !self.accepting.load(Ordering::Acquire) && kind != ControlKind::Shutdown {
            let _ignored = reply.send(Err(actor_failure(InputFailureKind::ActorStopped)));
            return receiver;
        }
        self.control.enqueue(kind, reply);
        receiver
    }
}

/// Owned join capability; dropping it never detaches another hidden restart.
pub struct InputActorJoin {
    thread: Option<JoinHandle<InputActorExit>>,
    control: Arc<SharedControl>,
    accepting: Arc<AtomicBool>,
}

impl InputActorJoin {
    /// Joins the dedicated actor OS thread and reports caught panic state.
    ///
    /// This blocks until explicit shutdown completes or all admission handles
    /// are dropped and their channels close.
    pub fn join(mut self) -> InputActorExit {
        let Some(thread) = self.thread.take() else {
            return InputActorExit::Stopped;
        };
        match thread.join() {
            Ok(exit) => exit,
            Err(_) => InputActorExit::Panicked,
        }
    }
}

impl Drop for InputActorJoin {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.accepting.store(false, Ordering::Release);
        let (reply, _receiver) = oneshot::channel();
        self.control.enqueue(ControlKind::Shutdown, reply);
        let _exit = thread.join();
    }
}

/// Starts an actor whose dedicated thread creates and exclusively owns X11.
pub fn spawn_input_actor(display: &str) -> Result<(InputActorHandle, InputActorJoin)> {
    let display = display.to_owned();
    #[cfg(feature = "native-xkbcommon")]
    {
        spawn_with_components(DEFAULT_INPUT_QUEUE_CAPACITY, move || {
            let backend = X11InputBackend::open(&display)?;
            let keyboard = super::keyboard_model::NativeActorKeyboardModel::connect(&display)?;
            Ok((backend, Box::new(keyboard)))
        })
    }
    #[cfg(not(feature = "native-xkbcommon"))]
    {
        spawn_with_backend(DEFAULT_INPUT_QUEUE_CAPACITY, move || {
            X11InputBackend::open(&display)
        })
    }
}

#[cfg(any(test, not(feature = "native-xkbcommon")))]
pub(super) fn spawn_with_backend<B, F>(
    capacity: usize,
    factory: F,
) -> Result<(InputActorHandle, InputActorJoin)>
where
    B: InputBackend,
    F: FnOnce() -> std::result::Result<B, super::backend::BackendFault> + Send + 'static,
{
    spawn_with_components(capacity, move || {
        factory().map(|backend| (backend, unavailable_keyboard_model()))
    })
}

pub(super) fn spawn_with_components<B, F>(
    capacity: usize,
    factory: F,
) -> Result<(InputActorHandle, InputActorJoin)>
where
    B: InputBackend,
    F: FnOnce() -> std::result::Result<
            (B, Box<dyn ActorKeyboardModel>),
            super::backend::BackendFault,
        > + Send
        + 'static,
{
    if capacity == 0 {
        return Err(X11Error::InvalidSetup(
            "input queue capacity must be positive",
        ));
    }
    let (ordinary_tx, ordinary_rx) = mpsc::channel(capacity);
    let (control_tx, control_rx) = mpsc::channel(CONTROL_CHANNEL_CAPACITY);
    let control = Arc::new(SharedControl {
        pending: Mutex::new(PendingControl::default()),
        wake: control_tx,
    });
    let health = Arc::new(RwLock::new(InputHealthSnapshot {
        input: InputHealth::Healthy,
        thread: ActorThreadState::Starting,
        button_mapping: None,
        min_keycode: 0,
        max_keycode: 0,
        keyboard_model: super::KeyboardModelDiagnostics {
            availability: crate::keyboard::availability(),
            generation: None,
            keymap_fingerprint: None,
        },
    }));
    let accepting = Arc::new(AtomicBool::new(true));
    let (startup_tx, startup_rx) = std_mpsc::channel();
    let thread_health = Arc::clone(&health);
    let thread_control = Arc::clone(&control);
    let thread_accepting = Arc::clone(&accepting);
    let thread = thread::Builder::new()
        .name("xenoteer-input-actor".to_owned())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let (backend, keyboard) = match factory() {
                    Ok(components) => components,
                    Err(_) => {
                        let _ignored = startup_tx.send(false);
                        return InputActorExit::Stopped;
                    }
                };
                let mut engine = match InputEngine::new_with_keyboard(backend, keyboard) {
                    Ok(engine) => engine,
                    Err(_) => {
                        let _ignored = startup_tx.send(false);
                        return InputActorExit::Stopped;
                    }
                };
                let runtime = match Builder::new_current_thread().build() {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        let _ignored = startup_tx.send(false);
                        return InputActorExit::Stopped;
                    }
                };
                engine.publish_health(&thread_health, ActorThreadState::Running);
                let _ignored = startup_tx.send(true);
                runtime.block_on(run_actor(
                    &mut engine,
                    ordinary_rx,
                    control_rx,
                    &thread_control,
                    &thread_health,
                    &thread_accepting,
                ))
            }));
            match result {
                Ok(exit) => {
                    thread_accepting.store(false, Ordering::Release);
                    let (failure, thread_state) = match exit {
                        InputActorExit::Stopped => {
                            (InputFailureKind::ActorStopped, ActorThreadState::Stopped)
                        }
                        InputActorExit::Panicked => {
                            (InputFailureKind::ActorPanicked, ActorThreadState::Panicked)
                        }
                    };
                    thread_control.close(failure);
                    let mut snapshot = write_lock(&thread_health);
                    snapshot.thread = thread_state;
                    if exit == InputActorExit::Panicked {
                        snapshot.input = InputHealth::Poisoned(PoisonReason::ActorPanicked);
                    }
                    exit
                }
                Err(_) => {
                    thread_accepting.store(false, Ordering::Release);
                    thread_control.close(InputFailureKind::ActorPanicked);
                    let mut snapshot = write_lock(&thread_health);
                    snapshot.thread = ActorThreadState::Panicked;
                    snapshot.input = InputHealth::Poisoned(PoisonReason::ActorPanicked);
                    InputActorExit::Panicked
                }
            }
        })
        .map_err(|error| X11Error::Poll(error.to_string()))?;

    match startup_rx.recv() {
        Ok(true) => Ok((
            InputActorHandle {
                ordinary: ordinary_tx,
                control: Arc::clone(&control),
                health,
                accepting: Arc::clone(&accepting),
            },
            InputActorJoin {
                thread: Some(thread),
                control,
                accepting,
            },
        )),
        Ok(false) | Err(_) => {
            let _exit = thread.join();
            Err(X11Error::InvalidSetup("input actor startup failed"))
        }
    }
}

async fn run_actor<B: InputBackend>(
    engine: &mut InputEngine<B>,
    mut ordinary: mpsc::Receiver<InputCommand>,
    mut control_wake: mpsc::Receiver<()>,
    control: &SharedControl,
    health: &RwLock<InputHealthSnapshot>,
    accepting: &AtomicBool,
) -> InputActorExit {
    loop {
        if let Some((kind, waiters)) = control.take() {
            match process_control(engine, kind, waiters, health, accepting) {
                ControlFlow::Continue => {}
                ControlFlow::Stopped => {
                    ordinary.close();
                    reject_queued(&mut ordinary, InputFailureKind::ActorStopped);
                    control.close(InputFailureKind::ActorStopped);
                    return InputActorExit::Stopped;
                }
                ControlFlow::Panicked => {
                    ordinary.close();
                    reject_queued(&mut ordinary, InputFailureKind::ActorPanicked);
                    control.close(InputFailureKind::ActorPanicked);
                    return InputActorExit::Panicked;
                }
            }
        }
        tokio::select! {
            biased;
            wake = control_wake.recv() => {
                if wake.is_none() && ordinary.is_closed() {
                    let _report = engine.reset_owned_input();
                    let exit = if engine.actor_panicked() {
                        InputActorExit::Panicked
                    } else {
                        InputActorExit::Stopped
                    };
                    control.close(match exit {
                        InputActorExit::Stopped => InputFailureKind::ActorStopped,
                        InputActorExit::Panicked => InputFailureKind::ActorPanicked,
                    });
                    return exit;
                }
            }
            command = ordinary.recv() => {
                match command {
                    Some(command) => {
                        let InputCommand { context, operation, cancellation, reply } = command;
                        let requested_pointer = requested_pointer(&operation);
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            engine.execute_operation(context, operation, &cancellation)
                        }));
                        match result {
                            Ok(result) => {
                                if engine.actor_panicked() {
                                    accepting.store(false, Ordering::Release);
                                    let _cleanup = catch_unwind(AssertUnwindSafe(|| {
                                        engine.emergency_cleanup_after_panic()
                                    }));
                                    let _ignored = reply.send(result);
                                    ordinary.close();
                                    reject_queued(&mut ordinary, InputFailureKind::ActorPanicked);
                                    control.close(InputFailureKind::ActorPanicked);
                                    engine.publish_health(health, ActorThreadState::Panicked);
                                    return InputActorExit::Panicked;
                                }
                                let _ignored = reply.send(result);
                                engine.publish_health(health, ActorThreadState::Running);
                            }
                            Err(_) => {
                                accepting.store(false, Ordering::Release);
                                let _cleanup = catch_unwind(AssertUnwindSafe(|| {
                                    engine.emergency_cleanup_after_panic()
                                }));
                                engine.mark_panicked();
                                let _ignored = reply.send(Err(active_panic_failure(
                                    context,
                                    requested_pointer,
                                )));
                                ordinary.close();
                                reject_queued(&mut ordinary, InputFailureKind::ActorPanicked);
                                control.close(InputFailureKind::ActorPanicked);
                                engine.publish_health(health, ActorThreadState::Panicked);
                                return InputActorExit::Panicked;
                            }
                        }
                    }
                    None => {
                        accepting.store(false, Ordering::Release);
                        let _report = engine.reset_owned_input();
                        let exit = if engine.actor_panicked() {
                            InputActorExit::Panicked
                        } else {
                            InputActorExit::Stopped
                        };
                        control.close(match exit {
                            InputActorExit::Stopped => InputFailureKind::ActorStopped,
                            InputActorExit::Panicked => InputFailureKind::ActorPanicked,
                        });
                        return exit;
                    }
                }
            }
        }
    }
}

fn process_control<B: InputBackend>(
    engine: &mut InputEngine<B>,
    kind: ControlKind,
    waiters: Vec<ControlWaiter>,
    health: &RwLock<InputHealthSnapshot>,
    accepting: &AtomicBool,
) -> ControlFlow {
    let operation = catch_unwind(AssertUnwindSafe(|| match kind {
        ControlKind::Probe => engine.probe().map(ControlOutcome::Probe),
        ControlKind::Reset => engine
            .reset_owned_input()
            .map(InputCleanupEvidence::from_control_report)
            .map(ControlOutcome::Reset),
        ControlKind::Shutdown => {
            accepting.store(false, Ordering::Release);
            engine
                .reset_owned_input()
                .map(InputCleanupEvidence::from_control_report)
                .map(ControlOutcome::Shutdown)
        }
    }));
    let result = match operation {
        Ok(result) => result,
        Err(_) => {
            accepting.store(false, Ordering::Release);
            let _cleanup =
                catch_unwind(AssertUnwindSafe(|| engine.emergency_cleanup_after_panic()));
            engine.mark_panicked();
            engine.publish_health(health, ActorThreadState::Panicked);
            for waiter in waiters {
                let _ignored = waiter
                    .reply
                    .send(Err(actor_failure(InputFailureKind::ActorPanicked)));
            }
            return ControlFlow::Panicked;
        }
    };
    if engine.actor_panicked() {
        accepting.store(false, Ordering::Release);
        let _cleanup = catch_unwind(AssertUnwindSafe(|| engine.emergency_cleanup_after_panic()));
        engine.publish_health(health, ActorThreadState::Panicked);
        let failure = match result {
            Err(failure) => failure,
            Ok(_) => actor_failure(InputFailureKind::ActorPanicked),
        };
        for waiter in waiters {
            let _ignored = waiter.reply.send(Err(failure.clone()));
        }
        return ControlFlow::Panicked;
    }
    engine.publish_health(
        health,
        if kind == ControlKind::Shutdown {
            ActorThreadState::Stopped
        } else {
            ActorThreadState::Running
        },
    );
    for waiter in waiters {
        let waiter_result = match (&result, waiter.requested) {
            (Ok(_), ControlKind::Probe) => Ok(ControlOutcome::Probe(read_lock(health).clone())),
            (
                Ok(ControlOutcome::Reset(report) | ControlOutcome::Shutdown(report)),
                ControlKind::Reset,
            ) => Ok(ControlOutcome::Reset(report.clone())),
            (Ok(ControlOutcome::Shutdown(report)), ControlKind::Shutdown) => {
                Ok(ControlOutcome::Shutdown(report.clone()))
            }
            (Ok(value), _) => Ok(value.clone()),
            (Err(error), _) => Err(error.clone()),
        };
        let _ignored = waiter.reply.send(waiter_result);
    }
    if kind == ControlKind::Shutdown {
        ControlFlow::Stopped
    } else {
        ControlFlow::Continue
    }
}

fn reject_queued(ordinary: &mut mpsc::Receiver<InputCommand>, kind: InputFailureKind) {
    while let Ok(command) = ordinary.try_recv() {
        let requested_pointer = requested_pointer(&command.operation);
        let _ignored = command.reply.send(Err(command_failure(
            command.context,
            kind,
            requested_pointer,
        )));
    }
}

fn fail_control_waiters(waiters: &mut Vec<ControlWaiter>, kind: InputFailureKind) {
    for waiter in waiters.drain(..) {
        let _ignored = waiter.reply.send(Err(actor_failure(kind)));
    }
}

fn actor_failure(kind: InputFailureKind) -> InputFailure {
    InputFailure {
        command_id: None,
        kind,
        events_emitted: 0,
        completed_units: 0,
        progress_known: kind != InputFailureKind::ActorPanicked,
        requested_pointer: None,
        last_observed_pointer: None,
        observed_logical_buttons_1_to_5: None,
        button_observation_partial: false,
        effects: None,
        cleanup: None,
        keyboard: None,
    }
}

fn command_failure(
    context: ActionContext,
    kind: InputFailureKind,
    requested_pointer: Option<xenoteer_core::domain::RootPoint>,
) -> InputFailure {
    InputFailure {
        command_id: Some(context.command_id),
        kind,
        events_emitted: 0,
        completed_units: 0,
        progress_known: true,
        requested_pointer,
        last_observed_pointer: None,
        observed_logical_buttons_1_to_5: None,
        button_observation_partial: false,
        effects: None,
        cleanup: None,
        keyboard: None,
    }
}

fn active_panic_failure(
    context: ActionContext,
    requested_pointer: Option<xenoteer_core::domain::RootPoint>,
) -> InputFailure {
    let mut failure = command_failure(context, InputFailureKind::ActorPanicked, requested_pointer);
    failure.progress_known = false;
    failure
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(super) use spawn_with_backend as spawn_test_actor;

#[cfg(test)]
pub(super) use spawn_with_components as spawn_test_actor_with_keyboard;
