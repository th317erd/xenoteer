use std::time::{Duration, Instant};

use super::*;

fn rect(x: i32, y: i32, width: u32, height: u32) -> RootDamageRect {
    RootDamageRect::new(x, y, width, height).unwrap_or(RootDamageRect {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    })
}

fn hint(area: RootDamageRect) -> RootDamageHint {
    RootDamageHint {
        area,
        root_region: rect(0, 0, 800, 600),
    }
}

#[test]
fn coalesces_overlapping_regions_after_one_frame() {
    let now = Instant::now();
    let mut accumulator = DamageAccumulator::default();
    accumulator.offer(hint(rect(10, 10, 20, 20)), now);
    accumulator.offer(hint(rect(20, 20, 30, 30)), now);
    assert!(accumulator.take_due(now).is_none());
    let batch = accumulator
        .take_due(now + DAMAGE_COALESCE_INTERVAL)
        .unwrap_or(RootDamageBatch {
            root_region: rect(0, 0, 1, 1),
            regions: Vec::new(),
            coverage: RootDamageCoverage::FullScreen,
            notifications: 0,
        });
    assert_eq!(batch.regions, vec![rect(10, 10, 40, 40)]);
    assert_eq!(batch.coverage, RootDamageCoverage::Regions);
    assert_eq!(batch.notifications, 2);
}

#[test]
fn clips_regions_and_ignores_damage_outside_root() {
    let now = Instant::now();
    let mut accumulator = DamageAccumulator::default();
    accumulator.offer(hint(rect(790, 590, 20, 20)), now);
    accumulator.offer(hint(rect(900, 700, 10, 10)), now);
    let batch = accumulator
        .take_due(now + DAMAGE_COALESCE_INTERVAL)
        .unwrap_or(RootDamageBatch {
            root_region: rect(0, 0, 1, 1),
            regions: Vec::new(),
            coverage: RootDamageCoverage::FullScreen,
            notifications: 0,
        });
    assert_eq!(batch.regions, vec![rect(790, 590, 10, 10)]);
    assert_eq!(batch.notifications, 1);
}

#[test]
fn collapses_excess_complexity_to_one_bounding_box() {
    let now = Instant::now();
    let mut accumulator = DamageAccumulator::default();
    for index in 0..=MAX_DAMAGE_REGIONS {
        accumulator.offer(hint(rect((index * 3) as i32, 10, 1, 1)), now);
    }
    let batch = accumulator
        .take_due(now + DAMAGE_COALESCE_INTERVAL)
        .unwrap_or(RootDamageBatch {
            root_region: rect(0, 0, 1, 1),
            regions: Vec::new(),
            coverage: RootDamageCoverage::FullScreen,
            notifications: 0,
        });
    assert_eq!(batch.regions, vec![rect(0, 10, 193, 1)]);
    assert_eq!(batch.coverage, RootDamageCoverage::BoundingBox);
    assert_eq!(batch.notifications, 65);
}

#[test]
fn root_geometry_change_promotes_to_full_screen() {
    let now = Instant::now();
    let mut accumulator = DamageAccumulator::default();
    accumulator.offer(hint(rect(1, 1, 1, 1)), now);
    accumulator.offer(
        RootDamageHint {
            area: rect(2, 2, 1, 1),
            root_region: rect(0, 0, 1024, 768),
        },
        now,
    );
    let batch = accumulator
        .take_due(now + DAMAGE_COALESCE_INTERVAL)
        .unwrap_or(RootDamageBatch {
            root_region: rect(0, 0, 1, 1),
            regions: Vec::new(),
            coverage: RootDamageCoverage::Regions,
            notifications: 0,
        });
    assert_eq!(batch.regions, vec![rect(0, 0, 1024, 768)]);
    assert_eq!(batch.coverage, RootDamageCoverage::FullScreen);
}

#[test]
fn pending_damage_shortens_idle_backstop() {
    let now = Instant::now();
    let mut accumulator = DamageAccumulator::default();
    assert_eq!(
        accumulator.wait_timeout(now, Duration::from_millis(25)),
        Duration::from_millis(25)
    );
    accumulator.offer(hint(rect(1, 1, 1, 1)), now);
    assert_eq!(
        accumulator.wait_timeout(now, Duration::from_millis(25)),
        DAMAGE_COALESCE_INTERVAL
    );
}
