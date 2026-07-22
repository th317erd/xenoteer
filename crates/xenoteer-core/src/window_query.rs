//! Pure, backend-independent window selection and wait evaluation.
//!
//! The observation actor supplies an immutable, generation-scoped snapshot
//! plus first-observed revision sidecars. This module never owns waiter
//! lifecycle: the actor must capture a revision, evaluate, register the wait,
//! and re-evaluate atomically before sleeping.

use std::{cmp::Ordering, collections::HashSet};

use regex_automata::{
    nfa::thompson::{
        NFA,
        pikevm::{Cache as PikeCache, PikeVM},
    },
    util::syntax,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use xenoteer_protocol::{
    DesktopGeneration, DesktopId, MAX_WINDOW_PAGE_LIMIT, MAX_WINDOW_REGEX_NFA_BYTES,
    MAX_WINDOW_WAIT_COUNT, WindowCountComparison, WindowGeometryArea, WindowGeometryPredicate,
    WindowMapState, WindowModelRevision, WindowOrder, WindowPredicate, WindowQueryValidationError,
    WindowRect, WindowRef, WindowSelector, WindowSingleMatchPolicy, WindowSnapshot,
    WindowStringMatch, WindowTextField, WindowValidationError, WindowWaitPredicate,
    WindowWaitSelectorQuantifier, WindowWaitTarget,
};

/// Domain separator for stable selector fingerprints embedded in continuations.
const SELECTOR_FINGERPRINT_DOMAIN: &[u8] = b"xenoteer-window-selector-v1\0";

/// One immutable window observation plus its non-inferable creation revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowQueryRecord {
    /// Snapshot projected at the view's common atomic revision.
    pub snapshot: WindowSnapshot,
    /// Revision at which this exact window birth first entered the model.
    pub created_revision: WindowModelRevision,
}

impl WindowQueryRecord {
    /// Creates a record only when explicit creation evidence is available.
    pub fn new(
        snapshot: WindowSnapshot,
        created_revision: WindowModelRevision,
    ) -> Result<Self, WindowQueryError> {
        snapshot
            .validate()
            .map_err(WindowQueryError::InvalidSnapshot)?;
        if created_revision > snapshot.model_revision {
            return Err(WindowQueryError::CreationRevisionAfterSnapshot);
        }
        Ok(Self {
            snapshot,
            created_revision,
        })
    }

    /// Converts optional sidecar evidence without inferring from the snapshot.
    pub fn with_optional_creation_revision(
        snapshot: WindowSnapshot,
        created_revision: Option<WindowModelRevision>,
    ) -> Result<Self, WindowQueryError> {
        let created_revision =
            created_revision.ok_or(WindowQueryError::CreationRevisionUnavailable)?;
        Self::new(snapshot, created_revision)
    }
}

/// Hash of the canonical bounded selector representation, not an authority token.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowSelectorFingerprint([u8; 32]);

impl WindowSelectorFingerprint {
    /// Returns the fingerprint bytes for a server-side signer.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for WindowSelectorFingerprint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("WindowSelectorFingerprint")
            .field(&"<opaque>")
            .finish()
    }
}

/// Query identity bound into a continuation before server authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WindowContinuationQuery {
    /// Unfiltered window list.
    List,
    /// Selector query bound to its stable fingerprint.
    Selector {
        /// Hash of the canonical selector wire representation.
        fingerprint: WindowSelectorFingerprint,
    },
}

/// Typed continuation state for the server to sign or store opaquely.
///
/// This descriptor is not authenticated by the core. A transport must bind it
/// to the principal and authenticate it before passing it back here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowContinuationDescriptor {
    /// Desktop resource owning the immutable snapshot.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime owning the immutable snapshot.
    pub desktop_generation: DesktopGeneration,
    /// Actor-local revision to which the offset applies.
    pub snapshot_revision: WindowModelRevision,
    /// Total ordering used before applying the offset.
    pub order: WindowOrder,
    /// List/query identity bound to this continuation.
    pub query: WindowContinuationQuery,
    /// Zero-based next element; always positive and short of the result length.
    pub next_offset: u32,
}

/// One immutable page ready for protocol projection and cursor signing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowPageProjection {
    /// Desktop resource owning the page.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime owning the page.
    pub desktop_generation: DesktopGeneration,
    /// Atomic model revision shared by every returned snapshot.
    pub snapshot_revision: WindowModelRevision,
    /// Bounded ordered snapshot projection.
    pub windows: Vec<WindowSnapshot>,
    /// Unsigned state for a server to authenticate, if another page exists.
    pub continuation: Option<WindowContinuationDescriptor>,
}

/// One successful single-window resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowResolveProjection {
    /// Desktop resource owning the resolution.
    pub desktop_id: DesktopId,
    /// Exact desktop lifetime owning the resolution.
    pub desktop_generation: DesktopGeneration,
    /// Atomic model revision at resolution.
    pub snapshot_revision: WindowModelRevision,
    /// Exact selected window birth.
    pub window: WindowSnapshot,
}

/// Pure evaluation evidence for one wait predicate at one model revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowWaitEvaluation<'a> {
    /// Whether the aggregate predicate is currently satisfied.
    pub predicate_satisfied: bool,
    /// Count selected by the reference or selector before state evaluation.
    pub selected_count: u32,
    /// Selected windows that individually satisfy the state predicate.
    pub satisfying_count: u32,
    /// Stable exact-reference order of individually satisfying windows.
    pub satisfying_windows: Vec<&'a WindowSnapshot>,
}

/// Validated immutable view over one desktop generation and atomic revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowQueryView<'a> {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    snapshot_revision: WindowModelRevision,
    records: &'a [WindowQueryRecord],
}

impl<'a> WindowQueryView<'a> {
    /// Validates scope, revision, cardinality, sidecars, and uniqueness once.
    pub fn new(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        snapshot_revision: WindowModelRevision,
        records: &'a [WindowQueryRecord],
    ) -> Result<Self, WindowQueryError> {
        if desktop_id.as_uuid().is_nil() || desktop_generation.as_uuid().is_nil() {
            return Err(WindowQueryError::NilIdentifier);
        }
        if records.len() > MAX_WINDOW_WAIT_COUNT as usize {
            return Err(WindowQueryError::TooManySnapshots);
        }
        let mut exact_references = HashSet::with_capacity(records.len());
        let mut xids = HashSet::with_capacity(records.len());
        for record in records {
            record
                .snapshot
                .validate()
                .map_err(WindowQueryError::InvalidSnapshot)?;
            if record.snapshot.window.desktop_id != desktop_id
                || record.snapshot.window.desktop_generation != desktop_generation
            {
                return Err(WindowQueryError::MixedScope);
            }
            if record.snapshot.model_revision != snapshot_revision {
                return Err(WindowQueryError::MixedRevision);
            }
            if record.created_revision > snapshot_revision {
                return Err(WindowQueryError::CreationRevisionAfterSnapshot);
            }
            if !exact_references.insert(record.snapshot.window.clone())
                || !xids.insert(record.snapshot.window.xid)
            {
                return Err(WindowQueryError::DuplicateSnapshot);
            }
        }
        Ok(Self {
            desktop_id,
            desktop_generation,
            snapshot_revision,
            records,
        })
    }

    /// Returns an unfiltered deterministic page.
    pub fn list(
        &self,
        order: WindowOrder,
        limit: u16,
        continuation: Option<&WindowContinuationDescriptor>,
    ) -> Result<WindowPageProjection, WindowQueryError> {
        let mut records = self.records.iter().collect::<Vec<_>>();
        sort_records(&mut records, order);
        self.project_page(
            records,
            order,
            WindowContinuationQuery::List,
            limit,
            continuation,
        )
    }

    /// Evaluates a selector once, then returns one deterministic page.
    pub fn query(
        &self,
        selector: &WindowSelector,
        order: WindowOrder,
        limit: u16,
        continuation: Option<&WindowContinuationDescriptor>,
    ) -> Result<WindowPageProjection, WindowQueryError> {
        let fingerprint = selector_fingerprint(selector)?;
        let mut compiled = self.compile_selector(selector)?;
        let mut records = self
            .records
            .iter()
            .filter(|record| compiled.matches(record))
            .collect::<Vec<_>>();
        sort_records(&mut records, order);
        self.project_page(
            records,
            order,
            WindowContinuationQuery::Selector { fingerprint },
            limit,
            continuation,
        )
    }

    /// Resolves exactly one match or an explicitly ordered first match.
    pub fn resolve(
        &self,
        selector: &WindowSelector,
        order: WindowOrder,
        policy: WindowSingleMatchPolicy,
    ) -> Result<WindowResolveProjection, WindowQueryError> {
        let mut compiled = self.compile_selector(selector)?;
        let mut matches = self
            .records
            .iter()
            .filter(|record| compiled.matches(record))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Err(WindowQueryError::NoMatch);
        }
        if policy == WindowSingleMatchPolicy::ExactlyOne && matches.len() != 1 {
            return Err(WindowQueryError::Ambiguous {
                matches: u32::try_from(matches.len())
                    .map_err(|_| WindowQueryError::TooManySnapshots)?,
            });
        }
        sort_records(&mut matches, order);
        let selected = matches[0];
        Ok(WindowResolveProjection {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            snapshot_revision: self.snapshot_revision,
            window: selected.snapshot.clone(),
        })
    }

    /// Evaluates current wait state without registering or retaining a waiter.
    pub fn evaluate_wait(
        &self,
        target: &WindowWaitTarget,
        predicate: &WindowWaitPredicate,
    ) -> Result<WindowWaitEvaluation<'a>, WindowQueryError> {
        match target {
            WindowWaitTarget::Reference { window } => {
                self.evaluate_reference_wait(window, predicate)
            }
            WindowWaitTarget::Selector {
                selector,
                quantifier,
            } => self.evaluate_selector_wait(selector, *quantifier, predicate),
        }
    }

    fn compile_selector(
        &self,
        selector: &WindowSelector,
    ) -> Result<CompiledSelector, WindowQueryError> {
        selector
            .validate_for_scope(self.desktop_id, self.desktop_generation)
            .map_err(WindowQueryError::InvalidSelector)?;
        CompiledSelector::compile(selector)
    }

    fn project_page(
        &self,
        records: Vec<&WindowQueryRecord>,
        order: WindowOrder,
        query: WindowContinuationQuery,
        limit: u16,
        continuation: Option<&WindowContinuationDescriptor>,
    ) -> Result<WindowPageProjection, WindowQueryError> {
        if limit == 0 || limit > MAX_WINDOW_PAGE_LIMIT {
            return Err(WindowQueryError::InvalidPageLimit);
        }
        let offset = if let Some(continuation) = continuation {
            if continuation.desktop_id != self.desktop_id
                || continuation.desktop_generation != self.desktop_generation
                || continuation.snapshot_revision != self.snapshot_revision
                || continuation.order != order
                || continuation.query != query
            {
                return Err(WindowQueryError::ContinuationMismatch);
            }
            let offset = usize::try_from(continuation.next_offset)
                .map_err(|_| WindowQueryError::ContinuationOutOfRange)?;
            if offset == 0 || offset >= records.len() {
                return Err(WindowQueryError::ContinuationOutOfRange);
            }
            offset
        } else {
            0
        };
        let end = offset.saturating_add(usize::from(limit)).min(records.len());
        let windows = records[offset..end]
            .iter()
            .map(|record| record.snapshot.clone())
            .collect();
        let continuation = if end < records.len() {
            Some(WindowContinuationDescriptor {
                desktop_id: self.desktop_id,
                desktop_generation: self.desktop_generation,
                snapshot_revision: self.snapshot_revision,
                order,
                query,
                next_offset: u32::try_from(end)
                    .map_err(|_| WindowQueryError::ContinuationOutOfRange)?,
            })
        } else {
            None
        };
        Ok(WindowPageProjection {
            desktop_id: self.desktop_id,
            desktop_generation: self.desktop_generation,
            snapshot_revision: self.snapshot_revision,
            windows,
            continuation,
        })
    }

    fn evaluate_reference_wait(
        &self,
        window: &WindowRef,
        predicate: &WindowWaitPredicate,
    ) -> Result<WindowWaitEvaluation<'a>, WindowQueryError> {
        window
            .validate_shape()
            .map_err(WindowQueryError::InvalidReference)?;
        if window.desktop_id != self.desktop_id
            || window.desktop_generation != self.desktop_generation
        {
            return Err(WindowQueryError::MixedScope);
        }
        if matches!(predicate, WindowWaitPredicate::Count { .. }) {
            return Err(WindowQueryError::IncompatibleWaitTarget);
        }
        if let WindowWaitPredicate::Geometry { predicate } = predicate {
            predicate
                .validate()
                .map_err(WindowQueryError::InvalidSelector)?;
        }
        let selected = self
            .records
            .iter()
            .find(|record| &record.snapshot.window == window);
        let selected_count = u32::from(selected.is_some());
        let (predicate_satisfied, satisfying_windows) = match predicate {
            WindowWaitPredicate::Exists => (
                selected.is_some(),
                selected
                    .into_iter()
                    .map(|record| &record.snapshot)
                    .collect(),
            ),
            WindowWaitPredicate::Closed => (selected.is_none(), Vec::new()),
            _ => {
                let satisfying = selected
                    .filter(|record| snapshot_satisfies(&record.snapshot, predicate))
                    .map(|record| &record.snapshot);
                (satisfying.is_some(), satisfying.into_iter().collect())
            }
        };
        let satisfying_count = u32::try_from(satisfying_windows.len())
            .map_err(|_| WindowQueryError::TooManySnapshots)?;
        Ok(WindowWaitEvaluation {
            predicate_satisfied,
            selected_count,
            satisfying_count,
            satisfying_windows,
        })
    }

    fn evaluate_selector_wait(
        &self,
        selector: &WindowSelector,
        quantifier: WindowWaitSelectorQuantifier,
        predicate: &WindowWaitPredicate,
    ) -> Result<WindowWaitEvaluation<'a>, WindowQueryError> {
        if matches!(
            predicate,
            WindowWaitPredicate::Closed | WindowWaitPredicate::Geometry { .. }
        ) {
            return Err(WindowQueryError::IncompatibleWaitTarget);
        }
        if matches!(
            predicate,
            WindowWaitPredicate::Exists | WindowWaitPredicate::Count { .. }
        ) && quantifier != WindowWaitSelectorQuantifier::Any
        {
            return Err(WindowQueryError::IncompatibleWaitTarget);
        }
        let mut compiled = self.compile_selector(selector)?;
        let mut selected = self
            .records
            .iter()
            .filter(|record| compiled.matches(record))
            .collect::<Vec<_>>();
        selected.sort_by(|left, right| {
            compare_exact_ref(&left.snapshot.window, &right.snapshot.window)
        });
        let selected_count =
            u32::try_from(selected.len()).map_err(|_| WindowQueryError::TooManySnapshots)?;

        if let WindowWaitPredicate::Count { predicate } = predicate {
            predicate
                .validate()
                .map_err(WindowQueryError::InvalidSelector)?;
            let predicate_satisfied = match predicate.comparison {
                WindowCountComparison::Exactly => selected_count == predicate.count,
                WindowCountComparison::AtLeast => selected_count >= predicate.count,
                WindowCountComparison::AtMost => selected_count <= predicate.count,
            };
            return Ok(WindowWaitEvaluation {
                predicate_satisfied,
                selected_count,
                satisfying_count: selected_count,
                satisfying_windows: selected.iter().map(|record| &record.snapshot).collect(),
            });
        }

        let satisfying_windows = if matches!(predicate, WindowWaitPredicate::Exists) {
            selected
                .iter()
                .map(|record| &record.snapshot)
                .collect::<Vec<_>>()
        } else {
            selected
                .iter()
                .filter(|record| snapshot_satisfies(&record.snapshot, predicate))
                .map(|record| &record.snapshot)
                .collect::<Vec<_>>()
        };
        let satisfying_count = u32::try_from(satisfying_windows.len())
            .map_err(|_| WindowQueryError::TooManySnapshots)?;
        let predicate_satisfied = match predicate {
            WindowWaitPredicate::Exists => selected_count > 0,
            _ => match quantifier {
                WindowWaitSelectorQuantifier::Any => satisfying_count > 0,
                WindowWaitSelectorQuantifier::All => {
                    selected_count > 0 && satisfying_count == selected_count
                }
                WindowWaitSelectorQuantifier::ExactlyOne => {
                    selected_count == 1 && satisfying_count == 1
                }
            },
        };
        Ok(WindowWaitEvaluation {
            predicate_satisfied,
            selected_count,
            satisfying_count,
            satisfying_windows,
        })
    }
}

enum CompiledSelector {
    Predicate(Box<CompiledPredicate>),
    All(Vec<Self>),
    Any(Vec<Self>),
    Not(Box<Self>),
}

impl CompiledSelector {
    fn compile(selector: &WindowSelector) -> Result<Self, WindowQueryError> {
        match selector {
            WindowSelector::Predicate { predicate } => Ok(Self::Predicate(Box::new(
                CompiledPredicate::compile(predicate)?,
            ))),
            WindowSelector::All { selectors } => selectors
                .iter()
                .map(Self::compile)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::All),
            WindowSelector::Any { selectors } => selectors
                .iter()
                .map(Self::compile)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::Any),
            WindowSelector::Not { selector } => {
                Self::compile(selector).map(Box::new).map(Self::Not)
            }
        }
    }

    fn matches(&mut self, record: &WindowQueryRecord) -> bool {
        match self {
            Self::Predicate(predicate) => predicate.matches(record),
            Self::All(selectors) => selectors
                .iter_mut()
                .all(|selector| selector.matches(record)),
            Self::Any(selectors) => selectors
                .iter_mut()
                .any(|selector| selector.matches(record)),
            Self::Not(selector) => !selector.matches(record),
        }
    }
}

enum CompiledPredicate {
    Text {
        field: WindowTextField,
        matcher: CompiledStringMatch,
    },
    ManagedProcess(xenoteer_protocol::ProcessRef),
    ReportedPid(u32),
    WindowType(xenoteer_protocol::WindowAtomName),
    State {
        value: xenoteer_protocol::WindowAtomName,
        present: bool,
    },
    MapState(WindowMapState),
    Workspace(u32),
    Active(bool),
    Focused(bool),
    TransientFor(WindowRef),
    GroupLeader(WindowRef),
    HasAccessibilityApplication(bool),
    CreatedAfter(WindowModelRevision),
}

impl CompiledPredicate {
    fn compile(predicate: &WindowPredicate) -> Result<Self, WindowQueryError> {
        Ok(match predicate {
            WindowPredicate::Text { field, matcher } => Self::Text {
                field: *field,
                matcher: CompiledStringMatch::compile(matcher)?,
            },
            WindowPredicate::ManagedProcess { process } => Self::ManagedProcess(*process),
            WindowPredicate::ReportedPid { pid } => Self::ReportedPid(*pid),
            WindowPredicate::WindowType { value } => Self::WindowType(value.clone()),
            WindowPredicate::State { value, present } => Self::State {
                value: value.clone(),
                present: *present,
            },
            WindowPredicate::MapState { value } => Self::MapState(*value),
            WindowPredicate::Workspace { workspace } => Self::Workspace(*workspace),
            WindowPredicate::Active { value } => Self::Active(*value),
            WindowPredicate::Focused { value } => Self::Focused(*value),
            WindowPredicate::TransientFor { window } => Self::TransientFor(window.clone()),
            WindowPredicate::GroupLeader { window } => Self::GroupLeader(window.clone()),
            WindowPredicate::HasAccessibilityApplication { value } => {
                Self::HasAccessibilityApplication(*value)
            }
            WindowPredicate::CreatedAfter { model_revision } => Self::CreatedAfter(*model_revision),
        })
    }

    fn matches(&mut self, record: &WindowQueryRecord) -> bool {
        let snapshot = &record.snapshot;
        match self {
            Self::Text { field, matcher } => {
                snapshot_text(snapshot, *field).is_some_and(|text| matcher.matches(text))
            }
            Self::ManagedProcess(process) => snapshot.process.managed_process == Some(*process),
            Self::ReportedPid(pid) => snapshot.process.reported_pid == Some(*pid),
            Self::WindowType(value) => snapshot.metadata.window_types.contains(value),
            Self::State { value, present } => snapshot.metadata.states.contains(value) == *present,
            Self::MapState(value) => snapshot.state.map_state == *value,
            Self::Workspace(workspace) => snapshot.workspace == Some(*workspace),
            Self::Active(value) => snapshot.state.active == *value,
            Self::Focused(value) => snapshot.state.focused == *value,
            Self::TransientFor(window) => snapshot.transient_for.as_ref() == Some(window),
            Self::GroupLeader(window) => snapshot.group_leader.as_ref() == Some(window),
            Self::HasAccessibilityApplication(value) => {
                snapshot.has_accessibility_application == *value
            }
            Self::CreatedAfter(revision) => record.created_revision > *revision,
        }
    }
}

enum CompiledStringMatch {
    Exact {
        value: String,
        case_sensitive: bool,
    },
    Contains {
        value: String,
        case_sensitive: bool,
    },
    Prefix {
        value: String,
        case_sensitive: bool,
    },
    Suffix {
        value: String,
        case_sensitive: bool,
    },
    Regex {
        regex: PikeVM,
        cache: Box<PikeCache>,
    },
}

impl CompiledStringMatch {
    fn compile(matcher: &WindowStringMatch) -> Result<Self, WindowQueryError> {
        Ok(match matcher {
            WindowStringMatch::Exact {
                value,
                case_sensitive,
            } => Self::Exact {
                value: fold_if_needed(value, *case_sensitive),
                case_sensitive: *case_sensitive,
            },
            WindowStringMatch::Contains {
                value,
                case_sensitive,
            } => Self::Contains {
                value: fold_if_needed(value, *case_sensitive),
                case_sensitive: *case_sensitive,
            },
            WindowStringMatch::Prefix {
                value,
                case_sensitive,
            } => Self::Prefix {
                value: fold_if_needed(value, *case_sensitive),
                case_sensitive: *case_sensitive,
            },
            WindowStringMatch::Suffix {
                value,
                case_sensitive,
            } => Self::Suffix {
                value: fold_if_needed(value, *case_sensitive),
                case_sensitive: *case_sensitive,
            },
            WindowStringMatch::Regex {
                pattern,
                case_sensitive,
            } => {
                let mut compiler = NFA::compiler();
                compiler
                    .configure(NFA::config().nfa_size_limit(Some(MAX_WINDOW_REGEX_NFA_BYTES)))
                    .syntax(
                        syntax::Config::new()
                            .utf8(true)
                            .unicode(true)
                            .case_insensitive(!case_sensitive),
                    );
                let nfa = compiler
                    .build(pattern)
                    .map_err(|_| WindowQueryError::RegexBuild)?;
                let regex = PikeVM::new_from_nfa(nfa).map_err(|_| WindowQueryError::RegexBuild)?;
                let cache = Box::new(regex.create_cache());
                Self::Regex { regex, cache }
            }
        })
    }

    fn matches(&mut self, candidate: &str) -> bool {
        match self {
            Self::Exact {
                value,
                case_sensitive,
            } => fold_if_needed(candidate, *case_sensitive) == *value,
            Self::Contains {
                value,
                case_sensitive,
            } => fold_if_needed(candidate, *case_sensitive).contains(value.as_str()),
            Self::Prefix {
                value,
                case_sensitive,
            } => fold_if_needed(candidate, *case_sensitive).starts_with(value.as_str()),
            Self::Suffix {
                value,
                case_sensitive,
            } => fold_if_needed(candidate, *case_sensitive).ends_with(value.as_str()),
            Self::Regex { regex, cache } => regex.is_match(cache, candidate),
        }
    }
}

fn fold_if_needed(value: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        value.to_owned()
    } else {
        value.chars().flat_map(char::to_lowercase).collect()
    }
}

fn snapshot_text(snapshot: &WindowSnapshot, field: WindowTextField) -> Option<&str> {
    match field {
        WindowTextField::Title => snapshot.metadata.title.as_ref(),
        WindowTextField::VisibleTitle => snapshot.metadata.visible_title.as_ref(),
        WindowTextField::IconTitle => snapshot.metadata.icon_title.as_ref(),
        WindowTextField::ClassInstance => snapshot
            .metadata
            .class
            .as_ref()
            .and_then(|class| class.instance.as_ref()),
        WindowTextField::Class => snapshot
            .metadata
            .class
            .as_ref()
            .and_then(|class| class.class.as_ref()),
        WindowTextField::ClientMachine => snapshot.metadata.client_machine.as_ref(),
    }
    .map(|text| text.value.as_str())
}

fn selector_fingerprint(
    selector: &WindowSelector,
) -> Result<WindowSelectorFingerprint, WindowQueryError> {
    let encoded = serde_json::to_vec(selector).map_err(|_| WindowQueryError::Fingerprint)?;
    let mut digest = Sha256::new();
    digest.update(SELECTOR_FINGERPRINT_DOMAIN);
    digest.update(encoded);
    Ok(WindowSelectorFingerprint(digest.finalize().into()))
}

fn sort_records(records: &mut [&WindowQueryRecord], order: WindowOrder) {
    records.sort_by(|left, right| compare_records(left, right, order));
}

fn compare_records(
    left: &WindowQueryRecord,
    right: &WindowQueryRecord,
    order: WindowOrder,
) -> Ordering {
    let primary = match order {
        WindowOrder::CreationAscending => left.created_revision.cmp(&right.created_revision),
        WindowOrder::CreationDescending => right.created_revision.cmp(&left.created_revision),
        WindowOrder::StackingBottomToTop => compare_optional_missing_last(
            left.snapshot.stacking_index,
            right.snapshot.stacking_index,
            false,
        ),
        WindowOrder::StackingTopToBottom => compare_optional_missing_last(
            left.snapshot.stacking_index,
            right.snapshot.stacking_index,
            true,
        ),
        WindowOrder::TitleAscending => compare_optional_missing_last(
            left.snapshot
                .metadata
                .title
                .as_ref()
                .map(|text| text.value.as_str()),
            right
                .snapshot
                .metadata
                .title
                .as_ref()
                .map(|text| text.value.as_str()),
            false,
        ),
        WindowOrder::TitleDescending => compare_optional_missing_last(
            left.snapshot
                .metadata
                .title
                .as_ref()
                .map(|text| text.value.as_str()),
            right
                .snapshot
                .metadata
                .title
                .as_ref()
                .map(|text| text.value.as_str()),
            true,
        ),
        WindowOrder::XidAscending => left.snapshot.window.xid.cmp(&right.snapshot.window.xid),
    };
    primary.then_with(|| compare_exact_ref(&left.snapshot.window, &right.snapshot.window))
}

fn compare_optional_missing_last<T: Ord>(
    left: Option<T>,
    right: Option<T>,
    reverse: bool,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) if reverse => right.cmp(&left),
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_exact_ref(left: &WindowRef, right: &WindowRef) -> Ordering {
    left.xid
        .cmp(&right.xid)
        .then_with(|| left.observed_generation.cmp(&right.observed_generation))
        .then_with(|| {
            left.identity_hash
                .as_str()
                .cmp(right.identity_hash.as_str())
        })
}

fn snapshot_satisfies(snapshot: &WindowSnapshot, predicate: &WindowWaitPredicate) -> bool {
    match predicate {
        WindowWaitPredicate::Exists => true,
        WindowWaitPredicate::Closed | WindowWaitPredicate::Count { .. } => false,
        WindowWaitPredicate::MapState { state } => snapshot.state.map_state == *state,
        WindowWaitPredicate::Active { desired } => snapshot.state.active == *desired,
        WindowWaitPredicate::Focused { desired } => snapshot.state.focused == *desired,
        WindowWaitPredicate::Geometry { predicate } => snapshot
            .geometry
            .as_ref()
            .and_then(|geometry| match predicate {
                WindowGeometryPredicate::Equals { area, expected } => {
                    geometry_area(geometry, *area).map(|actual| actual == *expected)
                }
                WindowGeometryPredicate::Intersects { area, region } => {
                    geometry_area(geometry, *area)
                        .map(|actual| rectangles_intersect(actual, *region))
                }
                WindowGeometryPredicate::ContainedBy { area, bounds } => {
                    geometry_area(geometry, *area).map(|actual| rectangle_contains(*bounds, actual))
                }
                WindowGeometryPredicate::ContainsPoint { area, point } => {
                    geometry_area(geometry, *area)
                        .map(|actual| rectangle_contains_point(actual, *point))
                }
            })
            .unwrap_or(false),
    }
}

fn geometry_area(
    geometry: &xenoteer_protocol::WindowGeometry,
    area: WindowGeometryArea,
) -> Option<WindowRect> {
    match area {
        WindowGeometryArea::Client => Some(geometry.client_rect),
        WindowGeometryArea::Frame => geometry.frame_rect,
        WindowGeometryArea::Content => Some(geometry.content_rect),
    }
}

fn rectangle_edges(rectangle: WindowRect) -> (i64, i64, i64, i64) {
    let origin = rectangle.rect.origin();
    let left = i64::from(origin.x());
    let top = i64::from(origin.y());
    let Ok(size) = rectangle.rect.size() else {
        return (left, top, left, top);
    };
    (
        left,
        top,
        left + i64::from(size.width()),
        top + i64::from(size.height()),
    )
}

fn rectangles_intersect(left: WindowRect, right: WindowRect) -> bool {
    let (left_x1, left_y1, left_x2, left_y2) = rectangle_edges(left);
    let (right_x1, right_y1, right_x2, right_y2) = rectangle_edges(right);
    left_x1 < right_x2 && right_x1 < left_x2 && left_y1 < right_y2 && right_y1 < left_y2
}

fn rectangle_contains(container: WindowRect, candidate: WindowRect) -> bool {
    let (outer_x1, outer_y1, outer_x2, outer_y2) = rectangle_edges(container);
    let (inner_x1, inner_y1, inner_x2, inner_y2) = rectangle_edges(candidate);
    outer_x1 <= inner_x1 && outer_y1 <= inner_y1 && outer_x2 >= inner_x2 && outer_y2 >= inner_y2
}

fn rectangle_contains_point(rectangle: WindowRect, point: xenoteer_protocol::Point) -> bool {
    let (x1, y1, x2, y2) = rectangle_edges(rectangle);
    let x = i64::from(point.x());
    let y = i64::from(point.y());
    x >= x1 && x < x2 && y >= y1 && y < y2
}

/// Pure query/evaluation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WindowQueryError {
    /// Scope identifiers must be non-nil.
    #[error("window query scope contains a nil identifier")]
    NilIdentifier,
    /// Input cardinality exceeded its configured protocol ceiling.
    #[error("window query snapshot exceeds the bounded model cardinality")]
    TooManySnapshots,
    /// One snapshot failed protocol validation.
    #[error("window query snapshot failed protocol validation")]
    InvalidSnapshot(WindowValidationError),
    /// One exact reference failed protocol shape validation.
    #[error("window query reference failed protocol shape validation")]
    InvalidReference(WindowValidationError),
    /// The selector failed protocol admission validation.
    #[error("window query selector failed protocol validation")]
    InvalidSelector(WindowQueryValidationError),
    /// First-observed evidence was absent and cannot safely be inferred.
    #[error("window creation revision evidence is required and cannot be inferred")]
    CreationRevisionUnavailable,
    /// First-observed evidence was newer than the containing snapshot.
    #[error("window creation revision is newer than its snapshot")]
    CreationRevisionAfterSnapshot,
    /// Input snapshots did not belong to one desktop lifetime.
    #[error("window query snapshots span desktop lifetimes")]
    MixedScope,
    /// Input snapshots did not share one actor-local revision.
    #[error("window query snapshots do not share one model revision")]
    MixedRevision,
    /// The immutable view contained a repeated exact birth or XID.
    #[error("window query snapshots contain a duplicate exact birth or live XID")]
    DuplicateSnapshot,
    /// A regex failed the shared syntax or compiled-size policy.
    #[error("window regex could not be built under the bounded linear-time policy")]
    RegexBuild,
    /// Canonical serialization failed while deriving a query fingerprint.
    #[error("window selector fingerprint could not be computed")]
    Fingerprint,
    /// A requested page size was zero or above the protocol ceiling.
    #[error("window page limit is outside the protocol range")]
    InvalidPageLimit,
    /// Signed continuation content did not bind to this query/view.
    #[error("window continuation does not match the current scope, revision, query, or order")]
    ContinuationMismatch,
    /// A continuation offset was zero, terminal, or outside the result set.
    #[error("window continuation offset cannot make forward progress")]
    ContinuationOutOfRange,
    /// Resolution found no selected window.
    #[error("window selector did not match any window")]
    NoMatch,
    /// Exactly-one resolution found more than one window.
    #[error("window selector is ambiguous across {matches} windows")]
    Ambiguous {
        /// Total number of selector matches.
        matches: u32,
    },
    /// The wait target kind cannot support the requested predicate.
    #[error("window wait target and predicate are incompatible")]
    IncompatibleWaitTarget,
}

#[cfg(test)]
mod tests {
    use xenoteer_protocol::{
        CoordinateSpace, LaunchId, Point, ProcessRef, Rect, WindowAtomName, WindowClass,
        WindowCountPredicate, WindowFrameExtents, WindowGeometry, WindowIdentityHash,
        WindowMetadata, WindowObservedState, WindowProcessConfidence, WindowProcessCorrelation,
        WindowProcessEvidence, WindowSnapshotWarning, WindowText,
    };

    use super::*;

    struct Fixture {
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        revision: WindowModelRevision,
    }

    impl Fixture {
        fn new(revision: u64) -> Result<Self, WindowValidationError> {
            Ok(Self {
                desktop_id: DesktopId::new(),
                generation: DesktopGeneration::new(),
                revision: WindowModelRevision::new(revision)?,
            })
        }

        fn record(
            &self,
            xid: u32,
            created_revision: u64,
            title: Option<&str>,
            stacking_index: Option<u32>,
        ) -> Result<WindowQueryRecord, Box<dyn std::error::Error>> {
            let window = WindowRef {
                desktop_id: self.desktop_id,
                desktop_generation: self.generation,
                xid,
                observed_generation: 1,
                identity_hash: WindowIdentityHash::new("a".repeat(64))?,
            };
            let snapshot = WindowSnapshot {
                xid_hex: window.xid_hex(),
                window,
                model_revision: self.revision,
                metadata: WindowMetadata {
                    title: title
                        .map(|value| WindowText::new(value, false))
                        .transpose()?,
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
                stacking_index,
                has_accessibility_application: false,
                warnings: Vec::<WindowSnapshotWarning>::new(),
            };
            Ok(WindowQueryRecord::new(
                snapshot,
                WindowModelRevision::new(created_revision)?,
            )?)
        }

        fn view<'a>(
            &self,
            records: &'a [WindowQueryRecord],
        ) -> Result<WindowQueryView<'a>, WindowQueryError> {
            WindowQueryView::new(self.desktop_id, self.generation, self.revision, records)
        }
    }

    fn leaf(predicate: WindowPredicate) -> WindowSelector {
        WindowSelector::Predicate { predicate }
    }

    fn xids(page: &WindowPageProjection) -> Vec<u32> {
        page.windows
            .iter()
            .map(|window| window.window.xid)
            .collect()
    }

    #[test]
    fn view_rejects_mixed_revision_scope_and_duplicate_xid()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(10)?;
        let first = fixture.record(1, 1, None, None)?;

        let mut mixed_revision = fixture.record(2, 2, None, None)?;
        mixed_revision.snapshot.model_revision = WindowModelRevision::new(9)?;
        assert_eq!(
            fixture.view(&[first.clone(), mixed_revision]),
            Err(WindowQueryError::MixedRevision)
        );

        let mut mixed_scope = fixture.record(2, 2, None, None)?;
        mixed_scope.snapshot.window.desktop_generation = DesktopGeneration::new();
        assert_eq!(
            fixture.view(&[first.clone(), mixed_scope]),
            Err(WindowQueryError::MixedScope)
        );

        let duplicate = WindowQueryRecord {
            snapshot: WindowSnapshot {
                window: WindowRef {
                    observed_generation: 2,
                    ..first.snapshot.window.clone()
                },
                ..first.snapshot.clone()
            },
            created_revision: WindowModelRevision::new(2)?,
        };
        assert_eq!(
            fixture.view(&[first, duplicate]),
            Err(WindowQueryError::DuplicateSnapshot)
        );
        Ok(())
    }

    #[test]
    fn creation_revision_is_explicit_and_never_inferred() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new(4)?;
        let record = fixture.record(1, 1, None, None)?;
        assert_eq!(
            WindowQueryRecord::with_optional_creation_revision(record.snapshot.clone(), None),
            Err(WindowQueryError::CreationRevisionUnavailable)
        );
        assert_eq!(
            WindowQueryRecord::new(record.snapshot, WindowModelRevision::new(5)?),
            Err(WindowQueryError::CreationRevisionAfterSnapshot)
        );
        Ok(())
    }

    #[test]
    fn every_selector_leaf_uses_snapshot_or_creation_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(8)?;
        let mut parent = fixture.record(1, 1, Some("parent"), Some(0))?;
        parent.snapshot.state.map_state = WindowMapState::Unmapped;
        let parent_ref = parent.snapshot.window.clone();

        let mut target = fixture.record(2, 4, Some("Secret Editor"), Some(1))?;
        target.snapshot.metadata.visible_title = Some(WindowText::new("Visible Editor", false)?);
        target.snapshot.metadata.icon_title = Some(WindowText::new("Editor Icon", false)?);
        target.snapshot.metadata.class = Some(WindowClass {
            instance: Some(WindowText::new("editor-instance", false)?),
            class: Some(WindowText::new("EditorClass", false)?),
        });
        target.snapshot.metadata.client_machine = Some(WindowText::new("desktop-host", false)?);
        target.snapshot.metadata.window_types =
            vec![WindowAtomName::new("_NET_WM_WINDOW_TYPE_NORMAL")?];
        target.snapshot.metadata.states = vec![WindowAtomName::new("_NET_WM_STATE_ABOVE")?];
        target.snapshot.workspace = Some(2);
        target.snapshot.state.active = true;
        target.snapshot.state.focused = true;
        target.snapshot.transient_for = Some(parent_ref.clone());
        target.snapshot.group_leader = Some(parent_ref.clone());
        target.snapshot.has_accessibility_application = true;
        let process = ProcessRef {
            desktop_generation: fixture.generation,
            pid: 77,
            proc_start_ticks: 9,
            launch_id: LaunchId::new(),
        };
        target.snapshot.process = WindowProcessCorrelation {
            reported_pid: Some(77),
            managed_process: Some(process),
            confidence: WindowProcessConfidence::High,
            evidence: vec![
                WindowProcessEvidence::ProcStartTime,
                WindowProcessEvidence::WmClass,
            ],
            conflict: false,
        };

        let records = vec![parent, target];
        let view = fixture.view(&records)?;
        let predicates = vec![
            WindowPredicate::Text {
                field: WindowTextField::Title,
                matcher: WindowStringMatch::Exact {
                    value: "secret editor".to_owned(),
                    case_sensitive: false,
                },
            },
            WindowPredicate::Text {
                field: WindowTextField::VisibleTitle,
                matcher: WindowStringMatch::Contains {
                    value: "Editor".to_owned(),
                    case_sensitive: true,
                },
            },
            WindowPredicate::Text {
                field: WindowTextField::IconTitle,
                matcher: WindowStringMatch::Suffix {
                    value: "Icon".to_owned(),
                    case_sensitive: true,
                },
            },
            WindowPredicate::Text {
                field: WindowTextField::ClassInstance,
                matcher: WindowStringMatch::Prefix {
                    value: "editor-".to_owned(),
                    case_sensitive: true,
                },
            },
            WindowPredicate::Text {
                field: WindowTextField::Class,
                matcher: WindowStringMatch::Exact {
                    value: "EditorClass".to_owned(),
                    case_sensitive: true,
                },
            },
            WindowPredicate::Text {
                field: WindowTextField::ClientMachine,
                matcher: WindowStringMatch::Exact {
                    value: "desktop-host".to_owned(),
                    case_sensitive: true,
                },
            },
            WindowPredicate::ManagedProcess { process },
            WindowPredicate::ReportedPid { pid: 77 },
            WindowPredicate::WindowType {
                value: WindowAtomName::new("_NET_WM_WINDOW_TYPE_NORMAL")?,
            },
            WindowPredicate::State {
                value: WindowAtomName::new("_NET_WM_STATE_ABOVE")?,
                present: true,
            },
            WindowPredicate::MapState {
                value: WindowMapState::Viewable,
            },
            WindowPredicate::Workspace { workspace: 2 },
            WindowPredicate::Active { value: true },
            WindowPredicate::Focused { value: true },
            WindowPredicate::TransientFor {
                window: parent_ref.clone(),
            },
            WindowPredicate::GroupLeader { window: parent_ref },
            WindowPredicate::HasAccessibilityApplication { value: true },
            WindowPredicate::CreatedAfter {
                model_revision: WindowModelRevision::new(3)?,
            },
        ];
        for predicate in predicates {
            let page = view.query(&leaf(predicate), WindowOrder::XidAscending, 10, None)?;
            assert_eq!(xids(&page), vec![2]);
        }
        Ok(())
    }

    #[test]
    fn regex_is_bounded_unicode_aware_and_composed_selectors_work()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(5)?;
        let records = vec![fixture.record(1, 1, Some("Δelta Editor"), None)?];
        let view = fixture.view(&records)?;
        let regex = leaf(WindowPredicate::Text {
            field: WindowTextField::Title,
            matcher: WindowStringMatch::Regex {
                pattern: "^δelta\\s+editor$".to_owned(),
                case_sensitive: false,
            },
        });
        let composed = WindowSelector::All {
            selectors: vec![
                regex,
                WindowSelector::Not {
                    selector: Box::new(leaf(WindowPredicate::Active { value: true })),
                },
            ],
        };
        assert_eq!(
            xids(&view.query(&composed, WindowOrder::XidAscending, 10, None)?),
            vec![1]
        );
        Ok(())
    }

    #[test]
    fn every_order_is_total_with_missing_values_last() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(10)?;
        let records = vec![
            fixture.record(4, 3, None, None)?,
            fixture.record(3, 2, Some("beta"), Some(2))?,
            fixture.record(2, 2, Some("alpha"), Some(1))?,
            fixture.record(1, 1, Some("alpha"), Some(1))?,
        ];
        let view = fixture.view(&records)?;
        let expected = [
            (WindowOrder::CreationAscending, vec![1, 2, 3, 4]),
            (WindowOrder::CreationDescending, vec![4, 2, 3, 1]),
            (WindowOrder::StackingBottomToTop, vec![1, 2, 3, 4]),
            (WindowOrder::StackingTopToBottom, vec![3, 1, 2, 4]),
            (WindowOrder::TitleAscending, vec![1, 2, 3, 4]),
            (WindowOrder::TitleDescending, vec![3, 1, 2, 4]),
            (WindowOrder::XidAscending, vec![1, 2, 3, 4]),
        ];
        for (order, expected_xids) in expected {
            assert_eq!(xids(&view.list(order, 10, None)?), expected_xids);
        }
        Ok(())
    }

    #[test]
    fn continuation_is_typed_scoped_and_forward_only() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(5)?;
        let records = (1..=5)
            .map(|xid| fixture.record(xid, u64::from(xid), None, None))
            .collect::<Result<Vec<_>, _>>()?;
        let view = fixture.view(&records)?;
        let first = view.list(WindowOrder::XidAscending, 2, None)?;
        assert_eq!(xids(&first), vec![1, 2]);
        let continuation = first
            .continuation
            .as_ref()
            .ok_or(WindowQueryError::NoMatch)?;
        assert_eq!(continuation.next_offset, 2);
        let second = view.list(WindowOrder::XidAscending, 2, Some(continuation))?;
        assert_eq!(xids(&second), vec![3, 4]);

        let mut tampered = continuation.clone();
        tampered.order = WindowOrder::TitleAscending;
        assert_eq!(
            view.list(WindowOrder::XidAscending, 2, Some(&tampered)),
            Err(WindowQueryError::ContinuationMismatch)
        );
        Ok(())
    }

    #[test]
    fn resolve_never_silently_chooses_an_ambiguous_target() -> Result<(), Box<dyn std::error::Error>>
    {
        let fixture = Fixture::new(4)?;
        let mut first = fixture.record(2, 1, None, None)?;
        first.snapshot.state.active = true;
        let mut second = fixture.record(1, 2, None, None)?;
        second.snapshot.state.active = true;
        let records = vec![first, second];
        let view = fixture.view(&records)?;
        let selector = leaf(WindowPredicate::Active { value: true });
        assert_eq!(
            view.resolve(
                &selector,
                WindowOrder::XidAscending,
                WindowSingleMatchPolicy::ExactlyOne,
            ),
            Err(WindowQueryError::Ambiguous { matches: 2 })
        );
        assert_eq!(
            view.resolve(
                &selector,
                WindowOrder::XidAscending,
                WindowSingleMatchPolicy::First,
            )?
            .window
            .window
            .xid,
            1
        );
        assert_eq!(
            view.resolve(
                &leaf(WindowPredicate::ReportedPid { pid: 999 }),
                WindowOrder::XidAscending,
                WindowSingleMatchPolicy::ExactlyOne,
            ),
            Err(WindowQueryError::NoMatch)
        );
        Ok(())
    }

    #[test]
    fn selector_wait_quantifiers_are_non_vacuous_on_empty_sets()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(3)?;
        let records = vec![fixture.record(1, 1, None, None)?];
        let view = fixture.view(&records)?;
        let no_matches = leaf(WindowPredicate::ReportedPid { pid: 999 });
        for quantifier in [
            WindowWaitSelectorQuantifier::Any,
            WindowWaitSelectorQuantifier::All,
            WindowWaitSelectorQuantifier::ExactlyOne,
        ] {
            let evaluated = view.evaluate_wait(
                &WindowWaitTarget::Selector {
                    selector: no_matches.clone(),
                    quantifier,
                },
                &WindowWaitPredicate::Focused { desired: true },
            )?;
            assert!(!evaluated.predicate_satisfied);
            assert_eq!(evaluated.selected_count, 0);
        }
        let exists = view.evaluate_wait(
            &WindowWaitTarget::Selector {
                selector: no_matches.clone(),
                quantifier: WindowWaitSelectorQuantifier::Any,
            },
            &WindowWaitPredicate::Exists,
        )?;
        assert!(!exists.predicate_satisfied);
        let count = view.evaluate_wait(
            &WindowWaitTarget::Selector {
                selector: no_matches,
                quantifier: WindowWaitSelectorQuantifier::Any,
            },
            &WindowWaitPredicate::Count {
                predicate: WindowCountPredicate {
                    comparison: WindowCountComparison::Exactly,
                    count: 0,
                },
            },
        )?;
        assert!(count.predicate_satisfied);
        Ok(())
    }

    #[test]
    fn reference_waits_cover_closed_and_geometry_without_lifecycle_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(3)?;
        let mut record = fixture.record(1, 1, None, None)?;
        let rectangle =
            WindowRect::new(CoordinateSpace::RootPhysical, Rect::new(10, 20, 100, 80)?)?;
        record.snapshot.geometry = Some(WindowGeometry {
            client_rect: rectangle,
            frame_rect: Some(rectangle),
            content_rect: rectangle,
            frame_extents: Some(WindowFrameExtents {
                left: 0,
                right: 0,
                top: 0,
                bottom: 0,
            }),
        });
        let reference = record.snapshot.window.clone();
        let records = vec![record];
        let view = fixture.view(&records)?;
        let geometry = view.evaluate_wait(
            &WindowWaitTarget::Reference {
                window: reference.clone(),
            },
            &WindowWaitPredicate::Geometry {
                predicate: WindowGeometryPredicate::ContainsPoint {
                    area: WindowGeometryArea::Client,
                    point: Point::new(109, 99),
                },
            },
        )?;
        assert!(geometry.predicate_satisfied);

        let missing = WindowRef {
            xid: 2,
            ..reference
        };
        let closed = view.evaluate_wait(
            &WindowWaitTarget::Reference { window: missing },
            &WindowWaitPredicate::Closed,
        )?;
        assert!(closed.predicate_satisfied);
        assert_eq!(closed.selected_count, 0);
        Ok(())
    }

    #[test]
    fn invalid_empty_selector_compositions_are_rejected_before_evaluation()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new(2)?;
        let records = vec![fixture.record(1, 1, None, None)?];
        let view = fixture.view(&records)?;
        for selector in [
            WindowSelector::All {
                selectors: Vec::new(),
            },
            WindowSelector::Any {
                selectors: Vec::new(),
            },
        ] {
            assert_eq!(
                view.query(&selector, WindowOrder::XidAscending, 10, None),
                Err(WindowQueryError::InvalidSelector(
                    WindowQueryValidationError::EmptyComposition
                ))
            );
        }
        Ok(())
    }
}
