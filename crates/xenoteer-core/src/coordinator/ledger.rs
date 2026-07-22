//! Bounded, generation-fenced command idempotency ledger.

use std::collections::BTreeMap;

use xenoteer_protocol::{CommandId, DesktopId};

use super::{GenerationToken, MonotonicMillis, PrincipalId};

/// A digest of the canonical, typed command request.
///
/// Hashing is an edge responsibility. The canonical input must include every
/// behavior-affecting field and the desktop generation. This type intentionally
/// accepts only a full 256-bit digest so the ledger never compares raw requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalCommandHash([u8; 32]);

impl CanonicalCommandHash {
    /// Wraps a caller-computed 256-bit canonical request digest.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the digest bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Hard memory and retention bounds for the idempotency ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandLedgerLimits {
    maximum_entries: usize,
    terminal_ttl_ms: u64,
}

impl CommandLedgerLimits {
    /// Creates non-zero capacity and terminal-retention limits.
    pub fn new(maximum_entries: usize, terminal_ttl_ms: u64) -> Result<Self, CommandLedgerError> {
        if maximum_entries == 0 || terminal_ttl_ms == 0 {
            return Err(CommandLedgerError::InvalidLimits);
        }
        Ok(Self {
            maximum_entries,
            terminal_ttl_ms,
        })
    }

    /// Returns the maximum number of records, including in-flight records.
    #[must_use]
    pub const fn maximum_entries(self) -> usize {
        self.maximum_entries
    }

    /// Returns terminal-result retention in monotonic milliseconds.
    #[must_use]
    pub const fn terminal_ttl_ms(self) -> u64 {
        self.terminal_ttl_ms
    }
}

impl Default for CommandLedgerLimits {
    fn default() -> Self {
        Self {
            maximum_entries: crate::config::MAX_RESULT_LEDGER_ENTRIES,
            terminal_ttl_ms: crate::config::MAX_RESULT_LEDGER_TTL_SECONDS * 1_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CommandKey {
    principal: PrincipalId,
    command_id: CommandId,
}

/// Durable-within-the-generation command lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRecordState<R> {
    /// Admission succeeded atomically, but execution has not started.
    Accepted,
    /// The serialized executor started the command.
    Running,
    /// An immutable success or failure result is retained.
    Terminal(R),
}

impl<R> CommandRecordState<R> {
    /// Returns whether this state may be evicted to admit a new command.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal(_))
    }
}

/// A public snapshot of one idempotency record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord<R> {
    /// Authenticated scope of this command ID.
    pub principal: PrincipalId,
    /// Client-authored command identifier.
    pub command_id: CommandId,
    /// Desktop lifetime in which admission occurred.
    pub generation: GenerationToken,
    /// Canonical request digest bound to the command ID.
    pub request_hash: CanonicalCommandHash,
    /// Current command lifecycle state.
    pub state: CommandRecordState<R>,
    /// First successful admission time.
    pub accepted_at: MonotonicMillis,
    /// Most recent lifecycle transition time.
    pub updated_at: MonotonicMillis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredCommand<R> {
    record: CommandRecord<R>,
    terminal_at: Option<MonotonicMillis>,
    access_ordinal: u64,
}

/// Outcome of atomic command-ID admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyDecision<R> {
    /// No equivalent record existed; this record owns the one permitted execution.
    Admitted(CommandRecord<R>),
    /// An exactly equivalent command already exists; do not execute it again.
    Existing(CommandRecord<R>),
}

/// A bounded LRU/TTL ledger for one desktop generation.
///
/// Capacity counts accepted and running records. Only terminal records are
/// evictable: overload therefore fails closed instead of forgetting an in-flight
/// command and accidentally executing it twice.
#[derive(Debug, Clone)]
pub struct CommandLedger<R> {
    desktop_id: DesktopId,
    generation: GenerationToken,
    limits: CommandLedgerLimits,
    records: BTreeMap<CommandKey, StoredCommand<R>>,
    next_access_ordinal: u64,
    last_observed_at: Option<MonotonicMillis>,
}

impl<R: Clone> CommandLedger<R> {
    /// Creates an empty ledger for exactly one desktop generation.
    pub fn new(
        desktop_id: DesktopId,
        generation: GenerationToken,
        limits: CommandLedgerLimits,
    ) -> Result<Self, CommandLedgerError> {
        if generation.desktop_id() != desktop_id {
            return Err(CommandLedgerError::WrongDesktop);
        }
        Ok(Self {
            desktop_id,
            generation,
            limits,
            records: BTreeMap::new(),
            next_access_ordinal: 1,
            last_observed_at: None,
        })
    }

    /// Returns the number of retained records, including accepted/running work.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns whether no records are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Returns the configured bounds.
    #[must_use]
    pub const fn limits(&self) -> CommandLedgerLimits {
        self.limits
    }

    /// Atomically reserves a command ID or returns its equivalent existing record.
    pub fn admit(
        &mut self,
        principal: PrincipalId,
        command_id: CommandId,
        request_hash: CanonicalCommandHash,
        now: MonotonicMillis,
        generation: GenerationToken,
    ) -> Result<IdempotencyDecision<R>, CommandLedgerError> {
        self.validate_generation(generation)?;
        self.observe(now)?;
        self.purge_expired_terminal(now);

        let key = CommandKey {
            principal,
            command_id,
        };
        if let Some(existing) = self.records.get(&key) {
            if existing.record.request_hash != request_hash {
                return Err(CommandLedgerError::CommandIdConflict);
            }
            let access_ordinal = self.take_access_ordinal()?;
            let existing = self
                .records
                .get_mut(&key)
                .ok_or(CommandLedgerError::InvariantViolation)?;
            existing.access_ordinal = access_ordinal;
            return Ok(IdempotencyDecision::Existing(existing.record.clone()));
        }

        let access_ordinal = self.take_access_ordinal()?;
        if self.records.len() >= self.limits.maximum_entries {
            self.evict_lru_terminal()?;
        }
        let record = CommandRecord {
            principal: key.principal.clone(),
            command_id,
            generation,
            request_hash,
            state: CommandRecordState::Accepted,
            accepted_at: now,
            updated_at: now,
        };
        self.records.insert(
            key,
            StoredCommand {
                record: record.clone(),
                terminal_at: None,
                access_ordinal,
            },
        );
        Ok(IdempotencyDecision::Admitted(record))
    }

    /// Looks up and LRU-touches a record in the authenticated principal scope.
    pub fn lookup(
        &mut self,
        principal: &PrincipalId,
        command_id: CommandId,
        now: MonotonicMillis,
        generation: GenerationToken,
    ) -> Result<Option<CommandRecord<R>>, CommandLedgerError> {
        self.validate_generation(generation)?;
        self.observe(now)?;
        self.purge_expired_terminal(now);
        let key = CommandKey {
            principal: principal.clone(),
            command_id,
        };
        if !self.records.contains_key(&key) {
            return Ok(None);
        }
        let access_ordinal = self.take_access_ordinal()?;
        let stored = self
            .records
            .get_mut(&key)
            .ok_or(CommandLedgerError::InvariantViolation)?;
        stored.access_ordinal = access_ordinal;
        Ok(Some(stored.record.clone()))
    }

    /// Transitions one admitted record to running exactly once.
    pub fn mark_running(
        &mut self,
        principal: &PrincipalId,
        command_id: CommandId,
        now: MonotonicMillis,
        generation: GenerationToken,
    ) -> Result<CommandRecord<R>, CommandLedgerError> {
        self.validate_generation(generation)?;
        self.observe(now)?;
        let key = CommandKey {
            principal: principal.clone(),
            command_id,
        };
        let existing = self
            .records
            .get(&key)
            .ok_or(CommandLedgerError::UnknownCommand)?;
        if !matches!(existing.record.state, CommandRecordState::Accepted) {
            return Err(CommandLedgerError::InvalidTransition);
        }
        let access_ordinal = self.take_access_ordinal()?;
        let stored = self
            .records
            .get_mut(&key)
            .ok_or(CommandLedgerError::InvariantViolation)?;
        stored.record.state = CommandRecordState::Running;
        stored.record.updated_at = now;
        stored.access_ordinal = access_ordinal;
        Ok(stored.record.clone())
    }

    /// Stores the one immutable terminal outcome.
    ///
    /// Completion from `Accepted` is valid for rejection/cancellation/deadline
    /// paths that become terminal before the executor starts.
    pub fn complete(
        &mut self,
        principal: &PrincipalId,
        command_id: CommandId,
        result: R,
        now: MonotonicMillis,
        generation: GenerationToken,
    ) -> Result<CommandRecord<R>, CommandLedgerError> {
        self.validate_generation(generation)?;
        self.observe(now)?;
        let key = CommandKey {
            principal: principal.clone(),
            command_id,
        };
        let existing = self
            .records
            .get(&key)
            .ok_or(CommandLedgerError::UnknownCommand)?;
        if existing.record.state.is_terminal() {
            return Err(CommandLedgerError::TerminalImmutable);
        }
        let access_ordinal = self.take_access_ordinal()?;
        let stored = self
            .records
            .get_mut(&key)
            .ok_or(CommandLedgerError::InvariantViolation)?;
        stored.record.state = CommandRecordState::Terminal(result);
        stored.record.updated_at = now;
        stored.terminal_at = Some(now);
        stored.access_ordinal = access_ordinal;
        Ok(stored.record.clone())
    }

    /// Clears all records while advancing to a new authoritative generation.
    ///
    /// Returns the number of invalidated records for metrics/audit reporting.
    pub fn rotate_generation(
        &mut self,
        generation: GenerationToken,
    ) -> Result<usize, CommandLedgerError> {
        if generation.desktop_id() != self.desktop_id {
            return Err(CommandLedgerError::WrongDesktop);
        }
        if generation.epoch() <= self.generation.epoch()
            || generation.generation() == self.generation.generation()
        {
            return Err(CommandLedgerError::StaleGeneration);
        }
        let invalidated = self.records.len();
        self.records.clear();
        self.generation = generation;
        self.next_access_ordinal = 1;
        self.last_observed_at = None;
        Ok(invalidated)
    }

    fn validate_generation(&self, generation: GenerationToken) -> Result<(), CommandLedgerError> {
        if generation.desktop_id() != self.desktop_id {
            return Err(CommandLedgerError::WrongDesktop);
        }
        if generation != self.generation {
            return Err(CommandLedgerError::StaleGeneration);
        }
        Ok(())
    }

    fn observe(&mut self, now: MonotonicMillis) -> Result<(), CommandLedgerError> {
        if self.last_observed_at.is_some_and(|previous| now < previous) {
            return Err(CommandLedgerError::ClockRegressed);
        }
        self.last_observed_at = Some(now);
        Ok(())
    }

    fn take_access_ordinal(&mut self) -> Result<u64, CommandLedgerError> {
        let current = self.next_access_ordinal;
        self.next_access_ordinal = current
            .checked_add(1)
            .ok_or(CommandLedgerError::AccessCounterExhausted)?;
        Ok(current)
    }

    fn purge_expired_terminal(&mut self, now: MonotonicMillis) -> usize {
        let before = self.records.len();
        let terminal_ttl_ms = self.limits.terminal_ttl_ms;
        self.records.retain(|_, stored| {
            let Some(terminal_at) = stored.terminal_at else {
                return true;
            };
            now.elapsed_since(terminal_at)
                .is_none_or(|age_ms| age_ms < terminal_ttl_ms)
        });
        before - self.records.len()
    }

    fn evict_lru_terminal(&mut self) -> Result<(), CommandLedgerError> {
        let victim = self
            .records
            .iter()
            .filter(|(_, stored)| stored.record.state.is_terminal())
            .min_by(|(left_key, left), (right_key, right)| {
                left.access_ordinal
                    .cmp(&right.access_ordinal)
                    .then_with(|| left_key.cmp(right_key))
            })
            .map(|(key, _)| (*key).clone())
            .ok_or(CommandLedgerError::CapacityExhausted)?;
        self.records.remove(&victim);
        Ok(())
    }
}

/// A command-ledger operation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CommandLedgerError {
    /// A limit was zero.
    #[error("command ledger limits must be non-zero")]
    InvalidLimits,
    /// The generation token belongs to another desktop.
    #[error("generation token belongs to another desktop")]
    WrongDesktop,
    /// The generation token is no longer authoritative.
    #[error("generation token is stale")]
    StaleGeneration,
    /// The same scoped command ID was reused for different canonical input.
    #[error("command ID is already bound to a different canonical request")]
    CommandIdConflict,
    /// All capacity is occupied by commands that may still execute.
    #[error("command ledger is full of non-terminal records")]
    CapacityExhausted,
    /// No record exists in this principal scope.
    #[error("command record was not found")]
    UnknownCommand,
    /// The lifecycle transition is invalid from the current state.
    #[error("command lifecycle transition is invalid")]
    InvalidTransition,
    /// A terminal record cannot be changed.
    #[error("terminal command result is immutable")]
    TerminalImmutable,
    /// A monotonic clock reading moved backwards.
    #[error("monotonic clock regressed")]
    ClockRegressed,
    /// LRU ordering can no longer advance safely.
    #[error("command ledger access counter exhausted")]
    AccessCounterExhausted,
    /// An internal lookup contradicted a prior membership check.
    #[error("command ledger invariant violation")]
    InvariantViolation,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::GenerationFence;
    use xenoteer_protocol::DesktopGeneration;

    fn setup(
        capacity: usize,
        ttl_ms: u64,
    ) -> Result<
        (CommandLedger<&'static str>, PrincipalId, GenerationToken),
        Box<dyn std::error::Error>,
    > {
        let desktop_id = DesktopId::new();
        let token = GenerationFence::new(desktop_id, DesktopGeneration::new()).capture();
        Ok((
            CommandLedger::new(
                desktop_id,
                token,
                CommandLedgerLimits::new(capacity, ttl_ms)?,
            )?,
            PrincipalId::new("alice")?,
            token,
        ))
    }

    #[test]
    fn exact_duplicate_returns_existing_without_reexecution()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut ledger, principal, generation) = setup(2, 100)?;
        let command_id = CommandId::new();
        let hash = CanonicalCommandHash::new([7; 32]);

        assert!(matches!(
            ledger.admit(
                principal.clone(),
                command_id,
                hash,
                MonotonicMillis::new(1),
                generation,
            )?,
            IdempotencyDecision::Admitted(_)
        ));
        assert!(matches!(
            ledger.admit(
                principal,
                command_id,
                hash,
                MonotonicMillis::new(2),
                generation,
            )?,
            IdempotencyDecision::Existing(_)
        ));
        assert_eq!(ledger.len(), 1);
        Ok(())
    }

    #[test]
    fn conflicting_reuse_and_terminal_mutation_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut ledger, principal, generation) = setup(2, 100)?;
        let command_id = CommandId::new();
        ledger.admit(
            principal.clone(),
            command_id,
            CanonicalCommandHash::new([1; 32]),
            MonotonicMillis::new(1),
            generation,
        )?;
        assert_eq!(
            ledger.admit(
                principal.clone(),
                command_id,
                CanonicalCommandHash::new([2; 32]),
                MonotonicMillis::new(2),
                generation,
            ),
            Err(CommandLedgerError::CommandIdConflict)
        );
        ledger.complete(
            &principal,
            command_id,
            "done",
            MonotonicMillis::new(3),
            generation,
        )?;
        assert_eq!(
            ledger.complete(
                &principal,
                command_id,
                "changed",
                MonotonicMillis::new(4),
                generation,
            ),
            Err(CommandLedgerError::TerminalImmutable)
        );
        Ok(())
    }

    #[test]
    fn capacity_never_evicts_in_flight_work() -> Result<(), Box<dyn std::error::Error>> {
        let (mut ledger, principal, generation) = setup(1, 100)?;
        ledger.admit(
            principal.clone(),
            CommandId::new(),
            CanonicalCommandHash::new([1; 32]),
            MonotonicMillis::new(1),
            generation,
        )?;
        assert_eq!(
            ledger.admit(
                principal,
                CommandId::new(),
                CanonicalCommandHash::new([2; 32]),
                MonotonicMillis::new(2),
                generation,
            ),
            Err(CommandLedgerError::CapacityExhausted)
        );
        assert_eq!(ledger.len(), 1);
        Ok(())
    }

    #[test]
    fn terminal_ttl_and_generation_rotation_remove_replayability()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let mut fence = GenerationFence::new(desktop_id, DesktopGeneration::new());
        let generation = fence.capture();
        let principal = PrincipalId::new("alice")?;
        let command_id = CommandId::new();
        let mut ledger =
            CommandLedger::new(desktop_id, generation, CommandLedgerLimits::new(2, 10)?)?;
        ledger.admit(
            principal.clone(),
            command_id,
            CanonicalCommandHash::new([1; 32]),
            MonotonicMillis::new(0),
            generation,
        )?;
        ledger.complete(
            &principal,
            command_id,
            "done",
            MonotonicMillis::new(1),
            generation,
        )?;
        assert_eq!(
            ledger.lookup(&principal, command_id, MonotonicMillis::new(11), generation,)?,
            None
        );

        let current = fence.rotate(DesktopGeneration::new())?;
        assert_eq!(ledger.rotate_generation(current)?, 0);
        assert_eq!(
            ledger.lookup(&principal, command_id, MonotonicMillis::new(0), generation,),
            Err(CommandLedgerError::StaleGeneration)
        );
        Ok(())
    }
}
