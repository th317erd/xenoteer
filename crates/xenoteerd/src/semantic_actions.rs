//! Exact, generation-fenced daemon composition for semantic AT-SPI actions.

use std::{future::Future, sync::Arc, time::Duration};

use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;
use xenoteer_atspi::{
    ActionSelector, RedactedText, ScrollPlacement, SelectionOperation, SemanticError,
    SemanticEvidence, SemanticObservationRequest, SemanticOperation, SemanticRequest,
    SemanticResult, TextInsertPosition, TextSelectionPolicy,
};
use xenoteer_core::correlation_authorizes_physical_effect;
use xenoteer_protocol::{
    AccessibilityQueryLimits, Command, EditableTextSelectionPolicy, ElementActionEvidence,
    ElementActionOperation, ElementActionResult, ElementActionTarget, ElementPostcondition,
    ElementRef, ElementScrollAlignment, ElementScrollTarget, ElementSelectionOperation,
    ElementSnapshot, ElementSnapshotExpansion, ElementSnapshotRequest, ElementState,
    ElementWaitPredicate, ElementWaitRequest, ElementWaitStatus, ElementWaitTarget, Rect,
    SemanticTextInsertEvidence, SemanticTextInsertOptions, SemanticTextInsertionPoint,
};
use xenoteer_server::AccessibilityPlaneError;

use crate::{
    accessibility_plane::DaemonAccessibilityPlane,
    accessibility_runtime::AccessibilitySemanticRuntime,
};

/// A semantic failure with enough stage information for conservative command mapping.
#[derive(Debug)]
pub(crate) enum SemanticActionFailure {
    PlaneBefore(AccessibilityPlaneError),
    PlaneAfter(AccessibilityPlaneError),
    Actor(SemanticError),
    Disabled,
    WeakWindowCorrelation,
    VerificationUnsupported,
    BackendRejected,
    PostconditionFailed,
    DeadlineAfterEffect,
    InvalidEvidence,
}

impl SemanticActionFailure {
    #[must_use]
    pub(crate) fn effect_may_have_occurred(&self) -> bool {
        matches!(
            self,
            Self::PlaneAfter(_)
                | Self::BackendRejected
                | Self::PostconditionFailed
                | Self::DeadlineAfterEffect
                | Self::InvalidEvidence
        ) || matches!(self, Self::Actor(error) if !error.effect_definitely_not_dispatched())
    }
}

/// Execute one non-physical element command through the actor and mirror fences.
pub(crate) async fn execute_semantic_action(
    runtime: &AccessibilitySemanticRuntime,
    command: Command,
    deadline: Instant,
    cancellation: CancellationToken,
) -> Result<ElementActionResult, SemanticActionFailure> {
    execute_semantic_action_inner(runtime, command, deadline, cancellation, || async {
        Ok(())
    })
    .await
}

async fn execute_semantic_action_inner<BeforeObservation, BeforeObservationFuture>(
    runtime: &AccessibilitySemanticRuntime,
    command: Command,
    deadline: Instant,
    cancellation: CancellationToken,
    mut before_observation: BeforeObservation,
) -> Result<ElementActionResult, SemanticActionFailure>
where
    BeforeObservation: FnMut() -> BeforeObservationFuture,
    BeforeObservationFuture: Future<Output = Result<(), SemanticActionFailure>>,
{
    let plan = SemanticActionPlan::new(command).ok_or(SemanticActionFailure::InvalidEvidence)?;
    require_supported_postcondition(plan.postcondition())?;
    let plane = runtime.plane();
    let actor = runtime.handle();
    let mut retried_pre_dispatch_stale = false;

    loop {
        ensure_before_deadline(deadline, &cancellation)?;
        let evidence = plane
            .resolve_action_target(plan.element())
            .await
            .map_err(SemanticActionFailure::PlaneBefore)?;
        let before = plane
            .snapshot_for(ElementSnapshotRequest {
                desktop_id: plan.element().desktop_id,
                desktop_generation: plan.element().desktop_generation,
                element: evidence.current_element().clone(),
                expansion: plan.expansion(),
            })
            .await
            .map_err(SemanticActionFailure::PlaneBefore)?;
        if before.snapshot_revision != evidence.cache_revision()
            || before.element.snapshot.element != *evidence.current_element()
        {
            if !retried_pre_dispatch_stale {
                retried_pre_dispatch_stale = true;
                continue;
            }
            return Err(SemanticActionFailure::PlaneBefore(
                AccessibilityPlaneError::StaleReference {
                    current_generation: None,
                },
            ));
        }
        plan.validate_preflight(&before.element.snapshot)?;

        let target = match timeout_at(
            deadline,
            actor.semantic_target(
                evidence.semantic_target_request(),
                cancellation.child_token(),
            ),
        )
        .await
        {
            Ok(Ok(target)) => target,
            Ok(Err(error)) if is_pre_dispatch_stale(&error) && !retried_pre_dispatch_stale => {
                retried_pre_dispatch_stale = true;
                continue;
            }
            Ok(Err(error)) => return Err(SemanticActionFailure::Actor(error)),
            Err(_) => {
                return Err(SemanticActionFailure::Actor(
                    SemanticError::DeadlineBeforeDispatch,
                ));
            }
        };

        if plan.needs_fresh_observation() {
            before_observation().await?;
            let observed = actor
                .observe_semantic(
                    SemanticObservationRequest {
                        target: target.clone(),
                        deadline,
                    },
                    cancellation.child_token(),
                )
                .await;
            match observed {
                // Read-only evidence may name a newer global revision after
                // unrelated cache churn. Exact plane and actor write fences
                // below still reject any target or dispatch-coordinate drift.
                Ok(observed) if evidence.validate_observation(&observed).is_ok() => {
                    plan.validate_fresh_observation(&observed.evidence)?;
                }
                Ok(_) => return Err(SemanticActionFailure::InvalidEvidence),
                Err(error) if is_pre_dispatch_stale(&error) && !retried_pre_dispatch_stale => {
                    retried_pre_dispatch_stale = true;
                    continue;
                }
                Err(error) => return Err(SemanticActionFailure::Actor(error)),
            }
        }

        if let Err(error) = plane.revalidate_action_target(&evidence).await {
            if !retried_pre_dispatch_stale {
                retried_pre_dispatch_stale = true;
                continue;
            }
            return Err(SemanticActionFailure::PlaneBefore(error));
        }
        ensure_before_deadline(deadline, &cancellation)?;

        let expected_generation = evidence.accessibility_generation();
        let expected_application_generation = evidence.application_generation();
        let expected_source_revision = evidence.source_revision();
        let result = actor
            .execute_semantic(
                SemanticRequest {
                    target,
                    operation: plan.operation()?,
                    deadline,
                },
                cancellation.child_token(),
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) if is_pre_dispatch_stale(&error) && !retried_pre_dispatch_stale => {
                retried_pre_dispatch_stale = true;
                continue;
            }
            Err(error) => return Err(SemanticActionFailure::Actor(error)),
        };
        if result.accessibility_generation != expected_generation
            || result.application_generation != expected_application_generation
            || result.cache_revision != expected_source_revision
        {
            return Err(SemanticActionFailure::InvalidEvidence);
        }

        return finish_action(&plane, &plan, before, result, deadline).await;
    }
}

#[cfg(test)]
pub(crate) async fn execute_semantic_action_with_pre_observation_hook<
    BeforeObservation,
    BeforeObservationFuture,
>(
    runtime: &AccessibilitySemanticRuntime,
    command: Command,
    deadline: Instant,
    cancellation: CancellationToken,
    before_observation: BeforeObservation,
) -> Result<ElementActionResult, SemanticActionFailure>
where
    BeforeObservation: FnMut() -> BeforeObservationFuture,
    BeforeObservationFuture: Future<Output = Result<(), SemanticActionFailure>>,
{
    execute_semantic_action_inner(runtime, command, deadline, cancellation, before_observation)
        .await
}

/// Insert secret text through EditableText while returning only content-free evidence.
pub(crate) async fn execute_semantic_text_insert(
    runtime: &AccessibilitySemanticRuntime,
    element: ElementRef,
    text: String,
    options: SemanticTextInsertOptions,
    deadline: Instant,
    cancellation: CancellationToken,
) -> Result<SemanticTextInsertEvidence, SemanticActionFailure> {
    execute_semantic_text_insert_inner(
        runtime,
        element,
        text,
        options,
        deadline,
        cancellation,
        || async { Ok(()) },
    )
    .await
}

async fn execute_semantic_text_insert_inner<BeforeObservation, BeforeObservationFuture>(
    runtime: &AccessibilitySemanticRuntime,
    element: ElementRef,
    text: String,
    options: SemanticTextInsertOptions,
    deadline: Instant,
    cancellation: CancellationToken,
    mut before_observation: BeforeObservation,
) -> Result<SemanticTextInsertEvidence, SemanticActionFailure>
where
    BeforeObservation: FnMut() -> BeforeObservationFuture,
    BeforeObservationFuture: Future<Output = Result<(), SemanticActionFailure>>,
{
    require_supported_postcondition(options.postcondition.as_ref())?;
    let plane = runtime.plane();
    let actor = runtime.handle();
    let inserted_characters = text_character_count(&text)?;
    let mut retried_pre_dispatch_stale = false;

    loop {
        ensure_before_deadline(deadline, &cancellation)?;
        let evidence = plane
            .resolve_action_target(&element)
            .await
            .map_err(SemanticActionFailure::PlaneBefore)?;
        let before = plane
            .snapshot_for(ElementSnapshotRequest {
                desktop_id: element.desktop_id,
                desktop_generation: element.desktop_generation,
                element: evidence.current_element().clone(),
                expansion: minimal_action_expansion(),
            })
            .await
            .map_err(SemanticActionFailure::PlaneBefore)?;
        if before.snapshot_revision != evidence.cache_revision()
            || before.element.snapshot.element != *evidence.current_element()
        {
            if !retried_pre_dispatch_stale {
                retried_pre_dispatch_stale = true;
                continue;
            }
            return Err(SemanticActionFailure::PlaneBefore(
                AccessibilityPlaneError::StaleReference {
                    current_generation: None,
                },
            ));
        }
        validate_text_verification(&before.element.snapshot, options.verify_length_only)?;

        let target = match timeout_at(
            deadline,
            actor.semantic_target(
                evidence.semantic_target_request(),
                cancellation.child_token(),
            ),
        )
        .await
        {
            Ok(Ok(target)) => target,
            Ok(Err(error)) if is_pre_dispatch_stale(&error) && !retried_pre_dispatch_stale => {
                retried_pre_dispatch_stale = true;
                continue;
            }
            Ok(Err(error)) => return Err(SemanticActionFailure::Actor(error)),
            Err(_) => {
                return Err(SemanticActionFailure::Actor(
                    SemanticError::DeadlineBeforeDispatch,
                ));
            }
        };
        before_observation().await?;
        let observed = actor
            .observe_semantic(
                SemanticObservationRequest {
                    target: target.clone(),
                    deadline,
                },
                cancellation.child_token(),
            )
            .await;
        let observed_text = match observed {
            // The observation revision is a lower-bound contract: unrelated
            // cache churn is safe to read through, while the exact write target
            // is independently revalidated before secret-bearing dispatch.
            Ok(observed) if evidence.validate_observation(&observed).is_ok() => observed
                .evidence
                .text
                .ok_or(SemanticActionFailure::VerificationUnsupported)?,
            Ok(_) => return Err(SemanticActionFailure::InvalidEvidence),
            Err(error) if is_pre_dispatch_stale(&error) && !retried_pre_dispatch_stale => {
                retried_pre_dispatch_stale = true;
                continue;
            }
            Err(error) => return Err(SemanticActionFailure::Actor(error)),
        };
        if let Err(error) = plane.revalidate_action_target(&evidence).await {
            if !retried_pre_dispatch_stale {
                retried_pre_dispatch_stale = true;
                continue;
            }
            return Err(SemanticActionFailure::PlaneBefore(error));
        }
        ensure_before_deadline(deadline, &cancellation)?;

        let insertion_offset = match options.insertion_point {
            SemanticTextInsertionPoint::Caret => u32::try_from(observed_text.caret_offset)
                .map_err(|_| SemanticActionFailure::VerificationUnsupported)?,
            SemanticTextInsertionPoint::Offset { offset } => {
                u32::try_from(offset).map_err(|_| SemanticActionFailure::InvalidEvidence)?
            }
        };
        if insertion_offset > observed_text.character_count {
            return Err(SemanticActionFailure::PostconditionFailed);
        }
        let operation = SemanticOperation::InsertText {
            position: match options.insertion_point {
                SemanticTextInsertionPoint::Caret => TextInsertPosition::LiveCaret,
                SemanticTextInsertionPoint::Offset { .. } => {
                    TextInsertPosition::Offset(insertion_offset)
                }
            },
            text: RedactedText::new(text.clone()).map_err(SemanticActionFailure::Actor)?,
            selection: text_selection(options.selection),
        };
        let result = actor
            .execute_semantic(
                SemanticRequest {
                    target,
                    operation,
                    deadline,
                },
                cancellation.child_token(),
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) if is_pre_dispatch_stale(&error) && !retried_pre_dispatch_stale => {
                retried_pre_dispatch_stale = true;
                continue;
            }
            Err(error) => return Err(SemanticActionFailure::Actor(error)),
        };
        if result.accessibility_generation != evidence.accessibility_generation()
            || result.application_generation != evidence.application_generation()
            || result.cache_revision != evidence.source_revision()
        {
            return Err(SemanticActionFailure::InvalidEvidence);
        }
        let SemanticEvidence::Text {
            accepted,
            before: backend_before,
            after,
        } = result.evidence
        else {
            return Err(SemanticActionFailure::InvalidEvidence);
        };
        require_accepted(accepted)?;
        let expected_after = backend_before
            .character_count
            .checked_add(inserted_characters)
            .ok_or(SemanticActionFailure::InvalidEvidence)?;
        if backend_before != observed_text || after.character_count != expected_after {
            return Err(SemanticActionFailure::PostconditionFailed);
        }

        let postcondition_satisfied = if let Some(postcondition) = &options.postcondition {
            let _ = wait_for_postcondition(
                &plane,
                &element,
                before.snapshot_revision,
                postcondition,
                deadline,
            )
            .await?;
            Some(true)
        } else {
            None
        };
        let revision = xenoteer_protocol::AccessibilityRevision::new(result.cache_revision)
            .map_err(|_| SemanticActionFailure::InvalidEvidence)?;
        let semantic = SemanticTextInsertEvidence {
            element,
            revision_before: revision,
            revision_after: revision,
            backend_accepted: accepted,
            insertion_offset,
            character_count_before: backend_before.character_count,
            character_count_after: after.character_count,
            caret_offset_after: Some(
                u32::try_from(after.caret_offset)
                    .map_err(|_| SemanticActionFailure::InvalidEvidence)?,
            ),
            selection_count_after: Some(
                u32::try_from(after.selections.len())
                    .map_err(|_| SemanticActionFailure::InvalidEvidence)?,
            ),
            verified_length_only: options.verify_length_only,
            postcondition_satisfied,
        };
        semantic
            .validate()
            .map_err(|_| SemanticActionFailure::InvalidEvidence)?;
        return Ok(semantic);
    }
}

#[cfg(test)]
pub(crate) async fn execute_semantic_text_insert_with_pre_observation_hook<
    BeforeObservation,
    BeforeObservationFuture,
>(
    runtime: &AccessibilitySemanticRuntime,
    element: ElementRef,
    text: String,
    options: SemanticTextInsertOptions,
    deadline: Instant,
    cancellation: CancellationToken,
    before_observation: BeforeObservation,
) -> Result<SemanticTextInsertEvidence, SemanticActionFailure>
where
    BeforeObservation: FnMut() -> BeforeObservationFuture,
    BeforeObservationFuture: Future<Output = Result<(), SemanticActionFailure>>,
{
    execute_semantic_text_insert_inner(
        runtime,
        element,
        text,
        options,
        deadline,
        cancellation,
        before_observation,
    )
    .await
}

/// Reject postconditions that the current daemon wait plane cannot evaluate.
///
/// This must run before any semantic dispatch or physical-click preparation so
/// a valid protocol predicate cannot turn into a post-effect capability error.
pub(crate) fn require_supported_postcondition(
    postcondition: Option<&ElementPostcondition>,
) -> Result<(), SemanticActionFailure> {
    if matches!(
        postcondition.map(|value| &value.predicate),
        Some(ElementWaitPredicate::Text { .. })
    ) {
        return Err(SemanticActionFailure::VerificationUnsupported);
    }
    Ok(())
}

fn ensure_before_deadline(
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<(), SemanticActionFailure> {
    if cancellation.is_cancelled() {
        return Err(SemanticActionFailure::Actor(
            SemanticError::CancelledBeforeDispatch,
        ));
    }
    if Instant::now() >= deadline {
        return Err(SemanticActionFailure::Actor(
            SemanticError::DeadlineBeforeDispatch,
        ));
    }
    Ok(())
}

fn is_pre_dispatch_stale(error: &SemanticError) -> bool {
    matches!(
        error,
        SemanticError::StaleAccessibilityGeneration { .. }
            | SemanticError::StaleApplicationGeneration { .. }
            | SemanticError::StaleCacheRevision { .. }
            | SemanticError::StaleIdentity
    )
}

async fn finish_action(
    plane: &Arc<DaemonAccessibilityPlane>,
    plan: &SemanticActionPlan,
    before: xenoteer_protocol::ElementSnapshotResult,
    result: SemanticResult,
    deadline: Instant,
) -> Result<ElementActionResult, SemanticActionFailure> {
    let mut public_evidence = plan.verify(&before.element.snapshot, result.evidence)?;
    let mut revision_after = before.snapshot_revision;
    let mut snapshot_after = None;

    if let Some(postcondition) = plan.postcondition() {
        let wait = wait_for_postcondition(
            plane,
            plan.element(),
            before.snapshot_revision,
            postcondition,
            deadline,
        )
        .await?;
        revision_after = wait.evaluated_revision;
        public_evidence.poll_fallback_used = wait.poll_fallback_used;
        public_evidence.postcondition_satisfied = Some(true);
    }

    match plane
        .snapshot_for(ElementSnapshotRequest {
            desktop_id: plan.element().desktop_id,
            desktop_generation: plan.element().desktop_generation,
            element: plan.element().clone(),
            expansion: plan.expansion(),
        })
        .await
    {
        Ok(after) => {
            revision_after = revision_after.max(after.snapshot_revision);
            snapshot_after = Some(Box::new(after.element.snapshot));
        }
        Err(AccessibilityPlaneError::NotFound | AccessibilityPlaneError::StaleReference { .. })
            if matches!(
                plan.postcondition().map(|value| &value.predicate),
                Some(ElementWaitPredicate::Gone)
            ) => {}
        Err(error) if plan.postcondition().is_some() => {
            return Err(SemanticActionFailure::PlaneAfter(error));
        }
        Err(_) => {}
    }

    let result = ElementActionResult {
        operation: plan.kind(),
        element: plan.element().clone(),
        revision_before: before.snapshot_revision,
        revision_after,
        snapshot_before: Some(Box::new(before.element.snapshot)),
        snapshot_after,
        evidence: public_evidence,
    };
    result
        .validate()
        .map_err(|_| SemanticActionFailure::InvalidEvidence)?;
    Ok(result)
}

pub(crate) async fn wait_for_postcondition(
    plane: &DaemonAccessibilityPlane,
    element: &ElementRef,
    after_revision: xenoteer_protocol::AccessibilityRevision,
    postcondition: &ElementPostcondition,
    deadline: Instant,
) -> Result<xenoteer_protocol::ElementWaitResult, SemanticActionFailure> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(SemanticActionFailure::DeadlineAfterEffect);
    }
    let requested = Duration::from_millis(u64::from(postcondition.timeout_ms));
    let timeout = remaining.min(requested);
    let timeout_ms = u32::try_from(timeout.as_millis().max(1))
        .unwrap_or(postcondition.timeout_ms)
        .min(postcondition.timeout_ms);
    let wait = plane
        .wait_for(ElementWaitRequest {
            desktop_id: element.desktop_id,
            desktop_generation: element.desktop_generation,
            target: ElementWaitTarget::Reference {
                element: element.clone(),
            },
            predicate: postcondition.predicate.clone(),
            after_revision: Some(after_revision),
            timeout_ms,
            allow_poll_fallback: postcondition.allow_poll_fallback,
            expansion: postcondition_expansion(&postcondition.predicate),
            limits: AccessibilityQueryLimits::default(),
        })
        .await
        .map_err(SemanticActionFailure::PlaneAfter)?;
    match wait.status {
        ElementWaitStatus::Matched if wait.predicate_satisfied => Ok(wait),
        ElementWaitStatus::TimedOut => Err(SemanticActionFailure::PostconditionFailed),
        ElementWaitStatus::ResyncRequired => Err(SemanticActionFailure::PlaneAfter(
            AccessibilityPlaneError::ResyncRequired {
                current_generation: None,
            },
        )),
        _ => Err(SemanticActionFailure::InvalidEvidence),
    }
}

fn postcondition_expansion(predicate: &ElementWaitPredicate) -> ElementSnapshotExpansion {
    let mut expansion = ElementSnapshotExpansion::default();
    match predicate {
        ElementWaitPredicate::Value { .. } => expansion.value = true,
        ElementWaitPredicate::Text { .. } => {
            expansion.text_metadata = true;
            expansion.text_content = true;
        }
        ElementWaitPredicate::Geometry { .. } => expansion.component = true,
        _ => {}
    }
    expansion
}

struct SemanticActionPlan {
    command: Command,
}

impl SemanticActionPlan {
    fn new(command: Command) -> Option<Self> {
        matches!(
            command,
            Command::ElementInvoke(_)
                | Command::ElementFocus(_)
                | Command::ElementSetValue(_)
                | Command::ElementSelection(_)
                | Command::ElementSetText(_)
                | Command::ElementInsertText(_)
                | Command::ElementScroll(_)
        )
        .then_some(Self { command })
    }

    fn element(&self) -> &ElementRef {
        match &self.command {
            Command::ElementInvoke(value) => &value.element,
            Command::ElementFocus(value) => &value.element,
            Command::ElementSetValue(value) => &value.element,
            Command::ElementSelection(value) => &value.element,
            Command::ElementSetText(value) => &value.element,
            Command::ElementInsertText(value) => &value.element,
            Command::ElementScroll(value) => &value.element,
            _ => unreachable!("semantic plan excludes non-semantic commands"),
        }
    }

    fn postcondition(&self) -> Option<&ElementPostcondition> {
        match &self.command {
            Command::ElementInvoke(value) => value.postcondition.as_ref(),
            Command::ElementFocus(value) => value.postcondition.as_ref(),
            Command::ElementSetValue(value) => value.postcondition.as_ref(),
            Command::ElementSelection(value) => value.postcondition.as_ref(),
            Command::ElementSetText(value) => value.postcondition.as_ref(),
            Command::ElementInsertText(value) => value.postcondition.as_ref(),
            Command::ElementScroll(value) => value.postcondition.as_ref(),
            _ => None,
        }
    }

    const fn kind(&self) -> ElementActionOperation {
        match self.command {
            Command::ElementInvoke(_) => ElementActionOperation::Invoke,
            Command::ElementFocus(_) => ElementActionOperation::Focus,
            Command::ElementSetValue(_) => ElementActionOperation::SetValue,
            Command::ElementSelection(_) => ElementActionOperation::Selection,
            Command::ElementSetText(_) => ElementActionOperation::SetText,
            Command::ElementInsertText(_) => ElementActionOperation::InsertText,
            Command::ElementScroll(_) => ElementActionOperation::Scroll,
            _ => unreachable!(),
        }
    }

    fn expansion(&self) -> ElementSnapshotExpansion {
        // Preflight consumes only base cache fields (role/state/correlation/child count).
        // Optional live fields come from the actor's exact observation/readback; asking
        // the bootstrap mirror for them would incorrectly reject otherwise valid targets.
        minimal_action_expansion()
    }

    fn needs_fresh_observation(&self) -> bool {
        matches!(
            self.command,
            Command::ElementFocus(_) | Command::ElementSetValue(_) | Command::ElementSelection(_)
        )
    }

    fn validate_fresh_observation(
        &self,
        observed: &xenoteer_atspi::SemanticObservationEvidence,
    ) -> Result<(), SemanticActionFailure> {
        match self.command {
            Command::ElementSetValue(_) if observed.value.is_none() => {
                Err(SemanticActionFailure::VerificationUnsupported)
            }
            Command::ElementSelection(_) if observed.selected_children.is_none() => {
                Err(SemanticActionFailure::VerificationUnsupported)
            }
            _ => Ok(()),
        }
    }

    fn validate_preflight(&self, snapshot: &ElementSnapshot) -> Result<(), SemanticActionFailure> {
        match &self.command {
            Command::ElementInvoke(command)
                if !command.allow_disabled
                    && (!snapshot.states.contains(&ElementState::Enabled)
                        || !snapshot.states.contains(&ElementState::Sensitive)) =>
            {
                Err(SemanticActionFailure::Disabled)
            }
            Command::ElementFocus(command)
                if command.require_window_focus_correlation
                    && !correlation_authorizes_physical_effect(&snapshot.window_correlation) =>
            {
                Err(SemanticActionFailure::WeakWindowCorrelation)
            }
            Command::ElementSelection(command)
                if command.operation == ElementSelectionOperation::SelectAll
                    && snapshot.child_count.is_none() =>
            {
                Err(SemanticActionFailure::VerificationUnsupported)
            }
            Command::ElementSetText(command) => {
                validate_text_verification(snapshot, command.verify_length_only)
            }
            Command::ElementInsertText(command) => {
                validate_text_verification(snapshot, command.verify_length_only)
            }
            _ => Ok(()),
        }
    }

    fn operation(&self) -> Result<SemanticOperation, SemanticActionFailure> {
        let operation = match &self.command {
            Command::ElementInvoke(command) => SemanticOperation::Invoke(match &command.action {
                ElementActionTarget::Name { name } => ActionSelector::Name(name.clone()),
                ElementActionTarget::Index { index } => ActionSelector::Index(u32::from(*index)),
                ElementActionTarget::Default => ActionSelector::Default,
            }),
            Command::ElementFocus(_) => SemanticOperation::Focus,
            Command::ElementSetValue(command) => SemanticOperation::SetValue(command.value),
            Command::ElementSelection(command) => {
                SemanticOperation::Selection(match command.operation {
                    ElementSelectionOperation::SelectChild { index } => {
                        SelectionOperation::SelectChild(index)
                    }
                    ElementSelectionOperation::DeselectChild { index } => {
                        SelectionOperation::DeselectChild(index)
                    }
                    ElementSelectionOperation::SelectAll => SelectionOperation::SelectAll,
                    ElementSelectionOperation::Clear => SelectionOperation::Clear,
                })
            }
            Command::ElementSetText(command) => SemanticOperation::SetText {
                text: RedactedText::new(command.text.expose().to_owned())
                    .map_err(SemanticActionFailure::Actor)?,
                selection: text_selection(command.selection),
            },
            Command::ElementInsertText(command) => SemanticOperation::InsertText {
                position: TextInsertPosition::Offset(
                    u32::try_from(command.offset)
                        .map_err(|_| SemanticActionFailure::InvalidEvidence)?,
                ),
                text: RedactedText::new(command.text.expose().to_owned())
                    .map_err(SemanticActionFailure::Actor)?,
                selection: text_selection(command.selection),
            },
            Command::ElementScroll(command) => match command.target {
                ElementScrollTarget::Alignment { alignment } => {
                    SemanticOperation::Scroll(match alignment {
                        ElementScrollAlignment::TopLeft => ScrollPlacement::TopLeft,
                        ElementScrollAlignment::BottomRight => ScrollPlacement::BottomRight,
                        ElementScrollAlignment::TopEdge => ScrollPlacement::TopEdge,
                        ElementScrollAlignment::BottomEdge => ScrollPlacement::BottomEdge,
                        ElementScrollAlignment::LeftEdge => ScrollPlacement::LeftEdge,
                        ElementScrollAlignment::RightEdge => ScrollPlacement::RightEdge,
                        ElementScrollAlignment::Anywhere => ScrollPlacement::Anywhere,
                    })
                }
                ElementScrollTarget::ScreenPoint { point } => SemanticOperation::ScrollToPoint {
                    x: point.x(),
                    y: point.y(),
                },
            },
            _ => return Err(SemanticActionFailure::InvalidEvidence),
        };
        operation.validate().map_err(SemanticActionFailure::Actor)?;
        Ok(operation)
    }

    fn verify(
        &self,
        before: &ElementSnapshot,
        evidence: SemanticEvidence,
    ) -> Result<ElementActionEvidence, SemanticActionFailure> {
        let mut result = ElementActionEvidence {
            resolved_action_name: None,
            resolved_action_index: None,
            backend_accepted: true,
            observed_state: None,
            observed_value: None,
            observed_selection_count: None,
            observed_text_length: None,
            protected_text_verified_by_length_only: false,
            extents_before: None,
            extents_after: None,
            postcondition_satisfied: None,
            poll_fallback_used: false,
        };
        match (&self.command, evidence) {
            (
                Command::ElementInvoke(_),
                SemanticEvidence::Action {
                    accepted,
                    invoked_index,
                    actions,
                },
            ) => {
                require_accepted(accepted)?;
                let action = actions
                    .iter()
                    .find(|action| action.index == invoked_index)
                    .ok_or(SemanticActionFailure::InvalidEvidence)?;
                result.resolved_action_name = Some(action.name.clone());
                result.resolved_action_index = Some(
                    u16::try_from(invoked_index)
                        .map_err(|_| SemanticActionFailure::InvalidEvidence)?,
                );
            }
            (Command::ElementFocus(_), SemanticEvidence::Focus { accepted, focused }) => {
                require_accepted(accepted)?;
                if !focused {
                    return Err(SemanticActionFailure::PostconditionFailed);
                }
                result.observed_state = Some(ElementState::Focused);
            }
            (Command::ElementSetValue(command), SemanticEvidence::Value { current, .. }) => {
                if !current.is_finite()
                    || (current - command.value).abs() > command.tolerance.unwrap_or(0.0)
                {
                    return Err(SemanticActionFailure::PostconditionFailed);
                }
                result.observed_value = Some(current);
            }
            (
                Command::ElementSelection(command),
                SemanticEvidence::Selection {
                    accepted,
                    selected_children,
                    addressed_child_selected,
                },
            ) => {
                require_accepted(accepted)?;
                let verified = match command.operation {
                    ElementSelectionOperation::SelectChild { .. } => {
                        addressed_child_selected == Some(true)
                    }
                    ElementSelectionOperation::DeselectChild { .. } => {
                        addressed_child_selected == Some(false)
                    }
                    ElementSelectionOperation::Clear => selected_children == 0,
                    ElementSelectionOperation::SelectAll => {
                        let child_count = before
                            .child_count
                            .ok_or(SemanticActionFailure::VerificationUnsupported)?;
                        child_count == selected_children
                    }
                };
                if !verified {
                    return Err(SemanticActionFailure::PostconditionFailed);
                }
                result.observed_selection_count = Some(selected_children);
            }
            (
                Command::ElementSetText(command),
                SemanticEvidence::Text {
                    accepted,
                    before: _,
                    after,
                },
            ) => {
                require_accepted(accepted)?;
                let requested = text_character_count(command.text.expose())?;
                if after.character_count != requested {
                    return Err(SemanticActionFailure::PostconditionFailed);
                }
                result.observed_text_length = Some(after.character_count);
                result.protected_text_verified_by_length_only =
                    before.is_protected() && command.verify_length_only;
            }
            (
                Command::ElementInsertText(command),
                SemanticEvidence::Text {
                    accepted,
                    before: text_before,
                    after,
                },
            ) => {
                require_accepted(accepted)?;
                let inserted = text_character_count(command.text.expose())?;
                let expected = text_before
                    .character_count
                    .checked_add(inserted)
                    .ok_or(SemanticActionFailure::InvalidEvidence)?;
                if u32::try_from(command.offset).ok() > Some(text_before.character_count)
                    || after.character_count != expected
                {
                    return Err(SemanticActionFailure::PostconditionFailed);
                }
                result.observed_text_length = Some(after.character_count);
                result.protected_text_verified_by_length_only =
                    before.is_protected() && command.verify_length_only;
            }
            (
                Command::ElementScroll(_),
                SemanticEvidence::Scroll {
                    accepted,
                    before,
                    after,
                },
            ) => {
                require_accepted(accepted)?;
                result.extents_before = Some(protocol_rect(before)?);
                result.extents_after = Some(protocol_rect(after)?);
            }
            _ => return Err(SemanticActionFailure::InvalidEvidence),
        }
        Ok(result)
    }
}

const fn minimal_action_expansion() -> ElementSnapshotExpansion {
    ElementSnapshotExpansion {
        actions: false,
        value: false,
        text_metadata: false,
        text_content: false,
        attributes: false,
        relations: false,
        component: false,
    }
}

fn validate_text_verification(
    snapshot: &ElementSnapshot,
    verify_length_only: bool,
) -> Result<(), SemanticActionFailure> {
    if !verify_length_only || snapshot.role.role == xenoteer_protocol::ElementRole::Unknown {
        return Err(SemanticActionFailure::VerificationUnsupported);
    }
    Ok(())
}

const fn text_selection(value: EditableTextSelectionPolicy) -> TextSelectionPolicy {
    match value {
        EditableTextSelectionPolicy::Preserve => TextSelectionPolicy::Preserve,
        EditableTextSelectionPolicy::CollapseBefore => TextSelectionPolicy::CollapseBefore,
        EditableTextSelectionPolicy::CollapseAfter => TextSelectionPolicy::CollapseAfter,
        EditableTextSelectionPolicy::SelectInserted => TextSelectionPolicy::SelectInserted,
    }
}

fn require_accepted(accepted: bool) -> Result<(), SemanticActionFailure> {
    accepted
        .then_some(())
        .ok_or(SemanticActionFailure::BackendRejected)
}

fn text_character_count(text: &str) -> Result<u32, SemanticActionFailure> {
    u32::try_from(text.chars().count()).map_err(|_| SemanticActionFailure::InvalidEvidence)
}

fn protocol_rect(value: xenoteer_atspi::SemanticRect) -> Result<Rect, SemanticActionFailure> {
    let width = u32::try_from(value.width).map_err(|_| SemanticActionFailure::InvalidEvidence)?;
    let height = u32::try_from(value.height).map_err(|_| SemanticActionFailure::InvalidEvidence)?;
    Rect::new(value.x, value.y, width, height).map_err(|_| SemanticActionFailure::InvalidEvidence)
}
