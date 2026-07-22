//! Bounded global event sequencing and replay retention.

use std::collections::VecDeque;

use xenoteer_protocol::DesktopId;

use super::GenerationToken;

/// Default number of normalized events retained for reconnect replay.
pub const DEFAULT_REPLAY_EVENT_LIMIT: usize = 10_000;

/// Default aggregate encoded replay-buffer size.
pub const DEFAULT_REPLAY_BYTE_LIMIT: usize = 16 * 1024 * 1024;

/// Count and encoded-byte bounds for replay retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventHubLimits {
    maximum_events: usize,
    maximum_encoded_bytes: usize,
}

impl EventHubLimits {
    /// Creates non-zero event-count and encoded-byte limits.
    pub fn new(maximum_events: usize, maximum_encoded_bytes: usize) -> Result<Self, EventHubError> {
        if maximum_events == 0 || maximum_encoded_bytes == 0 {
            return Err(EventHubError::InvalidLimits);
        }
        Ok(Self {
            maximum_events,
            maximum_encoded_bytes,
        })
    }

    /// Returns the retained event-count limit.
    #[must_use]
    pub const fn maximum_events(self) -> usize {
        self.maximum_events
    }

    /// Returns the aggregate encoded-byte limit.
    #[must_use]
    pub const fn maximum_encoded_bytes(self) -> usize {
        self.maximum_encoded_bytes
    }
}

impl Default for EventHubLimits {
    fn default() -> Self {
        Self {
            maximum_events: DEFAULT_REPLAY_EVENT_LIMIT,
            maximum_encoded_bytes: DEFAULT_REPLAY_BYTE_LIMIT,
        }
    }
}

/// A normalized event with its globally assigned sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord<E> {
    /// Desktop lifetime in which the event occurred.
    pub generation: GenerationToken,
    /// Global sequence assigned before subscriber filtering.
    pub sequence: u64,
    /// Exact encoded envelope size charged to the replay budget.
    pub encoded_size: usize,
    /// Normalized event data.
    pub event: E,
}

/// Effects of publishing one normalized event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishOutcome {
    /// Globally assigned sequence, whether or not the event fit replay retention.
    pub sequence: u64,
    /// Whether reconnect replay retains this event.
    pub retained: bool,
    /// Number of older retained events evicted by this publication.
    pub evicted_events: usize,
    /// Highest sequence known to be absent from otherwise-current replay history.
    pub dropped_through: u64,
}

/// Why a reconnect cannot be satisfied safely from retained history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayFailure {
    /// The requested generation is not the active desktop lifetime.
    GenerationChanged,
    /// At least one required event has already left replay retention.
    HistoryLost,
    /// The requested sequence is newer than any sequence assigned by this hub.
    SequenceAhead,
}

/// A replay query result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayResult<E> {
    /// A complete suffix after `since_sequence`; an empty suffix is valid.
    Events {
        /// Caller-supplied exclusive lower bound.
        since_sequence: u64,
        /// Latest sequence assigned in this generation.
        latest_sequence: u64,
        /// Complete retained suffix, before per-subscriber filtering.
        events: Vec<EventRecord<E>>,
    },
    /// The client must fetch authoritative snapshots before resubscribing.
    ResyncRequired {
        /// Specific unsafe-replay condition.
        reason: ReplayFailure,
        /// Current authoritative desktop lifetime.
        current_generation: GenerationToken,
        /// Highest sequence that is known not to be replayable.
        dropped_through: u64,
        /// Latest assigned sequence in the current generation.
        latest_sequence: u64,
    },
}

/// Assigns a global sequence and retains a bounded replay suffix.
///
/// Subscriber filtering and queue coalescing happen after this component. This
/// preserves meaningful global gaps while keeping replay independent of clients.
#[derive(Debug, Clone)]
pub struct EventHub<E> {
    desktop_id: DesktopId,
    generation: GenerationToken,
    limits: EventHubLimits,
    retained: VecDeque<EventRecord<E>>,
    retained_bytes: usize,
    latest_sequence: u64,
    dropped_through: u64,
}

impl<E: Clone> EventHub<E> {
    /// Creates an empty event hub for exactly one desktop generation.
    pub fn new(
        desktop_id: DesktopId,
        generation: GenerationToken,
        limits: EventHubLimits,
    ) -> Result<Self, EventHubError> {
        if generation.desktop_id() != desktop_id {
            return Err(EventHubError::WrongDesktop);
        }
        Ok(Self {
            desktop_id,
            generation,
            limits,
            retained: VecDeque::new(),
            retained_bytes: 0,
            latest_sequence: 0,
            dropped_through: 0,
        })
    }

    /// Returns current retention limits.
    #[must_use]
    pub const fn limits(&self) -> EventHubLimits {
        self.limits
    }

    /// Returns the latest sequence assigned in the active generation.
    #[must_use]
    pub const fn latest_sequence(&self) -> u64 {
        self.latest_sequence
    }

    /// Returns the number of retained events.
    #[must_use]
    pub fn retained_events(&self) -> usize {
        self.retained.len()
    }

    /// Returns the exact aggregate encoded size charged to retention.
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Returns the highest assigned sequence unavailable for complete replay.
    #[must_use]
    pub const fn dropped_through(&self) -> u64 {
        self.dropped_through
    }

    /// Assigns a sequence after normalization and applies both replay bounds.
    ///
    /// `encoded_size` must be measured from the final encoded event envelope by
    /// the transport edge. A single oversized event is published live but cannot
    /// be retained; it creates an explicit replay discontinuity.
    pub fn publish(
        &mut self,
        event: E,
        encoded_size: usize,
        generation: GenerationToken,
    ) -> Result<PublishOutcome, EventHubError> {
        self.validate_generation(generation)?;
        if encoded_size == 0 {
            return Err(EventHubError::ZeroEncodedSize);
        }
        let sequence = self
            .latest_sequence
            .checked_add(1)
            .ok_or(EventHubError::SequenceExhausted)?;
        self.latest_sequence = sequence;

        if encoded_size > self.limits.maximum_encoded_bytes {
            let evicted_events = self.retained.len();
            self.retained.clear();
            self.retained_bytes = 0;
            self.dropped_through = sequence;
            return Ok(PublishOutcome {
                sequence,
                retained: false,
                evicted_events,
                dropped_through: self.dropped_through,
            });
        }

        let mut evicted_events = 0;
        while self.retained.len() >= self.limits.maximum_events
            || self.retained_bytes > self.limits.maximum_encoded_bytes - encoded_size
        {
            let Some(evicted) = self.retained.pop_front() else {
                return Err(EventHubError::InvariantViolation);
            };
            self.retained_bytes = self
                .retained_bytes
                .checked_sub(evicted.encoded_size)
                .ok_or(EventHubError::InvariantViolation)?;
            self.dropped_through = self.dropped_through.max(evicted.sequence);
            evicted_events += 1;
        }

        self.retained_bytes = self
            .retained_bytes
            .checked_add(encoded_size)
            .ok_or(EventHubError::InvariantViolation)?;
        self.retained.push_back(EventRecord {
            generation,
            sequence,
            encoded_size,
            event,
        });
        Ok(PublishOutcome {
            sequence,
            retained: true,
            evicted_events,
            dropped_through: self.dropped_through,
        })
    }

    /// Returns a complete global suffix or requires authoritative resynchronization.
    ///
    /// Filtering by topic must happen only after this result is produced. Gaps
    /// caused by filtering are legitimate and do not alter replay completeness.
    #[must_use]
    pub fn replay_since(
        &self,
        generation: GenerationToken,
        since_sequence: u64,
    ) -> ReplayResult<E> {
        if generation != self.generation {
            return self.resync(ReplayFailure::GenerationChanged);
        }
        if since_sequence > self.latest_sequence {
            return self.resync(ReplayFailure::SequenceAhead);
        }
        if since_sequence < self.dropped_through {
            return self.resync(ReplayFailure::HistoryLost);
        }
        let events = self
            .retained
            .iter()
            .filter(|record| record.sequence > since_sequence)
            .cloned()
            .collect();
        ReplayResult::Events {
            since_sequence,
            latest_sequence: self.latest_sequence,
            events,
        }
    }

    /// Clears all replay history and starts sequencing at one in a new generation.
    ///
    /// Returns the number of invalidated retained events.
    pub fn rotate_generation(
        &mut self,
        generation: GenerationToken,
    ) -> Result<usize, EventHubError> {
        if generation.desktop_id() != self.desktop_id {
            return Err(EventHubError::WrongDesktop);
        }
        if generation.epoch() <= self.generation.epoch()
            || generation.generation() == self.generation.generation()
        {
            return Err(EventHubError::StaleGeneration);
        }
        let invalidated = self.retained.len();
        self.retained.clear();
        self.retained_bytes = 0;
        self.latest_sequence = 0;
        self.dropped_through = 0;
        self.generation = generation;
        Ok(invalidated)
    }

    fn validate_generation(&self, generation: GenerationToken) -> Result<(), EventHubError> {
        if generation.desktop_id() != self.desktop_id {
            return Err(EventHubError::WrongDesktop);
        }
        if generation != self.generation {
            return Err(EventHubError::StaleGeneration);
        }
        Ok(())
    }

    fn resync(&self, reason: ReplayFailure) -> ReplayResult<E> {
        ReplayResult::ResyncRequired {
            reason,
            current_generation: self.generation,
            dropped_through: self.dropped_through,
            latest_sequence: self.latest_sequence,
        }
    }
}

/// An event-hub operation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EventHubError {
    /// A retention bound was zero.
    #[error("event replay limits must be non-zero")]
    InvalidLimits,
    /// The generation token belongs to another desktop.
    #[error("generation token belongs to another desktop")]
    WrongDesktop,
    /// The generation token is no longer authoritative.
    #[error("generation token is stale")]
    StaleGeneration,
    /// The caller did not supply a measurable final encoded size.
    #[error("encoded event size must be non-zero")]
    ZeroEncodedSize,
    /// A live event cannot exceed the aggregate replay byte ceiling.
    #[error("live event exceeds the bounded event byte ceiling")]
    LiveEventTooLarge,
    /// The global event sequence cannot advance without wrapping.
    #[error("global event sequence exhausted")]
    SequenceExhausted,
    /// Internal accounting contradicted the retained queue.
    #[error("event replay invariant violation")]
    InvariantViolation,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::GenerationFence;
    use xenoteer_protocol::DesktopGeneration;

    fn hub(
        event_limit: usize,
        byte_limit: usize,
    ) -> Result<(EventHub<u8>, GenerationToken), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = GenerationFence::new(desktop_id, DesktopGeneration::new()).capture();
        Ok((
            EventHub::new(
                desktop_id,
                generation,
                EventHubLimits::new(event_limit, byte_limit)?,
            )?,
            generation,
        ))
    }

    #[test]
    fn count_and_bytes_evict_the_oldest_complete_prefix() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut hub, generation) = hub(3, 7)?;
        hub.publish(1, 3, generation)?;
        hub.publish(2, 3, generation)?;
        let third = hub.publish(3, 3, generation)?;

        assert_eq!(third.evicted_events, 1);
        assert_eq!(hub.retained_events(), 2);
        assert_eq!(hub.retained_bytes(), 6);
        assert!(matches!(
            hub.replay_since(generation, 0),
            ReplayResult::ResyncRequired {
                reason: ReplayFailure::HistoryLost,
                ..
            }
        ));
        assert!(matches!(
            hub.replay_since(generation, 1),
            ReplayResult::Events { events, .. } if events.len() == 2
        ));
        Ok(())
    }

    #[test]
    fn oversized_live_event_creates_explicit_replay_discontinuity()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut hub, generation) = hub(3, 4)?;
        hub.publish(1, 2, generation)?;
        let outcome = hub.publish(2, 5, generation)?;
        hub.publish(3, 2, generation)?;

        assert!(!outcome.retained);
        assert_eq!(outcome.dropped_through, 2);
        assert!(matches!(
            hub.replay_since(generation, 1),
            ReplayResult::ResyncRequired {
                reason: ReplayFailure::HistoryLost,
                ..
            }
        ));
        assert!(matches!(
            hub.replay_since(generation, 2),
            ReplayResult::Events { events, .. }
                if events.iter().map(|record| record.event).collect::<Vec<_>>() == vec![3]
        ));
        Ok(())
    }

    #[test]
    fn generation_rotation_resets_sequence_and_fences_old_replay()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let mut fence = GenerationFence::new(desktop_id, DesktopGeneration::new());
        let old = fence.capture();
        let mut hub = EventHub::new(desktop_id, old, EventHubLimits::new(3, 30)?)?;
        hub.publish(1, 2, old)?;
        let current = fence.rotate(DesktopGeneration::new())?;
        assert_eq!(hub.rotate_generation(current)?, 1);
        assert!(matches!(
            hub.replay_since(old, 0),
            ReplayResult::ResyncRequired {
                reason: ReplayFailure::GenerationChanged,
                ..
            }
        ));
        assert_eq!(hub.publish(2, 2, current)?.sequence, 1);
        Ok(())
    }
}
