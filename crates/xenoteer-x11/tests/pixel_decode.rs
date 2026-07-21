//! Pure X11 ZPixmap decoder conformance tests.

use proptest::prelude::*;
use xenoteer_x11::X11Error;
use xenoteer_x11::capture::{ByteOrder, PixelFormat, PixelVisualClass, RawImage, decode_bgra8};

fn fixture_format(byte_order: ByteOrder) -> PixelFormat {
    PixelFormat {
        visual_class: PixelVisualClass::TrueColor,
        depth: 24,
        bits_per_pixel: 32,
        scanline_pad: 32,
        byte_order,
        red_mask: 0x00ff_0000,
        green_mask: 0x0000_ff00,
        blue_mask: 0x0000_00ff,
    }
}

#[test]
fn decodes_depth_24_bpp_32_little_endian_color_bars() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = include_str!("../../../fixtures/x11/color-bars-24-depth-32-bpp-le.json");
    let fixture: serde_json::Value = serde_json::from_str(fixture)?;
    let data_hex = fixture["data_hex"]
        .as_str()
        .ok_or("fixture data_hex must be a string")?;
    let data = decode_hex(data_hex)?;
    let image = RawImage::new(5, 1, fixture_format(ByteOrder::LeastSignificantFirst), data)?;

    assert_eq!(
        decode_bgra8(&image)?,
        vec![
            0, 0, 255, 255, // red
            0, 255, 0, 255, // green
            255, 0, 0, 255, // blue
            255, 255, 255, 255, // white
            0, 0, 0, 255, // black
        ]
    );
    Ok(())
}

#[test]
fn decodes_depth_24_bpp_32_big_endian_color_bars() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = include_str!("../../../fixtures/x11/color-bars-24-depth-32-bpp-be.json");
    let fixture: serde_json::Value = serde_json::from_str(fixture)?;
    let data_hex = fixture["data_hex"]
        .as_str()
        .ok_or("fixture data_hex must be a string")?;
    let data = decode_hex(data_hex)?;
    let image = RawImage::new(5, 1, fixture_format(ByteOrder::MostSignificantFirst), data)?;

    assert_eq!(
        decode_bgra8(&image)?,
        vec![
            0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255,
        ]
    );
    Ok(())
}

#[test]
fn rejects_short_replies_before_indexing() {
    let result = RawImage::new(
        2,
        2,
        fixture_format(ByteOrder::LeastSignificantFirst),
        vec![0; 15],
    );
    assert!(result.is_err());
}

#[test]
fn accepts_only_bounded_x11_final_wire_padding() -> Result<(), Box<dyn std::error::Error>> {
    let format = PixelFormat {
        visual_class: PixelVisualClass::TrueColor,
        depth: 16,
        bits_per_pixel: 16,
        scanline_pad: 16,
        byte_order: ByteOrder::LeastSignificantFirst,
        red_mask: 0xf800,
        green_mask: 0x07e0,
        blue_mask: 0x001f,
    };
    assert!(RawImage::new(1, 1, format, vec![0; 2]).is_ok());
    assert!(RawImage::new(1, 1, format, vec![0; 4]).is_ok());
    assert!(RawImage::new(1, 1, format, vec![0; 5]).is_err());
    Ok(())
}

#[test]
fn rejects_excess_data_when_no_wire_padding_is_needed() {
    assert!(
        RawImage::new(
            1,
            1,
            fixture_format(ByteOrder::LeastSignificantFirst),
            vec![0; 5],
        )
        .is_err()
    );
}

#[test]
fn raw_image_hard_caps_reject_hostile_dimensions_before_length_checks() {
    let format = fixture_format(ByteOrder::LeastSignificantFirst);
    assert!(RawImage::new(8_193, 1, format, Vec::new()).is_err());
    assert!(RawImage::new(5_000, 5_000, format, Vec::new()).is_err());
}

#[test]
fn rejects_direct_color_instead_of_misreading_colormap_indices() {
    let mut format = fixture_format(ByteOrder::LeastSignificantFirst);
    format.visual_class = PixelVisualClass::DirectColor;
    let result = RawImage::new(1, 1, format, vec![0; 4]);
    assert!(matches!(
        result,
        Err(X11Error::UnsupportedVisualClass { visual_class: 5 })
    ));
}

#[test]
fn rejects_depth_larger_than_pixel_storage() {
    let mut format = fixture_format(ByteOrder::LeastSignificantFirst);
    format.depth = 25;
    format.bits_per_pixel = 24;
    assert!(RawImage::new(1, 1, format, vec![0; 4]).is_err());
}

#[test]
fn rejects_depth_that_disagrees_with_mask_significant_bits() {
    let mut format = fixture_format(ByteOrder::LeastSignificantFirst);
    format.depth = 23;
    assert!(RawImage::new(1, 1, format, vec![0; 4]).is_err());
}

#[test]
fn rejects_overlapping_rgb_masks() {
    let mut format = fixture_format(ByteOrder::LeastSignificantFirst);
    format.green_mask = format.red_mask;
    assert!(RawImage::new(1, 1, format, vec![0; 4]).is_err());
}

#[test]
fn raw_image_debug_contains_metadata_but_not_pixel_canary() -> Result<(), Box<dyn std::error::Error>>
{
    let canary = b"SENSITIVE_CANARY".to_vec();
    let numeric_canary = canary
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let image = RawImage::new(
        4,
        1,
        fixture_format(ByteOrder::LeastSignificantFirst),
        canary,
    )?;
    let rendered = format!("{image:?}");

    assert!(rendered.contains("width: 4"));
    assert!(rendered.contains("height: 1"));
    assert!(rendered.contains("data_len: 16"));
    assert!(!rendered.contains("SENSITIVE_CANARY"));
    assert!(!rendered.contains(&numeric_canary));
    assert!(!rendered.contains("data:"));
    Ok(())
}

proptest! {
    #[test]
    fn every_non_true_color_visual_is_rejected(wire_class in prop_oneof![0_u8..=3, 5_u8..=u8::MAX]) {
        let mut format = fixture_format(ByteOrder::LeastSignificantFirst);
        format.visual_class = PixelVisualClass::from_wire_value(wire_class);
        let result = RawImage::new(1, 1, format, vec![0; 4]);
        let rejected_as_unsupported = matches!(
            result,
            Err(X11Error::UnsupportedVisualClass { .. })
        );
        prop_assert!(rejected_as_unsupported);
    }

    #[test]
    fn eight_bit_rgb_masks_round_trip(red in any::<u8>(), green in any::<u8>(), blue in any::<u8>()) {
        let data = vec![blue, green, red, 0];
        let image = RawImage::new(
            1,
            1,
            fixture_format(ByteOrder::LeastSignificantFirst),
            data,
        )?;
        prop_assert_eq!(decode_bgra8(&image)?, vec![blue, green, red, 255]);
    }

    #[test]
    fn arbitrary_reply_lengths_never_panic(data in proptest::collection::vec(any::<u8>(), 0..128)) {
        if let Ok(image) = RawImage::new(
            4,
            4,
            fixture_format(ByteOrder::LeastSignificantFirst),
            data,
        ) {
            prop_assert_eq!(decode_bgra8(&image)?.len(), 64);
        }
    }
}

fn decode_hex(value: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let compact: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if !compact.len().is_multiple_of(2) {
        return Err("hex fixture must contain complete bytes".into());
    }
    (0..compact.len())
        .step_by(2)
        .map(|offset| Ok(u8::from_str_radix(&compact[offset..offset + 2], 16)?))
        .collect()
}
