//! Bounded encoding and resizing for normalized BGRA8 capture frames.

use std::io;

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use crate::{Result, X11Error};

const HARD_MAX_DIMENSION: u32 = 8_192;
const HARD_MAX_PIXELS: u64 = 16_000_000;
const HARD_MAX_ENCODED_BYTES: usize = 32 * 1_024 * 1_024;

pub(crate) fn validate_hard_capture_dimensions(
    width: u32,
    height: u32,
    label: &str,
) -> Result<u64> {
    if width == 0 || height == 0 {
        return Err(X11Error::Pixel(format!(
            "{label} dimensions must be nonzero"
        )));
    }
    if width > HARD_MAX_DIMENSION || height > HARD_MAX_DIMENSION {
        return Err(X11Error::Pixel(format!(
            "{label} dimensions {width}x{height} exceed the {HARD_MAX_DIMENSION} hard ceiling"
        )));
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| X11Error::Pixel(format!("{label} pixel count overflow")))?;
    if pixels > HARD_MAX_PIXELS {
        return Err(X11Error::Pixel(format!(
            "{label} pixel count {pixels} exceeds the {HARD_MAX_PIXELS} hard ceiling"
        )));
    }
    Ok(pixels)
}

/// Explicit allocation and dimensions ceilings for image operations.
///
/// These caller-selected values can only tighten the immutable hard ceilings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureImageLimits {
    /// Maximum accepted source or destination dimension.
    pub max_dimension: u32,
    /// Maximum accepted source or destination pixel count.
    pub max_pixels: u64,
    /// Maximum encoded PNG size.
    pub max_encoded_bytes: usize,
}

impl Default for CaptureImageLimits {
    fn default() -> Self {
        Self {
            max_dimension: HARD_MAX_DIMENSION,
            max_pixels: HARD_MAX_PIXELS,
            max_encoded_bytes: HARD_MAX_ENCODED_BYTES,
        }
    }
}

/// Deliberately narrow set of deterministic resize algorithms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResizeFilter {
    /// Pixel replication, useful for exact fixtures and masks.
    Nearest,
    /// High-quality convolution for user-facing screenshots.
    Lanczos3,
}

/// Resize a tightly packed BGRA8 image while enforcing bounds before allocation.
pub fn resize_bgra8(
    source_width: u32,
    source_height: u32,
    source: &[u8],
    destination_width: u32,
    destination_height: u32,
    filter: ResizeFilter,
    limits: CaptureImageLimits,
) -> Result<Vec<u8>> {
    validate_bgra8(source_width, source_height, source, limits, "source")?;
    let destination_pixels =
        validate_dimensions(destination_width, destination_height, limits, "destination")?;

    let source_image = ImageRef::new(source_width, source_height, source, PixelType::U8x4)
        .map_err(|error| X11Error::Pixel(format!("invalid resize source: {error}")))?;
    let destination_byte_length = destination_pixels
        .checked_mul(4)
        .and_then(|length| usize::try_from(length).ok())
        .ok_or_else(|| X11Error::Pixel("destination BGRA8 byte length overflow".to_owned()))?;
    let mut destination_bytes = Vec::new();
    destination_bytes
        .try_reserve_exact(destination_byte_length)
        .map_err(|error| X11Error::Pixel(format!("cannot allocate resize destination: {error}")))?;
    destination_bytes.resize(destination_byte_length, 0);
    let mut destination = Image::from_vec_u8(
        destination_width,
        destination_height,
        destination_bytes,
        PixelType::U8x4,
    )
    .map_err(|error| X11Error::Pixel(format!("invalid resize destination: {error}")))?;
    let algorithm = match filter {
        ResizeFilter::Nearest => ResizeAlg::Nearest,
        ResizeFilter::Lanczos3 => ResizeAlg::Convolution(FilterType::Lanczos3),
    };
    let options = ResizeOptions::new().resize_alg(algorithm);
    Resizer::new()
        .resize(&source_image, &mut destination, Some(&options))
        .map_err(|error| X11Error::Pixel(format!("BGRA8 resize failed: {error}")))?;
    Ok(destination.into_vec())
}

/// Encode tightly packed BGRA8 as a deterministic RGBA8 PNG with bounded output.
pub fn encode_png_bgra8(
    width: u32,
    height: u32,
    bgra: &[u8],
    limits: CaptureImageLimits,
) -> Result<Vec<u8>> {
    let byte_length = validate_bgra8(width, height, bgra, limits, "source")?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(byte_length)
        .map_err(|error| X11Error::Pixel(format!("cannot allocate RGBA conversion: {error}")))?;
    for pixel in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }

    let mut output = LimitedWriter::new(limits.max_encoded_bytes);
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Balanced);
        encoder.set_filter(png::Filter::Adaptive);
        let mut writer = encoder
            .write_header()
            .map_err(|error| X11Error::Pixel(format!("PNG header failed: {error}")))?;
        writer
            .write_image_data(&rgba)
            .map_err(|error| X11Error::Pixel(format!("PNG encoding failed: {error}")))?;
    }
    Ok(output.into_inner())
}

fn validate_bgra8(
    width: u32,
    height: u32,
    bytes: &[u8],
    limits: CaptureImageLimits,
    label: &str,
) -> Result<usize> {
    let expected = validate_dimensions(width, height, limits, label)?
        .checked_mul(4)
        .ok_or_else(|| X11Error::Pixel(format!("{label} BGRA8 byte length overflow")))?;
    let expected = usize::try_from(expected).map_err(|_| {
        X11Error::Pixel(format!(
            "{label} BGRA8 byte length does not fit host address space"
        ))
    })?;
    if bytes.len() != expected {
        return Err(X11Error::Pixel(format!(
            "{label} BGRA8 length mismatch: need {expected} bytes, got {}",
            bytes.len()
        )));
    }
    Ok(expected)
}

fn validate_dimensions(
    width: u32,
    height: u32,
    limits: CaptureImageLimits,
    label: &str,
) -> Result<u64> {
    let pixels = validate_hard_capture_dimensions(width, height, label)?;
    let max_dimension = limits.max_dimension.min(HARD_MAX_DIMENSION);
    if width > max_dimension || height > max_dimension {
        return Err(X11Error::Pixel(format!(
            "{label} dimensions {width}x{height} exceed the {} ceiling",
            max_dimension
        )));
    }
    let max_pixels = limits.max_pixels.min(HARD_MAX_PIXELS);
    if pixels > max_pixels {
        return Err(X11Error::Pixel(format!(
            "{label} pixel count {pixels} exceeds the {} ceiling",
            max_pixels
        )));
    }
    Ok(pixels)
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl LimitedWriter {
    const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit: if limit < HARD_MAX_ENCODED_BYTES {
                limit
            } else {
                HARD_MAX_ENCODED_BYTES
            },
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl io::Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let new_length = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|length| *length <= self.limit)
            .ok_or_else(|| io::Error::other("encoded PNG exceeds configured byte ceiling"))?;
        self.bytes
            .try_reserve(new_length.saturating_sub(self.bytes.len()))
            .map_err(io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{HARD_MAX_ENCODED_BYTES, LimitedWriter};

    #[test]
    fn hostile_encoded_limit_is_clamped_to_hard_ceiling() {
        assert_eq!(LimitedWriter::new(usize::MAX).limit, HARD_MAX_ENCODED_BYTES);
        assert_eq!(LimitedWriter::new(17).limit, 17);
    }
}
