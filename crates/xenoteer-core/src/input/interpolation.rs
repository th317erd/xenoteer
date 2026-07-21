//! Deterministic, exactly timed pointer-motion planning.

use thiserror::Error;

use crate::config::InputConfig;
use crate::domain::RootPoint;

/// Maximum duration of one complete pointer action.
pub const MAX_MOTION_DURATION_MS: u32 = 10_000;

/// Maximum number of emitted XTEST motion events in one action.
pub const MAX_MOTION_EVENTS: usize = 4_096;

/// Maximum number of caller-supplied waypoints.
pub const MAX_WAYPOINTS: usize = 1_024;

/// Minimum accepted interpolation sample rate.
pub const MIN_SAMPLE_RATE_HZ: u16 = 1;

/// Maximum accepted interpolation sample rate.
pub const MAX_SAMPLE_RATE_HZ: u16 = 240;

const DEFAULT_SAMPLE_RATE_HZ: u16 = 60;
const DEFAULT_NOMINAL_POINTER_SPEED_PX_PER_SECOND: u32 = 1_200;
const DEFAULT_MIN_AUTOMATIC_DURATION_MS: u32 = 80;
const DEFAULT_MAX_AUTOMATIC_DURATION_MS: u32 = 650;

/// Validated automatic pointer motion policy for one desktop profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionPolicy {
    sample_rate_hz: u16,
    nominal_speed_px_per_second: u32,
    minimum_duration_ms: u32,
    maximum_duration_ms: u32,
}

impl MotionPolicy {
    /// Creates a checked automatic motion policy.
    pub fn new(
        sample_rate_hz: u16,
        nominal_speed_px_per_second: u32,
        minimum_duration_ms: u32,
        maximum_duration_ms: u32,
    ) -> Result<Self, MotionPlanError> {
        validate_sample_rate(sample_rate_hz)?;
        if nominal_speed_px_per_second == 0 {
            return Err(MotionPlanError::NominalSpeedMustBePositive);
        }
        validate_duration(minimum_duration_ms)?;
        validate_duration(maximum_duration_ms)?;
        if minimum_duration_ms > maximum_duration_ms {
            return Err(MotionPlanError::AutomaticDurationRangeInvalid {
                minimum_ms: minimum_duration_ms,
                maximum_ms: maximum_duration_ms,
            });
        }
        Ok(Self {
            sample_rate_hz,
            nominal_speed_px_per_second,
            minimum_duration_ms,
            maximum_duration_ms,
        })
    }

    /// Validates and copies the input portion of daemon configuration.
    pub fn from_input_config(config: &InputConfig) -> Result<Self, MotionPlanError> {
        Self::new(
            config.pointer_sample_rate_hz(),
            config.pointer_nominal_speed_px_s(),
            config.pointer_move_min_ms(),
            config.pointer_move_max_ms(),
        )
    }

    /// Returns the interpolation sample rate.
    #[must_use]
    pub const fn sample_rate_hz(self) -> u16 {
        self.sample_rate_hz
    }

    /// Returns nominal automatic pointer speed.
    #[must_use]
    pub const fn nominal_speed_px_per_second(self) -> u32 {
        self.nominal_speed_px_per_second
    }

    /// Returns the minimum non-zero automatic duration.
    #[must_use]
    pub const fn minimum_duration_ms(self) -> u32 {
        self.minimum_duration_ms
    }

    /// Returns the maximum automatic duration.
    #[must_use]
    pub const fn maximum_duration_ms(self) -> u32 {
        self.maximum_duration_ms
    }
}

impl Default for MotionPolicy {
    fn default() -> Self {
        Self {
            sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
            nominal_speed_px_per_second: DEFAULT_NOMINAL_POINTER_SPEED_PX_PER_SECOND,
            minimum_duration_ms: DEFAULT_MIN_AUTOMATIC_DURATION_MS,
            maximum_duration_ms: DEFAULT_MAX_AUTOMATIC_DURATION_MS,
        }
    }
}

impl TryFrom<&InputConfig> for MotionPolicy {
    type Error = MotionPlanError;

    fn try_from(config: &InputConfig) -> Result<Self, Self::Error> {
        Self::from_input_config(config)
    }
}

/// A supported pointer interpolation curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionCurve {
    /// Emit a single endpoint event without an XTEST delay.
    Instant,
    /// Interpolate at a constant velocity.
    Linear,
    /// Apply deterministic smoothstep easing.
    Smooth,
}

/// Validated settings for a single motion segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionOptions {
    curve: MotionCurve,
    duration_ms: Option<u32>,
    policy: MotionPolicy,
    emit_noop: bool,
}

impl MotionOptions {
    /// Creates validated motion options.
    ///
    /// An omitted duration selects the automatic policy for interpolated motion.
    /// Instant motion accepts only an omitted or zero duration.
    pub fn new(
        curve: MotionCurve,
        duration_ms: Option<u32>,
        policy: MotionPolicy,
        emit_noop: bool,
    ) -> Result<Self, MotionPlanError> {
        validate_sample_rate(policy.sample_rate_hz)?;
        if let Some(duration_ms) = duration_ms {
            validate_duration(duration_ms)?;
            if curve == MotionCurve::Instant && duration_ms != 0 {
                return Err(MotionPlanError::InstantDurationMustBeZero);
            }
        }
        Ok(Self {
            curve,
            duration_ms,
            policy,
            emit_noop,
        })
    }

    /// Creates default 60 Hz interpolated motion options.
    pub fn interpolated(curve: MotionCurve) -> Result<Self, MotionPlanError> {
        if curve == MotionCurve::Instant {
            return Err(MotionPlanError::ExpectedInterpolatedCurve);
        }
        Self::new(curve, None, MotionPolicy::default(), false)
    }

    /// Creates instant motion options.
    #[must_use]
    pub const fn instant(emit_noop: bool) -> Self {
        Self {
            curve: MotionCurve::Instant,
            duration_ms: None,
            policy: MotionPolicy {
                sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
                nominal_speed_px_per_second: DEFAULT_NOMINAL_POINTER_SPEED_PX_PER_SECOND,
                minimum_duration_ms: DEFAULT_MIN_AUTOMATIC_DURATION_MS,
                maximum_duration_ms: DEFAULT_MAX_AUTOMATIC_DURATION_MS,
            },
            emit_noop,
        }
    }

    /// Returns the curve.
    #[must_use]
    pub const fn curve(self) -> MotionCurve {
        self.curve
    }

    /// Returns the explicit duration, or `None` for automatic duration.
    #[must_use]
    pub const fn duration_ms(self) -> Option<u32> {
        self.duration_ms
    }

    /// Returns the sampling frequency.
    #[must_use]
    pub const fn sample_rate_hz(self) -> u16 {
        self.policy.sample_rate_hz
    }

    /// Returns the desktop motion policy.
    #[must_use]
    pub const fn policy(self) -> MotionPolicy {
        self.policy
    }

    /// Returns whether a zero-distance diagnostic event is requested.
    #[must_use]
    pub const fn emit_noop(self) -> bool {
        self.emit_noop
    }
}

/// One absolute pointer event and its server-side delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionSample {
    point: RootPoint,
    delay_ms: u32,
}

impl MotionSample {
    /// Returns the absolute endpoint of this event.
    #[must_use]
    pub const fn point(self) -> RootPoint {
        self.point
    }

    /// Returns the XTEST server-side delay attached to this event.
    #[must_use]
    pub const fn delay_ms(self) -> u32 {
        self.delay_ms
    }
}

/// A bounded, validated sequence of motion events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MotionPlan {
    start: RootPoint,
    end: RootPoint,
    duration_ms: u32,
    raw_sample_count: usize,
    samples: Vec<MotionSample>,
    segment_ranges: Vec<std::ops::Range<usize>>,
}

impl MotionPlan {
    /// Returns the starting point used for planning.
    #[must_use]
    pub const fn start(&self) -> RootPoint {
        self.start
    }

    /// Returns the final requested point.
    #[must_use]
    pub const fn end(&self) -> RootPoint {
        self.end
    }

    /// Returns the exact total XTEST delay.
    #[must_use]
    pub const fn duration_ms(&self) -> u32 {
        self.duration_ms
    }

    /// Returns the sample count before rounded duplicate compaction.
    #[must_use]
    pub const fn raw_sample_count(&self) -> usize {
        self.raw_sample_count
    }

    /// Returns the events to send in order.
    #[must_use]
    pub fn samples(&self) -> &[MotionSample] {
        &self.samples
    }

    /// Returns the normalized motion segments in execution order.
    ///
    /// Each slice is one host-controlled cancellation boundary. Consecutive
    /// duplicate waypoints are removed before these boundaries are created;
    /// an explicitly requested final no-op remains its own segment.
    pub fn segments(
        &self,
    ) -> impl ExactSizeIterator<Item = &[MotionSample]> + DoubleEndedIterator + '_ {
        self.segment_ranges
            .iter()
            .map(|range| &self.samples[range.clone()])
    }

    /// Returns the number of normalized motion segments.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segment_ranges.len()
    }

    /// Returns the emitted event count.
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.samples.len()
    }
}

/// Duration allocation for a normalized waypoint path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaypointDurationPolicy {
    /// One duration for each non-zero normalized segment.
    PerSegment(Vec<u32>),
    /// Allocate one total duration in proportion to segment length.
    Total(u32),
    /// Apply automatic duration to every segment.
    Automatic {
        /// Proportionally clamp a path over ten seconds instead of rejecting it.
        clamp_path_to_limit: bool,
    },
}

/// Builds one deterministic motion segment.
pub fn plan_motion(
    start: RootPoint,
    end: RootPoint,
    options: MotionOptions,
) -> Result<MotionPlan, MotionPlanError> {
    validate_sample_rate(options.policy.sample_rate_hz)?;
    if options.curve == MotionCurve::Instant {
        if options.duration_ms.unwrap_or(0) != 0 {
            return Err(MotionPlanError::InstantDurationMustBeZero);
        }
        return plan_segment(start, end, options, 0);
    }

    let duration_ms = match options.duration_ms {
        Some(duration_ms) => {
            validate_duration(duration_ms)?;
            duration_ms
        }
        None => automatic_duration(start, end, options.policy),
    };
    plan_segment(start, end, options, duration_ms)
}

/// Builds a bounded motion action through ordered waypoints.
///
/// Consecutive duplicates are removed before duration allocation. For
/// `PerSegment`, the duration vector therefore corresponds to the normalized
/// non-zero segments, not the raw caller array.
pub fn plan_waypoint_motion(
    start: RootPoint,
    waypoints: &[RootPoint],
    curve: MotionCurve,
    policy: MotionPolicy,
    emit_final_noop: bool,
    duration_policy: WaypointDurationPolicy,
) -> Result<MotionPlan, MotionPlanError> {
    if waypoints.is_empty() || waypoints.len() > MAX_WAYPOINTS {
        return Err(MotionPlanError::WaypointCount {
            actual: waypoints.len(),
        });
    }
    validate_sample_rate(policy.sample_rate_hz)?;

    let requested_end = match waypoints.last() {
        Some(point) => *point,
        None => return Err(MotionPlanError::WaypointCount { actual: 0 }),
    };
    let mut points = Vec::with_capacity(waypoints.len().saturating_add(1));
    points.push(start);
    for point in waypoints {
        if points.last().copied() != Some(*point) {
            points.push(*point);
        }
    }
    let nonzero_segment_count = points.len().saturating_sub(1);
    let mut durations = if curve == MotionCurve::Instant {
        allocate_instant_durations(nonzero_segment_count, duration_policy)?
    } else {
        allocate_durations(&points, duration_policy, policy)?
    };
    if durations.len() != nonzero_segment_count {
        return Err(MotionPlanError::SegmentDurationCount {
            expected: nonzero_segment_count,
            actual: durations.len(),
        });
    }
    if emit_final_noop {
        points.push(requested_end);
        durations.push(0);
    }

    let segment_count = points.len().saturating_sub(1);
    if segment_count == 0 {
        return Ok(MotionPlan {
            start,
            end: requested_end,
            duration_ms: 0,
            raw_sample_count: 0,
            samples: Vec::new(),
            segment_ranges: Vec::new(),
        });
    }
    let mut samples = Vec::new();
    let mut segment_ranges = Vec::with_capacity(segment_count);
    let mut raw_sample_count = 0usize;
    let mut total_duration_ms = 0u32;
    for (index, duration_ms) in durations.into_iter().enumerate() {
        let segment_start = points[index];
        let segment_end = points[index + 1];
        let options = MotionOptions::new(
            curve,
            Some(duration_ms),
            policy,
            emit_final_noop && index + 1 == segment_count && segment_start == segment_end,
        )?;
        let segment = plan_motion(segment_start, segment_end, options)?;
        total_duration_ms = total_duration_ms
            .checked_add(segment.duration_ms)
            .ok_or(MotionPlanError::DurationLimitExceeded)?;
        raw_sample_count = raw_sample_count
            .checked_add(segment.raw_sample_count)
            .ok_or(MotionPlanError::EventLimitExceeded {
                maximum: MAX_MOTION_EVENTS,
            })?;
        if samples.len().saturating_add(segment.samples.len()) > MAX_MOTION_EVENTS {
            return Err(MotionPlanError::EventLimitExceeded {
                maximum: MAX_MOTION_EVENTS,
            });
        }
        let range_start = samples.len();
        samples.extend(segment.samples);
        segment_ranges.push(range_start..samples.len());
    }
    validate_duration(total_duration_ms)?;
    Ok(MotionPlan {
        start,
        end: requested_end,
        duration_ms: total_duration_ms,
        raw_sample_count,
        samples,
        segment_ranges,
    })
}

/// Rounds a finite number to the nearest integer, with exact ties away from zero.
pub fn round_ties_away(value: f64) -> Result<i32, MotionPlanError> {
    if !value.is_finite() {
        return Err(MotionPlanError::NonFiniteCalculation);
    }
    let rounded = if value.is_sign_negative() {
        (value - 0.5).ceil()
    } else {
        (value + 0.5).floor()
    };
    if rounded < f64::from(i32::MIN) || rounded > f64::from(i32::MAX) {
        return Err(MotionPlanError::RoundedCoordinateOverflow);
    }
    Ok(rounded as i32)
}

fn plan_segment(
    start: RootPoint,
    end: RootPoint,
    options: MotionOptions,
    duration_ms: u32,
) -> Result<MotionPlan, MotionPlanError> {
    validate_duration(duration_ms)?;
    if start == end && !options.emit_noop {
        return Ok(MotionPlan {
            start,
            end,
            duration_ms: 0,
            raw_sample_count: 0,
            samples: Vec::new(),
            segment_ranges: Vec::new(),
        });
    }

    let raw_sample_count = if options.curve == MotionCurve::Instant {
        1
    } else {
        usize::try_from(sample_count(duration_ms, options.policy.sample_rate_hz)?).map_err(
            |_| MotionPlanError::EventLimitExceeded {
                maximum: MAX_MOTION_EVENTS,
            },
        )?
    };
    if raw_sample_count > MAX_MOTION_EVENTS {
        return Err(MotionPlanError::EventLimitExceeded {
            maximum: MAX_MOTION_EVENTS,
        });
    }

    let mut samples = Vec::with_capacity(raw_sample_count);
    let mut previous_point = start;
    let mut previous_cumulative = 0u32;
    let mut pending_delay = 0u32;
    for index in 1..=raw_sample_count {
        let cumulative = if index == raw_sample_count {
            duration_ms
        } else {
            let numerator = u64::try_from(index)
                .map_err(|_| MotionPlanError::DurationLimitExceeded)?
                * u64::from(duration_ms);
            let denominator = u64::try_from(raw_sample_count)
                .map_err(|_| MotionPlanError::DurationLimitExceeded)?;
            u32::try_from((numerator + denominator / 2) / denominator)
                .map_err(|_| MotionPlanError::DurationLimitExceeded)?
        };
        let delay_ms = cumulative
            .checked_sub(previous_cumulative)
            .ok_or(MotionPlanError::DurationLimitExceeded)?;
        previous_cumulative = cumulative;
        pending_delay = pending_delay
            .checked_add(delay_ms)
            .ok_or(MotionPlanError::DurationLimitExceeded)?;

        let point = interpolate_point(start, end, options.curve, index, raw_sample_count)?;
        let is_final = index == raw_sample_count;
        if point != previous_point || is_final {
            samples.push(MotionSample {
                point,
                delay_ms: pending_delay,
            });
            pending_delay = 0;
            previous_point = point;
        }
    }

    if samples.len() > MAX_MOTION_EVENTS {
        return Err(MotionPlanError::EventLimitExceeded {
            maximum: MAX_MOTION_EVENTS,
        });
    }
    let sample_count = samples.len();
    Ok(MotionPlan {
        start,
        end,
        duration_ms,
        raw_sample_count,
        samples,
        segment_ranges: std::iter::once(0..sample_count).collect(),
    })
}

fn interpolate_point(
    start: RootPoint,
    end: RootPoint,
    curve: MotionCurve,
    index: usize,
    sample_count: usize,
) -> Result<RootPoint, MotionPlanError> {
    if index == sample_count || curve == MotionCurve::Instant {
        return Ok(end);
    }
    let index = u32::try_from(index).map_err(|_| MotionPlanError::EventLimitExceeded {
        maximum: MAX_MOTION_EVENTS,
    })?;
    let sample_count =
        u32::try_from(sample_count).map_err(|_| MotionPlanError::EventLimitExceeded {
            maximum: MAX_MOTION_EVENTS,
        })?;
    let t = f64::from(index) / f64::from(sample_count);
    let eased = match curve {
        MotionCurve::Instant | MotionCurve::Linear => t,
        MotionCurve::Smooth => t * t * (3.0 - 2.0 * t),
    };
    let x = f64::from(start.x()) + eased * (f64::from(end.x()) - f64::from(start.x()));
    let y = f64::from(start.y()) + eased * (f64::from(end.y()) - f64::from(start.y()));
    RootPoint::new(round_ties_away(x)?, round_ties_away(y)?)
        .map_err(|_| MotionPlanError::RoundedCoordinateOverflow)
}

fn sample_count(duration_ms: u32, sample_rate_hz: u16) -> Result<u32, MotionPlanError> {
    validate_duration(duration_ms)?;
    validate_sample_rate(sample_rate_hz)?;
    let numerator = u64::from(duration_ms) * u64::from(sample_rate_hz);
    let count = numerator.div_ceil(1_000).max(1);
    u32::try_from(count).map_err(|_| MotionPlanError::EventLimitExceeded {
        maximum: MAX_MOTION_EVENTS,
    })
}

fn automatic_duration(start: RootPoint, end: RootPoint, policy: MotionPolicy) -> u32 {
    let dx = f64::from(end.x()) - f64::from(start.x());
    let dy = f64::from(end.y()) - f64::from(start.y());
    let distance = dx.hypot(dy);
    if distance == 0.0 {
        return 0;
    }
    let duration = (1_000.0 * distance / f64::from(policy.nominal_speed_px_per_second)).round();
    (duration as u32).clamp(policy.minimum_duration_ms, policy.maximum_duration_ms)
}

fn allocate_durations(
    points: &[RootPoint],
    policy: WaypointDurationPolicy,
    motion_policy: MotionPolicy,
) -> Result<Vec<u32>, MotionPlanError> {
    let segment_count = points.len().saturating_sub(1);
    match policy {
        WaypointDurationPolicy::PerSegment(durations) => {
            if durations.len() != segment_count {
                return Err(MotionPlanError::SegmentDurationCount {
                    expected: segment_count,
                    actual: durations.len(),
                });
            }
            validate_durations(&durations)?;
            Ok(durations)
        }
        WaypointDurationPolicy::Total(total_ms) => {
            validate_duration(total_ms)?;
            proportional_durations(points, total_ms)
        }
        WaypointDurationPolicy::Automatic {
            clamp_path_to_limit,
        } => {
            let durations: Vec<u32> = points
                .windows(2)
                .map(|segment| automatic_duration(segment[0], segment[1], motion_policy))
                .collect();
            let total = durations.iter().try_fold(0u32, |sum, duration| {
                sum.checked_add(*duration)
                    .ok_or(MotionPlanError::DurationLimitExceeded)
            })?;
            if total <= MAX_MOTION_DURATION_MS {
                return Ok(durations);
            }
            if !clamp_path_to_limit {
                return Err(MotionPlanError::DurationLimitExceeded);
            }
            proportional_durations(points, MAX_MOTION_DURATION_MS)
        }
    }
}

fn allocate_instant_durations(
    segment_count: usize,
    policy: WaypointDurationPolicy,
) -> Result<Vec<u32>, MotionPlanError> {
    match policy {
        WaypointDurationPolicy::Automatic { .. } | WaypointDurationPolicy::Total(0) => {
            Ok(vec![0; segment_count])
        }
        WaypointDurationPolicy::Total(_) => Err(MotionPlanError::InstantDurationMustBeZero),
        WaypointDurationPolicy::PerSegment(durations) => {
            if durations.len() != segment_count {
                return Err(MotionPlanError::SegmentDurationCount {
                    expected: segment_count,
                    actual: durations.len(),
                });
            }
            if durations.iter().any(|duration| *duration != 0) {
                return Err(MotionPlanError::InstantDurationMustBeZero);
            }
            Ok(durations)
        }
    }
}

fn proportional_durations(
    points: &[RootPoint],
    total_ms: u32,
) -> Result<Vec<u32>, MotionPlanError> {
    let distances: Vec<f64> = points
        .windows(2)
        .map(|segment| {
            let dx = f64::from(segment[1].x()) - f64::from(segment[0].x());
            let dy = f64::from(segment[1].y()) - f64::from(segment[0].y());
            dx.hypot(dy)
        })
        .collect();
    let total_distance: f64 = distances.iter().sum();
    if total_distance == 0.0 {
        let mut durations = vec![0; distances.len()];
        if let Some(last) = durations.last_mut() {
            *last = total_ms;
        }
        return Ok(durations);
    }

    let mut result = Vec::with_capacity(distances.len());
    let mut cumulative_distance = 0.0;
    let mut previous_cumulative = 0u32;
    for (index, distance) in distances.iter().enumerate() {
        cumulative_distance += distance;
        let cumulative = if index + 1 == distances.len() {
            total_ms
        } else {
            let scaled = f64::from(total_ms) * cumulative_distance / total_distance;
            u32::try_from(round_ties_away(scaled)?)
                .map_err(|_| MotionPlanError::DurationLimitExceeded)?
        };
        result.push(
            cumulative
                .checked_sub(previous_cumulative)
                .ok_or(MotionPlanError::DurationLimitExceeded)?,
        );
        previous_cumulative = cumulative;
    }
    Ok(result)
}

fn validate_durations(durations: &[u32]) -> Result<(), MotionPlanError> {
    let total = durations.iter().try_fold(0u32, |sum, duration| {
        validate_duration(*duration)?;
        sum.checked_add(*duration)
            .ok_or(MotionPlanError::DurationLimitExceeded)
    })?;
    validate_duration(total)
}

fn validate_duration(duration_ms: u32) -> Result<(), MotionPlanError> {
    if duration_ms > MAX_MOTION_DURATION_MS {
        Err(MotionPlanError::DurationLimitExceeded)
    } else {
        Ok(())
    }
}

fn validate_sample_rate(sample_rate_hz: u16) -> Result<(), MotionPlanError> {
    if !(MIN_SAMPLE_RATE_HZ..=MAX_SAMPLE_RATE_HZ).contains(&sample_rate_hz) {
        Err(MotionPlanError::SampleRateOutOfRange {
            actual: sample_rate_hz,
        })
    } else {
        Ok(())
    }
}

/// Failure to construct a deterministic bounded motion plan.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MotionPlanError {
    /// A duration exceeded the whole-action limit.
    #[error("motion duration exceeds {MAX_MOTION_DURATION_MS} ms")]
    DurationLimitExceeded,
    /// Automatic speed must be non-zero.
    #[error("nominal pointer speed must be greater than zero")]
    NominalSpeedMustBePositive,
    /// Automatic minimum duration exceeds its maximum.
    #[error("automatic duration minimum {minimum_ms} ms exceeds maximum {maximum_ms} ms")]
    AutomaticDurationRangeInvalid {
        /// Rejected minimum.
        minimum_ms: u32,
        /// Rejected maximum.
        maximum_ms: u32,
    },
    /// Instant motion was given a non-zero duration.
    #[error("instant motion duration must be omitted or zero")]
    InstantDurationMustBeZero,
    /// A convenience constructor was given the instant curve.
    #[error("an interpolated curve is required")]
    ExpectedInterpolatedCurve,
    /// A sampling frequency is outside the supported range.
    #[error("sample rate {actual} Hz is outside 1..=240 Hz")]
    SampleRateOutOfRange {
        /// Rejected frequency.
        actual: u16,
    },
    /// The caller supplied no waypoint or too many waypoints.
    #[error("waypoint count {actual} is outside 1..=1024")]
    WaypointCount {
        /// Rejected count.
        actual: usize,
    },
    /// Per-segment durations do not match normalized segments.
    #[error("expected {expected} segment durations, received {actual}")]
    SegmentDurationCount {
        /// Required duration count.
        expected: usize,
        /// Supplied duration count.
        actual: usize,
    },
    /// Planning would exceed the event-count safety bound.
    #[error("motion plan exceeds {maximum} emitted events")]
    EventLimitExceeded {
        /// Maximum allowed events.
        maximum: usize,
    },
    /// A floating-point intermediate was not finite.
    #[error("motion calculation produced a non-finite value")]
    NonFiniteCalculation,
    /// A rounded coordinate is not representable by checked root geometry.
    #[error("rounded motion coordinate is outside the supported range")]
    RoundedCoordinateOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::GeometryError;

    fn point(x: i32, y: i32) -> Result<RootPoint, GeometryError> {
        RootPoint::new(x, y)
    }

    #[test]
    fn ties_round_away_from_zero() {
        assert_eq!(round_ties_away(1.5), Ok(2));
        assert_eq!(round_ties_away(-1.5), Ok(-2));
        assert_eq!(round_ties_away(1.49), Ok(1));
        assert_eq!(round_ties_away(-1.49), Ok(-1));
    }

    #[test]
    fn instant_rejects_nonzero_duration() {
        assert_eq!(
            MotionOptions::new(
                MotionCurve::Instant,
                Some(1),
                MotionPolicy::default(),
                false,
            ),
            Err(MotionPlanError::InstantDurationMustBeZero)
        );
    }

    #[test]
    fn omitted_zero_distance_motion_has_no_execution_segment()
    -> Result<(), Box<dyn std::error::Error>> {
        let origin = point(7, 11)?;
        let plan = plan_motion(
            origin,
            origin,
            MotionOptions::new(MotionCurve::Linear, None, MotionPolicy::default(), false)?,
        )?;
        assert!(plan.samples().is_empty());
        assert_eq!(plan.segment_count(), 0);
        Ok(())
    }

    #[test]
    fn duplicate_compaction_keeps_time_and_final_endpoint() -> Result<(), Box<dyn std::error::Error>>
    {
        let plan = plan_motion(
            point(0, 0)?,
            point(1, 0)?,
            MotionOptions::new(
                MotionCurve::Linear,
                Some(100),
                MotionPolicy::new(240, 1_200, 80, 650)?,
                false,
            )?,
        )?;
        assert!(plan.event_count() < plan.raw_sample_count());
        assert_eq!(
            plan.samples()
                .iter()
                .map(|sample| sample.delay_ms())
                .sum::<u32>(),
            100
        );
        assert_eq!(
            plan.samples().last().map(|sample| sample.point()),
            Some(point(1, 0)?)
        );
        Ok(())
    }

    #[test]
    fn waypoint_total_is_exact() -> Result<(), Box<dyn std::error::Error>> {
        let plan = plan_waypoint_motion(
            point(0, 0)?,
            &[point(10, 0)?, point(10, 10)?, point(20, 10)?],
            MotionCurve::Linear,
            MotionPolicy::default(),
            false,
            WaypointDurationPolicy::Total(101),
        )?;
        assert_eq!(plan.duration_ms(), 101);
        assert_eq!(
            plan.samples()
                .iter()
                .map(|sample| sample.delay_ms())
                .sum::<u32>(),
            101
        );
        assert_eq!(plan.segment_count(), 3);
        assert_eq!(
            plan.segments()
                .map(|segment| segment.last().map(|sample| sample.point()))
                .collect::<Vec<_>>(),
            vec![
                Some(point(10, 0)?),
                Some(point(10, 10)?),
                Some(point(20, 10)?),
            ]
        );
        Ok(())
    }

    #[test]
    fn waypoint_boundaries_follow_normalization_and_keep_explicit_noop()
    -> Result<(), Box<dyn std::error::Error>> {
        let plan = plan_waypoint_motion(
            point(0, 0)?,
            &[point(10, 0)?, point(10, 0)?, point(20, 0)?],
            MotionCurve::Linear,
            MotionPolicy::default(),
            true,
            WaypointDurationPolicy::Total(100),
        )?;

        let segments = plan.segments().collect::<Vec<_>>();
        assert_eq!(segments.len(), 3);
        assert_eq!(
            segments[0].last().map(|sample| sample.point()),
            Some(point(10, 0)?)
        );
        assert_eq!(
            segments[1].last().map(|sample| sample.point()),
            Some(point(20, 0)?)
        );
        assert_eq!(segments[2].len(), 1);
        assert_eq!(segments[2][0].point(), point(20, 0)?);
        assert_eq!(segments[2][0].delay_ms(), 0);
        assert_eq!(
            segments.iter().map(|segment| segment.len()).sum::<usize>(),
            plan.event_count()
        );
        Ok(())
    }

    #[test]
    fn instant_waypoints_treat_automatic_as_omitted_and_reject_nonzero()
    -> Result<(), Box<dyn std::error::Error>> {
        let start = point(0, 0)?;
        let waypoints = [point(10, 0)?, point(20, 0)?];
        let automatic = plan_waypoint_motion(
            start,
            &waypoints,
            MotionCurve::Instant,
            MotionPolicy::default(),
            false,
            WaypointDurationPolicy::Automatic {
                clamp_path_to_limit: false,
            },
        )?;
        assert_eq!(automatic.duration_ms(), 0);
        assert_eq!(automatic.event_count(), 2);
        assert!(
            plan_waypoint_motion(
                start,
                &waypoints,
                MotionCurve::Instant,
                MotionPolicy::default(),
                false,
                WaypointDurationPolicy::Total(1),
            )
            .is_err()
        );
        assert!(
            plan_waypoint_motion(
                start,
                &waypoints,
                MotionCurve::Instant,
                MotionPolicy::default(),
                false,
                WaypointDurationPolicy::PerSegment(vec![0, 1]),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn nondefault_motion_policy_controls_automatic_duration_and_sampling()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = MotionPolicy::new(120, 100, 10, 2_000)?;
        let plan = plan_motion(
            point(0, 0)?,
            point(100, 0)?,
            MotionOptions::new(MotionCurve::Linear, None, policy, false)?,
        )?;
        assert_eq!(plan.duration_ms(), 1_000);
        assert_eq!(plan.raw_sample_count(), 120);
        assert_eq!(policy.nominal_speed_px_per_second(), 100);
        assert_eq!(policy.minimum_duration_ms(), 10);
        assert_eq!(policy.maximum_duration_ms(), 2_000);
        Ok(())
    }
}
