//! Pure, bounded X DAMAGE rectangle normalization and frame coalescing.

use std::time::{Duration, Instant};

/// Initial frame-sized coalescing interval for root damage notifications.
pub const DAMAGE_COALESCE_INTERVAL: Duration = Duration::from_millis(16);
/// Maximum region complexity before one conservative bounding box replaces it.
pub const MAX_DAMAGE_REGIONS: usize = 64;

/// Non-empty raw root-coordinate rectangle derived from X DAMAGE.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootDamageRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl RootDamageRect {
    /// Creates a non-empty representable rectangle.
    #[must_use]
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let end_x = i64::from(x) + i64::from(width);
        let end_y = i64::from(y) + i64::from(height);
        if end_x > i64::from(i32::MAX) + 1 || end_y > i64::from(i32::MAX) + 1 {
            return None;
        }
        Some(Self {
            x,
            y,
            width,
            height,
        })
    }

    /// Root X coordinate.
    #[must_use]
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Root Y coordinate.
    #[must_use]
    pub const fn y(self) -> i32 {
        self.y
    }

    /// Rectangle width.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Rectangle height.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    fn end_x(self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    fn end_y(self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let x = i64::from(self.x).max(i64::from(other.x));
        let y = i64::from(self.y).max(i64::from(other.y));
        let end_x = self.end_x().min(other.end_x());
        let end_y = self.end_y().min(other.end_y());
        let width = u32::try_from(end_x.checked_sub(x)?).ok()?;
        let height = u32::try_from(end_y.checked_sub(y)?).ok()?;
        Self::new(
            i32::try_from(x).ok()?,
            i32::try_from(y).ok()?,
            width,
            height,
        )
    }

    fn overlaps(self, other: Self) -> bool {
        i64::from(self.x) < other.end_x()
            && i64::from(other.x) < self.end_x()
            && i64::from(self.y) < other.end_y()
            && i64::from(other.y) < self.end_y()
    }

    fn bounding(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let end_x = self.end_x().max(other.end_x());
        let end_y = self.end_y().max(other.end_y());
        // Both inputs were representable, so their bounding rectangle is too.
        Self {
            x,
            y,
            width: u32::try_from(end_x - i64::from(x)).unwrap_or(u32::MAX),
            height: u32::try_from(end_y - i64::from(y)).unwrap_or(u32::MAX),
        }
    }
}

/// Raw root damage notification after extension-event decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootDamageHint {
    /// Dirty rectangle, conservatively reported by X DAMAGE.
    pub area: RootDamageRect,
    /// Root drawable bounds carried by the notification.
    pub root_region: RootDamageRect,
}

/// Amount of region information retained by one actor batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootDamageCoverage {
    /// Source regions remain independently represented.
    Regions,
    /// Region complexity was collapsed into one bounding box.
    BoundingBox,
    /// Damage conservatively covers the complete root drawable.
    FullScreen,
}

/// One frame-coalesced root damage batch emitted by the observation actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootDamageBatch {
    /// Current root drawable bounds.
    pub root_region: RootDamageRect,
    /// Non-empty root-clipped regions.
    pub regions: Vec<RootDamageRect>,
    /// Whether rectangle complexity was collapsed.
    pub coverage: RootDamageCoverage,
    /// Raw notifications represented by this batch.
    pub notifications: u32,
}

#[derive(Default)]
pub(super) struct DamageAccumulator {
    pending: Option<PendingDamage>,
}

struct PendingDamage {
    deadline: Instant,
    root_region: RootDamageRect,
    regions: Vec<RootDamageRect>,
    coverage: RootDamageCoverage,
    notifications: u32,
}

impl DamageAccumulator {
    pub(super) fn offer(&mut self, hint: RootDamageHint, now: Instant) {
        let Some(area) = hint.area.intersect(hint.root_region) else {
            return;
        };
        let pending = self.pending.get_or_insert_with(|| PendingDamage {
            deadline: now + DAMAGE_COALESCE_INTERVAL,
            root_region: hint.root_region,
            regions: Vec::new(),
            coverage: RootDamageCoverage::Regions,
            notifications: 0,
        });
        pending.notifications = pending.notifications.saturating_add(1);
        if pending.root_region != hint.root_region {
            pending.root_region = hint.root_region;
            pending.regions.clear();
            pending.regions.push(hint.root_region);
            pending.coverage = RootDamageCoverage::FullScreen;
            return;
        }
        if pending.coverage == RootDamageCoverage::FullScreen {
            return;
        }
        if pending.coverage == RootDamageCoverage::BoundingBox {
            let bounded = pending.regions[0].bounding(area);
            set_collapsed_region(pending, bounded);
            return;
        }

        let mut merged = area;
        pending.regions.retain(|region| {
            if region.overlaps(merged) {
                merged = merged.bounding(*region);
                false
            } else {
                true
            }
        });
        if pending.regions.len() < MAX_DAMAGE_REGIONS {
            pending.regions.push(merged);
            return;
        }
        let bounded = pending
            .regions
            .iter()
            .copied()
            .fold(merged, RootDamageRect::bounding);
        set_collapsed_region(pending, bounded);
    }

    pub(super) fn take_due(&mut self, now: Instant) -> Option<RootDamageBatch> {
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| now < pending.deadline)
        {
            return None;
        }
        self.take()
    }

    pub(super) fn wait_timeout(&self, now: Instant, maximum: Duration) -> Duration {
        self.pending.as_ref().map_or(maximum, |pending| {
            pending.deadline.saturating_duration_since(now).min(maximum)
        })
    }

    fn take(&mut self) -> Option<RootDamageBatch> {
        let pending = self.pending.take()?;
        Some(RootDamageBatch {
            root_region: pending.root_region,
            regions: pending.regions,
            coverage: pending.coverage,
            notifications: pending.notifications,
        })
    }
}

fn set_collapsed_region(pending: &mut PendingDamage, region: RootDamageRect) {
    pending.regions.clear();
    if region == pending.root_region {
        pending.regions.push(pending.root_region);
        pending.coverage = RootDamageCoverage::FullScreen;
    } else {
        pending.regions.push(region);
        pending.coverage = RootDamageCoverage::BoundingBox;
    }
}

#[cfg(test)]
#[path = "damage_tests.rs"]
mod tests;
