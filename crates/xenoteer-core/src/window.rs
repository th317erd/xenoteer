//! Authoritative, generation-fenced window identity model.
//!
//! The X11 adapter is responsible for observing properties and building a
//! checked [`WindowSnapshot`]. This model owns the identity lifetime rules that
//! must remain backend independent: one live birth per XID, monotonically
//! increasing model revisions, bounded tombstones, and exact revalidation
//! immediately before an effect.

use std::collections::{BTreeMap, VecDeque};

use thiserror::Error;
use xenoteer_protocol::{
    DesktopGeneration, DesktopId, WindowModelRevision, WindowRef, WindowSnapshot,
    WindowValidationError,
};

use crate::{MonotonicMillis, window_query::WindowQueryRecord};

/// Default upper bound for simultaneously modeled client windows.
pub const DEFAULT_MAX_LIVE_WINDOWS: usize = 4_096;
/// Default upper bound for recently destroyed window births.
pub const DEFAULT_MAX_WINDOW_TOMBSTONES: usize = 8_192;
/// Default retention for destroyed identities, matching command dedupe safety.
pub const DEFAULT_WINDOW_TOMBSTONE_TTL_MS: u64 = 15 * 60 * 1_000;

/// Immutable ceilings for one desktop-lifetime window model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowModelLimits {
    /// Maximum simultaneously live window births.
    pub max_live_windows: usize,
    /// Maximum retained destroyed identities.
    pub max_tombstones: usize,
    /// Monotonic retention applied to a destroyed identity.
    pub tombstone_ttl_ms: u64,
}

impl Default for WindowModelLimits {
    fn default() -> Self {
        Self {
            max_live_windows: DEFAULT_MAX_LIVE_WINDOWS,
            max_tombstones: DEFAULT_MAX_WINDOW_TOMBSTONES,
            tombstone_ttl_ms: DEFAULT_WINDOW_TOMBSTONE_TTL_MS,
        }
    }
}

impl WindowModelLimits {
    /// Validates non-zero capacities and retention.
    pub const fn validate(self) -> Result<Self, WindowModelError> {
        if self.max_live_windows == 0 || self.max_tombstones == 0 || self.tombstone_ttl_ms == 0 {
            return Err(WindowModelError::InvalidLimits);
        }
        Ok(self)
    }
}

/// Retained evidence that one exact XID birth was destroyed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowTombstone {
    /// Exact destroyed identity; never a selector or successor hint.
    pub window: WindowRef,
    /// Model revision that removed the live snapshot.
    pub destroyed_revision: WindowModelRevision,
    expires_at: MonotonicMillis,
}

impl WindowTombstone {
    /// Monotonic expiry used only inside the owning daemon lifetime.
    #[must_use]
    pub const fn expires_at(&self) -> MonotonicMillis {
        self.expires_at
    }
}

/// Successful identity resolution against one atomic model revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWindow {
    /// Revision at which the exact identity was proven live.
    pub revision: WindowModelRevision,
    /// Immutable copy of the currently modeled snapshot.
    pub snapshot: WindowSnapshot,
}

/// Observable result of inserting or refreshing one exact birth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowModelChange {
    /// A previously absent XID birth became live.
    Created(WindowSnapshot),
    /// The same live identity received a newer observation.
    Updated(WindowSnapshot),
}

#[derive(Debug)]
struct LiveWindow {
    snapshot: WindowSnapshot,
    created_revision: WindowModelRevision,
}

/// Generation-fenced authoritative state owned by one observation actor.
#[derive(Debug)]
pub struct WindowModel {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    revision: WindowModelRevision,
    limits: WindowModelLimits,
    live: BTreeMap<u32, LiveWindow>,
    tombstones: VecDeque<WindowTombstone>,
    last_birth_serial: u64,
    last_now: MonotonicMillis,
}

impl WindowModel {
    /// Creates an empty model for exactly one desktop lifetime.
    pub fn new(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        limits: WindowModelLimits,
    ) -> Result<Self, WindowModelError> {
        if desktop_id.as_uuid().is_nil() || desktop_generation.as_uuid().is_nil() {
            return Err(WindowModelError::NilIdentifier);
        }
        Ok(Self {
            desktop_id,
            desktop_generation,
            revision: WindowModelRevision::new(1).map_err(WindowModelError::InvalidSnapshot)?,
            limits: limits.validate()?,
            live: BTreeMap::new(),
            tombstones: VecDeque::new(),
            last_birth_serial: 0,
            last_now: MonotonicMillis::new(0),
        })
    }

    /// Returns the desktop resource owned by this model.
    #[must_use]
    pub const fn desktop_id(&self) -> DesktopId {
        self.desktop_id
    }

    /// Returns the exact X server/session lifetime owned by this model.
    #[must_use]
    pub const fn desktop_generation(&self) -> DesktopGeneration {
        self.desktop_generation
    }

    /// Returns the current atomic model revision.
    #[must_use]
    pub const fn revision(&self) -> WindowModelRevision {
        self.revision
    }

    /// Returns current live/tombstone cardinalities for bounded diagnostics.
    #[must_use]
    pub fn counts(&self) -> (usize, usize) {
        (self.live.len(), self.tombstones.len())
    }

    /// Returns the next daemon-lifetime birth serial required for a new XID.
    ///
    /// The serial is global rather than per-XID, so bounded tombstone eviction
    /// never permits an old serialized reference to collide with a later
    /// reincarnation of the same XID.
    pub fn next_birth_serial(&self) -> Result<u64, WindowModelError> {
        self.last_birth_serial
            .checked_add(1)
            .ok_or(WindowModelError::BirthGenerationExhausted)
    }

    /// Returns the exact live reference currently owning an XID, if any.
    #[must_use]
    pub fn live_reference(&self, xid: u32) -> Option<&WindowRef> {
        self.live.get(&xid).map(|window| &window.snapshot.window)
    }

    /// Inserts a new birth or refreshes the exact currently live identity.
    ///
    /// The model, rather than the adapter, assigns the authoritative revision.
    /// A reused XID must carry the next observed-generation value and a new
    /// server-computed identity hash; it is never treated as an update.
    pub fn observe(
        &mut self,
        mut snapshot: WindowSnapshot,
        now: MonotonicMillis,
    ) -> Result<WindowModelChange, WindowModelError> {
        self.advance_time(now)?;
        snapshot
            .validate()
            .map_err(WindowModelError::InvalidSnapshot)?;
        self.require_scope(&snapshot.window)?;

        let xid = snapshot.window.xid;
        let existing_creation_revision = if let Some(current) = self.live.get(&xid) {
            if current.snapshot.window != snapshot.window {
                return Err(WindowModelError::XidAlreadyLive);
            }
            Some(current.created_revision)
        } else {
            if self.live.len() >= self.limits.max_live_windows {
                return Err(WindowModelError::LiveCapacityExhausted);
            }
            let expected_birth = self.next_birth_serial()?;
            if snapshot.window.observed_generation != expected_birth {
                return Err(WindowModelError::UnexpectedBirthGeneration {
                    expected: expected_birth,
                    actual: snapshot.window.observed_generation,
                });
            }
            None
        };

        snapshot.model_revision = self.next_revision()?;
        let created_revision = existing_creation_revision.unwrap_or(snapshot.model_revision);
        if existing_creation_revision.is_none() {
            self.last_birth_serial = snapshot.window.observed_generation;
        }
        self.live.insert(
            xid,
            LiveWindow {
                snapshot: snapshot.clone(),
                created_revision,
            },
        );
        if existing_creation_revision.is_some() {
            Ok(WindowModelChange::Updated(snapshot))
        } else {
            Ok(WindowModelChange::Created(snapshot))
        }
    }

    /// Removes exactly one live identity and retains a bounded tombstone.
    pub fn destroy(
        &mut self,
        reference: &WindowRef,
        now: MonotonicMillis,
    ) -> Result<WindowTombstone, WindowModelError> {
        self.advance_time(now)?;
        reference
            .validate()
            .map_err(WindowModelError::InvalidReference)?;
        self.require_scope(reference)?;
        let Some(current) = self.live.get(&reference.xid) else {
            if self
                .tombstones
                .iter()
                .any(|entry| &entry.window == reference)
            {
                return Err(WindowModelError::AlreadyDestroyed);
            }
            return Err(WindowModelError::NotFound);
        };
        if &current.snapshot.window != reference {
            return Err(WindowModelError::StaleReference);
        }

        let destroyed_revision = self.next_revision()?;
        self.live.remove(&reference.xid);
        let expires_at = now
            .checked_add(self.limits.tombstone_ttl_ms)
            .ok_or(WindowModelError::ClockOverflow)?;
        let tombstone = WindowTombstone {
            window: reference.clone(),
            destroyed_revision,
            expires_at,
        };
        self.tombstones.push_back(tombstone.clone());
        while self.tombstones.len() > self.limits.max_tombstones {
            self.tombstones.pop_front();
        }
        Ok(tombstone)
    }

    /// Resolves an exact identity against current live state.
    ///
    /// Effect executors must call this again immediately before the first X11
    /// effect; an earlier successful admission check is not sufficient.
    pub fn resolve_exact(
        &mut self,
        reference: &WindowRef,
        now: MonotonicMillis,
    ) -> Result<ResolvedWindow, WindowModelError> {
        self.advance_time(now)?;
        reference
            .validate()
            .map_err(WindowModelError::InvalidReference)?;
        self.require_scope(reference)?;
        if let Some(current) = self.live.get(&reference.xid) {
            if &current.snapshot.window != reference {
                return Err(WindowModelError::StaleReference);
            }
            return Ok(ResolvedWindow {
                revision: self.revision,
                snapshot: project_snapshot(current, self.revision),
            });
        }
        if self
            .tombstones
            .iter()
            .any(|entry| &entry.window == reference)
        {
            return Err(WindowModelError::DestroyedReference);
        }
        Err(WindowModelError::NotFound)
    }

    /// Captures all live windows under one model revision in deterministic XID
    /// order. Higher-level query ordering is applied to this immutable copy.
    pub fn snapshot_all(
        &mut self,
        now: MonotonicMillis,
    ) -> Result<(WindowModelRevision, Vec<WindowSnapshot>), WindowModelError> {
        self.advance_time(now)?;
        let snapshots = self
            .live
            .values()
            .map(|window| project_snapshot(window, self.revision))
            .collect();
        Ok((self.revision, snapshots))
    }

    /// Captures the atomic query view with an explicit creation revision for
    /// every exact window birth. Creation evidence is never inferred from a
    /// snapshot's last-change revision.
    pub fn snapshot_query_records(
        &mut self,
        now: MonotonicMillis,
    ) -> Result<(WindowModelRevision, Vec<WindowQueryRecord>), WindowModelError> {
        self.advance_time(now)?;
        let records = self
            .live
            .values()
            .map(|window| WindowQueryRecord {
                snapshot: project_snapshot(window, self.revision),
                created_revision: window.created_revision,
            })
            .collect();
        Ok((self.revision, records))
    }

    fn require_scope(&self, reference: &WindowRef) -> Result<(), WindowModelError> {
        if reference.desktop_id != self.desktop_id
            || reference.desktop_generation != self.desktop_generation
        {
            return Err(WindowModelError::WrongDesktopLifetime);
        }
        Ok(())
    }

    fn next_revision(&mut self) -> Result<WindowModelRevision, WindowModelError> {
        let next = self
            .revision
            .get()
            .checked_add(1)
            .ok_or(WindowModelError::RevisionExhausted)?;
        let revision = WindowModelRevision::new(next).map_err(WindowModelError::InvalidSnapshot)?;
        self.revision = revision;
        Ok(revision)
    }

    fn advance_time(&mut self, now: MonotonicMillis) -> Result<(), WindowModelError> {
        if now < self.last_now {
            return Err(WindowModelError::ClockMovedBackwards);
        }
        self.last_now = now;
        while self
            .tombstones
            .front()
            .is_some_and(|entry| entry.expires_at <= now)
        {
            self.tombstones.pop_front();
        }
        Ok(())
    }
}

fn project_snapshot(window: &LiveWindow, revision: WindowModelRevision) -> WindowSnapshot {
    let mut snapshot = window.snapshot.clone();
    snapshot.model_revision = revision;
    snapshot
}

/// Closed failures from exact window identity state transitions.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WindowModelError {
    /// Model scope identifiers cannot be nil.
    #[error("window model scope contains a nil identifier")]
    NilIdentifier,
    /// A model capacity or retention limit is zero.
    #[error("window model limits must be non-zero")]
    InvalidLimits,
    /// A snapshot failed protocol-level validation.
    #[error("window snapshot is invalid")]
    InvalidSnapshot(WindowValidationError),
    /// A reference failed protocol-level shape validation.
    #[error("window reference is invalid")]
    InvalidReference(WindowValidationError),
    /// The reference belongs to another desktop lifetime.
    #[error("window reference belongs to another desktop lifetime")]
    WrongDesktopLifetime,
    /// A different exact birth already owns the XID.
    #[error("a different window birth already owns this XID")]
    XidAlreadyLive,
    /// A newly observed XID did not carry its next birth counter.
    #[error("window birth counter mismatch: expected {expected}, received {actual}")]
    UnexpectedBirthGeneration {
        /// Required next counter value.
        expected: u64,
        /// Adapter-supplied counter value.
        actual: u64,
    },
    /// The live-window capacity is full.
    #[error("window model live capacity is exhausted")]
    LiveCapacityExhausted,
    /// The per-XID birth counter cannot advance.
    #[error("window observed-generation counter is exhausted")]
    BirthGenerationExhausted,
    /// The actor-local model revision cannot advance.
    #[error("window model revision is exhausted")]
    RevisionExhausted,
    /// A tombstone expiry could not be represented.
    #[error("window tombstone expiry overflowed")]
    ClockOverflow,
    /// Caller-supplied monotonic time regressed.
    #[error("window model monotonic clock moved backwards")]
    ClockMovedBackwards,
    /// No current or retained identity matches the reference.
    #[error("window identity was not found")]
    NotFound,
    /// The same XID is live, but for another exact birth.
    #[error("window reference is stale")]
    StaleReference,
    /// The exact identity is retained as destroyed.
    #[error("window reference was destroyed")]
    DestroyedReference,
    /// The exact identity was already destroyed.
    #[error("window reference was already destroyed")]
    AlreadyDestroyed,
}

#[cfg(test)]
mod tests {
    use xenoteer_protocol::{
        WindowIdentityHash, WindowMapState, WindowMetadata, WindowObservedState,
        WindowProcessConfidence, WindowProcessCorrelation, WindowSnapshotWarning,
    };

    use super::*;

    fn snapshot(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        xid: u32,
        birth: u64,
        hash_byte: char,
    ) -> Result<WindowSnapshot, Box<dyn std::error::Error>> {
        let window = WindowRef {
            desktop_id,
            desktop_generation: generation,
            xid,
            observed_generation: birth,
            identity_hash: WindowIdentityHash::new(hash_byte.to_string().repeat(64))?,
        };
        Ok(WindowSnapshot {
            xid_hex: window.xid_hex(),
            window,
            model_revision: WindowModelRevision::new(1)?,
            metadata: WindowMetadata {
                title: None,
                visible_title: None,
                icon_title: None,
                class: None,
                client_machine: None,
                window_types: Vec::new(),
                states: Vec::new(),
                allowed_actions: Vec::new(),
                protocols: Vec::new(),
            },
            process: WindowProcessCorrelation {
                reported_pid: None,
                managed_process: None,
                confidence: WindowProcessConfidence::None,
                evidence: Vec::new(),
                conflict: false,
            },
            state: WindowObservedState {
                map_state: WindowMapState::Viewable,
                minimized: false,
                hidden: false,
                urgent: false,
                modal: false,
                sticky: false,
                active: false,
                focused: false,
            },
            geometry: None,
            workspace: None,
            client_leader: None,
            transient_for: None,
            group_leader: None,
            stacking_index: None,
            has_accessibility_application: false,
            warnings: Vec::<WindowSnapshotWarning>::new(),
        })
    }

    #[test]
    fn xid_reuse_never_retargets_an_old_reference() -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let mut model = WindowModel::new(desktop_id, generation, WindowModelLimits::default())?;
        let first = snapshot(desktop_id, generation, 42, 1, 'a')?;
        let first_ref = first.window.clone();
        assert!(matches!(
            model.observe(first, MonotonicMillis::new(1))?,
            WindowModelChange::Created(_)
        ));
        model.destroy(&first_ref, MonotonicMillis::new(2))?;

        let second = snapshot(desktop_id, generation, 42, 2, 'b')?;
        model.observe(second.clone(), MonotonicMillis::new(3))?;
        assert_eq!(
            model.resolve_exact(&first_ref, MonotonicMillis::new(4)),
            Err(WindowModelError::StaleReference)
        );
        assert_eq!(
            model
                .resolve_exact(&second.window, MonotonicMillis::new(4))?
                .snapshot
                .window,
            second.window
        );
        Ok(())
    }

    #[test]
    fn queued_effect_must_revalidate_after_destroy() -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let mut model = WindowModel::new(desktop_id, generation, WindowModelLimits::default())?;
        let value = snapshot(desktop_id, generation, 7, 1, 'c')?;
        let reference = value.window.clone();
        model.observe(value, MonotonicMillis::new(10))?;
        model.resolve_exact(&reference, MonotonicMillis::new(11))?;
        model.destroy(&reference, MonotonicMillis::new(12))?;
        assert_eq!(
            model.resolve_exact(&reference, MonotonicMillis::new(13)),
            Err(WindowModelError::DestroyedReference)
        );
        Ok(())
    }

    #[test]
    fn invalid_birth_counter_and_clock_regression_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let mut model = WindowModel::new(desktop_id, generation, WindowModelLimits::default())?;
        let wrong_birth = snapshot(desktop_id, generation, 9, 2, 'd')?;
        assert!(matches!(
            model.observe(wrong_birth, MonotonicMillis::new(20)),
            Err(WindowModelError::UnexpectedBirthGeneration { .. })
        ));
        assert_eq!(
            model.snapshot_all(MonotonicMillis::new(19)),
            Err(WindowModelError::ClockMovedBackwards)
        );
        Ok(())
    }

    #[test]
    fn tombstones_expire_without_reauthorizing_old_refs() -> Result<(), Box<dyn std::error::Error>>
    {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let mut model = WindowModel::new(
            desktop_id,
            generation,
            WindowModelLimits {
                max_live_windows: 2,
                max_tombstones: 2,
                tombstone_ttl_ms: 5,
            },
        )?;
        let value = snapshot(desktop_id, generation, 11, 1, 'e')?;
        let reference = value.window.clone();
        model.observe(value, MonotonicMillis::new(1))?;
        model.destroy(&reference, MonotonicMillis::new(2))?;
        assert_eq!(model.counts(), (0, 1));
        model.snapshot_all(MonotonicMillis::new(7))?;
        assert_eq!(model.counts(), (0, 0));
        assert_eq!(
            model.resolve_exact(&reference, MonotonicMillis::new(8)),
            Err(WindowModelError::NotFound)
        );
        Ok(())
    }
}
