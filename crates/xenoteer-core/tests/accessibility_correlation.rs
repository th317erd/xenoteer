//! Adversarial proofs for bounded AT-SPI-to-X11 correlation.

use xenoteer_core::{
    AccessibilityCorrelationError, AccessibilityCorrelationLimits, AccessibilityCorrelationSubject,
    AccessibilityWindowCandidate, MonotonicMillis, NormalizedCorrelationText,
    correlate_accessibility_window, correlation_authorizes_physical_effect,
};
use xenoteer_protocol::{
    AccessibilityIdentityHash, ApplicationRef, AtspiBusName, AtspiGeneration, AtspiObjectPath,
    DesktopGeneration, DesktopId, ElementRef, Rect, WindowCorrelationConfidence,
    WindowCorrelationSignal, WindowIdentityHash, WindowRef,
};

struct Fixture {
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    application: ApplicationRef,
    element: ElementRef,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let atspi_generation = AtspiGeneration::new(1)?;
        let application = ApplicationRef {
            desktop_id,
            desktop_generation: generation,
            atspi_generation,
            unique_bus_name: AtspiBusName::new(":1.42")?,
            root_object_path: AtspiObjectPath::new("/org/example/App")?,
            app_instance_generation: 1,
            identity_hash: AccessibilityIdentityHash::new("a".repeat(64))?,
        };
        let element = ElementRef {
            desktop_id,
            desktop_generation: generation,
            atspi_generation,
            application: application.clone(),
            object_path: AtspiObjectPath::new("/org/example/App/root")?,
            object_identity_hash: AccessibilityIdentityHash::new("b".repeat(64))?,
            cache_sequence: 1,
        };
        Ok(Self {
            desktop_id,
            generation,
            application,
            element,
        })
    }

    fn window(&self, xid: u32, identity: char) -> Result<WindowRef, Box<dyn std::error::Error>> {
        Ok(WindowRef {
            desktop_id: self.desktop_id,
            desktop_generation: self.generation,
            xid,
            observed_generation: 1,
            identity_hash: WindowIdentityHash::new(identity.to_string().repeat(64))?,
        })
    }

    fn subject(&self) -> AccessibilityCorrelationSubject {
        AccessibilityCorrelationSubject {
            application: self.application.clone(),
            element: self.element.clone(),
            process_id: None,
            managed_process_id: None,
            top_level_extents: None,
            title: None,
            application_identity: None,
            toolkit_identity: None,
            focused: false,
            focus_changed_at: None,
            created_at: Some(MonotonicMillis::new(900)),
            observed_at: MonotonicMillis::new(1_000),
            explicit_window: None,
            client_leader: None,
        }
    }

    fn candidate(&self, window: WindowRef) -> AccessibilityWindowCandidate {
        AccessibilityWindowCandidate {
            window,
            live: true,
            process_id: None,
            managed_process_id: None,
            top_level_extents: None,
            title: None,
            application_identity: None,
            toolkit_identity: None,
            focused: false,
            focus_changed_at: None,
            created_at: Some(MonotonicMillis::new(900)),
            observed_at: MonotonicMillis::new(1_000),
            client_leader: None,
        }
    }
}

fn text(value: &str) -> Result<NormalizedCorrelationText, AccessibilityCorrelationError> {
    NormalizedCorrelationText::new(value)
}

fn correlate(
    subject: &AccessibilityCorrelationSubject,
    candidates: &[AccessibilityWindowCandidate],
) -> Result<xenoteer_protocol::ElementWindowCorrelation, AccessibilityCorrelationError> {
    correlate_accessibility_window(
        subject,
        candidates,
        MonotonicMillis::new(1_100),
        AccessibilityCorrelationLimits::default(),
    )
}

#[test]
fn title_alone_is_weak_and_never_authorizes_physical_effects()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let window = fixture.window(10, 'c')?;
    let mut subject = fixture.subject();
    subject.title = Some(text("  Login\tPORTAL  ")?);
    let mut candidate = fixture.candidate(window.clone());
    candidate.title = Some(text("login portal")?);

    let result = correlate(&subject, &[candidate])?;
    assert_eq!(result.window, Some(window));
    assert_eq!(result.confidence, WindowCorrelationConfidence::Weak);
    assert!(!result.conflicting_evidence);
    assert!(!correlation_authorizes_physical_effect(&result));
    assert_eq!(result.evidence[0].signal, WindowCorrelationSignal::Title);
    assert!(result.evidence[0].matched);
    Ok(())
}

#[test]
fn same_title_attacker_ties_instead_of_winning_by_input_order()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut subject = fixture.subject();
    subject.title = Some(text("Authorize payment")?);
    let mut first = fixture.candidate(fixture.window(20, 'c')?);
    first.title = subject.title.clone();
    let mut attacker = fixture.candidate(fixture.window(21, 'd')?);
    attacker.title = subject.title.clone();

    for candidates in [vec![first.clone(), attacker.clone()], vec![attacker, first]] {
        let result = correlate(&subject, &candidates)?;
        assert!(result.window.is_none());
        assert_eq!(result.confidence, WindowCorrelationConfidence::None);
        assert!(result.conflicting_evidence);
        assert!(!correlation_authorizes_physical_effect(&result));
    }
    Ok(())
}

#[test]
fn unique_raw_pid_labels_exact_process_but_cannot_authorize_without_corroboration()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let selected = fixture.window(30, 'c')?;
    let mut subject = fixture.subject();
    subject.process_id = Some(4_242);
    let mut matching = fixture.candidate(selected.clone());
    matching.process_id = Some(4_242);
    let mut other = fixture.candidate(fixture.window(31, 'd')?);
    other.process_id = Some(9_999);

    let result = correlate(&subject, &[other, matching])?;
    assert_eq!(result.window, Some(selected));
    assert_eq!(result.confidence, WindowCorrelationConfidence::ExactProcess);
    assert!(!correlation_authorizes_physical_effect(&result));
    Ok(())
}

#[test]
fn unique_raw_pid_plus_weak_geometry_overlap_still_cannot_authorize()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let selected = fixture.window(35, 'c')?;
    let mut subject = fixture.subject();
    subject.process_id = Some(4_242);
    subject.top_level_extents = Some(Rect::new(0, 0, 100, 100)?);
    let mut matching = fixture.candidate(selected.clone());
    matching.process_id = Some(4_242);
    matching.top_level_extents = Some(Rect::new(50, 0, 100, 100)?);
    let mut other = fixture.candidate(fixture.window(36, 'd')?);
    other.process_id = Some(9_999);

    let result = correlate(&subject, &[matching, other])?;
    assert_eq!(result.window, Some(selected));
    assert_eq!(result.confidence, WindowCorrelationConfidence::ExactProcess);
    assert!(!correlation_authorizes_physical_effect(&result));
    assert!(result.evidence.iter().any(|evidence| {
        evidence.signal == WindowCorrelationSignal::TopLevelExtents
            && !evidence.matched
            && evidence
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("weakly"))
    }));
    Ok(())
}

#[test]
fn shared_browser_pid_needs_non_title_discriminators_for_strong_confidence()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let selected = fixture.window(40, 'c')?;
    let mut subject = fixture.subject();
    subject.process_id = Some(7_000);
    subject.title = Some(text("Editor")?);
    let mut first = fixture.candidate(selected.clone());
    first.process_id = Some(7_000);
    first.title = subject.title.clone();
    let mut second = fixture.candidate(fixture.window(41, 'd')?);
    second.process_id = Some(7_000);
    second.title = Some(text("DevTools")?);

    let weak = correlate(&subject, &[first.clone(), second.clone()])?;
    assert_eq!(weak.window, Some(selected.clone()));
    assert_eq!(weak.confidence, WindowCorrelationConfidence::Weak);
    assert!(!correlation_authorizes_physical_effect(&weak));

    let extents = Rect::new(10, 20, 800, 600)?;
    subject.top_level_extents = Some(extents);
    subject.application_identity = Some(text("org.example.Editor")?);
    first.top_level_extents = Some(extents);
    first.application_identity = subject.application_identity.clone();
    second.top_level_extents = Some(Rect::new(1_000, 20, 800, 600)?);
    second.application_identity = Some(text("org.example.DevTools")?);
    let strong = correlate(&subject, &[second, first])?;
    assert_eq!(strong.window, Some(selected));
    assert_eq!(strong.confidence, WindowCorrelationConfidence::Strong);
    assert!(correlation_authorizes_physical_effect(&strong));
    Ok(())
}

#[test]
fn child_pid_mismatch_inside_verified_managed_group_is_not_a_conflict()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let selected = fixture.window(50, 'c')?;
    let mut subject = fixture.subject();
    subject.process_id = Some(10_001);
    subject.managed_process_id = Some(500);
    subject.top_level_extents = Some(Rect::new(0, 0, 640, 480)?);
    let mut candidate = fixture.candidate(selected.clone());
    candidate.process_id = Some(10_002);
    candidate.managed_process_id = Some(500);
    candidate.top_level_extents = subject.top_level_extents;

    let result = correlate(&subject, &[candidate])?;
    assert_eq!(result.window, Some(selected));
    assert_eq!(result.confidence, WindowCorrelationConfidence::Strong);
    assert!(!result.conflicting_evidence);
    assert_ne!(result.confidence, WindowCorrelationConfidence::ExactProcess);
    assert!(correlation_authorizes_physical_effect(&result));
    assert!(result.evidence.iter().any(|evidence| evidence.signal
        == WindowCorrelationSignal::ProcessId
        && !evidence.matched));
    Ok(())
}

#[test]
fn shared_managed_group_never_becomes_exact_process() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let selected = fixture.window(60, 'c')?;
    let mut subject = fixture.subject();
    subject.managed_process_id = Some(900);
    subject.top_level_extents = Some(Rect::new(0, 0, 400, 300)?);
    subject.application_identity = Some(text("browser-main")?);
    let mut first = fixture.candidate(selected.clone());
    first.managed_process_id = Some(900);
    first.top_level_extents = subject.top_level_extents;
    first.application_identity = subject.application_identity.clone();
    let mut second = fixture.candidate(fixture.window(61, 'd')?);
    second.managed_process_id = Some(900);
    second.top_level_extents = Some(Rect::new(500, 0, 400, 300)?);
    second.application_identity = Some(text("browser-popup")?);

    let result = correlate(&subject, &[first, second])?;
    assert_eq!(result.window, Some(selected));
    assert_eq!(result.confidence, WindowCorrelationConfidence::Strong);
    assert_ne!(result.confidence, WindowCorrelationConfidence::ExactProcess);
    Ok(())
}

#[test]
fn equal_shared_pid_geometry_and_identity_never_break_ties_by_xid()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let extents = Rect::new(20, 20, 500, 500)?;
    let mut subject = fixture.subject();
    subject.process_id = Some(123);
    subject.top_level_extents = Some(extents);
    subject.application_identity = Some(text("same-app")?);
    let mut first = fixture.candidate(fixture.window(70, 'c')?);
    first.process_id = Some(123);
    first.top_level_extents = Some(extents);
    first.application_identity = subject.application_identity.clone();
    let mut second = fixture.candidate(fixture.window(71, 'd')?);
    second.process_id = Some(123);
    second.top_level_extents = Some(extents);
    second.application_identity = subject.application_identity.clone();

    let result = correlate(&subject, &[second, first])?;
    assert!(result.window.is_none());
    assert!(result.conflicting_evidence);
    assert_eq!(result.confidence, WindowCorrelationConfidence::None);
    Ok(())
}

#[test]
fn fresh_focus_transition_beats_stale_focus_in_shared_process()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let selected = fixture.window(80, 'c')?;
    let mut subject = fixture.subject();
    subject.process_id = Some(321);
    subject.focused = true;
    subject.focus_changed_at = Some(MonotonicMillis::new(990));
    subject.application_identity = Some(text("browser")?);
    let mut fresh = fixture.candidate(selected.clone());
    fresh.process_id = Some(321);
    fresh.focused = true;
    fresh.focus_changed_at = Some(MonotonicMillis::new(995));
    fresh.application_identity = subject.application_identity.clone();
    let mut stale = fixture.candidate(fixture.window(81, 'd')?);
    stale.process_id = Some(321);
    stale.focused = true;
    stale.focus_changed_at = Some(MonotonicMillis::new(100));
    stale.application_identity = subject.application_identity.clone();

    let result = correlate(&subject, &[stale, fresh])?;
    assert_eq!(result.window, Some(selected));
    assert_eq!(result.confidence, WindowCorrelationConfidence::Strong);
    assert!(correlation_authorizes_physical_effect(&result));
    Ok(())
}

#[test]
fn contradictory_exact_ref_and_unique_pid_downgrade_and_flag()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let pid_window = fixture.window(90, 'c')?;
    let explicit_window = fixture.window(91, 'd')?;
    let mut subject = fixture.subject();
    subject.process_id = Some(55);
    subject.explicit_window = Some(explicit_window.clone());
    let mut pid_candidate = fixture.candidate(pid_window);
    pid_candidate.process_id = Some(55);
    let mut explicit_candidate = fixture.candidate(explicit_window.clone());
    explicit_candidate.process_id = Some(56);

    let result = correlate(&subject, &[pid_candidate, explicit_candidate])?;
    assert_eq!(result.window, Some(explicit_window));
    assert_eq!(result.confidence, WindowCorrelationConfidence::Weak);
    assert!(result.conflicting_evidence);
    assert!(!correlation_authorizes_physical_effect(&result));
    Ok(())
}

#[test]
fn extreme_valid_rectangles_do_not_overflow_and_order_is_deterministic()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let selected = fixture.window(100, 'c')?;
    let extreme = Rect::new(i32::MIN, i32::MIN, u32::MAX, u32::MAX)?;
    let mut subject = fixture.subject();
    subject.top_level_extents = Some(extreme);
    subject.application_identity = Some(text("extreme")?);
    subject.focused = true;
    subject.focus_changed_at = Some(MonotonicMillis::new(1_000));
    let mut matching = fixture.candidate(selected.clone());
    matching.top_level_extents = Some(extreme);
    matching.application_identity = subject.application_identity.clone();
    matching.focused = true;
    matching.focus_changed_at = subject.focus_changed_at;
    let mut other = fixture.candidate(fixture.window(101, 'd')?);
    other.top_level_extents = Some(Rect::new(i32::MAX, i32::MAX, 1, 1)?);
    other.application_identity = Some(text("other")?);

    let forward = correlate(&subject, &[matching.clone(), other.clone()])?;
    let reverse = correlate(&subject, &[other, matching])?;
    assert_eq!(forward, reverse);
    assert_eq!(forward.window, Some(selected));
    assert_eq!(forward.confidence, WindowCorrelationConfidence::Strong);
    Ok(())
}

#[test]
fn stale_generations_identity_mismatch_and_stale_live_observations_fail_closed()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let subject = fixture.subject();
    let mut stale_generation = fixture.candidate(fixture.window(110, 'c')?);
    stale_generation.window.desktop_generation = DesktopGeneration::new();
    assert_eq!(
        correlate(&subject, &[stale_generation]),
        Err(AccessibilityCorrelationError::ReferenceScope)
    );

    let mut mismatched_subject = fixture.subject();
    mismatched_subject.application.identity_hash = AccessibilityIdentityHash::new("f".repeat(64))?;
    assert_eq!(
        correlate(&mismatched_subject, &[]),
        Err(AccessibilityCorrelationError::ReferenceScope)
    );

    let mut stale = fixture.candidate(fixture.window(111, 'd')?);
    stale.observed_at = MonotonicMillis::new(0);
    stale.created_at = None;
    assert_eq!(
        correlate_accessibility_window(
            &subject,
            &[stale],
            MonotonicMillis::new(7_000),
            AccessibilityCorrelationLimits::default(),
        ),
        Err(AccessibilityCorrelationError::StaleObservation)
    );
    Ok(())
}

#[test]
fn candidate_string_and_live_birth_budgets_are_hard() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let subject = fixture.subject();
    let first = fixture.candidate(fixture.window(120, 'c')?);
    let second = fixture.candidate(fixture.window(121, 'd')?);
    let limits = AccessibilityCorrelationLimits {
        max_candidates: 1,
        ..AccessibilityCorrelationLimits::default()
    };
    assert_eq!(
        correlate_accessibility_window(
            &subject,
            &[first.clone(), second],
            MonotonicMillis::new(1_100),
            limits,
        ),
        Err(AccessibilityCorrelationError::CandidateLimit)
    );

    let mut string_subject = fixture.subject();
    string_subject.title = Some(text("two")?);
    let string_limits = AccessibilityCorrelationLimits {
        max_total_string_bytes: 1,
        ..AccessibilityCorrelationLimits::default()
    };
    assert_eq!(
        correlate_accessibility_window(
            &string_subject,
            &[],
            MonotonicMillis::new(1_100),
            string_limits,
        ),
        Err(AccessibilityCorrelationError::StringLimit)
    );
    assert_eq!(
        NormalizedCorrelationText::new("x".repeat(1_025)),
        Err(AccessibilityCorrelationError::StringLimit)
    );

    let mut conflicting_birth = first;
    conflicting_birth.window.observed_generation = 2;
    conflicting_birth.window.identity_hash = WindowIdentityHash::new("e".repeat(64))?;
    assert_eq!(
        correlate(
            &subject,
            &[
                fixture.candidate(fixture.window(120, 'c')?),
                conflicting_birth
            ]
        ),
        Err(AccessibilityCorrelationError::ConflictingWindowBirth)
    );
    Ok(())
}
