//! Bounded in-memory abuse controls shared across HTTP and WebSocket paths.

use std::{
    collections::HashMap,
    hash::Hash,
    net::IpAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const AUTH_FAILURES_PER_PERIOD: u32 = 10;
const AUTH_FAILURE_BURST: u32 = 10;
const AUTH_FAILURE_PERIOD: Duration = Duration::from_secs(60);
const MAX_AUTH_SOURCES: usize = 4_096;

const COMMAND_SUBMITS_PER_PERIOD: u32 = 120;
const COMMAND_SUBMIT_BURST: u32 = 30;
const COMMAND_SUBMIT_PERIOD: Duration = Duration::from_secs(60);
const MAX_COMMAND_PRINCIPALS: usize = 4_096;

const WEBSOCKET_MESSAGES_PER_PERIOD: u32 = 120;
const WEBSOCKET_MESSAGE_BURST: u32 = 30;
const WEBSOCKET_MESSAGE_PERIOD: Duration = Duration::from_secs(60);

const MAX_WEBSOCKET_SESSIONS: usize = 64;

/// Trusted transport source, with one deliberately shared router-test fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AuthenticationSource {
    Ip(IpAddr),
    Fallback,
}

/// Cloneable abuse-control state shared by every API transport.
#[derive(Clone)]
pub(crate) struct AbuseControls {
    authentication_failures: Arc<Mutex<KeyedTokenBuckets<AuthenticationSource>>>,
    command_submits: Arc<Mutex<KeyedTokenBuckets<String>>>,
    websocket_sessions: Arc<Semaphore>,
}

impl AbuseControls {
    pub(crate) fn new() -> Self {
        Self::with_limits(
            MAX_AUTH_SOURCES,
            MAX_COMMAND_PRINCIPALS,
            MAX_WEBSOCKET_SESSIONS,
        )
    }

    fn with_limits(
        maximum_authentication_sources: usize,
        maximum_command_principals: usize,
        maximum_websocket_sessions: usize,
    ) -> Self {
        let now = Instant::now();
        Self {
            authentication_failures: Arc::new(Mutex::new(KeyedTokenBuckets::new(
                Rate::new(
                    AUTH_FAILURE_BURST,
                    AUTH_FAILURES_PER_PERIOD,
                    AUTH_FAILURE_PERIOD,
                ),
                maximum_authentication_sources,
                now,
            ))),
            command_submits: Arc::new(Mutex::new(KeyedTokenBuckets::new(
                Rate::new(
                    COMMAND_SUBMIT_BURST,
                    COMMAND_SUBMITS_PER_PERIOD,
                    COMMAND_SUBMIT_PERIOD,
                ),
                maximum_command_principals,
                now,
            ))),
            websocket_sessions: Arc::new(Semaphore::new(maximum_websocket_sessions)),
        }
    }

    /// Rejects a blocked source before any credential work is performed.
    pub(crate) fn authentication_preflight(&self, source: AuthenticationSource) -> bool {
        lock_or_recover(&self.authentication_failures).permits(&source, Instant::now())
    }

    /// Charges one failed credential attempt without retaining credential bytes.
    pub(crate) fn record_authentication_failure(&self, source: AuthenticationSource) -> bool {
        lock_or_recover(&self.authentication_failures).try_take(source, Instant::now())
    }

    /// Charges one command submission across every REST and WebSocket session.
    pub(crate) fn admit_command_submit(&self, principal_id: &str) -> bool {
        self.admit_command_submit_at(principal_id, Instant::now())
    }

    fn admit_command_submit_at(&self, principal_id: &str, now: Instant) -> bool {
        lock_or_recover(&self.command_submits).try_take(principal_id.to_owned(), now)
    }

    /// Acquires one shared WebSocket-session slot for the complete upgrade life.
    pub(crate) fn try_acquire_websocket(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.websocket_sessions)
            .try_acquire_owned()
            .ok()
    }

    /// Creates one independent per-session application-message quota.
    pub(crate) fn websocket_message_limit(&self) -> SessionMessageLimit {
        self.websocket_message_limit_at(Instant::now())
    }

    fn websocket_message_limit_at(&self, now: Instant) -> SessionMessageLimit {
        SessionMessageLimit {
            bucket: TokenBucket::new(
                Rate::new(
                    WEBSOCKET_MESSAGE_BURST,
                    WEBSOCKET_MESSAGES_PER_PERIOD,
                    WEBSOCKET_MESSAGE_PERIOD,
                ),
                now,
            ),
        }
    }
}

/// Single-owner WebSocket application-message limiter.
pub(crate) struct SessionMessageLimit {
    bucket: TokenBucket,
}

impl SessionMessageLimit {
    pub(crate) fn try_take(&mut self) -> bool {
        self.try_take_at(Instant::now())
    }

    fn try_take_at(&mut self, now: Instant) -> bool {
        self.bucket.try_take(now)
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy)]
struct Rate {
    capacity: u32,
    refill_tokens: u32,
    refill_period: Duration,
}

impl Rate {
    const fn new(capacity: u32, refill_tokens: u32, refill_period: Duration) -> Self {
        Self {
            capacity,
            refill_tokens,
            refill_period,
        }
    }

    fn period_units(self) -> u128 {
        self.refill_period.as_nanos()
    }

    fn capacity_units(self) -> u128 {
        u128::from(self.capacity).saturating_mul(self.period_units())
    }
}

struct TokenBucket {
    rate: Rate,
    available_units: u128,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rate: Rate, now: Instant) -> Self {
        Self {
            rate,
            available_units: rate.capacity_units(),
            last_refill: now,
        }
    }

    fn permits(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.available_units >= self.rate.period_units()
    }

    fn try_take(&mut self, now: Instant) -> bool {
        if !self.permits(now) {
            return false;
        }
        self.available_units = self
            .available_units
            .saturating_sub(self.rate.period_units());
        true
    }

    fn is_full(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.available_units == self.rate.capacity_units()
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.last_refill = now.max(self.last_refill);
        let refill = elapsed
            .as_nanos()
            .saturating_mul(u128::from(self.rate.refill_tokens));
        self.available_units = self
            .available_units
            .saturating_add(refill)
            .min(self.rate.capacity_units());
    }
}

struct BucketEntry {
    bucket: TokenBucket,
    last_charge: Instant,
}

struct KeyedTokenBuckets<K> {
    rate: Rate,
    maximum_entries: usize,
    entries: HashMap<K, BucketEntry>,
    overflow: TokenBucket,
    last_prune: Instant,
}

impl<K> KeyedTokenBuckets<K>
where
    K: Eq + Hash,
{
    fn new(rate: Rate, maximum_entries: usize, now: Instant) -> Self {
        Self {
            rate,
            maximum_entries,
            entries: HashMap::with_capacity(maximum_entries.min(256)),
            overflow: TokenBucket::new(rate, now),
            last_prune: now,
        }
    }

    fn permits(&mut self, key: &K, now: Instant) -> bool {
        if let Some(entry) = self.entries.get_mut(key) {
            return entry.bucket.permits(now);
        }
        if self.entries.len() < self.maximum_entries {
            true
        } else {
            self.overflow.permits(now)
        }
    }

    fn try_take(&mut self, key: K, now: Instant) -> bool {
        if let Some(entry) = self.entries.get_mut(&key) {
            let admitted = entry.bucket.try_take(now);
            entry.last_charge = now;
            return admitted;
        }
        self.prune_refilled(now);
        if self.entries.len() >= self.maximum_entries {
            return self.overflow.try_take(now);
        }
        let mut bucket = TokenBucket::new(self.rate, now);
        let admitted = bucket.try_take(now);
        self.entries.insert(
            key,
            BucketEntry {
                bucket,
                last_charge: now,
            },
        );
        admitted
    }

    fn prune_refilled(&mut self, now: Instant) {
        let refill_period = self.rate.refill_period;
        if self.entries.len() < self.maximum_entries
            || now.saturating_duration_since(self.last_prune) < refill_period
        {
            return;
        }
        self.last_prune = now.max(self.last_prune);
        self.entries.retain(|_, entry| {
            !(now.saturating_duration_since(entry.last_charge) >= refill_period
                && entry.bucket.is_full(now))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_point_bucket_refills_without_wall_clock_or_float_rounding() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(Rate::new(10, 10, Duration::from_secs(60)), start);
        for _ in 0..10 {
            assert!(bucket.try_take(start));
        }
        assert!(!bucket.try_take(start));
        assert!(bucket.try_take(start + Duration::from_secs(6)));
        assert!(!bucket.try_take(start + Duration::from_secs(6)));
        for _ in 0..10 {
            assert!(bucket.try_take(start + Duration::from_secs(66)));
        }
        assert!(!bucket.try_take(start + Duration::from_secs(66)));
    }

    #[test]
    fn keyed_bucket_never_exceeds_entry_cap_and_prunes_only_refilled_entries() {
        let start = Instant::now();
        let rate = Rate::new(1, 1, Duration::from_secs(60));
        let mut buckets = KeyedTokenBuckets::new(rate, 2, start);
        assert!(buckets.try_take("one", start));
        assert!(buckets.try_take("two", start));
        assert!(buckets.try_take("overflow", start));
        assert_eq!(buckets.entries.len(), 2);
        assert!(!buckets.try_take("another-overflow", start));
        assert!(buckets.try_take("replacement", start + Duration::from_secs(60)));
        assert!(buckets.entries.len() <= 2);
        assert!(buckets.entries.contains_key("replacement"));
    }

    #[test]
    fn websocket_permits_are_owned_for_session_lifetime() {
        let controls = AbuseControls::with_limits(2, 2, 1);
        let permit = controls.try_acquire_websocket();
        assert!(permit.is_some());
        assert!(controls.try_acquire_websocket().is_none());
        drop(permit);
        assert!(controls.try_acquire_websocket().is_some());
    }

    #[test]
    fn command_quota_is_shared_by_principal_across_clones() {
        let controls = AbuseControls::with_limits(2, 2, 1);
        let start = Instant::now();
        let other_transport = controls.clone();
        for _ in 0..COMMAND_SUBMIT_BURST {
            assert!(controls.admit_command_submit_at("operator", start));
        }
        assert!(!other_transport.admit_command_submit_at("operator", start));
        assert!(
            other_transport.admit_command_submit_at("operator", start + Duration::from_millis(500))
        );
        assert!(other_transport.admit_command_submit_at("another-operator", start));
    }

    #[test]
    fn websocket_message_quota_has_an_independent_exact_burst() {
        let start = Instant::now();
        let controls = AbuseControls::with_limits(2, 2, 1);
        let mut first_session = controls.websocket_message_limit_at(start);
        let mut second_session = controls.websocket_message_limit_at(start);
        for _ in 0..WEBSOCKET_MESSAGE_BURST {
            assert!(first_session.try_take_at(start));
        }
        assert!(!first_session.try_take_at(start));
        assert!(first_session.try_take_at(start + Duration::from_millis(500)));
        assert!(second_session.try_take_at(start));
    }
}
