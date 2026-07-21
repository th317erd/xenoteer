//! Wakeable, bounded, single-owner X11 event polling adapter.
//!
//! x11rb may buffer events while processing replies, so sharing its file
//! descriptor with other connection users risks missed readiness. This adapter
//! gives the connection and buffered queue one owner. A bounded output queue
//! never blocks that owner: overflow drops/coalesces events behind exactly one
//! `ResyncRequired` marker.

use std::io;
use std::os::fd::AsRawFd;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvError, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::{Result, X11Error};

const X11_TOKEN: Token = Token(0);
const WAKE_TOKEN: Token = Token(1);
const POLL_BACKSTOP: Duration = Duration::from_millis(100);

/// Maximum normalized events retained between the worker and model owner.
pub const OBSERVATION_EVENT_CAPACITY: usize = 256;

/// Small normalized event set used by the platform spike and recorder tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PollThreadEvent {
    /// A child was created.
    Create {
        /// Created child XID.
        window: u32,
    },
    /// A child was mapped.
    Map {
        /// Mapped child XID.
        window: u32,
    },
    /// A child was unmapped.
    Unmap {
        /// Unmapped child XID.
        window: u32,
    },
    /// A child was destroyed.
    Destroy {
        /// Destroyed child XID.
        window: u32,
    },
    /// Pointer motion was delivered.
    Motion {
        /// Event window XID.
        window: u32,
        /// Root X coordinate.
        root_x: i16,
        /// Root Y coordinate.
        root_y: i16,
    },
    /// An event outside this spike's normalized subset was observed.
    Other {
        /// Raw core/extension response type.
        response_type: u8,
    },
    /// One or more events were dropped; rebuild observation state from X11.
    ResyncRequired,
    /// The worker stopped because the X connection failed.
    Failed {
        /// Bounded diagnostic message.
        message: String,
    },
}

trait WakeSignal: Send + Sync {
    fn wake(&self) -> io::Result<()>;
}

impl WakeSignal for Waker {
    fn wake(&self) -> io::Result<()> {
        Waker::wake(self)
    }
}

/// Receiver whose drop wakes the worker so a disconnected consumer cannot
/// leave an observation thread running.
pub struct ObservationEventReceiver {
    receiver: Receiver<PollThreadEvent>,
    receiver_alive: Arc<AtomicBool>,
    waker: Arc<dyn WakeSignal>,
}

impl ObservationEventReceiver {
    /// Wait indefinitely for the next normalized event.
    pub fn recv(&self) -> std::result::Result<PollThreadEvent, RecvError> {
        self.receiver.recv()
    }

    /// Wait up to `timeout` for the next normalized event.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<PollThreadEvent, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }

    /// Attempt to receive without blocking.
    pub fn try_recv(&self) -> std::result::Result<PollThreadEvent, TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for ObservationEventReceiver {
    fn drop(&mut self) {
        self.receiver_alive.store(false, Ordering::Release);
        let _ignored = self.waker.wake();
    }
}

/// Factory for the dedicated poll-thread fallback selected by the Phase 0 spike.
pub struct ObservationPollThread;

impl ObservationPollThread {
    /// Move a connection into one named worker thread and return its lifecycle
    /// handle and bounded normalized event receiver.
    pub fn spawn(
        connection: RustConnection,
    ) -> Result<(ObservationPollHandle, ObservationEventReceiver)> {
        let mut poll = Poll::new().map_err(|error| X11Error::Poll(error.to_string()))?;
        let raw_fd = connection.stream().as_raw_fd();
        poll.registry()
            .register(&mut SourceFd(&raw_fd), X11_TOKEN, Interest::READABLE)
            .map_err(|error| X11Error::Poll(error.to_string()))?;
        let waker: Arc<dyn WakeSignal> = Arc::new(
            Waker::new(poll.registry(), WAKE_TOKEN)
                .map_err(|error| X11Error::Poll(error.to_string()))?,
        );
        let running = Arc::new(AtomicBool::new(true));
        let receiver_alive = Arc::new(AtomicBool::new(true));
        let (event_sender, event_receiver) = mpsc::sync_channel(OBSERVATION_EVENT_CAPACITY);
        let worker_running = Arc::clone(&running);
        let worker_receiver_alive = Arc::clone(&receiver_alive);
        let join = thread::Builder::new()
            .name("xenoteer-x11-observation-poll".to_owned())
            .spawn(move || {
                run_poll_loop(
                    &mut poll,
                    &connection,
                    &worker_running,
                    &worker_receiver_alive,
                    event_sender,
                );
            })
            .map_err(|error| X11Error::Poll(error.to_string()))?;
        Ok((
            ObservationPollHandle {
                running,
                waker: Arc::clone(&waker),
                join: Some(join),
            },
            ObservationEventReceiver {
                receiver: event_receiver,
                receiver_alive,
                waker,
            },
        ))
    }
}

/// Shutdown and join handle for an observation poll worker.
pub struct ObservationPollHandle {
    running: Arc<AtomicBool>,
    waker: Arc<dyn WakeSignal>,
    join: Option<JoinHandle<()>>,
}

impl ObservationPollHandle {
    /// Stop, wake, and join the worker. A wake failure is returned only after
    /// the bounded poll backstop allowed the worker to exit and be joined.
    pub fn shutdown(mut self) -> Result<()> {
        let wake_result = self.request_stop();
        let join_result = self.join_worker();
        join_result?;
        wake_result
    }

    fn request_stop(&self) -> Result<()> {
        self.running.store(false, Ordering::Release);
        self.waker
            .wake()
            .map_err(|error| X11Error::Poll(error.to_string()))
    }

    fn join_worker(&mut self) -> Result<()> {
        if let Some(join) = self.join.take() {
            join.join().map_err(|_| X11Error::WorkerPanicked)?;
        }
        Ok(())
    }
}

impl Drop for ObservationPollHandle {
    fn drop(&mut self) {
        let _ignored = self.request_stop();
        let _ignored = self.join_worker();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryState {
    Continue,
    ReceiverDisconnected,
}

struct EventEmitter {
    sender: SyncSender<PollThreadEvent>,
    resync_latched: bool,
}

impl EventEmitter {
    const fn new(sender: SyncSender<PollThreadEvent>) -> Self {
        Self {
            sender,
            resync_latched: false,
        }
    }

    fn offer(&mut self, event: PollThreadEvent) -> DeliveryState {
        if self.resync_latched {
            match self.flush_resync() {
                DeliveryState::ReceiverDisconnected => return DeliveryState::ReceiverDisconnected,
                DeliveryState::Continue if self.resync_latched => return DeliveryState::Continue,
                DeliveryState::Continue => {}
            }
        }
        match self.sender.try_send(event) {
            Ok(()) => DeliveryState::Continue,
            Err(TrySendError::Full(_)) => {
                self.resync_latched = true;
                DeliveryState::Continue
            }
            Err(TrySendError::Disconnected(_)) => DeliveryState::ReceiverDisconnected,
        }
    }

    fn flush_resync(&mut self) -> DeliveryState {
        if !self.resync_latched {
            return DeliveryState::Continue;
        }
        match self.sender.try_send(PollThreadEvent::ResyncRequired) {
            Ok(()) => {
                self.resync_latched = false;
                DeliveryState::Continue
            }
            Err(TrySendError::Full(_)) => DeliveryState::Continue,
            Err(TrySendError::Disconnected(_)) => DeliveryState::ReceiverDisconnected,
        }
    }
}

fn run_poll_loop(
    poll: &mut Poll,
    connection: &RustConnection,
    running: &AtomicBool,
    receiver_alive: &AtomicBool,
    sender: SyncSender<PollThreadEvent>,
) {
    let mut emitter = EventEmitter::new(sender);
    let mut events = Events::with_capacity(16);
    while running.load(Ordering::Acquire) && receiver_alive.load(Ordering::Acquire) {
        match drain_x_events(connection, &mut emitter, running, receiver_alive) {
            Ok(DeliveryState::Continue) => {}
            Ok(DeliveryState::ReceiverDisconnected) => break,
            Err(error) => {
                let _ignored = emitter.offer(PollThreadEvent::Failed {
                    message: error.to_string(),
                });
                break;
            }
        }
        if emitter.flush_resync() == DeliveryState::ReceiverDisconnected {
            break;
        }
        if !running.load(Ordering::Acquire) || !receiver_alive.load(Ordering::Acquire) {
            break;
        }
        if let Err(error) = poll.poll(&mut events, Some(POLL_BACKSTOP)) {
            let _ignored = emitter.offer(PollThreadEvent::Failed {
                message: error.to_string(),
            });
            break;
        }
        if events.iter().any(|event| event.token() == WAKE_TOKEN)
            && (!running.load(Ordering::Acquire) || !receiver_alive.load(Ordering::Acquire))
        {
            break;
        }
    }
}

fn drain_x_events(
    connection: &RustConnection,
    emitter: &mut EventEmitter,
    running: &AtomicBool,
    receiver_alive: &AtomicBool,
) -> Result<DeliveryState> {
    while running.load(Ordering::Acquire) && receiver_alive.load(Ordering::Acquire) {
        let Some(event) = connection
            .poll_for_event()
            .map_err(|error| X11Error::Connection(error.to_string()))?
        else {
            break;
        };
        if emitter.offer(normalize_event(event)) == DeliveryState::ReceiverDisconnected {
            return Ok(DeliveryState::ReceiverDisconnected);
        }
    }
    Ok(DeliveryState::Continue)
}

fn normalize_event(event: Event) -> PollThreadEvent {
    match event {
        Event::CreateNotify(event) => PollThreadEvent::Create {
            window: event.window,
        },
        Event::MapNotify(event) => PollThreadEvent::Map {
            window: event.window,
        },
        Event::UnmapNotify(event) => PollThreadEvent::Unmap {
            window: event.window,
        },
        Event::DestroyNotify(event) => PollThreadEvent::Destroy {
            window: event.window,
        },
        Event::MotionNotify(event) => PollThreadEvent::Motion {
            window: event.event,
            root_x: event.root_x,
            root_y: event.root_y,
        },
        other => PollThreadEvent::Other {
            response_type: other.response_type(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, TryRecvError},
    };
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        DeliveryState, EventEmitter, ObservationEventReceiver, ObservationPollHandle,
        PollThreadEvent, WakeSignal,
    };
    use crate::X11Error;

    struct TestWake {
        fail: bool,
    }

    impl WakeSignal for TestWake {
        fn wake(&self) -> std::io::Result<()> {
            if self.fail {
                Err(std::io::Error::other("injected wake failure"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn flood_never_blocks_and_emits_exactly_one_resync_marker() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let mut emitter = EventEmitter::new(sender);
        let start = Instant::now();
        for window in 0..10_000 {
            assert_eq!(
                emitter.offer(PollThreadEvent::Create { window }),
                DeliveryState::Continue
            );
        }
        assert!(start.elapsed() < Duration::from_secs(1));

        assert!(matches!(
            receiver.try_recv(),
            Ok(PollThreadEvent::Create { .. })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(PollThreadEvent::Create { .. })
        ));
        assert_eq!(emitter.flush_resync(), DeliveryState::Continue);
        assert_eq!(receiver.try_recv(), Ok(PollThreadEvent::ResyncRequired));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn disconnected_receiver_stops_delivery() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let mut emitter = EventEmitter::new(sender);
        assert_eq!(
            emitter.offer(PollThreadEvent::Other { response_type: 1 }),
            DeliveryState::ReceiverDisconnected
        );
    }

    #[test]
    fn observation_receiver_drop_signals_worker_termination() {
        let running = Arc::new(AtomicBool::new(true));
        let receiver_alive = Arc::new(AtomicBool::new(true));
        let joined = Arc::new(AtomicBool::new(false));
        let worker_receiver_alive = Arc::clone(&receiver_alive);
        let worker_joined = Arc::clone(&joined);
        let join = thread::spawn(move || {
            while worker_receiver_alive.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            worker_joined.store(true, Ordering::Release);
        });
        let wake: Arc<dyn WakeSignal> = Arc::new(TestWake { fail: false });
        let (_sender, inner_receiver) = mpsc::sync_channel(1);
        let receiver = ObservationEventReceiver {
            receiver: inner_receiver,
            receiver_alive,
            waker: Arc::clone(&wake),
        };
        let handle = ObservationPollHandle {
            running,
            waker: wake,
            join: Some(join),
        };

        drop(receiver);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !joined.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(joined.load(Ordering::Acquire));
        drop(handle);
    }

    #[test]
    fn explicit_shutdown_is_quick_and_joins() -> Result<(), X11Error> {
        let joined = Arc::new(AtomicBool::new(false));
        let handle = test_handle(false, Arc::clone(&joined));
        let start = Instant::now();
        handle.shutdown()?;
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(joined.load(Ordering::Acquire));
        Ok(())
    }

    #[test]
    fn handle_drop_stops_and_joins_instead_of_detaching() {
        let joined = Arc::new(AtomicBool::new(false));
        let handle = test_handle(false, Arc::clone(&joined));
        drop(handle);
        assert!(joined.load(Ordering::Acquire));
    }

    #[test]
    fn injected_wake_failure_is_returned_only_after_join() {
        let joined = Arc::new(AtomicBool::new(false));
        let handle = test_handle(true, Arc::clone(&joined));
        assert!(matches!(handle.shutdown(), Err(X11Error::Poll(_))));
        assert!(joined.load(Ordering::Acquire));
    }

    fn test_handle(fail_wake: bool, joined: Arc<AtomicBool>) -> ObservationPollHandle {
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = Arc::clone(&running);
        let join = thread::spawn(move || {
            while worker_running.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            joined.store(true, Ordering::Release);
        });
        ObservationPollHandle {
            running,
            waker: Arc::new(TestWake { fail: fail_wake }),
            join: Some(join),
        }
    }
}
