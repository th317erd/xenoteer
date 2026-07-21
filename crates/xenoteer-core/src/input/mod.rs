//! Pure input actions, planning, state, and cleanup.

pub mod action;
pub mod cleanup;
pub mod effect;
pub mod interpolation;
pub mod keyboard_plan;
pub mod state;

pub use action::{
    ButtonDirection, ButtonMapping, ClickAction, DEFAULT_DOUBLE_CLICK_THRESHOLD_MS, DragAction,
    InputAction, InputActionError, InputDelayKind, KeyAction, LogicalButton, MAX_CLICK_COUNT,
    MAX_INPUT_ACTION_EVENTS, MAX_PHYSICAL_KEYCODE, MAX_SCROLL_COUNT, MAX_SCROLL_INTERVAL_MS,
    MAX_XTEST_DELAY_MS, MIN_PHYSICAL_KEYCODE, MoveAction, PhysicalButton, PhysicalKey,
    ScrollAction, ScrollDirection,
};
pub use cleanup::{CleanupAction, CleanupPlan, plan_cleanup};
pub use effect::{
    Effect, EffectCertainty, EffectJournal, EffectJournalError, EffectRecord, MAX_EFFECT_RECORDS,
};
pub use interpolation::{
    MAX_MOTION_DURATION_MS, MAX_MOTION_EVENTS, MAX_SAMPLE_RATE_HZ, MAX_WAYPOINTS,
    MIN_SAMPLE_RATE_HZ, MotionCurve, MotionOptions, MotionPlan, MotionPlanError, MotionPolicy,
    MotionSample, WaypointDurationPolicy, plan_motion, plan_waypoint_motion, round_ties_away,
};
pub use keyboard_plan::{
    KeyEvent, KeyEventKind, KeyPlan, KeyPlanError, MAX_CHORD_KEYS, ResolvedKey,
};
pub use state::{
    ActionPurpose, HealthEvent, InputHealth, InputState, InputStateError, OwnedKey, PoisonReason,
    ResetReason,
};
