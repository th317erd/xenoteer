//! Bounded, backend-independent accessibility identity and query model.
//!
//! The AT-SPI actor is the sole writer of [`AccessibilityCache`].  Readers use
//! immutable [`AccessibilitySnapshot`] values, so a continuation can never
//! silently move to a newer cache revision.  All externally usable identities
//! are fenced by desktop, AT-SPI, application-instance, and object-birth
//! generations.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use regex_automata::{
    nfa::thompson::{
        NFA,
        pikevm::{Cache as PikeCache, PikeVM},
    },
    util::syntax,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use xenoteer_protocol::{
    AccessibilityIdentityHash, AccessibilityQueryLimits, AccessibilityRevision,
    AccessibilityValidationError, AccessibilityWarning, ApplicationRef, AtspiBusName,
    AtspiGeneration, AtspiObjectPath, CoordinateSpace, DesktopGeneration, DesktopId,
    ElementCompleteness, ElementInterface, ElementOrder, ElementPredicate, ElementRef,
    ElementRelationType, ElementRole, ElementScope, ElementSelector, ElementSnapshot,
    ElementSnapshotEntry, ElementSnapshotExpansion, ElementState, ElementStringMatch,
    ElementWaitPredicate, ElementWaitQuantifier, ElementWaitRequest, ElementWaitTarget,
    MAX_ACCESSIBILITY_REGEX_BYTES, MAX_ACCESSIBILITY_WARNINGS, Rect, WindowCorrelationConfidence,
};

const SELECTOR_FINGERPRINT_DOMAIN: &[u8] = b"xenoteer-accessibility-selector-v1\0";

/// Default simultaneous live-object ceiling for one desktop.
pub const DEFAULT_MAX_LIVE_ACCESSIBILITY_NODES: usize = 100_000;
/// Default retained stale-object ceiling for one desktop.
pub const DEFAULT_MAX_ACCESSIBILITY_TOMBSTONES: usize = 100_000;

/// Hard capacities owned by the accessibility actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessibilityModelLimits {
    /// Maximum simultaneously live element births and retained distinct
    /// application identity keys.
    ///
    /// Application keys reuse this ceiling so older configuration shapes stay
    /// valid while register/remove churn remains bounded.
    pub max_live_nodes: usize,
    /// Maximum recently removed exact references.
    pub max_tombstones: usize,
}

impl Default for AccessibilityModelLimits {
    fn default() -> Self {
        Self {
            max_live_nodes: DEFAULT_MAX_LIVE_ACCESSIBILITY_NODES,
            max_tombstones: DEFAULT_MAX_ACCESSIBILITY_TOMBSTONES,
        }
    }
}

impl AccessibilityModelLimits {
    /// Rejects capacities that cannot retain useful fencing evidence.
    pub const fn validate(self) -> Result<Self, AccessibilityModelError> {
        if self.max_live_nodes == 0 || self.max_tombstones == 0 {
            return Err(AccessibilityModelError::InvalidLimits);
        }
        Ok(self)
    }
}

/// Why the cache crossed a barrier and invalidated all prior references.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessibilityResyncReason {
    /// The adapter reported a gap in its ordered event stream.
    EventGap,
    /// The bounded live-node capacity was exhausted.
    LiveCapacity,
    /// The retained distinct application-identity capacity was exhausted.
    ApplicationCapacity,
    /// The AT-SPI bus connection was replaced.
    BusReset,
    /// Graph evidence was contradictory after a bootstrap/reconciliation pass.
    MalformedGraph,
}

/// The result of a cache-wide invalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessibilityResyncBarrier {
    /// Generation accepted after the barrier.
    pub atspi_generation: AtspiGeneration,
    /// Empty-cache revision after the barrier.
    pub revision: AccessibilityRevision,
    /// Evidence that caused the barrier.
    pub reason: AccessibilityResyncReason,
}

/// Bounded evidence that an exact object birth is no longer live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibilityTombstone {
    /// Exact removed identity; it never aliases a path successor.
    pub element: ElementRef,
    /// Revision that removed the object.
    pub removed_revision: AccessibilityRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ApplicationKey {
    bus: AtspiBusName,
    root: AtspiObjectPath,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ObjectKey {
    application: ApplicationKey,
    path: AtspiObjectPath,
}

/// Single-writer, bounded identity/cache state for one desktop lifetime.
#[derive(Debug)]
pub struct AccessibilityCache {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    atspi_generation: AtspiGeneration,
    revision: AccessibilityRevision,
    limits: AccessibilityModelLimits,
    applications: BTreeMap<ApplicationKey, ApplicationRef>,
    last_application_generation: BTreeMap<ApplicationKey, u64>,
    nodes: BTreeMap<ObjectKey, ElementSnapshot>,
    protected_births: BTreeSet<ObjectKey>,
    tombstones: VecDeque<AccessibilityTombstone>,
    last_cache_sequence: u64,
}

impl AccessibilityCache {
    /// Creates an empty cache for exactly one desktop and AT-SPI bus lifetime.
    pub fn new(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        atspi_generation: AtspiGeneration,
        limits: AccessibilityModelLimits,
    ) -> Result<Self, AccessibilityModelError> {
        if desktop_id.as_uuid().is_nil() || desktop_generation.as_uuid().is_nil() {
            return Err(AccessibilityModelError::NilIdentifier);
        }
        Ok(Self {
            desktop_id,
            desktop_generation,
            atspi_generation,
            revision: AccessibilityRevision::new(1)
                .map_err(AccessibilityModelError::InvalidSnapshot)?,
            limits: limits.validate()?,
            applications: BTreeMap::new(),
            last_application_generation: BTreeMap::new(),
            nodes: BTreeMap::new(),
            protected_births: BTreeSet::new(),
            tombstones: VecDeque::new(),
            last_cache_sequence: 0,
        })
    }

    /// Current AT-SPI connection generation.
    #[must_use]
    pub const fn atspi_generation(&self) -> AtspiGeneration {
        self.atspi_generation
    }

    /// Current atomic cache revision.
    #[must_use]
    pub const fn revision(&self) -> AccessibilityRevision {
        self.revision
    }

    /// Current live-node and tombstone cardinalities.
    #[must_use]
    pub fn counts(&self) -> (usize, usize) {
        (self.nodes.len(), self.tombstones.len())
    }

    /// Registers an application owner, fencing owner/root reuse with a new
    /// application-instance generation.
    pub fn register_application(
        &mut self,
        unique_bus_name: AtspiBusName,
        root_object_path: AtspiObjectPath,
        identity_hash: AccessibilityIdentityHash,
    ) -> Result<ApplicationRef, AccessibilityModelError> {
        self.replace_application(unique_bus_name, root_object_path, identity_hash, false)
    }

    /// Fences an observed application-owner restart even when its derived
    /// identity evidence happens to equal the preceding instance.
    pub fn restart_application(
        &mut self,
        unique_bus_name: AtspiBusName,
        root_object_path: AtspiObjectPath,
        identity_hash: AccessibilityIdentityHash,
    ) -> Result<ApplicationRef, AccessibilityModelError> {
        self.replace_application(unique_bus_name, root_object_path, identity_hash, true)
    }

    /// Retires one exact application instance and all of its live nodes.
    pub fn remove_application(
        &mut self,
        application: &ApplicationRef,
    ) -> Result<(), AccessibilityModelError> {
        self.require_application(application)?;
        let key = application_key(application);
        let removed = self.application_elements(&key);
        let removal_revision = if removed.is_empty() {
            None
        } else {
            Some(self.revision_after(1)?)
        };
        let final_revision = self.revision_after(if removed.is_empty() { 1 } else { 2 })?;
        if let Some(revision) = removal_revision {
            self.remove_application_nodes_at(removed, revision);
        }
        self.applications.remove(&key);
        self.revision = final_revision;
        Ok(())
    }

    /// Constructs the only reference accepted for a new object birth.
    pub fn next_element_ref(
        &self,
        application: &ApplicationRef,
        object_path: AtspiObjectPath,
        object_identity_hash: AccessibilityIdentityHash,
    ) -> Result<ElementRef, AccessibilityModelError> {
        self.require_application(application)?;
        let cache_sequence = self
            .last_cache_sequence
            .checked_add(1)
            .ok_or(AccessibilityModelError::SequenceExhausted)?;
        Ok(ElementRef {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            atspi_generation: self.atspi_generation,
            application: application.clone(),
            object_path,
            object_identity_hash,
            cache_sequence,
        })
    }

    /// Inserts a new exact birth or refreshes the currently live exact birth.
    ///
    /// Protected text and value strings are removed before validation and can
    /// therefore never enter an immutable public snapshot.
    pub fn observe(
        &mut self,
        mut snapshot: ElementSnapshot,
    ) -> Result<AccessibilityRevision, AccessibilityModelError> {
        self.require_application(&snapshot.element.application)?;
        self.require_element_scope(&snapshot.element)?;
        let key = object_key(&snapshot.element);
        let is_refresh = self
            .nodes
            .get(&key)
            .is_some_and(|current| current.element == snapshot.element);
        let protected = snapshot_declares_protected(&snapshot)
            || (is_refresh && self.protected_births.contains(&key));
        redact_protected(&mut snapshot, protected);
        snapshot
            .validate()
            .map_err(AccessibilityModelError::InvalidSnapshot)?;
        if !is_refresh {
            if snapshot.element.cache_sequence
                != self
                    .last_cache_sequence
                    .checked_add(1)
                    .ok_or(AccessibilityModelError::SequenceExhausted)?
            {
                return Err(AccessibilityModelError::UnexpectedCacheSequence);
            }
            if self.nodes.len() >= self.limits.max_live_nodes && !self.nodes.contains_key(&key) {
                let barrier = self.force_resync(AccessibilityResyncReason::LiveCapacity)?;
                return Err(AccessibilityModelError::ResyncRequired(barrier));
            }
        }
        let revision = self.next_revision()?;
        if !is_refresh {
            if let Some(replaced) = self.nodes.remove(&key) {
                self.push_tombstone(replaced.element, revision);
            }
            self.protected_births.remove(&key);
            self.last_cache_sequence = snapshot.element.cache_sequence;
        }
        snapshot.revision = revision;
        self.nodes.insert(key.clone(), snapshot);
        if protected {
            self.protected_births.insert(key);
        }
        self.revision = revision;
        Ok(revision)
    }

    /// Removes exactly one live birth. A stale reference never removes its path successor.
    pub fn remove(
        &mut self,
        element: &ElementRef,
    ) -> Result<AccessibilityTombstone, AccessibilityModelError> {
        self.require_element_scope(element)?;
        let key = object_key(element);
        let Some(current) = self.nodes.get(&key) else {
            return if self
                .tombstones
                .iter()
                .any(|entry| &entry.element == element)
            {
                Err(AccessibilityModelError::AlreadyRemoved)
            } else {
                Err(AccessibilityModelError::NotFound)
            };
        };
        if &current.element != element {
            return Err(AccessibilityModelError::StaleReference);
        }
        let revision = self.next_revision()?;
        let removed = self
            .nodes
            .remove(&key)
            .ok_or(AccessibilityModelError::NotFound)?;
        self.protected_births.remove(&key);
        self.revision = revision;
        let tombstone = AccessibilityTombstone {
            element: removed.element,
            removed_revision: revision,
        };
        self.tombstones.push_back(tombstone.clone());
        while self.tombstones.len() > self.limits.max_tombstones {
            self.tombstones.pop_front();
        }
        Ok(tombstone)
    }

    /// Resolves only the exact currently live object birth.
    pub fn resolve_exact(
        &self,
        element: &ElementRef,
    ) -> Result<&ElementSnapshot, AccessibilityModelError> {
        self.require_element_scope(element)?;
        match self.nodes.get(&object_key(element)) {
            Some(snapshot) if &snapshot.element == element => Ok(snapshot),
            Some(_) => Err(AccessibilityModelError::StaleReference),
            None if self
                .tombstones
                .iter()
                .any(|entry| &entry.element == element) =>
            {
                Err(AccessibilityModelError::StaleReference)
            }
            None => Err(AccessibilityModelError::NotFound),
        }
    }

    /// Invalidates the cache after an event gap. The returned generation must
    /// be used when the adapter rebuilds its subscription and `GetItems` state.
    pub fn event_gap(&mut self) -> Result<AccessibilityResyncBarrier, AccessibilityModelError> {
        self.force_resync(AccessibilityResyncReason::EventGap)
    }

    /// Invalidates the cache after replacing the AT-SPI connection.
    pub fn reset_bus(&mut self) -> Result<AccessibilityResyncBarrier, AccessibilityModelError> {
        self.force_resync(AccessibilityResyncReason::BusReset)
    }

    /// Invalidates graph state after immutable diagnostics require reconciliation.
    pub fn malformed_graph(
        &mut self,
    ) -> Result<AccessibilityResyncBarrier, AccessibilityModelError> {
        self.force_resync(AccessibilityResyncReason::MalformedGraph)
    }

    /// Captures an immutable, common-revision query view.
    #[must_use]
    pub fn snapshot(&self) -> AccessibilitySnapshot {
        AccessibilitySnapshot::build(
            self.desktop_id,
            self.desktop_generation,
            self.atspi_generation,
            self.revision,
            self.applications.values().cloned().collect(),
            self.nodes.values().cloned().collect(),
        )
    }

    /// First half of the actor's check-register-recheck wait protocol.
    pub fn prepare_wait(
        &self,
        request: ElementWaitRequest,
    ) -> Result<PreparedAccessibilityWait, AccessibilityQueryError> {
        let view = self.snapshot();
        let evaluation = view.evaluate_wait(&request)?;
        Ok(PreparedAccessibilityWait {
            request,
            registered_at: self.revision,
            atspi_generation: self.atspi_generation,
            initial: evaluation,
        })
    }

    /// Re-evaluates after the caller inserted the prepared wait into its waiter
    /// table. A change between the first check and registration is observable.
    pub fn recheck_wait(
        &self,
        prepared: &PreparedAccessibilityWait,
    ) -> Result<AccessibilityWaitEvaluation, AccessibilityQueryError> {
        if prepared.atspi_generation != self.atspi_generation {
            return Err(AccessibilityQueryError::ResyncRequired);
        }
        if prepared.registered_at == self.revision {
            return Ok(prepared.initial.clone());
        }
        self.snapshot().evaluate_wait(&prepared.request)
    }

    fn force_resync(
        &mut self,
        reason: AccessibilityResyncReason,
    ) -> Result<AccessibilityResyncBarrier, AccessibilityModelError> {
        let next = self
            .atspi_generation
            .get()
            .checked_add(1)
            .ok_or(AccessibilityModelError::GenerationExhausted)?;
        let atspi_generation =
            AtspiGeneration::new(next).map_err(AccessibilityModelError::InvalidSnapshot)?;
        let revision = self.next_revision()?;
        self.atspi_generation = atspi_generation;
        self.nodes.clear();
        self.protected_births.clear();
        self.applications.clear();
        self.last_application_generation.clear();
        self.tombstones.clear();
        self.last_cache_sequence = 0;
        self.revision = revision;
        Ok(AccessibilityResyncBarrier {
            atspi_generation: self.atspi_generation,
            revision: self.revision,
            reason,
        })
    }

    fn replace_application(
        &mut self,
        unique_bus_name: AtspiBusName,
        root_object_path: AtspiObjectPath,
        identity_hash: AccessibilityIdentityHash,
        force_restart: bool,
    ) -> Result<ApplicationRef, AccessibilityModelError> {
        let key = ApplicationKey {
            bus: unique_bus_name.clone(),
            root: root_object_path.clone(),
        };
        if !force_restart
            && let Some(existing) = self.applications.get(&key)
            && existing.identity_hash == identity_hash
        {
            return Ok(existing.clone());
        }
        if !self.last_application_generation.contains_key(&key)
            && self.last_application_generation.len() >= self.limits.max_live_nodes
        {
            let barrier = self.force_resync(AccessibilityResyncReason::ApplicationCapacity)?;
            return Err(AccessibilityModelError::ResyncRequired(barrier));
        }
        let generation = self
            .last_application_generation
            .get(&key)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(AccessibilityModelError::GenerationExhausted)?;
        let application = ApplicationRef {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            atspi_generation: self.atspi_generation,
            unique_bus_name,
            root_object_path,
            app_instance_generation: generation,
            identity_hash,
        };
        application
            .validate()
            .map_err(AccessibilityModelError::InvalidApplication)?;
        let removed = self.application_elements(&key);
        let removal_revision = if removed.is_empty() {
            None
        } else {
            Some(self.revision_after(1)?)
        };
        let final_revision = self.revision_after(if removed.is_empty() { 1 } else { 2 })?;

        if let Some(revision) = removal_revision {
            self.remove_application_nodes_at(removed, revision);
        }
        self.last_application_generation
            .insert(key.clone(), generation);
        self.applications.insert(key, application.clone());
        self.revision = final_revision;
        Ok(application)
    }

    fn application_elements(&self, key: &ApplicationKey) -> Vec<ElementRef> {
        self.nodes
            .iter()
            .filter(|(object, _)| &object.application == key)
            .map(|(_, snapshot)| snapshot.element.clone())
            .collect()
    }

    fn remove_application_nodes_at(
        &mut self,
        removed: Vec<ElementRef>,
        revision: AccessibilityRevision,
    ) {
        for element in removed {
            let key = object_key(&element);
            self.nodes.remove(&key);
            self.protected_births.remove(&key);
            self.push_tombstone(element, revision);
        }
    }

    fn push_tombstone(&mut self, element: ElementRef, removed_revision: AccessibilityRevision) {
        self.tombstones.push_back(AccessibilityTombstone {
            element,
            removed_revision,
        });
        while self.tombstones.len() > self.limits.max_tombstones {
            self.tombstones.pop_front();
        }
    }

    fn require_application(
        &self,
        application: &ApplicationRef,
    ) -> Result<(), AccessibilityModelError> {
        application
            .validate()
            .map_err(AccessibilityModelError::InvalidApplication)?;
        if application.desktop_id != self.desktop_id
            || application.desktop_generation != self.desktop_generation
            || application.atspi_generation != self.atspi_generation
        {
            return Err(AccessibilityModelError::StaleGeneration);
        }
        match self.applications.get(&application_key(application)) {
            Some(current) if current == application => Ok(()),
            Some(_) => Err(AccessibilityModelError::StaleApplication),
            None => Err(AccessibilityModelError::ApplicationNotFound),
        }
    }

    fn require_element_scope(&self, element: &ElementRef) -> Result<(), AccessibilityModelError> {
        element
            .validate()
            .map_err(AccessibilityModelError::InvalidReference)?;
        if element.desktop_id != self.desktop_id
            || element.desktop_generation != self.desktop_generation
            || element.atspi_generation != self.atspi_generation
        {
            return Err(AccessibilityModelError::StaleGeneration);
        }
        self.require_application(&element.application)
    }

    fn next_revision(&self) -> Result<AccessibilityRevision, AccessibilityModelError> {
        self.revision_after(1)
    }

    fn revision_after(&self, steps: u64) -> Result<AccessibilityRevision, AccessibilityModelError> {
        AccessibilityRevision::new(
            self.revision
                .get()
                .checked_add(steps)
                .ok_or(AccessibilityModelError::RevisionExhausted)?,
        )
        .map_err(AccessibilityModelError::InvalidSnapshot)
    }

    #[cfg(test)]
    fn set_revision_for_exhaustion_test(&mut self, value: u64) {
        self.revision = AccessibilityRevision::new(value)
            .unwrap_or_else(|_| unreachable!("nonzero test revision is valid"));
    }

    #[cfg(test)]
    fn set_cache_sequence_for_exhaustion_test(&mut self, value: u64) {
        self.last_cache_sequence = value;
    }

    #[cfg(test)]
    fn set_application_generation_for_exhaustion_test(
        &mut self,
        application: &ApplicationRef,
        value: u64,
    ) {
        self.last_application_generation
            .insert(application_key(application), value);
    }
}

/// Stable traversal algorithms available to actor reconciliation code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessibilityTraversalOrder {
    /// Parent before descendants.
    Preorder,
    /// Descendants before parent.
    Postorder,
    /// Level-by-level traversal.
    BreadthFirst,
}

/// Immutable graph diagnostics captured with a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibilityGraphStatus {
    /// Contradictory or incomplete topology was observed.
    pub dirty: bool,
    /// The writer should reconcile from an authoritative cache snapshot.
    pub resync_required: bool,
    /// Bounded public diagnostic details.
    pub warnings: Vec<AccessibilityWarning>,
}

/// One immutable accessibility model revision.
#[derive(Debug, Clone)]
pub struct AccessibilitySnapshot {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    atspi_generation: AtspiGeneration,
    revision: AccessibilityRevision,
    applications: HashSet<ApplicationRef>,
    nodes: Vec<ElementSnapshot>,
    indices: HashMap<ElementRef, usize>,
    children: Vec<Vec<usize>>,
    roots: Vec<usize>,
    graph: AccessibilityGraphStatus,
}

impl AccessibilitySnapshot {
    fn build(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        atspi_generation: AtspiGeneration,
        revision: AccessibilityRevision,
        applications: Vec<ApplicationRef>,
        mut nodes: Vec<ElementSnapshot>,
    ) -> Self {
        nodes.sort_by(compare_snapshot_identity);
        let indices = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.element.clone(), index))
            .collect::<HashMap<_, _>>();
        let mut children = vec![Vec::new(); nodes.len()];
        let mut roots = Vec::new();
        let mut warnings = Vec::new();
        for (index, node) in nodes.iter().enumerate() {
            match node
                .parent
                .as_ref()
                .and_then(|parent| indices.get(parent).copied())
            {
                Some(parent) if parent != index => children[parent].push(index),
                Some(_) => {
                    roots.push(index);
                    push_warning(
                        &mut warnings,
                        "accessibility.self_parent",
                        "element names itself as parent",
                    );
                }
                None if node.parent.is_some() => {
                    roots.push(index);
                    push_warning(
                        &mut warnings,
                        "accessibility.orphan",
                        "element parent is absent from the immutable snapshot",
                    );
                }
                None => roots.push(index),
            }
        }
        for siblings in &mut children {
            siblings.sort_by(|left, right| compare_child(&nodes[*left], &nodes[*right]));
        }
        for (parent, siblings) in children.iter().enumerate() {
            let mut indices = HashSet::new();
            let duplicate_index = siblings.iter().any(|child| {
                nodes[*child]
                    .index_in_parent
                    .is_some_and(|index| !indices.insert(index))
            });
            let declared_count_conflict = nodes[parent].child_count.is_some_and(|declared| {
                usize::try_from(declared).map_or(true, |declared| siblings.len() > declared)
                    || siblings.iter().any(|child| {
                        nodes[*child]
                            .index_in_parent
                            .is_some_and(|index| index >= declared)
                    })
            });
            if duplicate_index || declared_count_conflict {
                push_warning(
                    &mut warnings,
                    "accessibility.parent_metadata_conflict",
                    "child indices or declared parent child count are contradictory",
                );
            }
        }
        roots.sort_by(|left, right| compare_child(&nodes[*left], &nodes[*right]));

        let mut reached = HashSet::new();
        for root in roots.clone() {
            mark_reachable(root, &children, &mut reached);
        }
        if reached.len() != nodes.len() {
            push_warning(
                &mut warnings,
                "accessibility.cycle",
                "cycle or parent-only component detached from all roots",
            );
            for index in 0..nodes.len() {
                if !reached.contains(&index) {
                    roots.push(index);
                    mark_reachable(index, &children, &mut reached);
                }
            }
        }
        let dirty = !warnings.is_empty();
        Self {
            desktop_id,
            desktop_generation,
            atspi_generation,
            revision,
            applications: applications.into_iter().collect(),
            nodes,
            indices,
            children,
            roots,
            graph: AccessibilityGraphStatus {
                dirty,
                resync_required: dirty,
                warnings,
            },
        }
    }

    /// Captured atomic cache revision.
    #[must_use]
    pub const fn revision(&self) -> AccessibilityRevision {
        self.revision
    }

    /// Captured graph integrity evidence.
    #[must_use]
    pub const fn graph_status(&self) -> &AccessibilityGraphStatus {
        &self.graph
    }

    /// Number of immutable live snapshots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether this view has no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Traverses every element exactly once despite cycles or malformed parents.
    #[must_use]
    pub fn traverse(&self, order: AccessibilityTraversalOrder) -> Vec<&ElementSnapshot> {
        self.traversal_indices(order)
            .into_iter()
            .map(|index| &self.nodes[index])
            .collect()
    }

    /// Resolves an exact birth at this immutable revision.
    pub fn resolve_exact(
        &self,
        element: &ElementRef,
        expansion: ElementSnapshotExpansion,
    ) -> Result<ElementSnapshotEntry, AccessibilityQueryError> {
        let index = self
            .indices
            .get(element)
            .copied()
            .ok_or(AccessibilityQueryError::StaleReference)?;
        Ok(ElementSnapshotEntry {
            snapshot: project_snapshot(&self.nodes[index], expansion),
        })
    }

    /// Evaluates a selector and returns a deterministic immutable slice.
    pub fn query(
        &self,
        selector: &ElementSelector,
        expansion: ElementSnapshotExpansion,
        limits: AccessibilityQueryLimits,
        limit: u16,
        offset: u32,
    ) -> Result<AccessibilityQueryProjection, AccessibilityQueryError> {
        limits
            .validate()
            .map_err(AccessibilityQueryError::InvalidRequest)?;
        let deadline = AccessibilityQueryDeadline::from_timeout_ms(limits.timeout_ms)?;
        self.query_with_deadline(selector, expansion, limits, limit, offset, deadline)
    }

    /// Evaluates a selector under a pre-established whole-request deadline.
    pub fn query_with_deadline(
        &self,
        selector: &ElementSelector,
        expansion: ElementSnapshotExpansion,
        limits: AccessibilityQueryLimits,
        limit: u16,
        offset: u32,
        deadline: AccessibilityQueryDeadline,
    ) -> Result<AccessibilityQueryProjection, AccessibilityQueryError> {
        deadline.check()?;
        selector
            .validate_for(self.desktop_id, self.desktop_generation)
            .map_err(AccessibilityQueryError::InvalidRequest)?;
        expansion
            .validate()
            .map_err(AccessibilityQueryError::InvalidRequest)?;
        limits
            .validate()
            .map_err(AccessibilityQueryError::InvalidRequest)?;
        if limit == 0 || limit > limits.max_matches {
            return Err(AccessibilityQueryError::InvalidPageLimit);
        }
        self.require_clean_graph()?;
        self.require_selector_generation(selector)?;
        let selector_fingerprint = accessibility_selector_fingerprint(selector)?;
        let mut compiled = CompiledSelector::compile(selector)?;
        deadline.check()?;
        let scoped = self.scope_indices(&selector.scope, limits.max_depth, deadline)?;
        let mut visited = 0_u32;
        let mut matches = Vec::new();
        for (index, depth) in scoped {
            deadline.check()?;
            visited = visited
                .checked_add(1)
                .ok_or(AccessibilityQueryError::LimitExceeded(
                    QueryLimit::VisitedNodes,
                ))?;
            if visited > limits.max_visited_nodes {
                return Err(AccessibilityQueryError::LimitExceeded(
                    QueryLimit::VisitedNodes,
                ));
            }
            if depth > limits.max_depth {
                return Err(AccessibilityQueryError::LimitExceeded(QueryLimit::Depth));
            }
            if compiled.matches(&self.nodes[index]) {
                matches.push(index);
                if matches.len() > usize::from(limits.max_matches) {
                    return Err(AccessibilityQueryError::LimitExceeded(QueryLimit::Matches));
                }
            }
        }
        order_indices(
            &mut matches,
            &self.nodes,
            selector.order,
            &self.children,
            &self.roots,
        );
        deadline.check()?;
        if let Some(result_index) = selector.result_index {
            matches = usize::try_from(result_index)
                .ok()
                .and_then(|index| matches.get(index).copied())
                .into_iter()
                .collect();
        }
        let offset = usize::try_from(offset).map_err(|_| AccessibilityQueryError::Offset)?;
        if offset > matches.len() || (offset == matches.len() && offset != 0) {
            return Err(AccessibilityQueryError::Offset);
        }
        let end = offset.saturating_add(usize::from(limit)).min(matches.len());
        let elements = matches[offset..end]
            .iter()
            .map(|index| ElementSnapshotEntry {
                snapshot: project_snapshot(&self.nodes[*index], expansion),
            })
            .collect();
        let next_offset = if end < matches.len() {
            Some(
                u32::try_from(end)
                    .map_err(|_| AccessibilityQueryError::LimitExceeded(QueryLimit::Matches))?,
            )
        } else {
            None
        };
        let continuation = next_offset.map(|next_offset| AccessibilityContinuationDescriptor {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            atspi_generation: self.atspi_generation,
            snapshot_revision: self.revision,
            selector_fingerprint,
            order: selector.order,
            expansion,
            next_offset,
        });
        deadline.check()?;
        Ok(AccessibilityQueryProjection {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            atspi_generation: self.atspi_generation,
            snapshot_revision: self.revision,
            elements,
            visited_nodes: visited,
            total_matches: u32::try_from(matches.len())
                .map_err(|_| AccessibilityQueryError::LimitExceeded(QueryLimit::Matches))?,
            next_offset,
            continuation,
            warnings: self.graph.warnings.clone(),
        })
    }

    /// Returns an unfiltered deterministic slice within a scope.
    pub fn list(
        &self,
        scope: &ElementScope,
        order: ElementOrder,
        expansion: ElementSnapshotExpansion,
        limits: AccessibilityQueryLimits,
        limit: u16,
        offset: u32,
    ) -> Result<AccessibilityQueryProjection, AccessibilityQueryError> {
        limits
            .validate()
            .map_err(AccessibilityQueryError::InvalidRequest)?;
        let deadline = AccessibilityQueryDeadline::from_timeout_ms(limits.timeout_ms)?;
        self.list_with_deadline(scope, order, expansion, limits, limit, offset, deadline)
    }

    /// Returns an unfiltered slice under a pre-established request deadline.
    #[allow(
        clippy::too_many_arguments,
        reason = "the deadline-aware form deliberately mirrors the stable list API"
    )]
    pub fn list_with_deadline(
        &self,
        scope: &ElementScope,
        order: ElementOrder,
        expansion: ElementSnapshotExpansion,
        limits: AccessibilityQueryLimits,
        limit: u16,
        offset: u32,
        deadline: AccessibilityQueryDeadline,
    ) -> Result<AccessibilityQueryProjection, AccessibilityQueryError> {
        self.query_with_deadline(
            &ElementSelector {
                scope: scope.clone(),
                predicates: Vec::new(),
                order,
                result_index: None,
            },
            expansion,
            limits,
            limit,
            offset,
            deadline,
        )
    }

    /// Continues only when every cursor-bound query property still matches this
    /// exact immutable view. Principal authentication remains a server concern.
    pub fn continue_query(
        &self,
        selector: &ElementSelector,
        limits: AccessibilityQueryLimits,
        limit: u16,
        continuation: &AccessibilityContinuationDescriptor,
    ) -> Result<AccessibilityQueryProjection, AccessibilityQueryError> {
        limits
            .validate()
            .map_err(AccessibilityQueryError::InvalidRequest)?;
        let deadline = AccessibilityQueryDeadline::from_timeout_ms(limits.timeout_ms)?;
        self.continue_query_with_deadline(selector, limits, limit, continuation, deadline)
    }

    /// Continues a cursor-bound query under an existing absolute deadline.
    pub fn continue_query_with_deadline(
        &self,
        selector: &ElementSelector,
        limits: AccessibilityQueryLimits,
        limit: u16,
        continuation: &AccessibilityContinuationDescriptor,
        deadline: AccessibilityQueryDeadline,
    ) -> Result<AccessibilityQueryProjection, AccessibilityQueryError> {
        deadline.check()?;
        if continuation.desktop_id != self.desktop_id
            || continuation.desktop_generation != self.desktop_generation
            || continuation.atspi_generation != self.atspi_generation
            || continuation.snapshot_revision != self.revision
            || continuation.selector_fingerprint != accessibility_selector_fingerprint(selector)?
            || continuation.order != selector.order
        {
            return Err(AccessibilityQueryError::ContinuationMismatch);
        }
        self.query_with_deadline(
            selector,
            continuation.expansion,
            limits,
            limit,
            continuation.next_offset,
            deadline,
        )
    }

    /// Requires exactly one selector match, making ambiguity explicit.
    pub fn resolve_exactly_one(
        &self,
        selector: &ElementSelector,
        expansion: ElementSnapshotExpansion,
        limits: AccessibilityQueryLimits,
    ) -> Result<ElementSnapshotEntry, AccessibilityQueryError> {
        limits
            .validate()
            .map_err(AccessibilityQueryError::InvalidRequest)?;
        let deadline = AccessibilityQueryDeadline::from_timeout_ms(limits.timeout_ms)?;
        self.resolve_exactly_one_with_deadline(selector, expansion, limits, deadline)
    }

    /// Resolves exact-one under an existing absolute deadline.
    pub fn resolve_exactly_one_with_deadline(
        &self,
        selector: &ElementSelector,
        expansion: ElementSnapshotExpansion,
        limits: AccessibilityQueryLimits,
        deadline: AccessibilityQueryDeadline,
    ) -> Result<ElementSnapshotEntry, AccessibilityQueryError> {
        let projection =
            self.query_with_deadline(selector, expansion, limits, limits.max_matches, 0, deadline)?;
        match projection.total_matches {
            0 => Err(AccessibilityQueryError::NoMatch),
            1 => projection
                .elements
                .into_iter()
                .next()
                .ok_or(AccessibilityQueryError::NoMatch),
            matches => Err(AccessibilityQueryError::Ambiguous { matches }),
        }
    }

    /// Evaluates current wait state without retaining a waiter.
    pub fn evaluate_wait(
        &self,
        request: &ElementWaitRequest,
    ) -> Result<AccessibilityWaitEvaluation, AccessibilityQueryError> {
        request
            .validate()
            .map_err(AccessibilityQueryError::InvalidRequest)?;
        let deadline = AccessibilityQueryDeadline::from_timeout_ms(request.limits.timeout_ms)?;
        self.evaluate_wait_with_deadline(request, deadline)
    }

    /// Evaluates current wait state under an existing absolute deadline.
    pub fn evaluate_wait_with_deadline(
        &self,
        request: &ElementWaitRequest,
        deadline: AccessibilityQueryDeadline,
    ) -> Result<AccessibilityWaitEvaluation, AccessibilityQueryError> {
        deadline.check()?;
        request
            .validate()
            .map_err(AccessibilityQueryError::InvalidRequest)?;
        self.require_clean_graph()?;
        if request
            .after_revision
            .is_some_and(|revision| revision > self.revision)
        {
            return Err(AccessibilityQueryError::FutureRevision);
        }
        let reference_target = matches!(&request.target, ElementWaitTarget::Reference { .. });
        let (selected, quantifier) = match &request.target {
            ElementWaitTarget::Reference { element } => (
                self.indices.get(element).copied().into_iter().collect(),
                ElementWaitQuantifier::Any,
            ),
            ElementWaitTarget::Selector {
                selector,
                quantifier,
            } => {
                let projection = self.query_with_deadline(
                    selector,
                    request.expansion,
                    request.limits,
                    request.limits.max_matches,
                    0,
                    deadline,
                )?;
                let refs: Vec<usize> = projection
                    .elements
                    .iter()
                    .filter_map(|entry| self.indices.get(&entry.snapshot.element).copied())
                    .collect();
                (refs, *quantifier)
            }
        };
        let selected_count = selected.len() as u32;
        let selector_count_satisfied = match &request.predicate {
            ElementWaitPredicate::SelectorCount { minimum, maximum } => Some(
                selected_count >= *minimum
                    && maximum.is_none_or(|maximum| selected_count <= maximum),
            ),
            _ => None,
        };
        let mut satisfying = Vec::new();
        if selector_count_satisfied.is_none() {
            for index in selected.iter().copied() {
                deadline.check()?;
                if wait_predicate_matches(&self.nodes[index], &request.predicate)? {
                    satisfying.push(index);
                }
            }
        }
        let satisfying_count = satisfying.len() as u32;
        let predicate_satisfied_at_revision = if reference_target {
            match request.predicate {
                ElementWaitPredicate::Exists => selected_count == 1,
                ElementWaitPredicate::Gone => selected_count == 0,
                _ => satisfying_count == 1,
            }
        } else {
            selector_count_satisfied.unwrap_or(match quantifier {
                ElementWaitQuantifier::Any => satisfying_count > 0,
                ElementWaitQuantifier::All => {
                    selected_count > 0 && satisfying_count == selected_count
                }
                ElementWaitQuantifier::ExactlyOne => satisfying_count == 1,
                ElementWaitQuantifier::None => satisfying_count == 0,
            })
        };
        let after_boundary_met = request
            .after_revision
            .is_none_or(|after| self.revision > after);
        deadline.check()?;
        Ok(AccessibilityWaitEvaluation {
            evaluated_revision: self.revision,
            predicate_satisfied: predicate_satisfied_at_revision && after_boundary_met,
            selected_count,
            satisfying_count,
            satisfying_elements: satisfying
                .into_iter()
                .map(|index| ElementSnapshotEntry {
                    snapshot: project_snapshot(&self.nodes[index], request.expansion),
                })
                .collect(),
            warnings: self.graph.warnings.clone(),
        })
    }

    fn traversal_indices(&self, order: AccessibilityTraversalOrder) -> Vec<usize> {
        let mut output = Vec::with_capacity(self.nodes.len());
        let mut seen = HashSet::with_capacity(self.nodes.len());
        match order {
            AccessibilityTraversalOrder::BreadthFirst => {
                let mut queue = self.roots.iter().copied().collect::<VecDeque<_>>();
                while let Some(index) = queue.pop_front() {
                    if seen.insert(index) {
                        output.push(index);
                        queue.extend(self.children[index].iter().copied());
                    }
                }
            }
            AccessibilityTraversalOrder::Preorder => {
                let mut stack = self.roots.iter().rev().copied().collect::<Vec<_>>();
                while let Some(index) = stack.pop() {
                    if seen.insert(index) {
                        output.push(index);
                        stack.extend(self.children[index].iter().rev().copied());
                    }
                }
            }
            AccessibilityTraversalOrder::Postorder => {
                let mut stack = self
                    .roots
                    .iter()
                    .rev()
                    .copied()
                    .map(|index| (index, false))
                    .collect::<Vec<_>>();
                while let Some((index, expanded)) = stack.pop() {
                    if expanded {
                        output.push(index);
                    } else if seen.insert(index) {
                        stack.push((index, true));
                        stack.extend(
                            self.children[index]
                                .iter()
                                .rev()
                                .copied()
                                .map(|child| (child, false)),
                        );
                    }
                }
            }
        }
        output
    }

    fn require_selector_generation(
        &self,
        selector: &ElementSelector,
    ) -> Result<(), AccessibilityQueryError> {
        let require_application = |application: &ApplicationRef| {
            (application.atspi_generation == self.atspi_generation
                && self.applications.contains(application))
            .then_some(())
            .ok_or(AccessibilityQueryError::StaleReference)
        };
        let require_element = |element: &ElementRef| {
            (element.atspi_generation == self.atspi_generation
                && element.application.atspi_generation == self.atspi_generation
                && self.indices.contains_key(element))
            .then_some(())
            .ok_or(AccessibilityQueryError::StaleReference)
        };
        match &selector.scope {
            ElementScope::Application { application } => require_application(application)?,
            ElementScope::Subtree { root, .. } => require_element(root)?,
            ElementScope::Children { parent } => require_element(parent)?,
            ElementScope::Desktop | ElementScope::Window { .. } => {}
        }
        for predicate in &selector.predicates {
            if let ElementPredicate::Relation { target, .. } = predicate {
                require_element(target)?;
            }
        }
        Ok(())
    }

    fn require_clean_graph(&self) -> Result<(), AccessibilityQueryError> {
        if self.graph.resync_required {
            Err(AccessibilityQueryError::ResyncRequired)
        } else {
            Ok(())
        }
    }

    fn scope_indices(
        &self,
        scope: &ElementScope,
        max_depth: u16,
        deadline: AccessibilityQueryDeadline,
    ) -> Result<Vec<(usize, u16)>, AccessibilityQueryError> {
        deadline.check()?;
        let roots = match scope {
            ElementScope::Desktop => self.roots.clone(),
            ElementScope::Application { application } => {
                let mut roots = Vec::new();
                for index in self.roots.iter().copied() {
                    deadline.check()?;
                    if self.nodes[index].element.application == *application {
                        roots.push(index);
                    }
                }
                roots
            }
            ElementScope::Window { window } => {
                let mut roots = Vec::new();
                for index in 0..self.nodes.len() {
                    deadline.check()?;
                    if self.is_highest_window_correlated(index, window) {
                        roots.push(index);
                    }
                }
                roots
            }
            ElementScope::Subtree { root, include_root } => {
                let root = *self
                    .indices
                    .get(root)
                    .ok_or(AccessibilityQueryError::StaleReference)?;
                if *include_root {
                    vec![root]
                } else {
                    self.children[root].clone()
                }
            }
            ElementScope::Children { parent } => {
                let parent = *self
                    .indices
                    .get(parent)
                    .ok_or(AccessibilityQueryError::StaleReference)?;
                deadline.check()?;
                return Ok(self.children[parent]
                    .iter()
                    .copied()
                    .map(|index| (index, 1))
                    .collect());
            }
        };
        let mut output = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = roots
            .into_iter()
            .rev()
            .map(|index| (index, 0_u16))
            .collect::<Vec<_>>();
        while let Some((index, depth)) = stack.pop() {
            deadline.check()?;
            if !seen.insert(index) {
                continue;
            }
            if depth > max_depth {
                return Err(AccessibilityQueryError::LimitExceeded(QueryLimit::Depth));
            }
            output.push((index, depth));
            let child_depth = depth
                .checked_add(1)
                .ok_or(AccessibilityQueryError::LimitExceeded(QueryLimit::Depth))?;
            stack.extend(
                self.children[index]
                    .iter()
                    .rev()
                    .copied()
                    .map(|child| (child, child_depth)),
            );
        }
        Ok(output)
    }

    fn is_highest_window_correlated(
        &self,
        index: usize,
        window: &xenoteer_protocol::WindowRef,
    ) -> bool {
        if !is_correlated_with_window(&self.nodes[index], window) {
            return false;
        }
        let mut parent = self.nodes[index]
            .parent
            .as_ref()
            .and_then(|parent| self.indices.get(parent).copied());
        while let Some(parent_index) = parent {
            if is_correlated_with_window(&self.nodes[parent_index], window) {
                return false;
            }
            parent = self.nodes[parent_index]
                .parent
                .as_ref()
                .and_then(|ancestor| self.indices.get(ancestor).copied());
        }
        true
    }
}

/// Immutable query response before a transport authenticates a cursor token.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessibilityQueryProjection {
    /// Desktop resource owning the snapshot.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime owning the snapshot.
    pub desktop_generation: DesktopGeneration,
    /// Exact AT-SPI connection lifetime owning every element.
    pub atspi_generation: AtspiGeneration,
    /// Immutable revision to bind into any continuation.
    pub snapshot_revision: AccessibilityRevision,
    /// Expanded page slice.
    pub elements: Vec<ElementSnapshotEntry>,
    /// Elements inspected while evaluating this result.
    pub visited_nodes: u32,
    /// Matches before slicing.
    pub total_matches: u32,
    /// Next offset in this exact immutable result, when another slice exists.
    pub next_offset: Option<u32>,
    /// Fully bound core continuation for the server to retain/authenticate.
    pub continuation: Option<AccessibilityContinuationDescriptor>,
    /// Bounded topology diagnostics captured with the snapshot.
    pub warnings: Vec<AccessibilityWarning>,
}

/// Query state that must be retained server-side or authenticated in a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibilityContinuationDescriptor {
    /// Desktop resource owning the immutable result.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime owning the immutable result.
    pub desktop_generation: DesktopGeneration,
    /// Exact AT-SPI lifetime owning every result reference.
    pub atspi_generation: AtspiGeneration,
    /// Atomic cache revision from which the result was evaluated.
    pub snapshot_revision: AccessibilityRevision,
    /// Canonical selector hash; not an authentication value.
    pub selector_fingerprint: [u8; 32],
    /// Stable result ordering.
    pub order: ElementOrder,
    /// Stable public-field projection.
    pub expansion: ElementSnapshotExpansion,
    /// Next offset in this exact immutable result.
    pub next_offset: u32,
}

/// Pure wait evidence at one immutable revision.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessibilityWaitEvaluation {
    /// Immutable revision at which the predicate was evaluated.
    pub evaluated_revision: AccessibilityRevision,
    /// Aggregate predicate result.
    pub predicate_satisfied: bool,
    /// Elements selected before applying the wait predicate.
    pub selected_count: u32,
    /// Selected elements that individually satisfied the predicate.
    pub satisfying_count: u32,
    /// Bounded expanded evidence for satisfying elements.
    pub satisfying_elements: Vec<ElementSnapshotEntry>,
    /// Bounded topology diagnostics captured with the evaluation.
    pub warnings: Vec<AccessibilityWarning>,
}

/// Ticket held while the actor installs a waiter between its two checks.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedAccessibilityWait {
    request: ElementWaitRequest,
    registered_at: AccessibilityRevision,
    atspi_generation: AtspiGeneration,
    initial: AccessibilityWaitEvaluation,
}

impl PreparedAccessibilityWait {
    /// Revision captured by the first check.
    #[must_use]
    pub const fn registration_revision(&self) -> AccessibilityRevision {
        self.registered_at
    }

    /// First evaluation, suitable for immediate completion before registration.
    #[must_use]
    pub const fn initial_evaluation(&self) -> &AccessibilityWaitEvaluation {
        &self.initial
    }
}

/// Opaque actor-local key for one pending accessibility wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccessibilityWaitToken(u64);

/// Bounded holder used between the registration and recheck steps.
#[derive(Debug)]
pub struct AccessibilityWaitRegistry {
    maximum: usize,
    next_token: u64,
    pending: BTreeMap<AccessibilityWaitToken, PreparedAccessibilityWait>,
}

impl AccessibilityWaitRegistry {
    /// Creates a registry with a non-zero hard pending-wait ceiling.
    pub fn new(maximum: usize) -> Result<Self, AccessibilityWaitRegistrationError> {
        if maximum == 0 {
            return Err(AccessibilityWaitRegistrationError::InvalidCapacity);
        }
        Ok(Self {
            maximum,
            next_token: 0,
            pending: BTreeMap::new(),
        })
    }

    /// Number of waits currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether no waits are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Installs the prepared wait before its mandatory second cache check.
    pub fn register(
        &mut self,
        prepared: PreparedAccessibilityWait,
    ) -> Result<AccessibilityWaitToken, AccessibilityWaitRegistrationError> {
        if self.pending.len() >= self.maximum {
            return Err(AccessibilityWaitRegistrationError::CapacityExhausted);
        }
        self.next_token = self
            .next_token
            .checked_add(1)
            .ok_or(AccessibilityWaitRegistrationError::TokenExhausted)?;
        let token = AccessibilityWaitToken(self.next_token);
        self.pending.insert(token, prepared);
        Ok(token)
    }

    /// Borrows a registered ticket for [`AccessibilityCache::recheck_wait`].
    #[must_use]
    pub fn get(&self, token: AccessibilityWaitToken) -> Option<&PreparedAccessibilityWait> {
        self.pending.get(&token)
    }

    /// Removes a completed, cancelled, timed-out, or resync-invalidated wait.
    pub fn remove(&mut self, token: AccessibilityWaitToken) -> Option<PreparedAccessibilityWait> {
        self.pending.remove(&token)
    }

    /// Drains every ticket after a generation/resync barrier.
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

/// Pending-wait admission failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AccessibilityWaitRegistrationError {
    /// The configured pending-wait capacity is zero.
    #[error("accessibility wait capacity must be non-zero")]
    InvalidCapacity,
    /// The hard pending-wait ceiling has been reached.
    #[error("accessibility wait capacity is exhausted")]
    CapacityExhausted,
    /// The actor-local wait token sequence cannot advance.
    #[error("accessibility wait token sequence is exhausted")]
    TokenExhausted,
}

/// Query budget whose exhaustion prevented a trustworthy result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryLimit {
    /// Maximum inspected candidates.
    VisitedNodes,
    /// Maximum graph distance from a scope root.
    Depth,
    /// Maximum matches collected before deterministic ordering.
    Matches,
    /// Absolute monotonic query deadline.
    Timeout,
}

/// Absolute monotonic deadline shared by every stage of one bounded query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessibilityQueryDeadline(Instant);

impl AccessibilityQueryDeadline {
    /// Creates an absolute deadline from a caller-validated timeout.
    pub fn from_timeout_ms(timeout_ms: u32) -> Result<Self, AccessibilityQueryError> {
        Instant::now()
            .checked_add(Duration::from_millis(u64::from(timeout_ms)))
            .map(Self)
            .ok_or(AccessibilityQueryError::LimitExceeded(QueryLimit::Timeout))
    }

    /// Wraps an already established monotonic deadline.
    #[must_use]
    pub const fn at(deadline: Instant) -> Self {
        Self(deadline)
    }

    /// Returns whichever absolute deadline expires first.
    #[must_use]
    pub fn earliest(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    fn check(self) -> Result<(), AccessibilityQueryError> {
        if Instant::now() >= self.0 {
            return Err(AccessibilityQueryError::LimitExceeded(QueryLimit::Timeout));
        }
        Ok(())
    }
}

/// Accessibility cache failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AccessibilityModelError {
    /// A zero capacity was configured.
    #[error("accessibility model limits must be non-zero")]
    InvalidLimits,
    /// Desktop scope contains a nil identifier.
    #[error("desktop identifiers must be non-nil")]
    NilIdentifier,
    /// Application reference did not satisfy the protocol contract.
    #[error("application failed protocol validation")]
    InvalidApplication(AccessibilityValidationError),
    /// Element snapshot did not satisfy the protocol contract.
    #[error("element snapshot failed protocol validation")]
    InvalidSnapshot(AccessibilityValidationError),
    /// Element reference did not satisfy the protocol contract.
    #[error("element reference failed protocol validation")]
    InvalidReference(AccessibilityValidationError),
    /// Reference belongs to an invalidated desktop or bus lifetime.
    #[error("reference belongs to a stale desktop or AT-SPI generation")]
    StaleGeneration,
    /// No live registered application owns the supplied reference.
    #[error("application is not registered")]
    ApplicationNotFound,
    /// The same application key now names a different instance.
    #[error("application instance has been replaced")]
    StaleApplication,
    /// A new object did not use the actor's next global birth sequence.
    #[error("object birth sequence was not the next actor-owned value")]
    UnexpectedCacheSequence,
    /// An object path now names a different exact birth.
    #[error("object reference is stale")]
    StaleReference,
    /// No live object or retained tombstone matches the reference.
    #[error("object is not live")]
    NotFound,
    /// The exact birth already has a tombstone.
    #[error("object was already removed")]
    AlreadyRemoved,
    /// A connection or application generation cannot advance.
    #[error("AT-SPI or application generation exhausted")]
    GenerationExhausted,
    /// The global object-birth sequence cannot advance.
    #[error("cache sequence exhausted")]
    SequenceExhausted,
    /// The atomic model revision cannot advance.
    #[error("accessibility revision exhausted")]
    RevisionExhausted,
    /// State was invalidated and callers must rebuild against the barrier.
    #[error("cache crossed a mandatory resynchronization barrier")]
    ResyncRequired(AccessibilityResyncBarrier),
}

/// Immutable query/evaluation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AccessibilityQueryError {
    /// Request validation failed before inspecting the cache.
    #[error("accessibility request failed protocol validation")]
    InvalidRequest(AccessibilityValidationError),
    /// Bounded regex construction failed.
    #[error("selector regex could not be compiled within its size ceiling")]
    RegexBuild,
    /// Canonical selector encoding failed.
    #[error("selector fingerprint could not be encoded")]
    Fingerprint,
    /// Page size is zero or larger than the admitted match budget.
    #[error("query page limit is zero or exceeds its admitted match budget")]
    InvalidPageLimit,
    /// A hard traversal or result budget was exhausted.
    #[error("query {0:?} budget was exhausted")]
    LimitExceeded(QueryLimit),
    /// Offset does not identify a slice boundary in this immutable result.
    #[error("query page offset is outside this immutable result")]
    Offset,
    /// Cursor state does not describe this snapshot and query.
    #[error("accessibility continuation does not match the immutable query")]
    ContinuationMismatch,
    /// Exact element is absent from this immutable revision.
    #[error("element reference is stale or absent")]
    StaleReference,
    /// Selector found no elements.
    #[error("selector matched no elements")]
    NoMatch,
    /// An exact-one operation found more than one element.
    #[error("selector required one element but matched {matches}")]
    Ambiguous {
        /// Exact number of matches observed within the admitted budget.
        matches: u32,
    },
    /// `after_revision` is newer than the actor's current state.
    #[error("wait names a future cache revision")]
    FutureRevision,
    /// The wait ticket belongs to a prior bus generation.
    #[error("wait crossed an AT-SPI resynchronization barrier")]
    ResyncRequired,
}

fn application_key(application: &ApplicationRef) -> ApplicationKey {
    ApplicationKey {
        bus: application.unique_bus_name.clone(),
        root: application.root_object_path.clone(),
    }
}

fn object_key(element: &ElementRef) -> ObjectKey {
    ObjectKey {
        application: application_key(&element.application),
        path: element.object_path.clone(),
    }
}

fn snapshot_declares_protected(snapshot: &ElementSnapshot) -> bool {
    snapshot.role.role == ElementRole::PasswordText
        || snapshot.states.contains(&ElementState::Protected)
        || snapshot.text.as_ref().is_some_and(|text| text.protected)
}

fn is_correlated_with_window(
    snapshot: &ElementSnapshot,
    window: &xenoteer_protocol::WindowRef,
) -> bool {
    snapshot.window_correlation.window.as_ref() == Some(window)
        && snapshot.window_correlation.confidence != WindowCorrelationConfidence::None
}

fn redact_protected(snapshot: &mut ElementSnapshot, protected: bool) {
    if !protected {
        return;
    }
    if let Some(text) = &mut snapshot.text {
        text.protected = true;
        text.content = None;
    }
    snapshot.value = None;
    snapshot.attributes.clear();
    snapshot.completeness = ElementCompleteness::Dirty;
    push_warning(
        &mut snapshot.warnings,
        "accessibility.protected_redacted",
        "protected text, value metadata, and attributes were removed",
    );
}

fn project_snapshot(
    snapshot: &ElementSnapshot,
    expansion: ElementSnapshotExpansion,
) -> ElementSnapshot {
    let mut projected = snapshot.clone();
    if !expansion.actions {
        projected.actions.clear();
    }
    if !expansion.value {
        projected.value = None;
    }
    if !expansion.text_metadata {
        projected.text = None;
    } else if !expansion.text_content
        && let Some(text) = &mut projected.text
    {
        text.content = None;
    }
    if !expansion.attributes {
        projected.attributes.clear();
    }
    if !expansion.relations {
        projected.relations.clear();
    }
    if !expansion.component {
        projected.component = None;
    }
    let protected = snapshot_declares_protected(&projected)
        || projected
            .warnings
            .iter()
            .any(|warning| warning.code == "accessibility.protected_redacted");
    redact_protected(&mut projected, protected);
    projected
}

fn push_warning(warnings: &mut Vec<AccessibilityWarning>, code: &str, message: &str) {
    if warnings.iter().any(|warning| warning.code == code) {
        return;
    }
    if warnings.len() < MAX_ACCESSIBILITY_WARNINGS {
        warnings.push(AccessibilityWarning {
            code: code.to_owned(),
            message: message.to_owned(),
        });
    }
}

fn mark_reachable(index: usize, children: &[Vec<usize>], reached: &mut HashSet<usize>) {
    let mut stack = vec![index];
    while let Some(index) = stack.pop() {
        if reached.insert(index) {
            stack.extend(children[index].iter().copied());
        }
    }
}

fn compare_snapshot_identity(left: &ElementSnapshot, right: &ElementSnapshot) -> Ordering {
    compare_ref(&left.element, &right.element)
}

fn compare_ref(left: &ElementRef, right: &ElementRef) -> Ordering {
    left.application
        .unique_bus_name
        .cmp(&right.application.unique_bus_name)
        .then_with(|| left.object_path.cmp(&right.object_path))
        .then_with(|| left.cache_sequence.cmp(&right.cache_sequence))
}

fn compare_child(left: &ElementSnapshot, right: &ElementSnapshot) -> Ordering {
    left.index_in_parent
        .unwrap_or(u32::MAX)
        .cmp(&right.index_in_parent.unwrap_or(u32::MAX))
        .then_with(|| compare_snapshot_identity(left, right))
}

fn order_indices(
    indices: &mut [usize],
    nodes: &[ElementSnapshot],
    order: ElementOrder,
    children: &[Vec<usize>],
    roots: &[usize],
) {
    if matches!(
        order,
        ElementOrder::Preorder | ElementOrder::ReversePreorder
    ) {
        let rank = preorder_rank(nodes.len(), children, roots);
        indices.sort_by_key(|index| rank[*index]);
        if order == ElementOrder::ReversePreorder {
            indices.reverse();
        }
        return;
    }
    indices.sort_by(|left, right| {
        let left = &nodes[*left];
        let right = &nodes[*right];
        let primary = match order {
            ElementOrder::NameAscending => compare_optional(&left.name, &right.name, false),
            ElementOrder::NameDescending => compare_optional(&left.name, &right.name, true),
            ElementOrder::RoleThenName => left
                .role
                .role
                .cmp(&right.role.role)
                .then_with(|| compare_optional(&left.name, &right.name, false)),
            ElementOrder::ObjectPathAscending => {
                left.element.object_path.cmp(&right.element.object_path)
            }
            ElementOrder::Preorder | ElementOrder::ReversePreorder => Ordering::Equal,
        };
        primary.then_with(|| compare_ref(&left.element, &right.element))
    });
}

fn preorder_rank(count: usize, children: &[Vec<usize>], roots: &[usize]) -> Vec<usize> {
    let mut rank = vec![usize::MAX; count];
    let mut seen = HashSet::new();
    let mut stack = roots.iter().rev().copied().collect::<Vec<_>>();
    let mut next = 0;
    while let Some(index) = stack.pop() {
        if seen.insert(index) {
            rank[index] = next;
            next += 1;
            stack.extend(children[index].iter().rev().copied());
        }
    }
    rank
}

fn compare_optional(left: &Option<String>, right: &Option<String>, reverse: bool) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) if reverse => right.cmp(left),
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

struct CompiledSelector {
    predicates: Vec<CompiledPredicate>,
}

impl CompiledSelector {
    fn compile(selector: &ElementSelector) -> Result<Self, AccessibilityQueryError> {
        Ok(Self {
            predicates: selector
                .predicates
                .iter()
                .map(CompiledPredicate::compile)
                .collect::<Result<_, _>>()?,
        })
    }

    fn matches(&mut self, snapshot: &ElementSnapshot) -> bool {
        self.predicates
            .iter_mut()
            .all(|predicate| predicate.matches(snapshot))
    }
}

enum CompiledPredicate {
    Role(HashSet<ElementRole>),
    Name(CompiledStringMatch),
    Description(CompiledStringMatch),
    AccessibleId(CompiledStringMatch),
    State(ElementState, bool),
    Interface(ElementInterface),
    Attribute(String, CompiledStringMatch),
    Action(CompiledStringMatch),
    ValueRange(Option<f64>, Option<f64>),
    IndexInParent(u32),
    ChildCount(Option<u32>, Option<u32>),
    Relation(ElementRelationType, ElementRef),
    ComponentIntersects(CoordinateSpace, Rect),
}

impl CompiledPredicate {
    fn compile(predicate: &ElementPredicate) -> Result<Self, AccessibilityQueryError> {
        Ok(match predicate {
            ElementPredicate::Role { roles } => Self::Role(roles.iter().copied().collect()),
            ElementPredicate::Name { matcher } => {
                Self::Name(CompiledStringMatch::compile(matcher)?)
            }
            ElementPredicate::Description { matcher } => {
                Self::Description(CompiledStringMatch::compile(matcher)?)
            }
            ElementPredicate::AccessibleId { matcher } => {
                Self::AccessibleId(CompiledStringMatch::compile(matcher)?)
            }
            ElementPredicate::State { state, value } => Self::State(*state, *value),
            ElementPredicate::Interface { interface } => Self::Interface(*interface),
            ElementPredicate::Attribute { name, matcher } => {
                Self::Attribute(name.clone(), CompiledStringMatch::compile(matcher)?)
            }
            ElementPredicate::Action { matcher } => {
                Self::Action(CompiledStringMatch::compile(matcher)?)
            }
            ElementPredicate::ValueRange { minimum, maximum } => {
                Self::ValueRange(*minimum, *maximum)
            }
            ElementPredicate::IndexInParent { index } => Self::IndexInParent(*index),
            ElementPredicate::ChildCount { minimum, maximum } => {
                Self::ChildCount(*minimum, *maximum)
            }
            ElementPredicate::Relation { relation, target } => {
                Self::Relation(*relation, target.clone())
            }
            ElementPredicate::ComponentIntersects {
                coordinate_space,
                rect,
            } => Self::ComponentIntersects(*coordinate_space, *rect),
        })
    }

    fn matches(&mut self, snapshot: &ElementSnapshot) -> bool {
        match self {
            Self::Role(roles) => roles.contains(&snapshot.role.role),
            Self::Name(matcher) => snapshot
                .name
                .as_deref()
                .is_some_and(|value| matcher.matches(value)),
            Self::Description(matcher) => snapshot
                .description
                .as_deref()
                .is_some_and(|value| matcher.matches(value)),
            Self::AccessibleId(matcher) => snapshot
                .accessible_id
                .as_deref()
                .is_some_and(|value| matcher.matches(value)),
            Self::State(state, value) => snapshot.states.contains(state) == *value,
            Self::Interface(interface) => snapshot.interfaces.contains(interface),
            Self::Attribute(name, matcher) => snapshot
                .attributes
                .iter()
                .any(|attribute| attribute.name == *name && matcher.matches(&attribute.value)),
            Self::Action(matcher) => snapshot
                .actions
                .iter()
                .any(|action| matcher.matches(&action.name)),
            Self::ValueRange(minimum, maximum) => snapshot
                .value
                .as_ref()
                .is_some_and(|value| in_f64_range(value.current, *minimum, *maximum)),
            Self::IndexInParent(index) => snapshot.index_in_parent == Some(*index),
            Self::ChildCount(minimum, maximum) => snapshot
                .child_count
                .is_some_and(|count| in_u32_range(count, *minimum, *maximum)),
            Self::Relation(relation, target) => snapshot.relations.iter().any(|candidate| {
                candidate.relation == *relation && candidate.targets.contains(target)
            }),
            Self::ComponentIntersects(coordinate_space, rect) => snapshot
                .component
                .as_ref()
                .filter(|component| component.coordinate_space == *coordinate_space)
                .and_then(|component| component.extents)
                .is_some_and(|candidate| rectangles_intersect(candidate, *rect)),
        }
    }
}

enum CompiledStringMatch {
    Exact(String, bool),
    Contains(String, bool),
    Prefix(String, bool),
    Suffix(String, bool),
    Regex(PikeVM, Box<PikeCache>),
}

impl CompiledStringMatch {
    fn compile(matcher: &ElementStringMatch) -> Result<Self, AccessibilityQueryError> {
        Ok(match matcher {
            ElementStringMatch::Exact {
                value,
                case_sensitive,
            } => Self::Exact(fold(value, *case_sensitive), *case_sensitive),
            ElementStringMatch::Contains {
                value,
                case_sensitive,
            } => Self::Contains(fold(value, *case_sensitive), *case_sensitive),
            ElementStringMatch::Prefix {
                value,
                case_sensitive,
            } => Self::Prefix(fold(value, *case_sensitive), *case_sensitive),
            ElementStringMatch::Suffix {
                value,
                case_sensitive,
            } => Self::Suffix(fold(value, *case_sensitive), *case_sensitive),
            ElementStringMatch::Regex {
                pattern,
                case_sensitive,
            } => {
                let mut compiler = NFA::compiler();
                compiler
                    .configure(
                        NFA::config().nfa_size_limit(Some(MAX_ACCESSIBILITY_REGEX_BYTES * 64)),
                    )
                    .syntax(
                        syntax::Config::new()
                            .utf8(true)
                            .unicode(true)
                            .case_insensitive(!case_sensitive),
                    );
                let nfa = compiler
                    .build(pattern)
                    .map_err(|_| AccessibilityQueryError::RegexBuild)?;
                let regex =
                    PikeVM::new_from_nfa(nfa).map_err(|_| AccessibilityQueryError::RegexBuild)?;
                let cache = Box::new(regex.create_cache());
                Self::Regex(regex, cache)
            }
        })
    }

    fn matches(&mut self, candidate: &str) -> bool {
        match self {
            Self::Exact(value, sensitive) => fold(candidate, *sensitive) == *value,
            Self::Contains(value, sensitive) => {
                fold(candidate, *sensitive).contains(value.as_str())
            }
            Self::Prefix(value, sensitive) => {
                fold(candidate, *sensitive).starts_with(value.as_str())
            }
            Self::Suffix(value, sensitive) => fold(candidate, *sensitive).ends_with(value.as_str()),
            Self::Regex(regex, cache) => regex.is_match(cache, candidate),
        }
    }
}

fn fold(value: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        value.to_owned()
    } else {
        value.chars().flat_map(char::to_lowercase).collect()
    }
}

fn wait_predicate_matches(
    snapshot: &ElementSnapshot,
    predicate: &ElementWaitPredicate,
) -> Result<bool, AccessibilityQueryError> {
    Ok(match predicate {
        ElementWaitPredicate::Exists => true,
        ElementWaitPredicate::Gone | ElementWaitPredicate::SelectorCount { .. } => false,
        ElementWaitPredicate::State { state, value } => snapshot.states.contains(state) == *value,
        ElementWaitPredicate::Name { matcher } => {
            let mut matcher = CompiledStringMatch::compile(matcher)?;
            snapshot
                .name
                .as_deref()
                .is_some_and(|value| matcher.matches(value))
        }
        ElementWaitPredicate::Value { minimum, maximum } => snapshot
            .value
            .as_ref()
            .is_some_and(|value| in_f64_range(value.current, *minimum, *maximum)),
        ElementWaitPredicate::Text { matcher } => {
            let mut matcher = CompiledStringMatch::compile(matcher)?;
            snapshot
                .text
                .as_ref()
                .and_then(|text| text.content.as_ref())
                .is_some_and(|text| matcher.matches(text.expose()))
        }
        ElementWaitPredicate::ChildCount { minimum, maximum } => snapshot
            .child_count
            .is_some_and(|count| in_u32_range(count, *minimum, *maximum)),
        ElementWaitPredicate::Geometry {
            coordinate_space,
            intersects,
        } => snapshot
            .component
            .as_ref()
            .filter(|component| component.coordinate_space == *coordinate_space)
            .and_then(|component| component.extents)
            .is_some_and(|candidate| rectangles_intersect(candidate, *intersects)),
    })
}

fn in_f64_range(value: f64, minimum: Option<f64>, maximum: Option<f64>) -> bool {
    minimum.is_none_or(|minimum| value >= minimum) && maximum.is_none_or(|maximum| value <= maximum)
}

fn in_u32_range(value: u32, minimum: Option<u32>, maximum: Option<u32>) -> bool {
    minimum.is_none_or(|minimum| value >= minimum) && maximum.is_none_or(|maximum| value <= maximum)
}

fn rectangles_intersect(left: Rect, right: Rect) -> bool {
    let left_origin = left.origin();
    let right_origin = right.origin();
    let Ok(left_size) = left.size() else {
        return false;
    };
    let Ok(right_size) = right.size() else {
        return false;
    };
    let left_x2 = i64::from(left_origin.x()) + i64::from(left_size.width());
    let left_y2 = i64::from(left_origin.y()) + i64::from(left_size.height());
    let right_x2 = i64::from(right_origin.x()) + i64::from(right_size.width());
    let right_y2 = i64::from(right_origin.y()) + i64::from(right_size.height());
    i64::from(left_origin.x()) < right_x2
        && i64::from(right_origin.x()) < left_x2
        && i64::from(left_origin.y()) < right_y2
        && i64::from(right_origin.y()) < left_y2
}

/// Stable hash for a server-side cursor binding. This is not an authority token.
pub fn accessibility_selector_fingerprint(
    selector: &ElementSelector,
) -> Result<[u8; 32], AccessibilityQueryError> {
    let encoded = serde_json::to_vec(selector).map_err(|_| AccessibilityQueryError::Fingerprint)?;
    let mut digest = Sha256::new();
    digest.update(SELECTOR_FINGERPRINT_DOMAIN);
    digest.update(encoded);
    Ok(digest.finalize().into())
}

#[cfg(test)]
mod exhaustion_tests {
    use super::*;
    use xenoteer_protocol::{
        ElementRoleSnapshot, ElementWindowCorrelation, WindowCorrelationConfidence,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn hash(value: char) -> Result<AccessibilityIdentityHash, AccessibilityValidationError> {
        AccessibilityIdentityHash::new(value.to_string().repeat(64))
    }

    fn populated_cache(
        atspi_generation: AtspiGeneration,
        role: ElementRole,
    ) -> Result<(AccessibilityCache, ApplicationRef, ElementRef), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let mut cache = AccessibilityCache::new(
            desktop_id,
            desktop_generation,
            atspi_generation,
            AccessibilityModelLimits::default(),
        )?;
        let application = cache.register_application(
            AtspiBusName::new(":1.42")?,
            AtspiObjectPath::new("/org/example/App")?,
            hash('a')?,
        )?;
        let element = cache.next_element_ref(
            &application,
            AtspiObjectPath::new("/org/example/App/root")?,
            hash('b')?,
        )?;
        cache.observe(snapshot(element.clone(), role))?;
        Ok((cache, application, element))
    }

    fn snapshot(element: ElementRef, role: ElementRole) -> ElementSnapshot {
        ElementSnapshot {
            element,
            parent: None,
            index_in_parent: None,
            child_count: Some(0),
            role: ElementRoleSnapshot {
                role,
                raw_name: None,
                raw_numeric: None,
            },
            name: Some("target".to_owned()),
            description: None,
            accessible_id: None,
            locale: None,
            states: vec![ElementState::Enabled],
            interfaces: vec![ElementInterface::Accessible],
            actions: Vec::new(),
            value: None,
            text: None,
            component: None,
            attributes: Vec::new(),
            relations: Vec::new(),
            window_correlation: ElementWindowCorrelation {
                window: None,
                confidence: WindowCorrelationConfidence::None,
                evidence: Vec::new(),
                conflicting_evidence: false,
            },
            revision: AccessibilityRevision::new(1)
                .unwrap_or_else(|_| unreachable!("fixed revision is valid")),
            completeness: ElementCompleteness::Complete,
            truncated: false,
            warnings: Vec::new(),
        }
    }

    fn fingerprint(cache: &AccessibilityCache) -> String {
        format!("{cache:#?}")
    }

    #[test]
    fn element_remove_and_replacement_are_atomic_at_revision_exhaustion() -> TestResult {
        let (mut cache, application, element) =
            populated_cache(AtspiGeneration::new(1)?, ElementRole::PasswordText)?;
        let replacement =
            cache.next_element_ref(&application, element.object_path.clone(), hash('c')?)?;
        cache.set_revision_for_exhaustion_test(u64::MAX);
        let before = fingerprint(&cache);
        assert_eq!(
            cache.remove(&element),
            Err(AccessibilityModelError::RevisionExhausted)
        );
        assert_eq!(fingerprint(&cache), before);
        assert!(cache.resolve_exact(&element).is_ok());

        assert_eq!(
            cache.observe(snapshot(replacement, ElementRole::Button)),
            Err(AccessibilityModelError::RevisionExhausted)
        );
        assert_eq!(fingerprint(&cache), before);
        assert!(cache.resolve_exact(&element).is_ok());
        Ok(())
    }

    #[test]
    fn application_mutations_preflight_every_required_revision() -> TestResult {
        let (mut register, application, element) =
            populated_cache(AtspiGeneration::new(1)?, ElementRole::Button)?;
        register.set_revision_for_exhaustion_test(u64::MAX - 1);
        let before = fingerprint(&register);
        assert_eq!(
            register.register_application(
                application.unique_bus_name.clone(),
                application.root_object_path.clone(),
                hash('c')?,
            ),
            Err(AccessibilityModelError::RevisionExhausted)
        );
        assert_eq!(fingerprint(&register), before);
        assert!(register.resolve_exact(&element).is_ok());

        let (mut restart, application, element) =
            populated_cache(AtspiGeneration::new(1)?, ElementRole::Button)?;
        restart.set_revision_for_exhaustion_test(u64::MAX - 1);
        let before = fingerprint(&restart);
        assert_eq!(
            restart.restart_application(
                application.unique_bus_name.clone(),
                application.root_object_path.clone(),
                application.identity_hash.clone(),
            ),
            Err(AccessibilityModelError::RevisionExhausted)
        );
        assert_eq!(fingerprint(&restart), before);
        assert!(restart.resolve_exact(&element).is_ok());

        let (mut remove, application, element) =
            populated_cache(AtspiGeneration::new(1)?, ElementRole::Button)?;
        remove.set_revision_for_exhaustion_test(u64::MAX - 1);
        let before = fingerprint(&remove);
        assert_eq!(
            remove.remove_application(&application),
            Err(AccessibilityModelError::RevisionExhausted)
        );
        assert_eq!(fingerprint(&remove), before);
        assert!(remove.resolve_exact(&element).is_ok());

        let (mut new_registration, _, _) =
            populated_cache(AtspiGeneration::new(1)?, ElementRole::Button)?;
        new_registration.set_revision_for_exhaustion_test(u64::MAX);
        let before = fingerprint(&new_registration);
        assert_eq!(
            new_registration.register_application(
                AtspiBusName::new(":1.99")?,
                AtspiObjectPath::new("/org/example/Other")?,
                hash('d')?,
            ),
            Err(AccessibilityModelError::RevisionExhausted)
        );
        assert_eq!(fingerprint(&new_registration), before);
        Ok(())
    }

    #[test]
    fn generation_and_sequence_exhaustion_never_partially_mutate() -> TestResult {
        let (mut application_generation, application, element) =
            populated_cache(AtspiGeneration::new(1)?, ElementRole::Button)?;
        application_generation
            .set_application_generation_for_exhaustion_test(&application, u64::MAX);
        let before = fingerprint(&application_generation);
        assert_eq!(
            application_generation.restart_application(
                application.unique_bus_name.clone(),
                application.root_object_path.clone(),
                hash('c')?,
            ),
            Err(AccessibilityModelError::GenerationExhausted)
        );
        assert_eq!(fingerprint(&application_generation), before);
        assert!(application_generation.resolve_exact(&element).is_ok());

        let (mut sequence, application, _) =
            populated_cache(AtspiGeneration::new(1)?, ElementRole::Button)?;
        sequence.set_cache_sequence_for_exhaustion_test(u64::MAX);
        let before = fingerprint(&sequence);
        assert_eq!(
            sequence.next_element_ref(
                &application,
                AtspiObjectPath::new("/org/example/App/other")?,
                hash('d')?,
            ),
            Err(AccessibilityModelError::SequenceExhausted)
        );
        assert_eq!(fingerprint(&sequence), before);
        let unexpected = ElementRef {
            desktop_id: application.desktop_id,
            desktop_generation: application.desktop_generation,
            atspi_generation: application.atspi_generation,
            application,
            object_path: AtspiObjectPath::new("/org/example/App/other")?,
            object_identity_hash: hash('e')?,
            cache_sequence: u64::MAX,
        };
        assert_eq!(
            sequence.observe(snapshot(unexpected, ElementRole::Button)),
            Err(AccessibilityModelError::SequenceExhausted)
        );
        assert_eq!(fingerprint(&sequence), before);
        Ok(())
    }

    #[test]
    fn resync_preflights_both_atspi_generation_and_revision() -> TestResult {
        let (mut generation, _, _) =
            populated_cache(AtspiGeneration::new(u64::MAX)?, ElementRole::Button)?;
        let before = fingerprint(&generation);
        assert_eq!(
            generation.event_gap(),
            Err(AccessibilityModelError::GenerationExhausted)
        );
        assert_eq!(fingerprint(&generation), before);

        let (mut revision, _, element) =
            populated_cache(AtspiGeneration::new(1)?, ElementRole::Button)?;
        revision.set_revision_for_exhaustion_test(u64::MAX);
        let before = fingerprint(&revision);
        assert_eq!(
            revision.reset_bus(),
            Err(AccessibilityModelError::RevisionExhausted)
        );
        assert_eq!(fingerprint(&revision), before);
        assert!(revision.resolve_exact(&element).is_ok());
        Ok(())
    }

    #[test]
    fn distinct_application_churn_is_bounded_but_existing_keys_restart_at_capacity() -> TestResult {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let limits = AccessibilityModelLimits {
            max_live_nodes: 2,
            max_tombstones: 4,
        };
        let mut churn = AccessibilityCache::new(
            desktop_id,
            desktop_generation,
            AtspiGeneration::new(1)?,
            limits,
        )?;
        for index in 0..2 {
            let application = churn.register_application(
                AtspiBusName::new(format!(":1.{}", index + 10))?,
                AtspiObjectPath::new(format!("/org/example/App{index}"))?,
                hash(char::from(b'a' + index as u8))?,
            )?;
            assert!(churn.last_application_generation.len() <= limits.max_live_nodes);
            churn.remove_application(&application)?;
            assert!(churn.applications.is_empty());
            assert!(churn.last_application_generation.len() <= limits.max_live_nodes);
        }
        let error = match churn.register_application(
            AtspiBusName::new(":1.99")?,
            AtspiObjectPath::new("/org/example/Overflow")?,
            hash('c')?,
        ) {
            Ok(_) => return Err("a new application key bypassed its capacity barrier".into()),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            AccessibilityModelError::ResyncRequired(barrier)
                if barrier.reason == AccessibilityResyncReason::ApplicationCapacity
                    && barrier.atspi_generation.get() == 2
        ));
        assert!(churn.applications.is_empty());
        assert!(churn.last_application_generation.is_empty());
        assert!(churn.last_application_generation.len() <= limits.max_live_nodes);
        churn.register_application(
            AtspiBusName::new(":1.99")?,
            AtspiObjectPath::new("/org/example/Overflow")?,
            hash('c')?,
        )?;
        assert_eq!(churn.last_application_generation.len(), 1);

        let restart_limits = AccessibilityModelLimits {
            max_live_nodes: 1,
            max_tombstones: 2,
        };
        let mut restart = AccessibilityCache::new(
            DesktopId::new(),
            DesktopGeneration::new(),
            AtspiGeneration::new(1)?,
            restart_limits,
        )?;
        let application = restart.register_application(
            AtspiBusName::new(":1.42")?,
            AtspiObjectPath::new("/org/example/App")?,
            hash('a')?,
        )?;
        let same = restart.register_application(
            application.unique_bus_name.clone(),
            application.root_object_path.clone(),
            application.identity_hash.clone(),
        )?;
        assert_eq!(same, application);
        let element = restart.next_element_ref(
            &application,
            AtspiObjectPath::new("/org/example/App/root")?,
            hash('b')?,
        )?;
        restart.observe(snapshot(element, ElementRole::Button))?;
        let replacement = restart.restart_application(
            application.unique_bus_name.clone(),
            application.root_object_path.clone(),
            application.identity_hash.clone(),
        )?;
        assert_eq!(replacement.app_instance_generation, 2);
        assert_eq!(restart.atspi_generation().get(), 1);
        assert_eq!(restart.applications.len(), 1);
        assert_eq!(restart.last_application_generation.len(), 1);
        assert!(restart.nodes.is_empty());
        Ok(())
    }
}
