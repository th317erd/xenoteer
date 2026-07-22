use super::*;
use crate::Rect;
use uuid::Uuid;

fn identifier_scope() -> (DesktopId, DesktopGeneration) {
    (
        DesktopId::from_uuid(Uuid::from_u128(1)),
        DesktopGeneration::from_uuid(Uuid::from_u128(2)),
    )
}

fn root_rect(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<WindowRect, Box<dyn std::error::Error>> {
    Ok(WindowRect::new(
        CoordinateSpace::RootPhysical,
        Rect::new(x, y, width, height)?,
    )?)
}

fn valid_event() -> Result<ScreenDamageEvent, Box<dyn std::error::Error>> {
    let (desktop_id, desktop_generation) = identifier_scope();
    Ok(ScreenDamageEvent {
        desktop_id,
        desktop_generation,
        root_region: root_rect(0, 0, 800, 600)?,
        damaged_regions: vec![root_rect(10, 20, 30, 40)?],
        coverage: ScreenDamageCoverage::Regions,
        coalesced_notifications: 2,
    })
}

#[test]
fn accepts_bounded_root_clipped_damage() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(valid_event()?.validate(), Ok(()));
    Ok(())
}

#[test]
fn rejects_out_of_root_and_duplicate_regions() -> Result<(), Box<dyn std::error::Error>> {
    let mut outside = valid_event()?;
    outside.damaged_regions = vec![root_rect(790, 590, 20, 20)?];
    assert_eq!(
        outside.validate(),
        Err(ScreenDamageValidationError::Regions)
    );

    let mut duplicate = valid_event()?;
    duplicate.damaged_regions.push(duplicate.damaged_regions[0]);
    assert_eq!(
        duplicate.validate(),
        Err(ScreenDamageValidationError::Regions)
    );
    Ok(())
}

#[test]
fn collapse_claims_bind_the_payload_shape() -> Result<(), Box<dyn std::error::Error>> {
    let mut bounding = valid_event()?;
    bounding.coverage = ScreenDamageCoverage::BoundingBox;
    assert_eq!(bounding.validate(), Ok(()));
    bounding.damaged_regions = vec![bounding.root_region];
    assert_eq!(
        bounding.validate(),
        Err(ScreenDamageValidationError::Coverage)
    );

    let mut full = valid_event()?;
    full.coverage = ScreenDamageCoverage::FullScreen;
    assert_eq!(full.validate(), Err(ScreenDamageValidationError::Coverage));
    full.damaged_regions = vec![full.root_region];
    assert_eq!(full.validate(), Ok(()));
    Ok(())
}

#[test]
fn rejects_wrong_coordinate_space_and_zero_notification_count()
-> Result<(), Box<dyn std::error::Error>> {
    let mut event = valid_event()?;
    event.damaged_regions[0].coordinate_space = CoordinateSpace::WindowClient;
    assert_eq!(
        event.validate(),
        Err(ScreenDamageValidationError::CoordinateSpace)
    );

    let mut empty_count = valid_event()?;
    empty_count.coalesced_notifications = 0;
    assert_eq!(
        empty_count.validate(),
        Err(ScreenDamageValidationError::Regions)
    );
    Ok(())
}
