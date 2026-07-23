//! Pure, bounded correlation between AT-SPI identities and live X11 windows.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use thiserror::Error;
use xenoteer_protocol::{
    ApplicationRef, ElementRef, ElementWindowCorrelation, Rect, WindowCorrelationConfidence,
    WindowCorrelationEvidence, WindowCorrelationSignal, WindowRef,
};

use crate::MonotonicMillis;

/// Hard ceiling for live and recently-dead candidates considered in one pass.
pub const MAX_ACCESSIBILITY_CORRELATION_CANDIDATES: usize = 128;
/// Hard ceiling for one raw string presented for normalization.
pub const MAX_ACCESSIBILITY_CORRELATION_RAW_STRING_BYTES: usize = 1_024;
/// Hard ceiling for one normalized title or identity.
pub const MAX_ACCESSIBILITY_CORRELATION_STRING_BYTES: usize = 256;
/// Hard ceiling for normalized string bytes across one correlation pass.
pub const MAX_ACCESSIBILITY_CORRELATION_TOTAL_STRING_BYTES: usize = 32 * 1_024;
/// Hard ceiling for the age of an observation admitted as live evidence.
pub const MAX_ACCESSIBILITY_CORRELATION_OBSERVATION_AGE_MS: u64 = 60_000;
/// Hard ceiling for focus-transition proximity.
pub const MAX_ACCESSIBILITY_CORRELATION_FOCUS_DELTA_MS: u64 = 10_000;
/// Hard ceiling for creation-time proximity.
pub const MAX_ACCESSIBILITY_CORRELATION_CREATION_DELTA_MS: u64 = 30_000;

/// Caller-selectable limits beneath compile-time correlation ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessibilityCorrelationLimits {
    /// Maximum number of candidates accepted in one call.
    pub max_candidates: usize,
    /// Maximum aggregate normalized string bytes accepted in one call.
    pub max_total_string_bytes: usize,
    /// Maximum age of subject and live-candidate observations.
    pub max_observation_age_ms: u64,
    /// Maximum separation between matching focus transitions.
    pub max_focus_delta_ms: u64,
    /// Maximum separation between matching creation observations.
    pub max_creation_delta_ms: u64,
}

impl Default for AccessibilityCorrelationLimits {
    fn default() -> Self {
        Self {
            max_candidates: 64,
            max_total_string_bytes: 16 * 1_024,
            max_observation_age_ms: 5_000,
            max_focus_delta_ms: 750,
            max_creation_delta_ms: 2_000,
        }
    }
}

impl AccessibilityCorrelationLimits {
    /// Validates caller limits against hard ceilings.
    pub fn validate(self) -> Result<(), AccessibilityCorrelationError> {
        if self.max_candidates == 0
            || self.max_candidates > MAX_ACCESSIBILITY_CORRELATION_CANDIDATES
            || self.max_total_string_bytes == 0
            || self.max_total_string_bytes > MAX_ACCESSIBILITY_CORRELATION_TOTAL_STRING_BYTES
            || self.max_observation_age_ms > MAX_ACCESSIBILITY_CORRELATION_OBSERVATION_AGE_MS
            || self.max_focus_delta_ms > MAX_ACCESSIBILITY_CORRELATION_FOCUS_DELTA_MS
            || self.max_creation_delta_ms > MAX_ACCESSIBILITY_CORRELATION_CREATION_DELTA_MS
        {
            return Err(AccessibilityCorrelationError::InvalidLimits);
        }
        Ok(())
    }
}

/// A bounded, canonical, case-folded correlation string.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NormalizedCorrelationText(String);

impl NormalizedCorrelationText {
    /// Normalizes Unicode case and whitespace while rejecting control data.
    pub fn new(value: impl AsRef<str>) -> Result<Self, AccessibilityCorrelationError> {
        let value = value.as_ref();
        if value.len() > MAX_ACCESSIBILITY_CORRELATION_RAW_STRING_BYTES {
            return Err(AccessibilityCorrelationError::StringLimit);
        }
        let mut normalized =
            String::with_capacity(value.len().min(MAX_ACCESSIBILITY_CORRELATION_STRING_BYTES));
        let mut pending_space = false;
        for character in value.chars() {
            if character.is_control() && !character.is_whitespace() {
                return Err(AccessibilityCorrelationError::InvalidString);
            }
            if character.is_whitespace() {
                pending_space = !normalized.is_empty();
                continue;
            }
            if pending_space {
                normalized.push(' ');
                pending_space = false;
            }
            normalized.extend(character.to_lowercase());
            if normalized.len() > MAX_ACCESSIBILITY_CORRELATION_STRING_BYTES {
                return Err(AccessibilityCorrelationError::StringLimit);
            }
        }
        if normalized.is_empty() {
            return Err(AccessibilityCorrelationError::InvalidString);
        }
        Ok(Self(normalized))
    }

    /// Returns the canonical value for equality and actor-owned indexing.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for NormalizedCorrelationText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NormalizedCorrelationText(<redacted>)")
    }
}

/// Exact accessibility identity and bounded AT-SPI-side evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibilityCorrelationSubject {
    /// Exact application incarnation that owns the accessible element.
    pub application: ApplicationRef,
    /// Exact accessible element incarnation being correlated.
    pub element: ElementRef,
    /// Toolkit-reported application process identifier.
    pub process_id: Option<u32>,
    /// Independently managed process identifier, when available.
    pub managed_process_id: Option<u32>,
    /// Root-physical bounds of the highest accessible top-level object.
    pub top_level_extents: Option<Rect>,
    /// Canonical accessible top-level title.
    pub title: Option<NormalizedCorrelationText>,
    /// Canonical application identity, such as desktop id or executable id.
    pub application_identity: Option<NormalizedCorrelationText>,
    /// Canonical toolkit-specific application identity.
    pub toolkit_identity: Option<NormalizedCorrelationText>,
    /// Whether AT-SPI currently reports the subject as focused.
    pub focused: bool,
    /// Monotonic time of the last AT-SPI focus transition.
    pub focus_changed_at: Option<MonotonicMillis>,
    /// Monotonic time at which the application incarnation was first observed.
    pub created_at: Option<MonotonicMillis>,
    /// Monotonic time of this evidence snapshot.
    pub observed_at: MonotonicMillis,
    /// Optional exact caller-provided X11 target that all evidence must agree with.
    pub explicit_window: Option<WindowRef>,
    /// Optional exact client-leader group expected for the target window.
    pub client_leader: Option<WindowRef>,
}

/// One exact X11 window candidate and bounded X11-side evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibilityWindowCandidate {
    /// Exact XID birth represented by this candidate.
    pub window: WindowRef,
    /// Whether the authoritative window model still considers this exact ref live.
    pub live: bool,
    /// `_NET_WM_PID` or equivalent client-reported process identifier.
    pub process_id: Option<u32>,
    /// Independently managed process identifier, when available.
    pub managed_process_id: Option<u32>,
    /// Root-physical top-level window bounds.
    pub top_level_extents: Option<Rect>,
    /// Canonical window title.
    pub title: Option<NormalizedCorrelationText>,
    /// Canonical application or desktop-file identity.
    pub application_identity: Option<NormalizedCorrelationText>,
    /// Canonical toolkit-specific identity.
    pub toolkit_identity: Option<NormalizedCorrelationText>,
    /// Whether the X11 model currently reports this window focused.
    pub focused: bool,
    /// Monotonic time of the last X11 focus transition.
    pub focus_changed_at: Option<MonotonicMillis>,
    /// Monotonic first-observation time for this exact XID birth.
    pub created_at: Option<MonotonicMillis>,
    /// Monotonic time of this candidate snapshot.
    pub observed_at: MonotonicMillis,
    /// Exact live client leader, if one was resolved.
    pub client_leader: Option<WindowRef>,
}

/// Closed failures for malformed, stale, or unbounded correlation inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AccessibilityCorrelationError {
    /// Caller-selected limits exceed a hard ceiling or are unusable.
    #[error("accessibility correlation limits are invalid")]
    InvalidLimits,
    /// Candidate count exceeds the caller-selected bounded limit.
    #[error("accessibility correlation candidate budget exceeded")]
    CandidateLimit,
    /// One string or the aggregate normalized string set exceeds its budget.
    #[error("accessibility correlation string budget exceeded")]
    StringLimit,
    /// A normalized title or identity is empty or contains forbidden controls.
    #[error("accessibility correlation string is invalid")]
    InvalidString,
    /// A process identifier is zero and therefore unusable as evidence.
    #[error("accessibility correlation process evidence is invalid")]
    InvalidProcessEvidence,
    /// An exact application, element, window, or leader reference is malformed.
    #[error("accessibility correlation exact reference is invalid")]
    InvalidReference,
    /// Exact references do not belong to the same desktop or generation.
    #[error("accessibility correlation reference scope is inconsistent")]
    ReferenceScope,
    /// A root-physical rectangle is malformed.
    #[error("accessibility correlation geometry is invalid")]
    InvalidGeometry,
    /// Observation, creation, or focus timestamps are inconsistent.
    #[error("accessibility correlation monotonic timestamps are inconsistent")]
    InvalidTime,
    /// Evidence claimed as live is older than the allowed observation age.
    #[error("accessibility correlation live evidence is stale")]
    StaleObservation,
    /// The same exact window was supplied more than once.
    #[error("accessibility correlation contains a duplicate window")]
    DuplicateWindow,
    /// More than one live birth was supplied for one XID.
    #[error("accessibility correlation contains conflicting live XID births")]
    ConflictingWindowBirth,
}

/// Correlates one exact accessible element to at most one exact live X11 window.
///
/// The result is deterministic under candidate permutation. Equal top scores
/// intentionally return no target. Regexes, fuzzy title matching, and external
/// lookups are deliberately outside this pure trust boundary.
pub fn correlate_accessibility_window(
    subject: &AccessibilityCorrelationSubject,
    candidates: &[AccessibilityWindowCandidate],
    now: MonotonicMillis,
    limits: AccessibilityCorrelationLimits,
) -> Result<ElementWindowCorrelation, AccessibilityCorrelationError> {
    validate_inputs(subject, candidates, now, limits)?;

    let mut ordered = candidates
        .iter()
        .filter(|candidate| candidate.live)
        .collect::<Vec<_>>();
    ordered.sort_by(|left, right| window_key(&left.window).cmp(&window_key(&right.window)));

    if ordered.is_empty() {
        return Ok(no_correlation(false, Vec::new()));
    }
    if let Some(explicit) = &subject.explicit_window
        && !ordered
            .iter()
            .any(|candidate| candidate.window == *explicit)
    {
        return Ok(no_correlation(
            true,
            vec![evidence(
                WindowCorrelationSignal::ExplicitCallerReference,
                false,
                "exact live window reference unavailable",
            )],
        ));
    }

    let subject_pid = subject.process_id;
    let pid_universe_complete = subject_pid.is_some()
        && ordered
            .iter()
            .all(|candidate| candidate.process_id.is_some());
    let pid_match_count = subject_pid.map_or(0, |pid| {
        ordered
            .iter()
            .filter(|candidate| candidate_pid_matches(candidate, pid))
            .count()
    });
    let managed_match_count = subject.managed_process_id.map_or(0, |managed_pid| {
        ordered
            .iter()
            .filter(|candidate| candidate.managed_process_id == Some(managed_pid))
            .count()
    });

    let mut scores = ordered
        .iter()
        .map(|candidate| score_candidate(subject, candidate, limits))
        .collect::<Vec<_>>();
    let global_conflict = anchor_conflict(subject, &ordered, &scores);
    if global_conflict {
        for score in &mut scores {
            score.conflict = true;
        }
    }

    let Some(best_score) = scores.iter().map(|score| score.score).max() else {
        return Ok(no_correlation(false, Vec::new()));
    };
    if best_score <= 0 {
        return Ok(no_correlation(
            scores.iter().any(|score| score.conflict),
            Vec::new(),
        ));
    }
    let winners = scores
        .iter()
        .enumerate()
        .filter(|(_, score)| score.score == best_score)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if winners.len() != 1 {
        return Ok(no_correlation(true, Vec::new()));
    }

    let winner_index = winners[0];
    let winner = ordered[winner_index];
    let score = &scores[winner_index];
    let exact_process =
        !score.conflict && score.pid_match && pid_universe_complete && pid_match_count == 1;
    let confidence = if score.conflict {
        WindowCorrelationConfidence::Weak
    } else if exact_process {
        WindowCorrelationConfidence::ExactProcess
    } else if score.explicit_match
        || (score.pid_match && score.strong_discriminators >= 2 && score.geometry_or_focus_match)
        || (score.managed_match
            && managed_match_count == 1
            && score.strong_discriminators >= 1
            && score.geometry_or_focus_match)
        || (score.managed_match
            && managed_match_count > 1
            && score.strong_discriminators >= 2
            && score.geometry_or_focus_match)
        || (!score.pid_match
            && !score.managed_match
            && score.strong_discriminators >= 3
            && score.geometry_or_focus_match)
    {
        WindowCorrelationConfidence::Strong
    } else {
        WindowCorrelationConfidence::Weak
    };

    Ok(ElementWindowCorrelation {
        window: Some(winner.window.clone()),
        confidence,
        evidence: score.evidence.clone(),
        conflicting_evidence: score.conflict,
    })
}

/// Whether the correlation is strong enough to gate a physical side effect.
///
/// A confidence label is not sufficient by itself: title and client-reported
/// PID evidence cannot authorize an effect without an exact caller reference,
/// independently managed-process evidence, geometry, client leader, or a fresh
/// focus transition.
///
/// The caller must still re-resolve the exact `WindowRef` immediately before
/// the effect. Correlation never proves liveness beyond its input snapshot.
#[must_use]
pub fn correlation_authorizes_physical_effect(correlation: &ElementWindowCorrelation) -> bool {
    let has_independent_corroboration = correlation.evidence.iter().any(|evidence| {
        evidence.matched
            && matches!(
                evidence.signal,
                WindowCorrelationSignal::ManagedProcess
                    | WindowCorrelationSignal::TopLevelExtents
                    | WindowCorrelationSignal::ClientLeader
                    | WindowCorrelationSignal::FocusTransition
                    | WindowCorrelationSignal::ExplicitCallerReference
            )
    });
    correlation.window.is_some()
        && !correlation.conflicting_evidence
        && has_independent_corroboration
        && matches!(
            correlation.confidence,
            WindowCorrelationConfidence::Strong | WindowCorrelationConfidence::ExactProcess
        )
}

#[derive(Debug, Clone)]
struct CandidateScore {
    score: i32,
    pid_match: bool,
    managed_match: bool,
    explicit_match: bool,
    strong_discriminators: u8,
    geometry_or_focus_match: bool,
    conflict: bool,
    evidence: Vec<WindowCorrelationEvidence>,
    exact_geometry: bool,
    focus_match: bool,
    leader_match: bool,
}

fn score_candidate(
    subject: &AccessibilityCorrelationSubject,
    candidate: &AccessibilityWindowCandidate,
    limits: AccessibilityCorrelationLimits,
) -> CandidateScore {
    let mut result = CandidateScore {
        score: 0,
        pid_match: false,
        managed_match: false,
        explicit_match: false,
        strong_discriminators: 0,
        geometry_or_focus_match: false,
        conflict: false,
        evidence: Vec::with_capacity(9),
        exact_geometry: false,
        focus_match: false,
        leader_match: false,
    };

    if let Some(explicit) = &subject.explicit_window {
        let matched = candidate.window == *explicit;
        result.evidence.push(evidence(
            WindowCorrelationSignal::ExplicitCallerReference,
            matched,
            if matched {
                "exact window reference matched"
            } else {
                "exact window reference differed"
            },
        ));
        if matched {
            result.score += 100;
            result.explicit_match = true;
        } else {
            result.score -= 100;
            result.conflict = true;
        }
    }

    let common_managed_group = subject
        .managed_process_id
        .zip(candidate.managed_process_id)
        .is_some_and(|(expected, actual)| expected == actual);
    if let (Some(expected), Some(actual)) = (subject.process_id, candidate.process_id) {
        let matched = expected == actual;
        result.evidence.push(evidence(
            WindowCorrelationSignal::ProcessId,
            matched,
            if matched {
                "process identifier matched"
            } else if common_managed_group {
                "process identifiers differed within one managed group"
            } else {
                "process identifier conflicted"
            },
        ));
        if matched {
            result.score += 60;
            result.pid_match = true;
        } else if !common_managed_group {
            result.score -= 80;
            result.conflict = true;
        }
    }

    if let (Some(expected), Some(actual)) =
        (subject.managed_process_id, candidate.managed_process_id)
    {
        let matched = expected == actual;
        result.evidence.push(evidence(
            WindowCorrelationSignal::ManagedProcess,
            matched,
            if matched {
                "managed process identifier matched"
            } else {
                "managed process identifier conflicted"
            },
        ));
        if matched {
            result.score += 50;
            result.managed_match = true;
        } else {
            result.score -= 80;
            result.conflict = true;
        }
    }

    if let (Some(expected), Some(actual)) = (subject.top_level_extents, candidate.top_level_extents)
    {
        let comparison = geometry_comparison(expected, actual);
        let matched = matches!(
            comparison,
            GeometryComparison::Exact | GeometryComparison::Strong
        );
        result.evidence.push(evidence(
            WindowCorrelationSignal::TopLevelExtents,
            matched,
            match comparison {
                GeometryComparison::Exact => "root-physical extents matched exactly",
                GeometryComparison::Strong => "root-physical extents strongly overlapped",
                GeometryComparison::Weak => "root-physical extents weakly overlapped",
                GeometryComparison::Miss => "root-physical extents conflicted",
            },
        ));
        match comparison {
            GeometryComparison::Exact => {
                result.score += 40;
                result.strong_discriminators += 1;
                result.geometry_or_focus_match = true;
                result.exact_geometry = true;
            }
            GeometryComparison::Strong => {
                result.score += 30;
                result.strong_discriminators += 1;
                result.geometry_or_focus_match = true;
            }
            GeometryComparison::Weak => result.score += 12,
            GeometryComparison::Miss => {
                result.score -= 40;
                result.conflict = true;
            }
        }
    }

    if let (Some(expected), Some(actual)) = (&subject.title, &candidate.title) {
        let matched = expected == actual;
        result.evidence.push(evidence(
            WindowCorrelationSignal::Title,
            matched,
            if matched {
                "normalized title matched"
            } else {
                "normalized title differed"
            },
        ));
        if matched {
            result.score += 5;
        }
    }

    let identity = identity_comparison(subject, candidate);
    if let Some(identity) = identity {
        result.evidence.push(evidence(
            WindowCorrelationSignal::ToolkitIdentity,
            identity.matched,
            identity.detail,
        ));
        if identity.matched {
            result.score += identity.score;
            result.strong_discriminators += 1;
        } else {
            result.score -= 30;
            result.conflict = true;
        }
    }

    if let Some(expected) = &subject.client_leader {
        let matched = candidate.window == *expected
            || candidate
                .client_leader
                .as_ref()
                .is_some_and(|leader| leader == expected);
        result.evidence.push(evidence(
            WindowCorrelationSignal::ClientLeader,
            matched,
            if matched {
                "exact client-leader group matched"
            } else {
                "exact client-leader group conflicted"
            },
        ));
        if matched {
            result.score += 24;
            result.strong_discriminators += 1;
            result.leader_match = true;
        } else {
            result.score -= 20;
            result.conflict = true;
        }
    }

    if subject.focused {
        let matched = candidate.focused
            && subject
                .focus_changed_at
                .zip(candidate.focus_changed_at)
                .is_some_and(|(left, right)| {
                    absolute_time_delta(left, right) <= limits.max_focus_delta_ms
                });
        result.evidence.push(evidence(
            WindowCorrelationSignal::FocusTransition,
            matched,
            if matched {
                "fresh focus transitions matched"
            } else {
                "focus evidence was absent or stale"
            },
        ));
        if matched {
            result.score += 24;
            result.strong_discriminators += 1;
            result.geometry_or_focus_match = true;
            result.focus_match = true;
        } else if observations_are_close(subject.observed_at, candidate.observed_at, limits)
            && !candidate.focused
        {
            result.score -= 12;
            result.conflict = true;
        }
    }

    if let (Some(left), Some(right)) = (subject.created_at, candidate.created_at) {
        let matched = absolute_time_delta(left, right) <= limits.max_creation_delta_ms;
        result.evidence.push(evidence(
            WindowCorrelationSignal::CreationProximity,
            matched,
            if matched {
                "creation observations were proximate"
            } else {
                "creation observations were not proximate"
            },
        ));
        if matched {
            result.score += 8;
        }
    }

    result
}

fn anchor_conflict(
    subject: &AccessibilityCorrelationSubject,
    candidates: &[&AccessibilityWindowCandidate],
    scores: &[CandidateScore],
) -> bool {
    let mut anchors = HashSet::new();
    if let Some(explicit) = &subject.explicit_window
        && let Some(index) = candidates
            .iter()
            .position(|candidate| candidate.window == *explicit)
    {
        anchors.insert(index);
    }
    insert_unique_anchor(
        &mut anchors,
        scores
            .iter()
            .enumerate()
            .filter(|(_, score)| score.pid_match)
            .map(|(index, _)| index),
    );
    insert_unique_anchor(
        &mut anchors,
        scores
            .iter()
            .enumerate()
            .filter(|(_, score)| score.managed_match)
            .map(|(index, _)| index),
    );
    insert_unique_anchor(
        &mut anchors,
        scores
            .iter()
            .enumerate()
            .filter(|(_, score)| score.exact_geometry)
            .map(|(index, _)| index),
    );
    insert_unique_anchor(
        &mut anchors,
        scores
            .iter()
            .enumerate()
            .filter(|(_, score)| score.focus_match)
            .map(|(index, _)| index),
    );
    insert_unique_anchor(
        &mut anchors,
        scores
            .iter()
            .enumerate()
            .filter(|(_, score)| score.leader_match)
            .map(|(index, _)| index),
    );
    anchors.len() > 1
}

fn insert_unique_anchor(anchors: &mut HashSet<usize>, values: impl Iterator<Item = usize>) {
    let values = values.collect::<Vec<_>>();
    if values.len() == 1 {
        anchors.insert(values[0]);
    }
}

fn validate_inputs(
    subject: &AccessibilityCorrelationSubject,
    candidates: &[AccessibilityWindowCandidate],
    now: MonotonicMillis,
    limits: AccessibilityCorrelationLimits,
) -> Result<(), AccessibilityCorrelationError> {
    limits.validate()?;
    if candidates.len() > limits.max_candidates {
        return Err(AccessibilityCorrelationError::CandidateLimit);
    }
    subject
        .application
        .validate()
        .map_err(|_| AccessibilityCorrelationError::InvalidReference)?;
    subject
        .element
        .validate()
        .map_err(|_| AccessibilityCorrelationError::InvalidReference)?;
    if subject.element.application != subject.application {
        return Err(AccessibilityCorrelationError::ReferenceScope);
    }
    validate_process_pair(subject.process_id, subject.managed_process_id)?;
    validate_rect(subject.top_level_extents)?;
    validate_time_set(
        subject.created_at,
        subject.focus_changed_at,
        subject.observed_at,
        now,
        true,
        limits,
    )?;
    validate_optional_window(
        subject.explicit_window.as_ref(),
        subject.application.desktop_id,
        subject.application.desktop_generation,
    )?;
    validate_optional_window(
        subject.client_leader.as_ref(),
        subject.application.desktop_id,
        subject.application.desktop_generation,
    )?;

    let mut exact_windows = HashSet::with_capacity(candidates.len());
    let mut live_xids: HashMap<u32, &WindowRef> = HashMap::with_capacity(candidates.len());
    for candidate in candidates {
        candidate
            .window
            .validate_shape()
            .map_err(|_| AccessibilityCorrelationError::InvalidReference)?;
        if candidate.window.desktop_id != subject.application.desktop_id
            || candidate.window.desktop_generation != subject.application.desktop_generation
        {
            return Err(AccessibilityCorrelationError::ReferenceScope);
        }
        if !exact_windows.insert(candidate.window.clone()) {
            return Err(AccessibilityCorrelationError::DuplicateWindow);
        }
        if candidate.live
            && let Some(previous) = live_xids.insert(candidate.window.xid, &candidate.window)
            && previous != &candidate.window
        {
            return Err(AccessibilityCorrelationError::ConflictingWindowBirth);
        }
        validate_process_pair(candidate.process_id, candidate.managed_process_id)?;
        validate_rect(candidate.top_level_extents)?;
        validate_time_set(
            candidate.created_at,
            candidate.focus_changed_at,
            candidate.observed_at,
            now,
            candidate.live,
            limits,
        )?;
        validate_optional_window(
            candidate.client_leader.as_ref(),
            subject.application.desktop_id,
            subject.application.desktop_generation,
        )?;
    }
    validate_string_budget(subject, candidates, limits)
}

fn validate_process_pair(
    process_id: Option<u32>,
    managed_process_id: Option<u32>,
) -> Result<(), AccessibilityCorrelationError> {
    if process_id == Some(0) || managed_process_id == Some(0) {
        return Err(AccessibilityCorrelationError::InvalidProcessEvidence);
    }
    Ok(())
}

fn validate_rect(rect: Option<Rect>) -> Result<(), AccessibilityCorrelationError> {
    if rect.is_some_and(|rect| rect.validate().is_err()) {
        return Err(AccessibilityCorrelationError::InvalidGeometry);
    }
    Ok(())
}

fn validate_time_set(
    created_at: Option<MonotonicMillis>,
    focus_changed_at: Option<MonotonicMillis>,
    observed_at: MonotonicMillis,
    now: MonotonicMillis,
    require_fresh: bool,
    limits: AccessibilityCorrelationLimits,
) -> Result<(), AccessibilityCorrelationError> {
    if observed_at > now
        || created_at.is_some_and(|created| created > observed_at)
        || focus_changed_at.is_some_and(|focused| focused > observed_at)
    {
        return Err(AccessibilityCorrelationError::InvalidTime);
    }
    if require_fresh
        && now
            .elapsed_since(observed_at)
            .is_none_or(|age| age > limits.max_observation_age_ms)
    {
        return Err(AccessibilityCorrelationError::StaleObservation);
    }
    Ok(())
}

fn validate_optional_window(
    window: Option<&WindowRef>,
    desktop_id: xenoteer_protocol::DesktopId,
    desktop_generation: xenoteer_protocol::DesktopGeneration,
) -> Result<(), AccessibilityCorrelationError> {
    let Some(window) = window else {
        return Ok(());
    };
    window
        .validate_shape()
        .map_err(|_| AccessibilityCorrelationError::InvalidReference)?;
    if window.desktop_id != desktop_id || window.desktop_generation != desktop_generation {
        return Err(AccessibilityCorrelationError::ReferenceScope);
    }
    Ok(())
}

fn validate_string_budget(
    subject: &AccessibilityCorrelationSubject,
    candidates: &[AccessibilityWindowCandidate],
    limits: AccessibilityCorrelationLimits,
) -> Result<(), AccessibilityCorrelationError> {
    let mut total = subject_string_bytes(subject);
    for candidate in candidates {
        total = total
            .checked_add(candidate_string_bytes(candidate))
            .ok_or(AccessibilityCorrelationError::StringLimit)?;
        if total > limits.max_total_string_bytes {
            return Err(AccessibilityCorrelationError::StringLimit);
        }
    }
    if total > limits.max_total_string_bytes {
        return Err(AccessibilityCorrelationError::StringLimit);
    }
    Ok(())
}

fn subject_string_bytes(subject: &AccessibilityCorrelationSubject) -> usize {
    [
        subject.title.as_ref(),
        subject.application_identity.as_ref(),
        subject.toolkit_identity.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|value| value.as_str().len())
    .sum()
}

fn candidate_string_bytes(candidate: &AccessibilityWindowCandidate) -> usize {
    [
        candidate.title.as_ref(),
        candidate.application_identity.as_ref(),
        candidate.toolkit_identity.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|value| value.as_str().len())
    .sum()
}

fn candidate_pid_matches(candidate: &AccessibilityWindowCandidate, expected: u32) -> bool {
    candidate.process_id == Some(expected)
}

fn observations_are_close(
    left: MonotonicMillis,
    right: MonotonicMillis,
    limits: AccessibilityCorrelationLimits,
) -> bool {
    absolute_time_delta(left, right) <= limits.max_focus_delta_ms
}

fn absolute_time_delta(left: MonotonicMillis, right: MonotonicMillis) -> u64 {
    left.get().abs_diff(right.get())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeometryComparison {
    Exact,
    Strong,
    Weak,
    Miss,
}

fn geometry_comparison(left: Rect, right: Rect) -> GeometryComparison {
    if left == right {
        return GeometryComparison::Exact;
    }
    let Some(intersection) = intersection_area(left, right) else {
        return GeometryComparison::Miss;
    };
    let Some(left_area) = rectangle_area(left) else {
        return GeometryComparison::Miss;
    };
    let Some(right_area) = rectangle_area(right) else {
        return GeometryComparison::Miss;
    };
    let smaller = left_area.min(right_area);
    if intersection * 100 >= smaller * 80 {
        GeometryComparison::Strong
    } else if intersection * 100 >= smaller * 50 {
        GeometryComparison::Weak
    } else {
        GeometryComparison::Miss
    }
}

fn rectangle_area(rect: Rect) -> Option<u128> {
    let size = rect.size().ok()?;
    Some(u128::from(size.width()) * u128::from(size.height()))
}

fn intersection_area(left: Rect, right: Rect) -> Option<u128> {
    let left_size = left.size().ok()?;
    let right_size = right.size().ok()?;
    let left_origin = left.origin();
    let right_origin = right.origin();
    let left_x2 = i128::from(left_origin.x()) + i128::from(left_size.width());
    let left_y2 = i128::from(left_origin.y()) + i128::from(left_size.height());
    let right_x2 = i128::from(right_origin.x()) + i128::from(right_size.width());
    let right_y2 = i128::from(right_origin.y()) + i128::from(right_size.height());
    let width = left_x2.min(right_x2) - i128::from(left_origin.x().max(right_origin.x()));
    let height = left_y2.min(right_y2) - i128::from(left_origin.y().max(right_origin.y()));
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(u128::try_from(width).ok()? * u128::try_from(height).ok()?)
}

struct IdentityComparison {
    matched: bool,
    score: i32,
    detail: &'static str,
}

fn identity_comparison(
    subject: &AccessibilityCorrelationSubject,
    candidate: &AccessibilityWindowCandidate,
) -> Option<IdentityComparison> {
    let application = subject
        .application_identity
        .as_ref()
        .zip(candidate.application_identity.as_ref())
        .map(|(left, right)| left == right);
    let toolkit = subject
        .toolkit_identity
        .as_ref()
        .zip(candidate.toolkit_identity.as_ref())
        .map(|(left, right)| left == right);
    match (application, toolkit) {
        (None, None) => None,
        (Some(true), Some(true)) => Some(IdentityComparison {
            matched: true,
            score: 24,
            detail: "application and toolkit identities matched",
        }),
        (Some(true), None) => Some(IdentityComparison {
            matched: true,
            score: 18,
            detail: "application identity matched",
        }),
        (None, Some(true)) => Some(IdentityComparison {
            matched: true,
            score: 18,
            detail: "toolkit identity matched",
        }),
        _ => Some(IdentityComparison {
            matched: false,
            score: 0,
            detail: "application or toolkit identity conflicted",
        }),
    }
}

fn no_correlation(
    conflicting_evidence: bool,
    evidence: Vec<WindowCorrelationEvidence>,
) -> ElementWindowCorrelation {
    ElementWindowCorrelation {
        window: None,
        confidence: WindowCorrelationConfidence::None,
        evidence,
        conflicting_evidence,
    }
}

fn evidence(
    signal: WindowCorrelationSignal,
    matched: bool,
    detail: &'static str,
) -> WindowCorrelationEvidence {
    WindowCorrelationEvidence {
        signal,
        matched,
        detail: Some(detail.to_owned()),
    }
}

fn window_key(window: &WindowRef) -> (u32, u64, &str) {
    (
        window.xid,
        window.observed_generation,
        window.identity_hash.as_str(),
    )
}
