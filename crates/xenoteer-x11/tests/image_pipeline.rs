//! Golden and limit tests for the bounded BGRA8 image pipeline.

use std::io::Cursor;

use xenoteer_x11::capture::{CaptureImageLimits, ResizeFilter, encode_png_bgra8, resize_bgra8};

const GOLDEN_BGRA: [u8; 8] = [
    0, 0, 255, 255, // red
    255, 0, 0, 128, // half-alpha blue
];
const GOLDEN_RGBA: [u8; 8] = [
    255, 0, 0, 255, // red
    0, 0, 255, 128, // half-alpha blue
];

#[test]
fn png_round_trip_matches_golden_pixels_and_dimensions() -> Result<(), Box<dyn std::error::Error>> {
    let encoded = encode_png_bgra8(2, 1, &GOLDEN_BGRA, CaptureImageLimits::default())?;
    assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");

    let decoder = png::Decoder::new(Cursor::new(&encoded));
    let mut reader = decoder.read_info()?;
    let output_size = reader
        .output_buffer_size()
        .ok_or("decoded PNG size is not representable")?;
    let mut decoded = vec![0; output_size];
    let info = reader.next_frame(&mut decoded)?;

    assert_eq!((info.width, info.height), (2, 1));
    assert_eq!(info.color_type, png::ColorType::Rgba);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    assert_eq!(&decoded[..info.buffer_size()], &GOLDEN_RGBA);
    assert_eq!(
        encoded,
        encode_png_bgra8(2, 1, &GOLDEN_BGRA, CaptureImageLimits::default())?
    );
    Ok(())
}

#[test]
fn nearest_resize_is_exact_and_deterministic() -> Result<(), Box<dyn std::error::Error>> {
    let resized = resize_bgra8(
        2,
        1,
        &GOLDEN_BGRA,
        4,
        2,
        ResizeFilter::Nearest,
        CaptureImageLimits::default(),
    )?;
    let expected_row = [
        0, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0, 128, 255, 0, 0, 128,
    ];
    assert_eq!(resized, [expected_row, expected_row].concat());
    Ok(())
}

#[test]
fn lanczos_resize_has_requested_dimensions() -> Result<(), Box<dyn std::error::Error>> {
    let resized = resize_bgra8(
        2,
        1,
        &GOLDEN_BGRA,
        3,
        3,
        ResizeFilter::Lanczos3,
        CaptureImageLimits::default(),
    )?;
    assert_eq!(resized.len(), 3 * 3 * 4);
    Ok(())
}

#[test]
fn image_limits_reject_bad_dimensions_lengths_and_encoded_size() {
    let limits = CaptureImageLimits {
        max_dimension: 4,
        max_pixels: 8,
        max_encoded_bytes: 16,
    };
    assert!(encode_png_bgra8(0, 1, &[], limits).is_err());
    assert!(encode_png_bgra8(5, 1, &[0; 20], limits).is_err());
    assert!(encode_png_bgra8(2, 1, &[0; 7], limits).is_err());
    assert!(encode_png_bgra8(2, 1, &GOLDEN_BGRA, limits).is_err());
    assert!(resize_bgra8(2, 1, &GOLDEN_BGRA, 3, 3, ResizeFilter::Nearest, limits).is_err());
}

#[test]
fn hostile_max_limits_cannot_bypass_hard_dimension_or_pixel_ceilings() {
    let hostile = CaptureImageLimits {
        max_dimension: u32::MAX,
        max_pixels: u64::MAX,
        max_encoded_bytes: usize::MAX,
    };
    assert!(encode_png_bgra8(8_193, 1, &[], hostile).is_err());
    assert!(encode_png_bgra8(5_000, 5_000, &[], hostile).is_err());
    assert!(resize_bgra8(1, 1, &[0; 4], 8_193, 1, ResizeFilter::Nearest, hostile,).is_err());
    assert!(resize_bgra8(1, 1, &[0; 4], 5_000, 5_000, ResizeFilter::Nearest, hostile,).is_err());
}
