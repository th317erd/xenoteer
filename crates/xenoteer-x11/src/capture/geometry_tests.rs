#![allow(clippy::unwrap_used)]

use xenoteer_protocol::{
    DesktopGeneration, DesktopId, ScreenshotTarget, WindowCaptureSpace, WindowIdentityHash,
    WindowRef,
};

use super::*;

fn reference() -> WindowRef {
    WindowRef {
        desktop_id: DesktopId::new(),
        desktop_generation: DesktopGeneration::new(),
        xid: 77,
        observed_generation: 1,
        identity_hash: WindowIdentityHash::new("a".repeat(64)).unwrap(),
    }
}

fn geometry() -> RawWindowCaptureGeometry {
    RawWindowCaptureGeometry {
        root: 1,
        window: 77,
        client_root: Rect::new(-10, 10, 100, 80).unwrap(),
        frame_root: Some(Rect::new(-14, 6, 108, 88).unwrap()),
        viewable: true,
    }
}

#[test]
fn root_region_must_be_fully_inside_screen() -> Result<(), Box<dyn std::error::Error>> {
    let full = resolve_root_area(1, 200, 100, None).unwrap();
    assert_eq!(full.root_region, Rect::new(0, 0, 200, 100)?);
    assert!(resolve_root_area(1, 200, 100, Some(Rect::new(199, 0, 2, 1)?)).is_err());
    Ok(())
}

#[test]
fn window_visible_is_root_crop_and_clips_offscreen_edges() -> Result<(), Box<dyn std::error::Error>>
{
    let target = ScreenshotTarget::WindowVisible {
        window: reference(),
        coordinate_space: WindowCaptureSpace::Client,
    };
    let area = resolve_window_area(&target, None, 200, 100, geometry()).unwrap();
    assert_eq!(area.drawable, 1);
    assert_eq!(area.root_region, Rect::new(0, 10, 90, 80)?);
    assert_eq!((area.drawable_x, area.drawable_y), (0, 10));
    Ok(())
}

#[test]
fn drawable_region_stays_client_local_and_requires_viewability()
-> Result<(), Box<dyn std::error::Error>> {
    let target = ScreenshotTarget::WindowDrawable {
        window: reference(),
    };
    let crop = Rect::new(10, 5, 20, 30)?;
    let area = resolve_window_area(&target, Some(crop), 200, 100, geometry()).unwrap();
    assert_eq!(area.drawable, 77);
    assert_eq!((area.drawable_x, area.drawable_y), (10, 5));
    assert_eq!(area.root_region, Rect::new(0, 15, 20, 30)?);

    let mut hidden = geometry();
    hidden.viewable = false;
    assert_eq!(
        resolve_window_area(&target, None, 200, 100, hidden),
        Err(CaptureActorFailureKind::WindowNotViewable)
    );
    Ok(())
}

#[test]
fn target_local_region_cannot_escape_client_or_frame_bounds()
-> Result<(), Box<dyn std::error::Error>> {
    let target = ScreenshotTarget::WindowVisible {
        window: reference(),
        coordinate_space: WindowCaptureSpace::Frame,
    };
    assert_eq!(
        resolve_window_area(
            &target,
            Some(Rect::new(107, 0, 2, 1)?),
            200,
            100,
            geometry(),
        ),
        Err(CaptureActorFailureKind::RegionOutOfBounds)
    );
    Ok(())
}
