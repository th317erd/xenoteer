//! Property gates for coordinator fencing, leases, idempotency, and replay.

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use xenoteer_core::coordinator::{
    CanonicalCommandHash, CommandLedger, CommandLedgerError, CommandLedgerLimits, EventHub,
    EventHubLimits, GenerationFence, IdempotencyDecision, LeaseMachine, LeasePhase, LeasePolicy,
    LeaseSnapshot, MonotonicMillis, PrincipalId, ReplayResult,
};
use xenoteer_protocol::{CommandId, ControlLeaseId, DesktopGeneration, DesktopId};

fn case_error(error: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(error.to_string())
}

fn principal(value: &str) -> Result<PrincipalId, TestCaseError> {
    PrincipalId::new(value).map_err(case_error)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn replay_retention_is_always_bounded_and_contiguous(
        maximum_events in 1usize..=24,
        maximum_bytes in 1usize..=512,
        encoded_sizes in prop::collection::vec(1usize..=1_024, 0..100),
    ) {
        let desktop_id = DesktopId::new();
        let generation =
            GenerationFence::new(desktop_id, DesktopGeneration::new()).capture();
        let limits = EventHubLimits::new(maximum_events, maximum_bytes).map_err(case_error)?;
        let mut hub = EventHub::new(desktop_id, generation, limits).map_err(case_error)?;

        for (event, encoded_size) in encoded_sizes.into_iter().enumerate() {
            hub.publish(event, encoded_size, generation).map_err(case_error)?;
            prop_assert!(hub.retained_events() <= maximum_events);
            prop_assert!(hub.retained_bytes() <= maximum_bytes);
        }

        let dropped_through = hub.dropped_through();
        let latest_sequence = hub.latest_sequence();
        let ReplayResult::Events { events, .. } =
            hub.replay_since(generation, dropped_through)
        else {
            return Err(TestCaseError::fail(
                "replay from the dropped-through watermark must be complete",
            ));
        };
        let sequences: Vec<u64> = events.iter().map(|record| record.sequence).collect();
        let expected: Vec<u64> = ((dropped_through + 1)..=latest_sequence).collect();
        prop_assert_eq!(sequences, expected);
        prop_assert_eq!(
            events.iter().map(|record| record.encoded_size).sum::<usize>(),
            hub.retained_bytes()
        );
    }

    #[test]
    fn admitted_command_id_never_executes_twice_while_retained(
        capacity in 1usize..=16,
        operations in prop::collection::vec((0u8..=3, 0u8..=31), 1..200),
    ) {
        let desktop_id = DesktopId::new();
        let generation =
            GenerationFence::new(desktop_id, DesktopGeneration::new()).capture();
        let limits = CommandLedgerLimits::new(capacity, 1_000_000).map_err(case_error)?;
        let mut ledger = CommandLedger::<u8>::new(desktop_id, generation, limits)
            .map_err(case_error)?;
        let principal = principal("property-agent")?;
        let command_ids: Vec<CommandId> = (0..32).map(|_| CommandId::new()).collect();
        let mut now = 0u64;

        for (operation, index) in operations {
            now += 1;
            let command_id = command_ids[usize::from(index)];
            let hash = CanonicalCommandHash::new([index; 32]);
            match operation {
                0 => {
                    let decision = ledger.admit(
                        principal.clone(),
                        command_id,
                        hash,
                        MonotonicMillis::new(now),
                        generation,
                    );
                    if matches!(decision, Ok(IdempotencyDecision::Admitted(_))) {
                        now += 1;
                        let duplicate = ledger.admit(
                            principal.clone(),
                            command_id,
                            hash,
                            MonotonicMillis::new(now),
                            generation,
                        ).map_err(case_error)?;
                        prop_assert!(matches!(duplicate, IdempotencyDecision::Existing(_)));
                    }
                }
                1 => {
                    let _ = ledger.mark_running(
                        &principal,
                        command_id,
                        MonotonicMillis::new(now),
                        generation,
                    );
                }
                2 => {
                    let completed = ledger.complete(
                        &principal,
                        command_id,
                        index,
                        MonotonicMillis::new(now),
                        generation,
                    );
                    if completed.is_ok() {
                        now += 1;
                        prop_assert_eq!(
                            ledger.complete(
                                &principal,
                                command_id,
                                index.wrapping_add(1),
                                MonotonicMillis::new(now),
                                generation,
                            ),
                            Err(CommandLedgerError::TerminalImmutable)
                        );
                    }
                }
                _ => {
                    let _ = ledger.lookup(
                        &principal,
                        command_id,
                        MonotonicMillis::new(now),
                        generation,
                    );
                }
            }
            prop_assert!(ledger.len() <= capacity);
        }
    }

    #[test]
    fn lease_never_reopens_before_reset_and_stale_generations_never_authorize(
        actions in prop::collection::vec((0u8..=8, 0u16..=2_000), 1..200),
    ) {
        let desktop_id = DesktopId::new();
        let mut fence = GenerationFence::new(desktop_id, DesktopGeneration::new());
        let mut generation = fence.capture();
        let mut leases = LeaseMachine::new(
            desktop_id,
            generation,
            LeasePolicy::new(1_000, 2_000).map_err(case_error)?,
        ).map_err(case_error)?;
        let owner = principal("property-owner")?;
        let other = principal("property-other")?;
        let mut lease_id = ControlLeaseId::new();
        let mut now = 0u64;

        for (action, advance_ms) in actions {
            now = now.saturating_add(u64::from(advance_ms));
            leases.advance_time(MonotonicMillis::new(now)).map_err(case_error)?;
            let before = leases.phase();
            match action {
                0 => {
                    if before == LeasePhase::Vacant {
                        lease_id = ControlLeaseId::new();
                    }
                    let _ = leases.acquire(
                        owner.clone(),
                        lease_id,
                        None,
                        MonotonicMillis::new(now),
                        generation,
                    );
                }
                1 => {
                    let _ = leases.authorize(
                        &owner,
                        lease_id,
                        MonotonicMillis::new(now),
                        generation,
                    );
                }
                2 => {
                    let _ = leases.renew(
                        &owner,
                        lease_id,
                        Some(1_500),
                        MonotonicMillis::new(now),
                        generation,
                    );
                }
                3 => {
                    let _ = leases.release(
                        &owner,
                        lease_id,
                        MonotonicMillis::new(now),
                        generation,
                    );
                }
                4 => {
                    let _ = leases.advance_time(MonotonicMillis::new(now));
                }
                5 => {
                    let _ = leases.begin_reset();
                }
                6 => {
                    let _ = leases.finish_reset();
                }
                7 => {
                    let stale = generation;
                    generation = fence.rotate(DesktopGeneration::new()).map_err(case_error)?;
                    leases.rotate_generation(generation).map_err(case_error)?;
                    prop_assert!(leases.authorize(
                        &owner,
                        lease_id,
                        MonotonicMillis::new(now),
                        stale,
                    ).is_err());
                }
                _ => {
                    let _ = leases.authorize(
                        &other,
                        lease_id,
                        MonotonicMillis::new(now),
                        generation,
                    );
                }
            }

            if matches!(before, LeasePhase::Revoking | LeasePhase::Resetting)
                && action != 5
                && action != 6
                && action != 7
            {
                prop_assert_ne!(leases.phase(), LeasePhase::Vacant);
            }
            if let LeaseSnapshot::Held(grant) = leases.snapshot() {
                prop_assert_eq!(grant.generation, generation);
                prop_assert!(MonotonicMillis::new(now) < grant.expires_at);
            }
        }
    }
}
