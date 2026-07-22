use super::*;

fn snapshot(x: i16, y: i16, serial: u32, pixels: Vec<u32>) -> CursorSnapshot {
    CursorSnapshot {
        x,
        y,
        width: 2,
        height: 2,
        xhot: 1,
        yhot: 1,
        serial,
        premultiplied_argb: pixels,
    }
}

#[test]
fn hotspot_alpha_and_edge_clipping_are_exact() -> std::result::Result<(), Box<dyn std::error::Error>>
{
    let transparent = 0;
    let half_red = 0x8080_0000;
    let opaque_green = 0xff00_ff00;
    let before = snapshot(
        10,
        10,
        7,
        vec![transparent, half_red, opaque_green, transparent],
    );
    let after = before.clone();
    let region = Rect::new(10, 9, 2, 2)?;
    let mut frame = [255, 0, 0, 255].repeat(4);
    let evidence = compose_cursor(&mut frame, region, &before, &after)?;
    assert!(evidence.composited);
    assert!(!evidence.moved_during_capture);
    assert_eq!(&frame[0..4], &[127, 0, 128, 255]);
    // The opaque green pixel lies one root pixel left of the requested region
    // and must not leak into the clipped frame.
    assert_eq!(&frame[8..12], &[255, 0, 0, 255]);
    assert_eq!(&frame[4..8], &[255, 0, 0, 255]);
    Ok(())
}

#[test]
fn nonintersecting_cursor_reports_observed_but_not_composited()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let before = snapshot(100, 100, 2, vec![0xffff_ffff; 4]);
    let mut after = before.clone();
    after.x = 101;
    let mut frame = vec![0; 16];
    let evidence = compose_cursor(&mut frame, Rect::new(0, 0, 2, 2)?, &before, &after)?;
    assert!(!evidence.composited);
    assert!(evidence.moved_during_capture);
    assert_eq!(evidence.serial_before, Some(2));
    assert_eq!(evidence.serial_after, Some(2));
    Ok(())
}

#[test]
fn hostile_cursor_geometry_length_hotspot_and_alpha_fail_before_indexing()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut cursor = snapshot(0, 0, 1, vec![0; 3]);
    assert!(cursor.validate().is_err());
    cursor.premultiplied_argb = vec![0; 4];
    cursor.xhot = 2;
    assert!(cursor.validate().is_err());
    cursor.xhot = 0;
    cursor.premultiplied_argb[0] = 0x40ff_0000;
    assert!(cursor.validate().is_err());
    Ok(())
}
