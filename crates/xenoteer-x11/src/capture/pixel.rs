//! Pure X11 ZPixmap validation and conversion.

use std::fmt;

use super::image::validate_hard_capture_dimensions;
use crate::{Result, X11Error};

/// Byte order declared by the X server setup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ByteOrder {
    /// Low-order byte appears first in each pixel unit.
    LeastSignificantFirst,
    /// High-order byte appears first in each pixel unit.
    MostSignificantFirst,
}

/// Core X11 visual class associated with a GetImage visual.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelVisualClass {
    /// Pixel components are literal RGB bitfields and can be decoded from masks.
    TrueColor,
    /// RGB mask fields select colormap indices rather than literal components.
    DirectColor,
    /// Pixel values index a mutable shared colormap.
    PseudoColor,
    /// Pixel values index a fixed color map.
    StaticColor,
    /// Pixel values index a mutable gray map.
    GrayScale,
    /// Pixel values index a fixed gray map.
    StaticGray,
    /// Server supplied a visual class outside the core protocol's known set.
    Unknown(u8),
}

impl PixelVisualClass {
    /// Convert the core protocol's numeric visual class into a typed value.
    #[must_use]
    pub const fn from_wire_value(value: u8) -> Self {
        match value {
            0 => Self::StaticGray,
            1 => Self::GrayScale,
            2 => Self::StaticColor,
            3 => Self::PseudoColor,
            4 => Self::TrueColor,
            5 => Self::DirectColor,
            other => Self::Unknown(other),
        }
    }

    /// Core protocol numeric visual class.
    #[must_use]
    pub const fn wire_value(self) -> u8 {
        match self {
            Self::StaticGray => 0,
            Self::GrayScale => 1,
            Self::StaticColor => 2,
            Self::PseudoColor => 3,
            Self::TrueColor => 4,
            Self::DirectColor => 5,
            Self::Unknown(value) => value,
        }
    }
}

/// Storage and visual metadata required to interpret a ZPixmap reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelFormat {
    /// Visual class that defines the meaning of masks/pixel values.
    pub visual_class: PixelVisualClass,
    /// Meaningful visual depth.
    pub depth: u8,
    /// Storage bits allocated to one pixel.
    pub bits_per_pixel: u8,
    /// Scanline alignment in bits.
    pub scanline_pad: u8,
    /// Pixel byte order.
    pub byte_order: ByteOrder,
    /// Visual red mask.
    pub red_mask: u32,
    /// Visual green mask.
    pub green_mask: u32,
    /// Visual blue mask.
    pub blue_mask: u32,
}

/// Validated raw ZPixmap data.
#[derive(Clone, Eq, PartialEq)]
pub struct RawImage {
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    data: Vec<u8>,
}

impl fmt::Debug for RawImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("format", &self.format)
            .field("data_len", &self.data.len())
            .finish()
    }
}

impl RawImage {
    /// Validate dimensions, format, masks, stride, and reply length before any
    /// pixel indexing can occur.
    pub fn new(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> Result<Self> {
        validate_hard_capture_dimensions(width, height, "raw image")?;
        if !matches!(format.bits_per_pixel, 16 | 24 | 32) {
            return Err(X11Error::Pixel(format!(
                "unsupported bits-per-pixel {}",
                format.bits_per_pixel
            )));
        }
        if !matches!(format.scanline_pad, 8 | 16 | 32) {
            return Err(X11Error::Pixel(format!(
                "unsupported scanline pad {}",
                format.scanline_pad
            )));
        }
        validate_masks(format)?;
        let stride = checked_stride(width, format.bits_per_pixel, format.scanline_pad)?;
        let required = stride
            .checked_mul(usize::try_from(height).map_err(|_| {
                X11Error::Pixel("image height does not fit host address space".to_owned())
            })?)
            .ok_or_else(|| X11Error::Pixel("image byte length overflow".to_owned()))?;
        let wire_aligned = required
            .checked_add(3)
            .map(|length| length & !3)
            .ok_or_else(|| X11Error::Pixel("wire-aligned image byte length overflow".to_owned()))?;
        if data.len() < required {
            return Err(X11Error::Pixel(format!(
                "short image reply: need {required} bytes, got {}",
                data.len()
            )));
        }
        if data.len() > wire_aligned {
            return Err(X11Error::Pixel(format!(
                "excess image reply data: expected at most {wire_aligned} bytes including X11 wire padding, got {}",
                data.len()
            )));
        }
        Ok(Self {
            width,
            height,
            stride,
            format,
            data,
        })
    }

    /// Width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Bytes between consecutive scanline starts.
    #[must_use]
    pub const fn stride(&self) -> usize {
        self.stride
    }
}

/// Convert a validated ZPixmap to opaque, unpremultiplied BGRA8.
pub fn decode_bgra8(image: &RawImage) -> Result<Vec<u8>> {
    let pixels = usize::try_from(image.width)
        .ok()
        .and_then(|width| {
            usize::try_from(image.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| X11Error::Pixel("output pixel count overflow".to_owned()))?;
    let output_byte_length = pixels
        .checked_mul(4)
        .ok_or_else(|| X11Error::Pixel("output byte length overflow".to_owned()))?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(output_byte_length)
        .map_err(|error| X11Error::Pixel(format!("cannot allocate decoded BGRA8: {error}")))?;
    let bytes_per_pixel = usize::from(image.format.bits_per_pixel / 8);
    for y in 0..usize::try_from(image.height)
        .map_err(|_| X11Error::Pixel("height conversion failed".to_owned()))?
    {
        let row_start = y
            .checked_mul(image.stride)
            .ok_or_else(|| X11Error::Pixel("row offset overflow".to_owned()))?;
        for x in 0..usize::try_from(image.width)
            .map_err(|_| X11Error::Pixel("width conversion failed".to_owned()))?
        {
            let start = row_start
                .checked_add(
                    x.checked_mul(bytes_per_pixel)
                        .ok_or_else(|| X11Error::Pixel("pixel offset overflow".to_owned()))?,
                )
                .ok_or_else(|| X11Error::Pixel("pixel offset overflow".to_owned()))?;
            let end = start
                .checked_add(bytes_per_pixel)
                .ok_or_else(|| X11Error::Pixel("pixel end overflow".to_owned()))?;
            let bytes = image
                .data
                .get(start..end)
                .ok_or_else(|| X11Error::Pixel("validated image became short".to_owned()))?;
            let pixel = read_pixel(bytes, image.format.byte_order);
            output.push(scale_channel(pixel, image.format.blue_mask)?);
            output.push(scale_channel(pixel, image.format.green_mask)?);
            output.push(scale_channel(pixel, image.format.red_mask)?);
            output.push(255);
        }
    }
    Ok(output)
}

fn checked_stride(width: u32, bits_per_pixel: u8, scanline_pad: u8) -> Result<usize> {
    let bits = u64::from(width)
        .checked_mul(u64::from(bits_per_pixel))
        .ok_or_else(|| X11Error::Pixel("scanline bit length overflow".to_owned()))?;
    let pad = u64::from(scanline_pad);
    let padded_bits = bits
        .checked_add(pad - 1)
        .map(|value| value / pad * pad)
        .ok_or_else(|| X11Error::Pixel("scanline padding overflow".to_owned()))?;
    usize::try_from(padded_bits / 8)
        .map_err(|_| X11Error::Pixel("stride does not fit host address space".to_owned()))
}

fn validate_masks(format: PixelFormat) -> Result<()> {
    if format.visual_class != PixelVisualClass::TrueColor {
        return Err(X11Error::UnsupportedVisualClass {
            visual_class: format.visual_class.wire_value(),
        });
    }
    if format.depth == 0 || format.depth > format.bits_per_pixel {
        return Err(X11Error::Pixel(format!(
            "visual depth {} is incompatible with {} bits-per-pixel",
            format.depth, format.bits_per_pixel
        )));
    }
    let masks = [format.red_mask, format.green_mask, format.blue_mask];
    if masks.contains(&0) {
        return Err(X11Error::Pixel(
            "RGB visual masks must be nonzero".to_owned(),
        ));
    }
    if format.red_mask & format.green_mask != 0
        || format.red_mask & format.blue_mask != 0
        || format.green_mask & format.blue_mask != 0
    {
        return Err(X11Error::Pixel("RGB visual masks overlap".to_owned()));
    }
    let storage_mask = if format.bits_per_pixel == 32 {
        u32::MAX
    } else {
        (1_u32 << format.bits_per_pixel) - 1
    };
    for mask in masks {
        if mask & !storage_mask != 0 {
            return Err(X11Error::Pixel(
                "visual mask exceeds pixel storage".to_owned(),
            ));
        }
        let shifted = mask >> mask.trailing_zeros();
        if shifted & shifted.wrapping_add(1) != 0 {
            return Err(X11Error::Pixel(
                "non-contiguous visual mask is unsupported".to_owned(),
            ));
        }
    }
    let color_bits = (format.red_mask | format.green_mask | format.blue_mask).count_ones();
    if color_bits != u32::from(format.depth) {
        return Err(X11Error::Pixel(format!(
            "RGB masks describe {color_bits} significant bits but visual depth is {}",
            format.depth
        )));
    }
    Ok(())
}

fn read_pixel(bytes: &[u8], order: ByteOrder) -> u32 {
    match order {
        ByteOrder::LeastSignificantFirst => bytes
            .iter()
            .enumerate()
            .fold(0_u32, |value, (index, byte)| {
                value | (u32::from(*byte) << (index * 8))
            }),
        ByteOrder::MostSignificantFirst => bytes
            .iter()
            .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte)),
    }
}

fn scale_channel(pixel: u32, mask: u32) -> Result<u8> {
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    let value = (pixel & mask) >> shift;
    let scaled = u64::from(value)
        .checked_mul(255)
        .and_then(|candidate| candidate.checked_add(u64::from(maximum / 2)))
        .map(|candidate| candidate / u64::from(maximum))
        .ok_or_else(|| X11Error::Pixel("channel scaling overflow".to_owned()))?;
    u8::try_from(scaled).map_err(|_| X11Error::Pixel("scaled channel exceeds u8".to_owned()))
}
