//! Bounded, protocol-independent AT-SPI cache normalization and reconciliation.

use std::collections::{BTreeMap, BTreeSet};

use crate::semantic::{
    IdentityFingerprint, MAX_SELECTION_RANGES, SelectionRangeEvidence, SemanticRect,
    SemanticValueEvidence, TextProtection,
};

/// Maximum bytes admitted for a bus name or object path by this adapter.
pub const MAX_ADDRESS_BYTES: usize = 4 * 1_024;
/// D-Bus bus names are bounded independently from potentially longer object paths.
pub const MAX_BUS_NAME_BYTES: usize = 255;
/// Maximum children admitted from one legacy Qt cache item.
pub const MAX_LEGACY_CHILDREN: usize = 100_000;
/// Largest removed-address list copied into one incremental public mutation.
pub const MAX_MUTATION_ADDRESSES: usize = 4_096;

/// Owned identity of an AT-SPI object on the central accessibility bus.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectAddress {
    bus_name: String,
    object_path: String,
}

impl ObjectAddress {
    /// Validate and construct an owned object address.
    pub fn new(
        bus_name: impl Into<String>,
        object_path: impl Into<String>,
    ) -> Result<Self, CacheError> {
        let bus_name = bus_name.into();
        let object_path = object_path.into();
        if bus_name.is_empty() || !bus_name.starts_with(':') {
            return Err(CacheError::InvalidAddress("bus name is not unique"));
        }
        if object_path.is_empty() || !object_path.starts_with('/') {
            return Err(CacheError::InvalidAddress("object path is not absolute"));
        }
        if bus_name.len() > MAX_BUS_NAME_BYTES || object_path.len() > MAX_ADDRESS_BYTES {
            return Err(CacheError::LimitExceeded {
                resource: "object address bytes",
                actual: if bus_name.len() > MAX_BUS_NAME_BYTES {
                    bus_name.len()
                } else {
                    object_path.len()
                },
                max: if bus_name.len() > MAX_BUS_NAME_BYTES {
                    MAX_BUS_NAME_BYTES
                } else {
                    MAX_ADDRESS_BYTES
                },
            });
        }
        Ok(Self {
            bus_name,
            object_path,
        })
    }

    /// Unique D-Bus name of the owning application.
    #[must_use]
    pub fn bus_name(&self) -> &str {
        &self.bus_name
    }

    /// Object path within the owning application.
    #[must_use]
    pub fn object_path(&self) -> &str {
        &self.object_path
    }
}

/// Resource ceilings applied before cache data becomes actor state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheLimits {
    /// Maximum live normalized nodes.
    pub max_nodes: usize,
    /// Maximum bytes in one normalized text field.
    pub max_string_bytes: usize,
    /// Maximum aggregate normalized bytes admitted for one cache item.
    pub max_item_bytes: usize,
    /// Maximum aggregate normalized bytes retained by the cache.
    pub max_total_bytes: usize,
    /// Maximum raw `Cache.GetItems` reply bytes admitted before deserialization.
    pub max_bootstrap_bytes: usize,
    /// Maximum interface names in one node.
    pub max_interfaces: usize,
    /// Maximum state codes in one node.
    pub max_states: usize,
    /// Maximum child references in one legacy node.
    pub max_children: usize,
}

impl CacheLimits {
    /// Validate that every limit has useful, finite capacity.
    pub fn validate(self) -> Result<Self, CacheError> {
        for (resource, value) in [
            ("cached nodes", self.max_nodes),
            ("string bytes", self.max_string_bytes),
            ("cache item bytes", self.max_item_bytes),
            ("total cache bytes", self.max_total_bytes),
            ("cache bootstrap bytes", self.max_bootstrap_bytes),
            ("interfaces", self.max_interfaces),
            ("states", self.max_states),
            ("legacy children", self.max_children),
        ] {
            if value == 0 {
                return Err(CacheError::InvalidLimit(resource));
            }
        }
        if self.max_children > MAX_LEGACY_CHILDREN {
            return Err(CacheError::LimitExceeded {
                resource: "legacy children configuration",
                actual: self.max_children,
                max: MAX_LEGACY_CHILDREN,
            });
        }
        if self.max_item_bytes > self.max_total_bytes {
            return Err(CacheError::Malformed(
                "cache item byte limit exceeds total cache byte limit",
            ));
        }
        Ok(self)
    }
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            max_nodes: 100_000,
            max_string_bytes: 64 * 1_024,
            max_item_bytes: 256 * 1_024,
            max_total_bytes: 256 * 1_024 * 1_024,
            max_bootstrap_bytes: 128 * 1_024 * 1_024,
            max_interfaces: 64,
            max_states: 128,
            max_children: MAX_LEGACY_CHILDREN,
        }
    }
}

/// Crate-owned cache item independent of zbus and upstream enum exhaustiveness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedCacheItem {
    /// Object described by this item.
    pub object: ObjectAddress,
    /// Root object of the owning application.
    pub application: ObjectAddress,
    /// Parent object, or `None` for a root/null parent.
    pub parent: Option<ObjectAddress>,
    /// Stable toolkit-provided child index when available.
    pub index_in_parent: Option<usize>,
    /// Toolkit-reported child count when available.
    pub child_count: Option<usize>,
    /// Explicit children supplied by the legacy Qt cache signature.
    pub legacy_children: Vec<ObjectAddress>,
    /// Raw interface names retained without coupling callers to upstream enums.
    pub interfaces: Vec<String>,
    /// Accessible name (`short_name` in the pinned Rust cache type).
    pub name: String,
    /// Accessible description (`name` in the pinned Rust cache type).
    pub description: String,
    /// Raw AT-SPI role number for forward-compatible normalization above this crate.
    pub role: u32,
    /// Adapter-owned password-role classification used to fail closed on writes.
    pub text_protection: TextProtection,
    /// Raw AT-SPI state-set words for forward-compatible normalization above this crate.
    pub states: Vec<u32>,
}

impl NormalizedCacheItem {
    /// Derive the same secret-safe identity fingerprint stored by the actor.
    #[must_use]
    pub fn identity_fingerprint(&self) -> IdentityFingerprint {
        IdentityFingerprint::from_parts(
            &self.object,
            &self.application,
            self.parent.as_ref(),
            self.index_in_parent,
            &self.name,
            &self.description,
        )
    }

    /// Conservative retained-byte estimate used by bounded staging queues.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        estimated_item_bytes(self)
    }
}

/// Fields represented by the current AT-SPI Cache `GetItems` signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModernCacheItem {
    /// Object described by this item.
    pub object: ObjectAddress,
    /// Root object of the owning application.
    pub application: ObjectAddress,
    /// Parent object, or `None` for the root/null reference.
    pub parent: Option<ObjectAddress>,
    /// Signed wire index.
    pub index_in_parent: i32,
    /// Signed wire child count.
    pub child_count: i32,
    /// Interface names.
    pub interfaces: Vec<String>,
    /// Accessible name (`short_name` on the wire binding).
    pub short_name: String,
    /// Raw role number.
    pub role: u32,
    /// Accessible description (`name` on the wire binding).
    pub name: String,
    /// Raw AT-SPI state-set words.
    pub states: Vec<u32>,
}

/// Fields represented by the old Qt/registry Cache `GetItems` signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyCacheItem {
    /// Object described by this item.
    pub object: ObjectAddress,
    /// Root object of the owning application.
    pub application: ObjectAddress,
    /// Parent object, or `None` for the root/null reference.
    pub parent: Option<ObjectAddress>,
    /// Explicit child references used instead of index/count.
    pub children: Vec<ObjectAddress>,
    /// Interface names.
    pub interfaces: Vec<String>,
    /// Accessible name (`short_name` on the wire binding).
    pub short_name: String,
    /// Raw role number.
    pub role: u32,
    /// Accessible description (`name` on the wire binding).
    pub name: String,
    /// Raw AT-SPI state-set words.
    pub states: Vec<u32>,
}

/// Normalize the current Cache signature while enforcing admission limits.
pub fn normalize_modern(
    item: ModernCacheItem,
    limits: CacheLimits,
) -> Result<NormalizedCacheItem, CacheError> {
    if item.index_in_parent < -1 {
        return Err(CacheError::Malformed("negative child index"));
    }
    if item.child_count < -1 {
        return Err(CacheError::Malformed("negative child count"));
    }
    validate_common(
        &item.short_name,
        &item.name,
        &item.interfaces,
        &item.states,
        limits,
    )?;
    let external_application_parent =
        application_root_has_external_parent(&item.object, &item.application, item.parent.as_ref());
    let transient_menu_parent = item.index_in_parent == -1
        && matches!(item.role, 8 | 35 | 45 | 59)
        && item.object.bus_name() == item.application.bus_name()
        && item.parent.as_ref().is_some_and(|parent| {
            parent.bus_name() == item.application.bus_name() && parent != &item.object
        });
    let detach_parent = external_application_parent || transient_menu_parent;
    let normalized = NormalizedCacheItem {
        object: item.object,
        application: item.application,
        parent: if detach_parent { None } else { item.parent },
        index_in_parent: if detach_parent {
            None
        } else {
            usize::try_from(item.index_in_parent).ok()
        },
        child_count: usize::try_from(item.child_count).ok(),
        legacy_children: Vec::new(),
        interfaces: item.interfaces,
        name: item.short_name,
        description: item.name,
        role: item.role,
        text_protection: classify_text_protection(item.role),
        states: item.states,
    };
    validate_item_bytes(&normalized, limits)?;
    Ok(normalized)
}

/// Normalize the old Qt Cache signature while enforcing admission limits.
pub fn normalize_legacy(
    item: LegacyCacheItem,
    limits: CacheLimits,
) -> Result<NormalizedCacheItem, CacheError> {
    validate_common(
        &item.short_name,
        &item.name,
        &item.interfaces,
        &item.states,
        limits,
    )?;
    if item.children.len() > limits.max_children {
        return Err(CacheError::LimitExceeded {
            resource: "legacy children",
            actual: item.children.len(),
            max: limits.max_children,
        });
    }
    let child_count = item.children.len();
    let external_application_parent =
        application_root_has_external_parent(&item.object, &item.application, item.parent.as_ref());
    let normalized = NormalizedCacheItem {
        object: item.object,
        application: item.application,
        parent: if external_application_parent {
            None
        } else {
            item.parent
        },
        index_in_parent: None,
        child_count: Some(child_count),
        legacy_children: item.children,
        interfaces: item.interfaces,
        name: item.short_name,
        description: item.name,
        role: item.role,
        text_protection: classify_text_protection(item.role),
        states: item.states,
    };
    validate_item_bytes(&normalized, limits)?;
    Ok(normalized)
}

pub(crate) fn application_root_has_external_parent(
    object: &ObjectAddress,
    application: &ObjectAddress,
    parent: Option<&ObjectAddress>,
) -> bool {
    object == application
        && parent.is_some_and(|parent| parent.bus_name() != application.bus_name())
}

fn classify_text_protection(role: u32) -> TextProtection {
    match role {
        // AT-SPI's stable Role::PasswordText numeric value.
        40 => TextProtection::Protected,
        // Pinned standard role range. Future roles fail closed until reviewed.
        0..=129 => TextProtection::Unprotected,
        _ => TextProtection::Unknown,
    }
}

fn validate_item_bytes(
    item: &NormalizedCacheItem,
    limits: CacheLimits,
) -> Result<usize, CacheError> {
    let bytes = estimated_item_bytes(item);
    if bytes > limits.max_item_bytes {
        return Err(CacheError::LimitExceeded {
            resource: "cache item bytes",
            actual: bytes,
            max: limits.max_item_bytes,
        });
    }
    Ok(bytes)
}

fn validate_normalized_item(
    item: &NormalizedCacheItem,
    limits: CacheLimits,
) -> Result<(), CacheError> {
    validate_common(
        &item.name,
        &item.description,
        &item.interfaces,
        &item.states,
        limits,
    )?;
    if item.legacy_children.len() > limits.max_children {
        return Err(CacheError::LimitExceeded {
            resource: "legacy children",
            actual: item.legacy_children.len(),
            max: limits.max_children,
        });
    }
    validate_item_bytes(item, limits).map(|_| ())
}

fn validate_common(
    name: &str,
    description: &str,
    interfaces: &[String],
    states: &[u32],
    limits: CacheLimits,
) -> Result<(), CacheError> {
    for (resource, value) in [
        ("accessible name", name),
        ("accessible description", description),
    ] {
        if value.len() > limits.max_string_bytes {
            return Err(CacheError::LimitExceeded {
                resource,
                actual: value.len(),
                max: limits.max_string_bytes,
            });
        }
    }
    if interfaces.len() > limits.max_interfaces {
        return Err(CacheError::LimitExceeded {
            resource: "interfaces",
            actual: interfaces.len(),
            max: limits.max_interfaces,
        });
    }
    if states.len() > limits.max_states {
        return Err(CacheError::LimitExceeded {
            resource: "states",
            actual: states.len(),
            max: limits.max_states,
        });
    }
    if interfaces
        .iter()
        .any(|interface| interface.len() > limits.max_string_bytes)
    {
        return Err(CacheError::LimitExceeded {
            resource: "interface name bytes",
            actual: interfaces.iter().map(String::len).max().unwrap_or(0),
            max: limits.max_string_bytes,
        });
    }
    Ok(())
}

/// Incremental cache input accepted by the single-owner actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheEvent {
    /// Insert or replace one normalized object.
    Upsert(Box<NormalizedCacheItem>),
    /// Remove one object and descendants whose parent links are known.
    Remove(ObjectAddress),
    /// A unique application owner disappeared or restarted.
    InvalidateApplication(String),
    /// Event ordering or decoding became untrustworthy.
    ProtocolGap,
}

/// Observable result of applying one incremental cache input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheMutationKind {
    /// A node was added or replaced.
    Upserted,
    /// An existing object's fields refreshed without changing its public birth.
    Refreshed,
    /// One or more nodes were removed.
    Removed,
    /// An application generation advanced and its nodes were removed.
    ApplicationInvalidated,
    /// No state changed because the object was already absent.
    Unchanged,
    /// Partial history must be discarded and a full rebuild scheduled.
    ResyncRequired,
}

/// Bounded mutation data sufficient for a mirror to update without a tree scan.
#[derive(Clone, Debug, PartialEq)]
pub enum CacheMutationDetail {
    /// Exact node inserted or replaced at this revision.
    Upserted(Box<CachedNode>),
    /// Exact existing node refreshed while preserving identity continuity.
    Refreshed(Box<CachedNode>),
    /// Exact object addresses removed by one subtree mutation.
    Removed(Vec<ObjectAddress>),
    /// Exact application invalidation and bounded removed addresses.
    ApplicationInvalidated {
        /// Unique application owner that was invalidated.
        bus_name: String,
        /// New actor-owned application generation.
        application_generation: u64,
        /// Addresses removed from the mirror.
        removed: Vec<ObjectAddress>,
    },
    /// No observable cache state changed.
    Unchanged,
    /// Mutation was coherent locally but too large to copy, or history was lost.
    ResyncRequired,
}

/// Cache mutation metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheMutation {
    /// Monotonic cache revision after the operation.
    pub revision: u64,
    /// Semantic mutation category.
    pub kind: CacheMutationKind,
    /// Bounded node/address detail for incremental mirrors.
    pub detail: CacheMutationDetail,
}

/// One cache entry with adapter-owned generation and revision evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedNode {
    /// Normalized toolkit data.
    pub item: NormalizedCacheItem,
    /// Application instance generation at insertion time.
    pub application_generation: u64,
    /// Cache revision at insertion time.
    pub revision: u64,
    /// Secret-safe cache identity used for fresh pre-dispatch comparison.
    pub identity_fingerprint: IdentityFingerprint,
    /// Optional bounded live metadata populated by targeted refreshes.
    pub live: CachedLiveMetadata,
}

/// Secret-safe live fields not present in the standard Cache item signature.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CachedLiveMetadata {
    /// Screen-coordinate component bounds.
    pub bounds: Option<SemanticRect>,
    /// Finite Value interface properties.
    pub value: Option<SemanticValueEvidence>,
    /// Content-free Text metrics. Known-protected nodes retain only safe offsets and lengths.
    pub text: Option<CachedTextMetadata>,
    /// Selected-child count from Selection.
    pub selected_children: Option<u32>,
}

/// Content-free Text metrics retained by the cache.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CachedTextMetadata {
    /// Character count, omitted only when protection cannot be classified safely.
    pub character_count: Option<u32>,
    /// Caret offset, `-1` for no caret, or omitted when protection cannot be classified safely.
    pub caret_offset: Option<i32>,
    /// Selection offsets, empty when protection cannot be classified safely.
    pub selections: Vec<SelectionRangeEvidence>,
}

/// One common cache item plus optional targeted live metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct RefreshedCacheItem {
    /// Refreshed common Accessible fields.
    pub item: NormalizedCacheItem,
    /// Refreshed secret-safe optional interface fields.
    pub live: CachedLiveMetadata,
}

/// Deterministic bounded cache page copied out of the actor-owned model.
#[derive(Clone, Debug, PartialEq)]
pub struct CachePage {
    /// Accessibility connection generation that owns every node in this page.
    pub accessibility_generation: u64,
    /// Cache revision shared by every node in this page request.
    pub revision: u64,
    /// Public-event overflow epoch atomically fenced by the actor page handler.
    pub event_overflow_epoch: u64,
    /// Exact exclusive address supplied by the page request.
    pub after: Option<ObjectAddress>,
    /// Nodes ordered by unique bus name and object path.
    pub nodes: Vec<CachedNode>,
    /// Last returned address when more nodes remain at this revision.
    pub next_after: Option<ObjectAddress>,
    /// Estimated normalized bytes in `nodes`.
    pub estimated_bytes: usize,
}

/// Bounded cache owned exclusively by the AT-SPI actor task.
#[derive(Debug)]
pub struct BoundedCache {
    limits: CacheLimits,
    nodes: BTreeMap<ObjectAddress, CachedNode>,
    application_generations: BTreeMap<String, u64>,
    revision: u64,
    bytes: usize,
}

impl BoundedCache {
    /// Create an empty cache after validating its fixed ceilings.
    pub fn new(limits: CacheLimits) -> Result<Self, CacheError> {
        Ok(Self {
            limits: limits.validate()?,
            nodes: BTreeMap::new(),
            application_generations: BTreeMap::new(),
            revision: 0,
            bytes: 0,
        })
    }

    /// Number of live cache nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether no live cache nodes exist.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Current monotonic revision.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Estimated normalized bytes retained by live cache items.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Fixed admission limits governing this actor-owned cache.
    #[must_use]
    pub const fn limits(&self) -> CacheLimits {
        self.limits
    }

    /// Look up one object without exposing a bus proxy.
    #[must_use]
    pub fn get(&self, address: &ObjectAddress) -> Option<&CachedNode> {
        self.nodes.get(address)
    }

    /// Copy a deterministic page under explicit node and byte ceilings.
    pub fn page(
        &self,
        accessibility_generation: u64,
        after: Option<&ObjectAddress>,
        max_nodes: usize,
        max_bytes: usize,
    ) -> Result<CachePage, CacheError> {
        if max_nodes == 0 || max_bytes == 0 {
            return Err(CacheError::InvalidLimit("cache page"));
        }
        let mut eligible = self
            .nodes
            .iter()
            .filter(|(address, _)| after.is_none_or(|after| *address > after))
            .peekable();
        let mut nodes = Vec::new();
        let mut estimated_bytes = 0_usize;
        while nodes.len() < max_nodes {
            let Some((address, node)) = eligible.peek() else {
                break;
            };
            let node_bytes = estimated_cached_node_bytes(node);
            let next_bytes =
                estimated_bytes
                    .checked_add(node_bytes)
                    .ok_or(CacheError::LimitExceeded {
                        resource: "cache page bytes",
                        actual: usize::MAX,
                        max: max_bytes,
                    })?;
            if next_bytes > max_bytes {
                if nodes.is_empty() {
                    return Err(CacheError::LimitExceeded {
                        resource: "cache page bytes",
                        actual: node_bytes,
                        max: max_bytes,
                    });
                }
                break;
            }
            let address = (*address).clone();
            let node = (*node).clone();
            let _consumed = eligible.next();
            estimated_bytes = next_bytes;
            nodes.push((address, node));
        }
        let next_after = if eligible.peek().is_some() {
            nodes.last().map(|(address, _)| address.clone())
        } else {
            None
        };
        Ok(CachePage {
            accessibility_generation,
            revision: self.revision,
            event_overflow_epoch: 0,
            after: after.cloned(),
            nodes: nodes.into_iter().map(|(_, node)| node).collect(),
            next_after,
            estimated_bytes,
        })
    }

    /// Current instance generation for one unique application bus name.
    #[must_use]
    pub fn application_generation(&self, bus_name: &str) -> u64 {
        self.application_generations
            .get(bus_name)
            .copied()
            .unwrap_or(1)
    }

    /// Atomically replace the cache after a bounded `GetItems` bootstrap.
    pub fn replace(&mut self, items: Vec<NormalizedCacheItem>) -> Result<u64, CacheError> {
        if items.len() > self.limits.max_nodes {
            return Err(CacheError::LimitExceeded {
                resource: "cached nodes",
                actual: items.len(),
                max: self.limits.max_nodes,
            });
        }
        let unique: BTreeSet<_> = items.iter().map(|item| item.object.clone()).collect();
        if unique.len() != items.len() {
            return Err(CacheError::Malformed("duplicate object in cache bootstrap"));
        }
        let mut total_bytes = 0_usize;
        for item in &items {
            validate_normalized_item(item, self.limits)?;
            total_bytes = total_bytes
                .checked_add(validate_item_bytes(item, self.limits)?)
                .ok_or(CacheError::LimitExceeded {
                    resource: "total cache bytes",
                    actual: usize::MAX,
                    max: self.limits.max_total_bytes,
                })?;
        }
        if total_bytes > self.limits.max_total_bytes {
            return Err(CacheError::LimitExceeded {
                resource: "total cache bytes",
                actual: total_bytes,
                max: self.limits.max_total_bytes,
            });
        }
        let revision = self.bump_revision()?;
        let mut nodes = BTreeMap::new();
        for item in items {
            let generation = self.application_generation(item.application.bus_name());
            nodes.insert(
                item.object.clone(),
                CachedNode {
                    identity_fingerprint: item.identity_fingerprint(),
                    item,
                    application_generation: generation,
                    revision,
                    live: CachedLiveMetadata::default(),
                },
            );
        }
        self.nodes = nodes;
        self.bytes = total_bytes;
        Ok(revision)
    }

    /// Apply one actor-owned targeted refresh, preserving revision on no-op reads.
    pub fn refresh(&mut self, refreshed: RefreshedCacheItem) -> Result<CacheMutation, CacheError> {
        let RefreshedCacheItem { item, live } = refreshed;
        validate_normalized_item(&item, self.limits)?;
        validate_live_metadata(&live, self.limits)?;
        if !self.nodes.contains_key(&item.object) {
            return Err(CacheError::Malformed(
                "targeted refresh source is absent from cache",
            ));
        }
        if self.nodes.get(&item.object).is_some_and(|current| {
            current.item == item
                && current.live == live
                && current.application_generation
                    == self.application_generation(item.application.bus_name())
        }) {
            return Ok(CacheMutation {
                revision: self.revision,
                kind: CacheMutationKind::Unchanged,
                detail: CacheMutationDetail::Unchanged,
            });
        }
        let item_bytes = validate_item_bytes(&item, self.limits)?;
        let live_bytes = estimated_live_metadata_bytes(&live);
        let refreshed_bytes =
            item_bytes
                .checked_add(live_bytes)
                .ok_or(CacheError::LimitExceeded {
                    resource: "refreshed cache item bytes",
                    actual: usize::MAX,
                    max: self.limits.max_item_bytes,
                })?;
        if refreshed_bytes > self.limits.max_item_bytes {
            return Err(CacheError::LimitExceeded {
                resource: "refreshed cache item bytes",
                actual: refreshed_bytes,
                max: self.limits.max_item_bytes,
            });
        }
        let replaced_bytes = self
            .nodes
            .get(&item.object)
            .map(estimated_cached_node_bytes)
            .unwrap_or(0);
        let total_bytes = self
            .bytes
            .saturating_sub(replaced_bytes)
            .checked_add(refreshed_bytes)
            .ok_or(CacheError::LimitExceeded {
                resource: "total cache bytes",
                actual: usize::MAX,
                max: self.limits.max_total_bytes,
            })?;
        if total_bytes > self.limits.max_total_bytes {
            return Err(CacheError::LimitExceeded {
                resource: "total cache bytes",
                actual: total_bytes,
                max: self.limits.max_total_bytes,
            });
        }
        self.bump_revision()?;
        let generation = self.application_generation(item.application.bus_name());
        let object = item.object.clone();
        self.nodes.insert(
            object.clone(),
            CachedNode {
                identity_fingerprint: item.identity_fingerprint(),
                item,
                application_generation: generation,
                revision: self.revision,
                live,
            },
        );
        self.bytes = total_bytes;
        let node = self
            .nodes
            .get(&object)
            .cloned()
            .ok_or(CacheError::Malformed("refreshed cache node disappeared"))?;
        Ok(CacheMutation {
            revision: self.revision,
            kind: CacheMutationKind::Refreshed,
            detail: CacheMutationDetail::Refreshed(Box::new(node)),
        })
    }

    /// Apply an incremental cache event without ever exceeding configured memory cardinality.
    pub fn apply(&mut self, event: CacheEvent) -> Result<CacheMutation, CacheError> {
        let (kind, detail) = match event {
            CacheEvent::Upsert(item) => {
                let item = *item;
                validate_normalized_item(&item, self.limits)?;
                if !self.nodes.contains_key(&item.object)
                    && self.nodes.len() == self.limits.max_nodes
                {
                    return Err(CacheError::LimitExceeded {
                        resource: "cached nodes",
                        actual: self.nodes.len().saturating_add(1),
                        max: self.limits.max_nodes,
                    });
                }
                let item_bytes = validate_item_bytes(&item, self.limits)?;
                let replaced_bytes = self
                    .nodes
                    .get(&item.object)
                    .map(estimated_cached_node_bytes)
                    .unwrap_or(0);
                let total_bytes = self
                    .bytes
                    .saturating_sub(replaced_bytes)
                    .checked_add(item_bytes)
                    .ok_or(CacheError::LimitExceeded {
                        resource: "total cache bytes",
                        actual: usize::MAX,
                        max: self.limits.max_total_bytes,
                    })?;
                if total_bytes > self.limits.max_total_bytes {
                    return Err(CacheError::LimitExceeded {
                        resource: "total cache bytes",
                        actual: total_bytes,
                        max: self.limits.max_total_bytes,
                    });
                }
                self.bump_revision()?;
                let generation = self.application_generation(item.application.bus_name());
                let object = item.object.clone();
                self.nodes.insert(
                    item.object.clone(),
                    CachedNode {
                        identity_fingerprint: item.identity_fingerprint(),
                        item,
                        application_generation: generation,
                        revision: self.revision,
                        live: CachedLiveMetadata::default(),
                    },
                );
                self.bytes = total_bytes;
                let node = self
                    .nodes
                    .get(&object)
                    .cloned()
                    .ok_or(CacheError::Malformed("upserted cache node disappeared"))?;
                (
                    CacheMutationKind::Upserted,
                    CacheMutationDetail::Upserted(Box::new(node)),
                )
            }
            CacheEvent::Remove(address) => self.remove_subtree(&address)?,
            CacheEvent::InvalidateApplication(bus_name) => {
                if bus_name.is_empty()
                    || !bus_name.starts_with(':')
                    || bus_name.len() > MAX_ADDRESS_BYTES
                {
                    return Err(CacheError::InvalidAddress(
                        "application bus name is not a bounded unique name",
                    ));
                }
                let known_application = self.application_generations.contains_key(&bus_name)
                    || self.nodes.values().any(|node| {
                        node.item.application.bus_name() == bus_name
                            || node.item.object.bus_name() == bus_name
                    });
                if !known_application {
                    return Ok(CacheMutation {
                        revision: self.revision,
                        kind: CacheMutationKind::Unchanged,
                        detail: CacheMutationDetail::Unchanged,
                    });
                }
                if !self.application_generations.contains_key(&bus_name)
                    && self.application_generations.len() == self.limits.max_nodes
                {
                    return Err(CacheError::LimitExceeded {
                        resource: "application generation tombstones",
                        actual: self.application_generations.len().saturating_add(1),
                        max: self.limits.max_nodes,
                    });
                }
                let generation = self
                    .application_generation(&bus_name)
                    .checked_add(1)
                    .ok_or(CacheError::GenerationExhausted("application"))?;
                let removed = self
                    .nodes
                    .iter()
                    .filter(|(_, node)| {
                        node.item.application.bus_name() == bus_name
                            || node.item.object.bus_name() == bus_name
                    })
                    .map(|(address, _)| address.clone())
                    .take(MAX_MUTATION_ADDRESSES.saturating_add(1))
                    .collect::<Vec<_>>();
                self.bump_revision()?;
                self.application_generations
                    .insert(bus_name.clone(), generation);
                self.nodes.retain(|_, node| {
                    node.item.application.bus_name() != bus_name
                        && node.item.object.bus_name() != bus_name
                });
                self.recalculate_bytes();
                if removed.len() > MAX_MUTATION_ADDRESSES
                    || !mutation_addresses_fit(&removed, self.limits.max_item_bytes)
                {
                    (
                        CacheMutationKind::ResyncRequired,
                        CacheMutationDetail::ResyncRequired,
                    )
                } else {
                    (
                        CacheMutationKind::ApplicationInvalidated,
                        CacheMutationDetail::ApplicationInvalidated {
                            bus_name,
                            application_generation: generation,
                            removed,
                        },
                    )
                }
            }
            CacheEvent::ProtocolGap => {
                self.bump_revision()?;
                self.nodes.clear();
                self.bytes = 0;
                (
                    CacheMutationKind::ResyncRequired,
                    CacheMutationDetail::ResyncRequired,
                )
            }
        };
        Ok(CacheMutation {
            revision: self.revision,
            kind,
            detail,
        })
    }

    /// Clear nodes and application generations after the actor advances its global generation.
    pub fn invalidate_all(&mut self) -> Result<u64, CacheError> {
        let revision = self.bump_revision()?;
        self.nodes.clear();
        self.application_generations.clear();
        self.bytes = 0;
        Ok(revision)
    }

    fn remove_subtree(
        &mut self,
        address: &ObjectAddress,
    ) -> Result<(CacheMutationKind, CacheMutationDetail), CacheError> {
        if !self.nodes.contains_key(address) {
            return Ok((CacheMutationKind::Unchanged, CacheMutationDetail::Unchanged));
        }
        let mut children = BTreeMap::<ObjectAddress, Vec<ObjectAddress>>::new();
        for (key, node) in &self.nodes {
            if let Some(parent) = &node.item.parent {
                children
                    .entry(parent.clone())
                    .or_default()
                    .push(key.clone());
            }
        }
        let mut remove = BTreeSet::new();
        let mut pending = vec![address.clone()];
        while let Some(current) = pending.pop() {
            if !remove.insert(current.clone()) {
                continue;
            }
            if let Some(descendants) = children.get(&current) {
                pending.extend(descendants.iter().cloned());
            }
        }
        self.bump_revision()?;
        self.nodes.retain(|key, _| !remove.contains(key));
        self.recalculate_bytes();
        let removed = remove.into_iter().collect::<Vec<_>>();
        if removed.len() > MAX_MUTATION_ADDRESSES
            || !mutation_addresses_fit(&removed, self.limits.max_item_bytes)
        {
            Ok((
                CacheMutationKind::ResyncRequired,
                CacheMutationDetail::ResyncRequired,
            ))
        } else {
            Ok((
                CacheMutationKind::Removed,
                CacheMutationDetail::Removed(removed),
            ))
        }
    }

    fn bump_revision(&mut self) -> Result<u64, CacheError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(CacheError::GenerationExhausted("cache revision"))?;
        Ok(self.revision)
    }

    #[cfg(test)]
    fn set_revision_for_test(&mut self, revision: u64) {
        self.revision = revision;
    }

    #[cfg(test)]
    fn set_application_generation_for_test(&mut self, bus_name: String, generation: u64) {
        self.application_generations.insert(bus_name, generation);
    }

    fn recalculate_bytes(&mut self) {
        self.bytes = self.nodes.values().fold(0_usize, |total, node| {
            total.saturating_add(estimated_cached_node_bytes(node))
        });
    }
}

fn mutation_addresses_fit(addresses: &[ObjectAddress], max_bytes: usize) -> bool {
    addresses
        .iter()
        .try_fold(0_usize, |total, address| {
            total
                .checked_add(std::mem::size_of::<ObjectAddress>())?
                .checked_add(address.bus_name().len())?
                .checked_add(address.object_path().len())
        })
        .is_some_and(|bytes| bytes <= max_bytes)
}

fn estimated_item_bytes(item: &NormalizedCacheItem) -> usize {
    let mut bytes = std::mem::size_of::<CachedNode>()
        .saturating_add(
            item.object
                .bus_name()
                .len()
                .saturating_add(item.object.object_path().len())
                .saturating_add(item.application.bus_name().len())
                .saturating_add(item.application.object_path().len())
                .saturating_add(item.name.len())
                .saturating_add(item.description.len()),
        )
        .saturating_add(item.states.len().saturating_mul(std::mem::size_of::<u32>()))
        .saturating_add(
            item.interfaces
                .len()
                .saturating_mul(std::mem::size_of::<String>()),
        )
        .saturating_add(
            item.legacy_children
                .len()
                .saturating_mul(std::mem::size_of::<ObjectAddress>()),
        );
    if let Some(parent) = &item.parent {
        bytes = bytes
            .saturating_add(parent.bus_name().len())
            .saturating_add(parent.object_path().len());
    }
    for interface in &item.interfaces {
        bytes = bytes.saturating_add(interface.len());
    }
    for child in &item.legacy_children {
        bytes = bytes
            .saturating_add(child.bus_name().len())
            .saturating_add(child.object_path().len());
    }
    bytes
}

fn estimated_cached_node_bytes(node: &CachedNode) -> usize {
    estimated_item_bytes(&node.item).saturating_add(estimated_live_metadata_bytes(&node.live))
}

fn estimated_live_metadata_bytes(live: &CachedLiveMetadata) -> usize {
    live.text.as_ref().map_or(0, |text| {
        text.selections
            .len()
            .saturating_mul(std::mem::size_of::<SelectionRangeEvidence>())
    })
}

fn validate_live_metadata(
    live: &CachedLiveMetadata,
    limits: CacheLimits,
) -> Result<(), CacheError> {
    if live.value.is_some_and(|value| {
        !value.current.is_finite()
            || !value.minimum.is_finite()
            || !value.maximum.is_finite()
            || !value.minimum_increment.is_finite()
    }) {
        return Err(CacheError::Malformed("non-finite live Value metadata"));
    }
    if let Some(text) = &live.text {
        if text.selections.len() > MAX_SELECTION_RANGES || text.selections.len() > limits.max_states
        {
            return Err(CacheError::LimitExceeded {
                resource: "live Text selection ranges",
                actual: text.selections.len(),
                max: MAX_SELECTION_RANGES.min(limits.max_states),
            });
        }
        match (text.character_count, text.caret_offset) {
            (None, None) if text.selections.is_empty() => {}
            (Some(_), Some(_)) => {}
            _ => {
                return Err(CacheError::Malformed(
                    "live Text metadata is partial or retained ranges without safe bounds",
                ));
            }
        }
        if let (Some(count), Some(caret)) = (text.character_count, text.caret_offset)
            && (caret < -1 || u32::try_from(caret).is_ok_and(|caret| caret > count))
        {
            return Err(CacheError::Malformed(
                "live Text caret is below -1 or exceeds character count",
            ));
        }
    }
    Ok(())
}

/// Cache normalization or bounded-state error.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CacheError {
    /// A configured limit cannot represent any useful value.
    #[error("AT-SPI cache limit must be nonzero: {0}")]
    InvalidLimit(&'static str),
    /// A toolkit object address failed basic central-bus validation.
    #[error("invalid AT-SPI object address: {0}")]
    InvalidAddress(&'static str),
    /// Cache data was structurally impossible or contradictory.
    #[error("malformed AT-SPI cache data: {0}")]
    Malformed(&'static str),
    /// Monotonic freshness evidence reached its integer ceiling.
    #[error("AT-SPI freshness generation exhausted: {0}")]
    GenerationExhausted(&'static str),
    /// Input exceeded a fixed cache admission ceiling.
    #[error("AT-SPI {resource} limit exceeded: {actual} > {max}")]
    LimitExceeded {
        /// Static resource label.
        resource: &'static str,
        /// Observed value.
        actual: usize,
        /// Enforced maximum.
        max: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(bus: &str, suffix: &str) -> Result<ObjectAddress, CacheError> {
        ObjectAddress::new(bus, format!("/test/{suffix}"))
    }

    fn item(
        bus: &str,
        suffix: &str,
        parent: Option<&str>,
    ) -> Result<NormalizedCacheItem, CacheError> {
        Ok(NormalizedCacheItem {
            object: address(bus, suffix)?,
            application: address(bus, "app")?,
            parent: parent.map(|value| address(bus, value)).transpose()?,
            index_in_parent: Some(0),
            child_count: Some(0),
            legacy_children: Vec::new(),
            interfaces: vec!["org.a11y.atspi.Accessible".to_owned()],
            name: suffix.to_owned(),
            description: String::new(),
            role: 0,
            text_protection: TextProtection::Unprotected,
            states: Vec::new(),
        })
    }

    #[test]
    fn modern_and_legacy_names_follow_pinned_wire_semantics() -> Result<(), CacheError> {
        let modern = normalize_modern(
            ModernCacheItem {
                object: address(":1.1", "node")?,
                application: address(":1.1", "app")?,
                parent: None,
                index_in_parent: -1,
                child_count: 0,
                interfaces: Vec::new(),
                short_name: "name".to_owned(),
                role: 7,
                name: "description".to_owned(),
                states: Vec::new(),
            },
            CacheLimits::default(),
        )?;
        assert_eq!(modern.name, "name");
        assert_eq!(modern.description, "description");
        assert_eq!(modern.index_in_parent, None);

        let child = address(":1.1", "child")?;
        let legacy = normalize_legacy(
            LegacyCacheItem {
                object: address(":1.1", "node")?,
                application: address(":1.1", "app")?,
                parent: None,
                children: vec![child.clone()],
                interfaces: Vec::new(),
                short_name: "legacy".to_owned(),
                role: 9,
                name: "legacy description".to_owned(),
                states: Vec::new(),
            },
            CacheLimits::default(),
        )?;
        assert_eq!(legacy.child_count, Some(1));
        assert_eq!(legacy.legacy_children, vec![child]);
        Ok(())
    }

    #[test]
    fn modern_child_count_accepts_only_the_standard_unknown_sentinel() -> Result<(), CacheError> {
        let normalized = |child_count| {
            normalize_modern(
                ModernCacheItem {
                    object: address(":1.1", "node")?,
                    application: address(":1.1", "app")?,
                    parent: None,
                    index_in_parent: 0,
                    child_count,
                    interfaces: Vec::new(),
                    short_name: String::new(),
                    role: 0,
                    name: String::new(),
                    states: Vec::new(),
                },
                CacheLimits::default(),
            )
        };

        assert_eq!(normalized(-1)?.child_count, None);
        assert_eq!(normalized(0)?.child_count, Some(0));
        assert_eq!(normalized(7)?.child_count, Some(7));
        assert!(matches!(
            normalized(-2),
            Err(CacheError::Malformed("negative child count"))
        ));
        Ok(())
    }

    #[test]
    fn modern_unindexed_menu_items_detach_only_safe_transient_parents() -> Result<(), CacheError> {
        let application = address(":1.1", "app")?;
        let parent = address(":1.1", "menu")?;
        let normalized = |role, index_in_parent, object, parent| {
            normalize_modern(
                ModernCacheItem {
                    object,
                    application: application.clone(),
                    parent: Some(parent),
                    index_in_parent,
                    child_count: 0,
                    interfaces: Vec::new(),
                    short_name: String::new(),
                    role,
                    name: String::new(),
                    states: Vec::new(),
                },
                CacheLimits::default(),
            )
        };

        for role in [8, 35, 45, 59] {
            let item = normalized(role, -1, address(":1.1", "item")?, parent.clone())?;
            assert_eq!(item.parent, None);
            assert_eq!(item.index_in_parent, None);
        }
        for role in [43, 129, u32::MAX] {
            let item = normalized(role, -1, address(":1.1", "item")?, parent.clone())?;
            assert_eq!(item.parent.as_ref(), Some(&parent));
            assert_eq!(item.index_in_parent, None);
        }

        let indexed = normalized(35, 0, address(":1.1", "item")?, parent.clone())?;
        assert_eq!(indexed.parent.as_ref(), Some(&parent));
        assert_eq!(indexed.index_in_parent, Some(0));

        let object = address(":1.1", "item")?;
        let self_parent = normalized(35, -1, object.clone(), object.clone())?;
        assert_eq!(self_parent.parent, Some(object));

        let external_parent = address(":1.2", "menu")?;
        let cross_owner = normalized(35, -1, address(":1.1", "item")?, external_parent.clone())?;
        assert_eq!(cross_owner.parent, Some(external_parent));

        let legacy_parent = address(":1.1", "legacy-menu")?;
        let legacy = normalize_legacy(
            LegacyCacheItem {
                object: address(":1.1", "legacy-item")?,
                application,
                parent: Some(legacy_parent.clone()),
                children: Vec::new(),
                interfaces: Vec::new(),
                short_name: String::new(),
                role: 35,
                name: String::new(),
                states: Vec::new(),
            },
            CacheLimits::default(),
        )?;
        assert_eq!(legacy.parent, Some(legacy_parent));
        Ok(())
    }

    #[test]
    fn application_root_normalization_detaches_only_cross_owner_registry_edges()
    -> Result<(), CacheError> {
        let application = address(":1.2", "app")?;
        let registry = address(":1.1", "registry")?;
        let modern = normalize_modern(
            ModernCacheItem {
                object: application.clone(),
                application: application.clone(),
                parent: Some(registry.clone()),
                index_in_parent: 7,
                child_count: 1,
                interfaces: Vec::new(),
                short_name: "application".to_owned(),
                role: 75,
                name: String::new(),
                states: Vec::new(),
            },
            CacheLimits::default(),
        )?;
        assert_eq!(modern.parent, None);
        assert_eq!(modern.index_in_parent, None);

        let legacy = normalize_legacy(
            LegacyCacheItem {
                object: application.clone(),
                application: application.clone(),
                parent: Some(registry),
                children: Vec::new(),
                interfaces: Vec::new(),
                short_name: "application".to_owned(),
                role: 75,
                name: String::new(),
                states: Vec::new(),
            },
            CacheLimits::default(),
        )?;
        assert_eq!(legacy.parent, None);

        let same_owner_parent = address(":1.2", "malformed-parent")?;
        let malformed_root = normalize_modern(
            ModernCacheItem {
                object: application.clone(),
                application: application.clone(),
                parent: Some(same_owner_parent.clone()),
                index_in_parent: 3,
                child_count: 0,
                interfaces: Vec::new(),
                short_name: String::new(),
                role: 75,
                name: String::new(),
                states: Vec::new(),
            },
            CacheLimits::default(),
        )?;
        assert_eq!(malformed_root.parent, Some(same_owner_parent));
        assert_eq!(malformed_root.index_in_parent, Some(3));

        let child = normalize_modern(
            ModernCacheItem {
                object: address(":1.2", "child")?,
                application,
                parent: Some(address(":1.2", "app")?),
                index_in_parent: 0,
                child_count: 0,
                interfaces: Vec::new(),
                short_name: String::new(),
                role: 0,
                name: String::new(),
                states: Vec::new(),
            },
            CacheLimits::default(),
        )?;
        assert_eq!(child.parent, Some(address(":1.2", "app")?));
        assert_eq!(child.index_in_parent, Some(0));
        Ok(())
    }

    #[test]
    fn bootstrap_is_atomic_and_rejects_duplicates_or_overflow() -> Result<(), CacheError> {
        let limits = CacheLimits {
            max_nodes: 2,
            ..CacheLimits::default()
        };
        let mut cache = BoundedCache::new(limits)?;
        cache.replace(vec![item(":1.1", "one", None)?])?;
        let revision = cache.revision();
        assert!(matches!(
            cache.replace(vec![item(":1.1", "x", None)?, item(":1.1", "x", None)?]),
            Err(CacheError::Malformed(_))
        ));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.revision(), revision);
        assert!(matches!(
            cache.replace(vec![
                item(":1.1", "one", None)?,
                item(":1.1", "two", None)?,
                item(":1.1", "three", None)?,
            ]),
            Err(CacheError::LimitExceeded { .. })
        ));
        assert_eq!(cache.len(), 1);
        Ok(())
    }

    #[test]
    fn remove_cascades_by_parent_without_recursive_stack_growth() -> Result<(), CacheError> {
        let mut cache = BoundedCache::new(CacheLimits::default())?;
        cache.replace(vec![
            item(":1.2", "root", None)?,
            item(":1.2", "child", Some("root"))?,
            item(":1.2", "grandchild", Some("child"))?,
        ])?;
        let result = cache.apply(CacheEvent::Remove(address(":1.2", "root")?))?;
        assert_eq!(result.kind, CacheMutationKind::Removed);
        assert!(cache.is_empty());
        Ok(())
    }

    #[test]
    fn application_owner_loss_fences_reused_paths() -> Result<(), CacheError> {
        let mut cache = BoundedCache::new(CacheLimits::default())?;
        let node = item(":1.3", "node", None)?;
        cache.apply(CacheEvent::Upsert(Box::new(node.clone())))?;
        assert_eq!(
            cache
                .get(&node.object)
                .map(|value| value.application_generation),
            Some(1)
        );
        cache.apply(CacheEvent::InvalidateApplication(":1.3".to_owned()))?;
        assert_eq!(cache.application_generation(":1.3"), 2);
        cache.apply(CacheEvent::Upsert(Box::new(node.clone())))?;
        assert_eq!(
            cache
                .get(&node.object)
                .map(|value| value.application_generation),
            Some(2)
        );
        Ok(())
    }

    #[test]
    fn full_cache_rejects_new_node_without_losing_existing_state() -> Result<(), CacheError> {
        let mut cache = BoundedCache::new(CacheLimits {
            max_nodes: 1,
            ..CacheLimits::default()
        })?;
        let first = item(":1.4", "first", None)?;
        cache.apply(CacheEvent::Upsert(Box::new(first.clone())))?;
        assert!(matches!(
            cache.apply(CacheEvent::Upsert(Box::new(item(":1.4", "second", None)?))),
            Err(CacheError::LimitExceeded { .. })
        ));
        assert!(cache.get(&first.object).is_some());
        Ok(())
    }

    #[test]
    fn aggregate_item_bytes_are_bounded_even_for_pre_normalized_input() -> Result<(), CacheError> {
        let limits = CacheLimits {
            max_item_bytes: 32,
            ..CacheLimits::default()
        };
        let mut cache = BoundedCache::new(limits)?;
        let oversized = item(":1.5", "this-name-alone-exceeds-the-aggregate", None)?;
        assert!(matches!(
            cache.apply(CacheEvent::Upsert(Box::new(oversized))),
            Err(CacheError::LimitExceeded {
                resource: "cache item bytes",
                ..
            })
        ));
        assert!(cache.is_empty());
        Ok(())
    }

    #[test]
    fn aggregate_cache_bytes_are_accounted_across_replace_upsert_and_remove()
    -> Result<(), CacheError> {
        let first = item(":1.6", "first", None)?;
        let second = item(":1.6", "two", None)?;
        let one_item_bytes = estimated_item_bytes(&first);
        let mut cache = BoundedCache::new(CacheLimits {
            max_total_bytes: one_item_bytes,
            max_item_bytes: one_item_bytes,
            ..CacheLimits::default()
        })?;
        cache.apply(CacheEvent::Upsert(Box::new(first.clone())))?;
        assert_eq!(cache.bytes(), one_item_bytes);
        assert!(matches!(
            cache.apply(CacheEvent::Upsert(Box::new(second))),
            Err(CacheError::LimitExceeded {
                resource: "total cache bytes",
                ..
            })
        ));
        cache.apply(CacheEvent::Remove(first.object))?;
        assert_eq!(cache.bytes(), 0);
        Ok(())
    }

    #[test]
    fn cache_pages_echo_the_exact_exclusive_cursor() -> Result<(), CacheError> {
        let mut cache = BoundedCache::new(CacheLimits::default())?;
        cache.replace(vec![
            item(":1.7", "alpha", None)?,
            item(":1.7", "bravo", None)?,
            item(":1.7", "charlie", None)?,
        ])?;

        let first = cache.page(9, None, 1, usize::MAX)?;
        assert_eq!(first.after, None);
        let after = first
            .next_after
            .ok_or(CacheError::Malformed("missing cache page continuation"))?;
        assert_eq!(after, address(":1.7", "alpha")?);

        let second = cache.page(9, Some(&after), 1, usize::MAX)?;
        assert_eq!(second.after, Some(after));
        assert_eq!(second.nodes.len(), 1);
        assert_eq!(second.nodes[0].item.object, address(":1.7", "bravo")?);
        Ok(())
    }

    #[test]
    fn unrelated_bus_owner_churn_does_not_allocate_generation_tombstones() -> Result<(), CacheError>
    {
        let mut cache = BoundedCache::new(CacheLimits {
            max_nodes: 1,
            ..CacheLimits::default()
        })?;
        for index in 0..10 {
            let mutation = cache.apply(CacheEvent::InvalidateApplication(format!(":9.{index}")))?;
            assert_eq!(mutation.kind, CacheMutationKind::Unchanged);
        }
        assert!(cache.application_generations.is_empty());
        Ok(())
    }

    #[test]
    fn revision_exhaustion_fails_before_mutating_cache_state() -> Result<(), CacheError> {
        let mut cache = BoundedCache::new(CacheLimits::default())?;
        let original = item(":1.99", "original", None)?;
        cache.replace(vec![original.clone()])?;
        cache.set_revision_for_test(u64::MAX);
        let replacement = item(":1.99", "replacement", None)?;
        assert_eq!(
            cache.apply(CacheEvent::Upsert(Box::new(replacement))),
            Err(CacheError::GenerationExhausted("cache revision"))
        );
        assert!(cache.get(&original.object).is_some());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.revision(), u64::MAX);
        Ok(())
    }

    #[test]
    fn application_generation_exhaustion_fails_before_invalidation() -> Result<(), CacheError> {
        let mut cache = BoundedCache::new(CacheLimits::default())?;
        let original = item(":1.100", "original", None)?;
        cache.replace(vec![original.clone()])?;
        let revision = cache.revision();
        cache.set_application_generation_for_test(":1.100".to_owned(), u64::MAX);
        assert_eq!(
            cache.apply(CacheEvent::InvalidateApplication(":1.100".to_owned())),
            Err(CacheError::GenerationExhausted("application"))
        );
        assert!(cache.get(&original.object).is_some());
        assert_eq!(cache.revision(), revision);
        Ok(())
    }

    #[test]
    fn targeted_live_refresh_is_paged_and_noop_preserves_revision() -> Result<(), CacheError> {
        let mut cache = BoundedCache::new(CacheLimits::default())?;
        let item = item(":1.101", "target", None)?;
        cache.replace(vec![item.clone()])?;
        let unchanged = cache.refresh(RefreshedCacheItem {
            item: item.clone(),
            live: CachedLiveMetadata::default(),
        })?;
        assert_eq!(unchanged.kind, CacheMutationKind::Unchanged);
        assert_eq!(cache.revision(), 1);

        let changed = cache.refresh(RefreshedCacheItem {
            item,
            live: CachedLiveMetadata {
                bounds: Some(SemanticRect {
                    x: 10,
                    y: 20,
                    width: 30,
                    height: 40,
                }),
                value: Some(SemanticValueEvidence {
                    current: 2.0,
                    minimum: 0.0,
                    maximum: 5.0,
                    minimum_increment: 1.0,
                }),
                text: Some(CachedTextMetadata {
                    character_count: Some(3),
                    // AT-SPI uses -1 for Text objects, such as Chromium
                    // buttons, that expose content but no active caret.
                    caret_offset: Some(-1),
                    selections: vec![SelectionRangeEvidence { start: 1, end: 2 }],
                }),
                selected_children: Some(1),
            },
        })?;
        assert_eq!(changed.kind, CacheMutationKind::Refreshed);
        assert_eq!(cache.revision(), 2);
        let page = cache.page(1, None, 1, usize::MAX)?;
        assert_eq!(
            page.nodes[0].live.bounds.map(|bounds| bounds.width),
            Some(30)
        );
        assert_eq!(
            page.nodes[0]
                .live
                .text
                .as_ref()
                .and_then(|text| text.character_count),
            Some(3)
        );
        assert_eq!(
            page.nodes[0]
                .live
                .text
                .as_ref()
                .and_then(|text| text.caret_offset),
            Some(-1)
        );
        Ok(())
    }

    #[test]
    fn targeted_live_refresh_rejects_nonfinite_and_partial_protected_metadata()
    -> Result<(), CacheError> {
        let mut cache = BoundedCache::new(CacheLimits::default())?;
        let item = item(":1.102", "target", None)?;
        cache.replace(vec![item.clone()])?;
        let nonfinite = cache.refresh(RefreshedCacheItem {
            item: item.clone(),
            live: CachedLiveMetadata {
                value: Some(SemanticValueEvidence {
                    current: f64::NAN,
                    minimum: 0.0,
                    maximum: 1.0,
                    minimum_increment: 1.0,
                }),
                ..CachedLiveMetadata::default()
            },
        });
        assert!(matches!(nonfinite, Err(CacheError::Malformed(_))));
        let partial_protected = cache.refresh(RefreshedCacheItem {
            item,
            live: CachedLiveMetadata {
                text: Some(CachedTextMetadata {
                    character_count: None,
                    caret_offset: Some(1),
                    selections: Vec::new(),
                }),
                ..CachedLiveMetadata::default()
            },
        });
        assert!(matches!(partial_protected, Err(CacheError::Malformed(_))));
        assert_eq!(cache.revision(), 1);
        Ok(())
    }

    #[test]
    fn targeted_refresh_rejects_an_unknown_object_without_mutation() -> Result<(), CacheError> {
        let mut cache = BoundedCache::new(CacheLimits::default())?;
        let known = item(":1.103", "known", None)?;
        cache.replace(vec![known.clone()])?;
        let result = cache.refresh(RefreshedCacheItem {
            item: item(":1.103", "unknown", None)?,
            live: CachedLiveMetadata::default(),
        });
        assert_eq!(
            result,
            Err(CacheError::Malformed(
                "targeted refresh source is absent from cache"
            ))
        );
        assert_eq!(cache.revision(), 1);
        assert!(cache.get(&known.object).is_some());
        Ok(())
    }
}
