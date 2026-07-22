//! Bounded, explicit screenshot request and evidence contracts.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::geometry::{StrictRect, deserialize_optional_strict_rect};
use crate::window::StrictWindowRef;
use crate::{
    ArtifactRef, CoordinateSpace, DesktopGeneration, DesktopId, Rect, Sha256Digest, Size,
    WindowRect, WindowRef,
};

/// Maximum accepted source or destination dimension.
pub const MAX_SCREENSHOT_DIMENSION: u32 = 8_192;
/// Maximum source or destination pixel count.
pub const MAX_SCREENSHOT_PIXELS: u64 = 16_000_000;
/// Maximum encoded or raw screenshot response.
pub const MAX_SCREENSHOT_BYTES: u64 = 32 * 1_024 * 1_024;
/// HTTP media type for release-one PNG screenshot bodies.
pub const SCREENSHOT_PNG_CONTENT_TYPE: &str = "image/png";
/// HTTP media type for release-one tightly packed raw BGRA bodies.
pub const SCREENSHOT_RAW_BGRA_CONTENT_TYPE: &str = "application/vnd.xenoteer.raw-bgra";

/// What X11 pixels a screenshot represents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScreenshotTarget {
    /// Current visible root framebuffer. Occlusion is represented truthfully.
    Root,
    /// Root-framebuffer crop of a current window rectangle; occluders remain visible.
    WindowVisible {
        /// Generation-bound window identity revalidated near capture.
        window: WindowRef,
        /// Which observed window rectangle defines target-local region coordinates.
        coordinate_space: WindowCaptureSpace,
    },
    /// Core GetImage of the viewable client drawable; obscured backing is undefined.
    WindowDrawable {
        /// Generation-bound window identity revalidated near capture.
        window: WindowRef,
    },
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(rename = "ScreenshotTarget")]
enum StrictScreenshotTarget {
    Root {},
    WindowVisible {
        #[schemars(with = "StrictWindowRef")]
        window: StrictWindowRef,
        coordinate_space: WindowCaptureSpace,
    },
    WindowDrawable {
        #[schemars(with = "StrictWindowRef")]
        window: StrictWindowRef,
    },
}

impl From<StrictScreenshotTarget> for ScreenshotTarget {
    fn from(value: StrictScreenshotTarget) -> Self {
        match value {
            StrictScreenshotTarget::Root {} => Self::Root,
            StrictScreenshotTarget::WindowVisible {
                window,
                coordinate_space,
            } => Self::WindowVisible {
                window: window.into(),
                coordinate_space,
            },
            StrictScreenshotTarget::WindowDrawable { window } => Self::WindowDrawable {
                window: window.into(),
            },
        }
    }
}

fn deserialize_strict_screenshot_target<'de, D>(
    deserializer: D,
) -> Result<ScreenshotTarget, D::Error>
where
    D: serde::Deserializer<'de>,
{
    StrictScreenshotTarget::deserialize(deserializer).map(Into::into)
}

/// Window-relative space used to interpret a `window_visible` region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowCaptureSpace {
    /// Rectangle relative to the ICCCM client window.
    Client,
    /// Rectangle relative to the window-manager frame, including decorations.
    Frame,
}

/// Public screenshot representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScreenshotFormat {
    /// Lossless PNG body.
    Png,
    /// Tightly packed unpremultiplied BGRA8 body with explicit metadata.
    RawBgra,
}

/// Closed deterministic resize-filter set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScreenshotResizeFilter {
    /// Pixel replication for exact diagnostic fixtures.
    Nearest,
    /// High-quality Lanczos3 convolution.
    Lanczos,
}

/// Requested destination size. One omitted dimension preserves aspect ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotScale {
    /// Explicit output width, or aspect-derived width when omitted.
    #[schemars(range(min = 1, max = MAX_SCREENSHOT_DIMENSION))]
    pub width: Option<u32>,
    /// Explicit output height, or aspect-derived height when omitted.
    #[schemars(range(min = 1, max = MAX_SCREENSHOT_DIMENSION))]
    pub height: Option<u32>,
    /// Deterministic resampling algorithm.
    pub filter: ScreenshotResizeFilter,
}

impl ScreenshotScale {
    /// Requires at least one non-zero bounded dimension.
    pub fn validate(self) -> Result<(), CaptureValidationError> {
        if self.width.is_none() && self.height.is_none() {
            return Err(CaptureValidationError::EmptyScale);
        }
        if self
            .width
            .into_iter()
            .chain(self.height)
            .any(|value| value == 0 || value > MAX_SCREENSHOT_DIMENSION)
        {
            return Err(CaptureValidationError::ScaleDimension);
        }
        if let (Some(width), Some(height)) = (self.width, self.height) {
            validate_dimensions(width, height)?;
        }
        Ok(())
    }

    fn output_size(self, source: Size) -> Result<Size, CaptureValidationError> {
        self.validate()?;
        validate_dimensions(source.width(), source.height())?;
        let (width, height) = match (self.width, self.height) {
            (Some(width), Some(height)) => (width, height),
            (Some(width), None) => (
                width,
                scale_preserving_aspect(source.height(), width, source.width())?,
            ),
            (None, Some(height)) => (
                scale_preserving_aspect(source.width(), height, source.height())?,
                height,
            ),
            (None, None) => return Err(CaptureValidationError::EmptyScale),
        };
        validate_dimensions(width, height)?;
        Size::new(width, height).map_err(|_| CaptureValidationError::OutputDimensions)
    }
}

/// One bounded screenshot request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotRequest {
    /// Pixel-source semantics.
    #[serde(deserialize_with = "deserialize_strict_screenshot_target")]
    #[schemars(with = "StrictScreenshotTarget")]
    pub target: ScreenshotTarget,
    /// Optional target-local crop. Root uses root-physical coordinates; window
    /// targets use their declared client/frame coordinate space.
    #[serde(deserialize_with = "deserialize_optional_strict_rect")]
    #[schemars(with = "Option<StrictRect>")]
    pub region: Option<Rect>,
    /// Output encoding.
    pub format: ScreenshotFormat,
    /// Whether the XFIXES cursor is weak-snapshot composited into visible pixels.
    pub include_cursor: bool,
    /// Optional resize applied after capture and cursor composition.
    pub scale: Option<ScreenshotScale>,
    /// Caller-selected response ceiling, which may only tighten the server limit.
    #[schemars(range(min = 1, max = MAX_SCREENSHOT_BYTES))]
    pub max_bytes: Option<u64>,
}

impl ScreenshotRequest {
    /// Revalidates region, cursor semantics, scaling, and caller byte ceiling.
    pub fn validate(&self) -> Result<(), CaptureValidationError> {
        validate_target(&self.target)?;
        if let Some(region) = self.region {
            region
                .validate()
                .map_err(|_| CaptureValidationError::Region)?;
            let size = region.size().map_err(|_| CaptureValidationError::Region)?;
            validate_dimensions(size.width(), size.height())?;
        }
        if matches!(&self.target, ScreenshotTarget::WindowDrawable { .. }) && self.include_cursor {
            return Err(CaptureValidationError::DrawableCursor);
        }
        if let Some(scale) = self.scale {
            scale.validate()?;
        }
        if self
            .max_bytes
            .is_some_and(|bytes| bytes == 0 || bytes > MAX_SCREENSHOT_BYTES)
        {
            return Err(CaptureValidationError::MaximumBytes);
        }
        Ok(())
    }

    /// Performs shape validation and binds a window target to the route's
    /// desktop lifetime. The observation actor must still prove the exact XID
    /// birth is currently live immediately before capture.
    pub fn validate_for_desktop(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), CaptureValidationError> {
        self.validate()?;
        validate_target_scope(&self.target, desktop_id, desktop_generation)
    }

    /// Resolves and validates output dimensions before allocation.
    ///
    /// `source_size` is the bounded source after crop/intersection and before
    /// scaling. The returned size is deterministic: a derived aspect dimension
    /// rounds to nearest, with halves upward, and is never allowed to become zero.
    pub fn validate_for_source(&self, source_size: Size) -> Result<Size, CaptureValidationError> {
        self.validate()?;
        source_size
            .validate()
            .map_err(|_| CaptureValidationError::OutputDimensions)?;
        validate_dimensions(source_size.width(), source_size.height())?;
        let output_size = match self.scale {
            Some(scale) => scale.output_size(source_size)?,
            None => source_size,
        };
        if self.format == ScreenshotFormat::RawBgra {
            let raw_bytes = raw_bgra_length(output_size)?;
            let response_ceiling = self.max_bytes.unwrap_or(MAX_SCREENSHOT_BYTES);
            if raw_bytes > response_ceiling {
                return Err(CaptureValidationError::OutputBytes);
            }
        }
        Ok(output_size)
    }

    /// Returns the coordinate space used for a supplied region.
    #[must_use]
    pub const fn region_coordinate_space(&self) -> CoordinateSpace {
        match &self.target {
            ScreenshotTarget::Root => CoordinateSpace::RootPhysical,
            ScreenshotTarget::WindowVisible {
                coordinate_space: WindowCaptureSpace::Client,
                ..
            }
            | ScreenshotTarget::WindowDrawable { .. } => CoordinateSpace::WindowClient,
            ScreenshotTarget::WindowVisible {
                coordinate_space: WindowCaptureSpace::Frame,
                ..
            } => CoordinateSpace::WindowFrame,
        }
    }
}

/// Evidence about the deliberately non-atomic framebuffer/cursor snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CursorCaptureEvidence {
    /// Whether composition was requested.
    pub requested: bool,
    /// Whether a cursor image intersected and was composited into the result.
    pub composited: bool,
    /// XFIXES cursor serial observed immediately before framebuffer capture.
    pub serial_before: Option<u64>,
    /// XFIXES cursor serial observed immediately after framebuffer capture.
    pub serial_after: Option<u64>,
    /// Whether position or serial changed across the weak snapshot.
    pub moved_during_capture: bool,
}

impl CursorCaptureEvidence {
    /// Prevents cursor evidence from claiming observations when not requested.
    pub fn validate(self) -> Result<(), CaptureValidationError> {
        if !self.requested
            && (self.composited
                || self.serial_before.is_some()
                || self.serial_after.is_some()
                || self.moved_during_capture)
        {
            return Err(CaptureValidationError::CursorEvidence);
        }
        if self.requested && (self.serial_before.is_none() || self.serial_after.is_none()) {
            return Err(CaptureValidationError::CursorEvidence);
        }
        if self
            .serial_before
            .zip(self.serial_after)
            .is_some_and(|(serial_before, serial_after)| {
                serial_before != serial_after && !self.moved_during_capture
            })
        {
            return Err(CaptureValidationError::CursorEvidence);
        }
        Ok(())
    }
}

/// Exact raw output channel and stride declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RawBgraMetadata {
    /// Output pixel dimensions.
    pub size: Size,
    /// Bytes between consecutive rows. Release one requires tightly packed width*4.
    #[schemars(range(min = 4))]
    pub stride_bytes: u32,
    /// Channels are blue, green, red, alpha in byte order.
    pub channel_order: RawChannelOrder,
    /// Alpha is an ordinary unpremultiplied byte.
    pub alpha_mode: RawAlphaMode,
}

impl RawBgraMetadata {
    /// Requires tightly packed BGRA8 length metadata.
    pub fn validate(self) -> Result<(), CaptureValidationError> {
        self.size
            .validate()
            .map_err(|_| CaptureValidationError::OutputDimensions)?;
        validate_dimensions(self.size.width(), self.size.height())?;
        let expected = self
            .size
            .width()
            .checked_mul(4)
            .ok_or(CaptureValidationError::OutputDimensions)?;
        if self.stride_bytes != expected {
            return Err(CaptureValidationError::RawStride);
        }
        Ok(())
    }
}

/// Closed raw channel order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RawChannelOrder {
    /// Blue, green, red, alpha bytes.
    Bgra8,
}

/// Closed raw alpha semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RawAlphaMode {
    /// Unpremultiplied alpha; ordinary root visuals are normalized to 255.
    Unpremultiplied,
}

/// Truthful limitation of the X11 pixel source used for one screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScreenshotSourceLimitation {
    /// The root framebuffer contains only pixels visible at capture time.
    RootVisibleFramebuffer,
    /// A window-shaped root crop remains affected by overlapping windows.
    WindowVisibleIncludesOccluders,
    /// Obscured client-drawable pixels depend on X11 backing-store behavior.
    WindowDrawableObscuredUndefined,
}

/// How screenshot bytes are delivered outside routine JSON/event messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "delivery", rename_all = "snake_case")]
pub enum ScreenshotDelivery {
    /// Bytes are the HTTP response body, not embedded in this JSON object.
    InlineBody {
        /// Exact body length.
        #[schemars(range(min = 1, max = MAX_SCREENSHOT_BYTES))]
        content_length: u64,
    },
    /// Bytes are retained in a private authenticated artifact.
    Artifact {
        /// Purpose-bound screenshot artifact.
        artifact: ArtifactRef,
    },
}

/// Safe metadata returned alongside screenshot bytes or an artifact reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScreenshotResult {
    /// Semantics actually captured.
    pub target: ScreenshotTarget,
    /// Exact pre-scale source rectangle expressed in root-physical coordinates.
    pub source_region: WindowRect,
    /// Exact pre-scale source dimensions, equal to `source_region.rect`'s size.
    pub source_size: Size,
    /// Deliberate visibility/occlusion limitation of the selected X11 source.
    pub limitation: ScreenshotSourceLimitation,
    /// Actual output format.
    pub format: ScreenshotFormat,
    /// Actual output pixel dimensions.
    pub size: Size,
    /// Present exactly for raw BGRA output.
    pub raw: Option<RawBgraMetadata>,
    /// Weak-snapshot cursor evidence.
    pub cursor: CursorCaptureEvidence,
    /// Identity of the exact encoded/raw response body.
    pub sha256: Sha256Digest,
    /// Body or artifact delivery.
    pub delivery: ScreenshotDelivery,
}

impl ScreenshotResult {
    /// Revalidates output shape without inspecting content bytes.
    pub fn validate(&self) -> Result<(), CaptureValidationError> {
        validate_target(&self.target)?;
        self.source_region
            .validate()
            .map_err(|_| CaptureValidationError::SourceEvidence)?;
        self.source_size
            .validate()
            .map_err(|_| CaptureValidationError::SourceEvidence)?;
        validate_dimensions(self.source_size.width(), self.source_size.height())?;
        let source_region_size = self
            .source_region
            .rect
            .size()
            .map_err(|_| CaptureValidationError::SourceEvidence)?;
        let limitation_matches = matches!(
            (&self.target, self.limitation),
            (
                ScreenshotTarget::Root,
                ScreenshotSourceLimitation::RootVisibleFramebuffer
            ) | (
                ScreenshotTarget::WindowVisible { .. },
                ScreenshotSourceLimitation::WindowVisibleIncludesOccluders
            ) | (
                ScreenshotTarget::WindowDrawable { .. },
                ScreenshotSourceLimitation::WindowDrawableObscuredUndefined
            )
        );
        if self.source_region.coordinate_space != CoordinateSpace::RootPhysical
            || source_region_size != self.source_size
            || !limitation_matches
        {
            return Err(CaptureValidationError::SourceEvidence);
        }
        self.size
            .validate()
            .map_err(|_| CaptureValidationError::OutputDimensions)?;
        validate_dimensions(self.size.width(), self.size.height())?;
        match (self.format, self.raw) {
            (ScreenshotFormat::RawBgra, Some(raw)) if raw.size == self.size => raw.validate()?,
            (ScreenshotFormat::Png, None) => {}
            _ => return Err(CaptureValidationError::OutputShape),
        }
        self.cursor.validate()?;
        if matches!(&self.target, ScreenshotTarget::WindowDrawable { .. }) && self.cursor.requested
        {
            return Err(CaptureValidationError::DrawableCursor);
        }
        Sha256Digest::new(self.sha256.as_str()).map_err(|_| CaptureValidationError::OutputShape)?;
        let delivered_length = match &self.delivery {
            ScreenshotDelivery::InlineBody { content_length }
                if *content_length > 0 && *content_length <= MAX_SCREENSHOT_BYTES =>
            {
                *content_length
            }
            ScreenshotDelivery::Artifact { artifact }
                if artifact.purpose == crate::ArtifactPurpose::Screenshot =>
            {
                artifact
                    .validate()
                    .map_err(|_| CaptureValidationError::Artifact)?;
                let expected_content_type = match self.format {
                    ScreenshotFormat::Png => SCREENSHOT_PNG_CONTENT_TYPE,
                    ScreenshotFormat::RawBgra => SCREENSHOT_RAW_BGRA_CONTENT_TYPE,
                };
                if artifact.content_type.as_str() != expected_content_type {
                    return Err(CaptureValidationError::Artifact);
                }
                if artifact.sha256.as_str() != self.sha256.as_str() {
                    return Err(CaptureValidationError::Artifact);
                }
                artifact.content_length
            }
            _ => return Err(CaptureValidationError::Delivery),
        };
        if self.format == ScreenshotFormat::RawBgra {
            let expected_length = u64::from(self.size.width())
                .checked_mul(u64::from(self.size.height()))
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or(CaptureValidationError::OutputDimensions)?;
            if delivered_length != expected_length {
                return Err(CaptureValidationError::Delivery);
            }
        }
        Ok(())
    }

    /// Performs shape validation and binds all window/artifact references to a
    /// route desktop lifetime. Live authorization, artifact ownership/expiry,
    /// and current window-birth checks remain execution-layer responsibilities.
    pub fn validate_for_desktop(
        &self,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<(), CaptureValidationError> {
        self.validate()?;
        validate_target_scope(&self.target, desktop_id, desktop_generation)?;
        if let ScreenshotDelivery::Artifact { artifact } = &self.delivery
            && (artifact.desktop_id != desktop_id
                || artifact.desktop_generation != desktop_generation)
        {
            return Err(CaptureValidationError::ReferenceScope);
        }
        Ok(())
    }

    /// Cross-validates result evidence against its admitted request and resolved
    /// pre-scale source size.
    pub fn validate_against(
        &self,
        request: &ScreenshotRequest,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        source_size: Size,
    ) -> Result<(), CaptureValidationError> {
        request.validate_for_desktop(desktop_id, desktop_generation)?;
        self.validate_for_desktop(desktop_id, desktop_generation)?;
        if self.target != request.target
            || self.source_size != source_size
            || self.format != request.format
            || self.cursor.requested != request.include_cursor
            || self.size != request.validate_for_source(source_size)?
        {
            return Err(CaptureValidationError::RequestMismatch);
        }
        match (&request.target, request.region) {
            (ScreenshotTarget::Root, Some(region)) if self.source_region.rect != region => {
                return Err(CaptureValidationError::RequestMismatch);
            }
            (ScreenshotTarget::Root, None)
                if self.source_region.rect.origin().x() != 0
                    || self.source_region.rect.origin().y() != 0 =>
            {
                return Err(CaptureValidationError::RequestMismatch);
            }
            _ => {}
        }
        if request
            .max_bytes
            .is_some_and(|maximum| self.delivered_length() > maximum)
        {
            return Err(CaptureValidationError::OutputBytes);
        }
        Ok(())
    }

    fn delivered_length(&self) -> u64 {
        match &self.delivery {
            ScreenshotDelivery::InlineBody { content_length } => *content_length,
            ScreenshotDelivery::Artifact { artifact } => artifact.content_length,
        }
    }
}

fn validate_target(target: &ScreenshotTarget) -> Result<(), CaptureValidationError> {
    match target {
        ScreenshotTarget::Root => Ok(()),
        ScreenshotTarget::WindowVisible { window, .. }
        | ScreenshotTarget::WindowDrawable { window } => window
            .validate()
            .map_err(|_| CaptureValidationError::Target),
    }
}

fn validate_target_scope(
    target: &ScreenshotTarget,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
) -> Result<(), CaptureValidationError> {
    match target {
        ScreenshotTarget::Root => Ok(()),
        ScreenshotTarget::WindowVisible { window, .. }
        | ScreenshotTarget::WindowDrawable { window }
            if window.desktop_id == desktop_id
                && window.desktop_generation == desktop_generation =>
        {
            Ok(())
        }
        ScreenshotTarget::WindowVisible { .. } | ScreenshotTarget::WindowDrawable { .. } => {
            Err(CaptureValidationError::ReferenceScope)
        }
    }
}

fn scale_preserving_aspect(
    source_dimension: u32,
    requested_dimension: u32,
    source_basis: u32,
) -> Result<u32, CaptureValidationError> {
    let numerator = u64::from(source_dimension)
        .checked_mul(u64::from(requested_dimension))
        .ok_or(CaptureValidationError::OutputDimensions)?;
    let denominator = u64::from(source_basis);
    let rounded = numerator
        .checked_add(denominator / 2)
        .ok_or(CaptureValidationError::OutputDimensions)?
        / denominator;
    u32::try_from(rounded.max(1)).map_err(|_| CaptureValidationError::OutputDimensions)
}

fn raw_bgra_length(size: Size) -> Result<u64, CaptureValidationError> {
    u64::from(size.width())
        .checked_mul(u64::from(size.height()))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(CaptureValidationError::OutputDimensions)
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), CaptureValidationError> {
    if width == 0
        || height == 0
        || width > MAX_SCREENSHOT_DIMENSION
        || height > MAX_SCREENSHOT_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_SCREENSHOT_PIXELS
    {
        return Err(CaptureValidationError::OutputDimensions);
    }
    Ok(())
}

/// Invalid screenshot request or result metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CaptureValidationError {
    /// Crop geometry is empty, overflowing, or exceeds capture bounds.
    #[error("screenshot region is invalid")]
    Region,
    /// A scale contains neither width nor height.
    #[error("screenshot scale requires at least one dimension")]
    EmptyScale,
    /// A scale dimension exceeds the release-one bound.
    #[error("screenshot scale dimension is invalid")]
    ScaleDimension,
    /// Source or output dimensions exceed release-one bounds.
    #[error("screenshot dimensions exceed release-one bounds")]
    OutputDimensions,
    /// A format-specific output body exceeds the admitted byte ceiling.
    #[error("screenshot output exceeds its byte ceiling")]
    OutputBytes,
    /// Core drawable capture cannot truthfully include a cursor.
    #[error("window drawable capture cannot include the cursor")]
    DrawableCursor,
    /// The caller byte ceiling is zero or above the server ceiling.
    #[error("screenshot maximum byte limit is invalid")]
    MaximumBytes,
    /// Cursor evidence contradicts the request.
    #[error("screenshot cursor evidence is inconsistent")]
    CursorEvidence,
    /// Raw output is not tightly packed BGRA8.
    #[error("raw screenshot stride is invalid")]
    RawStride,
    /// Source rectangle, dimensions, coordinate space, and limitation conflict.
    #[error("screenshot source evidence is inconsistent")]
    SourceEvidence,
    /// Format-specific result fields are inconsistent.
    #[error("screenshot output shape is inconsistent")]
    OutputShape,
    /// Referenced screenshot artifact is invalid.
    #[error("screenshot artifact reference is invalid")]
    Artifact,
    /// Delivery metadata is empty, excessive, or purpose-inconsistent.
    #[error("screenshot delivery metadata is invalid")]
    Delivery,
    /// A generation-bound capture target is invalid.
    #[error("screenshot target reference is invalid")]
    Target,
    /// A capture window or artifact belongs to another desktop lifetime.
    #[error("screenshot reference belongs to another desktop lifetime")]
    ReferenceScope,
    /// Result target, format, dimensions, or cursor request differs from admission.
    #[error("screenshot result does not match its admitted request")]
    RequestMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_ref(
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
    ) -> Result<WindowRef, Box<dyn std::error::Error>> {
        Ok(WindowRef {
            desktop_id,
            desktop_generation,
            xid: 7,
            observed_generation: 1,
            identity_hash: crate::WindowIdentityHash::new("a".repeat(64))?,
        })
    }

    #[test]
    fn scale_requires_a_bounded_destination() {
        assert_eq!(
            ScreenshotScale {
                width: None,
                height: None,
                filter: ScreenshotResizeFilter::Lanczos,
            }
            .validate(),
            Err(CaptureValidationError::EmptyScale)
        );
        assert!(
            ScreenshotScale {
                width: Some(800),
                height: None,
                filter: ScreenshotResizeFilter::Lanczos,
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn omitted_scale_dimension_uses_deterministic_aspect_rounding()
    -> Result<(), Box<dyn std::error::Error>> {
        let request = ScreenshotRequest {
            target: ScreenshotTarget::Root,
            region: None,
            format: ScreenshotFormat::Png,
            include_cursor: false,
            scale: Some(ScreenshotScale {
                width: Some(2),
                height: None,
                filter: ScreenshotResizeFilter::Lanczos,
            }),
            max_bytes: None,
        };
        assert_eq!(
            request.validate_for_source(Size::new(4, 3)?)?,
            Size::new(2, 2)?
        );
        Ok(())
    }

    #[test]
    fn root_and_drawable_cursor_semantics_are_distinct() {
        let root = ScreenshotRequest {
            target: ScreenshotTarget::Root,
            region: Rect::new(0, 0, 100, 100).ok(),
            format: ScreenshotFormat::Png,
            include_cursor: true,
            scale: None,
            max_bytes: Some(1_024),
        };
        assert!(root.validate().is_ok());
        assert_eq!(
            root.region_coordinate_space(),
            CoordinateSpace::RootPhysical
        );
    }

    #[test]
    fn cursor_evidence_cannot_claim_unrequested_observations() {
        assert_eq!(
            CursorCaptureEvidence {
                requested: false,
                composited: true,
                serial_before: None,
                serial_after: None,
                moved_during_capture: false,
            }
            .validate(),
            Err(CaptureValidationError::CursorEvidence)
        );
    }

    #[test]
    fn cursor_serial_change_requires_movement_evidence() {
        assert_eq!(
            CursorCaptureEvidence {
                requested: true,
                composited: true,
                serial_before: Some(1),
                serial_after: Some(2),
                moved_during_capture: false,
            }
            .validate(),
            Err(CaptureValidationError::CursorEvidence)
        );
    }

    #[test]
    fn raw_output_is_rejected_before_an_excessive_allocation()
    -> Result<(), Box<dyn std::error::Error>> {
        let request = ScreenshotRequest {
            target: ScreenshotTarget::Root,
            region: None,
            format: ScreenshotFormat::RawBgra,
            include_cursor: false,
            scale: None,
            max_bytes: None,
        };
        assert_eq!(
            request.validate_for_source(Size::new(4_000, 4_000)?),
            Err(CaptureValidationError::OutputBytes)
        );
        Ok(())
    }

    #[test]
    fn request_context_rejects_a_window_from_another_desktop()
    -> Result<(), Box<dyn std::error::Error>> {
        let route_desktop = DesktopId::new();
        let generation = DesktopGeneration::new();
        let request = ScreenshotRequest {
            target: ScreenshotTarget::WindowVisible {
                window: window_ref(DesktopId::new(), generation)?,
                coordinate_space: WindowCaptureSpace::Client,
            },
            region: None,
            format: ScreenshotFormat::Png,
            include_cursor: false,
            scale: None,
            max_bytes: None,
        };
        assert_eq!(
            request.validate_for_desktop(route_desktop, generation),
            Err(CaptureValidationError::ReferenceScope)
        );
        Ok(())
    }

    #[test]
    fn result_is_cross_validated_against_its_admitted_request()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let desktop_generation = DesktopGeneration::new();
        let request = ScreenshotRequest {
            target: ScreenshotTarget::Root,
            region: None,
            format: ScreenshotFormat::Png,
            include_cursor: false,
            scale: None,
            max_bytes: Some(1_024),
        };
        let result = ScreenshotResult {
            target: ScreenshotTarget::Root,
            source_region: WindowRect::new(
                CoordinateSpace::RootPhysical,
                Rect::new(0, 0, 10, 10)?,
            )?,
            source_size: Size::new(10, 10)?,
            limitation: ScreenshotSourceLimitation::RootVisibleFramebuffer,
            format: ScreenshotFormat::Png,
            size: Size::new(10, 10)?,
            raw: None,
            cursor: CursorCaptureEvidence {
                requested: true,
                composited: false,
                serial_before: Some(1),
                serial_after: Some(1),
                moved_during_capture: false,
            },
            sha256: Sha256Digest::new("00".repeat(32))?,
            delivery: ScreenshotDelivery::InlineBody { content_length: 10 },
        };
        assert!(result.validate().is_ok());
        let mut inconsistent_source = result.clone();
        inconsistent_source.source_region.coordinate_space = CoordinateSpace::WindowClient;
        assert_eq!(
            inconsistent_source.validate(),
            Err(CaptureValidationError::SourceEvidence)
        );
        let mut inconsistent_limitation = result.clone();
        inconsistent_limitation.limitation =
            ScreenshotSourceLimitation::WindowVisibleIncludesOccluders;
        assert_eq!(
            inconsistent_limitation.validate(),
            Err(CaptureValidationError::SourceEvidence)
        );
        assert_eq!(
            result.validate_against(&request, desktop_id, desktop_generation, Size::new(10, 10)?,),
            Err(CaptureValidationError::RequestMismatch)
        );
        Ok(())
    }

    #[test]
    fn raw_metadata_requires_tightly_packed_bgra() -> Result<(), Box<dyn std::error::Error>> {
        let size = Size::new(10, 3)?;
        assert!(
            RawBgraMetadata {
                size,
                stride_bytes: 40,
                channel_order: RawChannelOrder::Bgra8,
                alpha_mode: RawAlphaMode::Unpremultiplied,
            }
            .validate()
            .is_ok()
        );
        assert_eq!(
            RawBgraMetadata {
                size,
                stride_bytes: 39,
                channel_order: RawChannelOrder::Bgra8,
                alpha_mode: RawAlphaMode::Unpremultiplied,
            }
            .validate(),
            Err(CaptureValidationError::RawStride)
        );
        Ok(())
    }
}
