//! Bounded property retrieval and pure decoders.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, ConnectionExt as _, GetPropertyReply, Window};
use xenoteer_protocol::{MAX_WINDOW_ATOMS, MAX_WINDOW_TEXT_BYTES, WindowClass, WindowText};

use crate::{Result, X11Error};

/// Hard ceiling for one retained X11 property value.
pub const MAX_PROPERTY_BYTES: usize = 64 * 1024;

/// A bounded property reply suitable for pure decoding and fuzzing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawProperty {
    /// Actual X11 type atom, or zero when absent.
    pub type_atom: Atom,
    /// X11 element width: 0, 8, 16, or 32.
    pub format: u8,
    /// Retained value bytes in native byte order after x11rb decoding.
    pub value: Vec<u8>,
    /// Server-reported bytes not returned by the bounded request.
    pub bytes_after: u32,
    /// Whether local enforcement also had to discard bytes.
    pub locally_truncated: bool,
}

impl RawProperty {
    /// Construct bounded raw input. Oversized test or adapter input is
    /// truncated before retention.
    #[must_use]
    pub fn new(
        type_atom: Atom,
        format: u8,
        mut value: Vec<u8>,
        bytes_after: u32,
        max_bytes: usize,
    ) -> Self {
        let max_bytes = max_bytes.min(MAX_PROPERTY_BYTES);
        let locally_truncated = value.len() > max_bytes;
        value.truncate(max_bytes);
        Self {
            type_atom,
            format,
            value,
            bytes_after,
            locally_truncated,
        }
    }

    /// Whether the property did not exist at reply time.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        self.type_atom == 0 && self.format == 0
    }
}

/// Non-fatal evidence produced while decoding an untrusted property.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PropertyWarning {
    /// More bytes existed than the bounded read retained.
    Truncated,
    /// Invalid text bytes required replacement.
    LossyText,
    /// Actual property type did not match the contract.
    UnexpectedType,
    /// Element width did not match the contract.
    UnexpectedFormat,
    /// Payload cardinality or terminators were malformed.
    Malformed,
    /// Numeric atom was outside the reviewed fixed identity inventory.
    UnknownAtom,
}

/// A recoverable decoded value and its bounded warning set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedProperty<T> {
    /// Decoded value. `None` means absent or unrecoverably malformed.
    pub value: Option<T>,
    /// Deduplicated decode warnings.
    pub warnings: Vec<PropertyWarning>,
}

impl<T> DecodedProperty<T> {
    fn absent() -> Self {
        Self {
            value: None,
            warnings: Vec::new(),
        }
    }
}

/// Issue exactly one bounded `GetProperty` request.
///
/// `long_length` is derived from the checked byte ceiling before the request;
/// a multi-part fetch is intentionally never attempted.
pub fn read_property_bounded<C: Connection>(
    connection: &C,
    window: Window,
    property: Atom,
    expected_type: Atom,
    max_bytes: usize,
) -> Result<RawProperty> {
    if max_bytes == 0 || max_bytes > MAX_PROPERTY_BYTES {
        return Err(X11Error::InvalidSetup(
            "observation property byte ceiling is invalid",
        ));
    }
    let long_length = u32::try_from(max_bytes.div_ceil(4))
        .map_err(|_| X11Error::InvalidSetup("observation property byte ceiling overflow"))?;
    let reply = connection
        .get_property(false, window, property, expected_type, 0, long_length)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    Ok(raw_from_reply(reply, max_bytes))
}

fn raw_from_reply(reply: GetPropertyReply, max_bytes: usize) -> RawProperty {
    RawProperty::new(
        reply.type_,
        reply.format,
        reply.value,
        reply.bytes_after,
        max_bytes,
    )
}

/// Decode an `UTF8_STRING/8` value into the bounded protocol text type.
#[must_use]
pub fn decode_utf8_string(raw: &RawProperty, utf8_type: Atom) -> DecodedProperty<WindowText> {
    decode_text(raw, utf8_type, TextEncoding::Utf8)
}

/// Decode an ICCCM `STRING/8` value as ISO-8859-1.
#[must_use]
pub fn decode_string(raw: &RawProperty, string_type: Atom) -> DecodedProperty<WindowText> {
    decode_text(raw, string_type, TextEncoding::Latin1)
}

#[derive(Clone, Copy)]
enum TextEncoding {
    Utf8,
    Latin1,
}

fn decode_text(
    raw: &RawProperty,
    expected_type: Atom,
    encoding: TextEncoding,
) -> DecodedProperty<WindowText> {
    let Some(mut warnings) = validate_shape(raw, expected_type, 8) else {
        return DecodedProperty::absent();
    };
    if warnings.contains(&PropertyWarning::UnexpectedType)
        || warnings.contains(&PropertyWarning::UnexpectedFormat)
    {
        return DecodedProperty {
            value: None,
            warnings,
        };
    }

    let (mut text, lossy) = match encoding {
        TextEncoding::Utf8 => match std::str::from_utf8(&raw.value) {
            Ok(text) => (text.to_owned(), false),
            Err(_) => (String::from_utf8_lossy(&raw.value).into_owned(), true),
        },
        TextEncoding::Latin1 => (
            raw.value.iter().map(|byte| char::from(*byte)).collect(),
            false,
        ),
    };
    if lossy {
        push_warning(&mut warnings, PropertyWarning::LossyText);
    }
    if truncate_utf8(&mut text, MAX_WINDOW_TEXT_BYTES) {
        push_warning(&mut warnings, PropertyWarning::Truncated);
    }
    let value = WindowText::new(text, lossy).ok();
    if value.is_none() {
        push_warning(&mut warnings, PropertyWarning::Malformed);
    }
    DecodedProperty { value, warnings }
}

/// Decode a bounded `ATOM/32` vector.
#[must_use]
pub fn decode_atom_list(raw: &RawProperty, atom_type: Atom) -> DecodedProperty<Vec<Atom>> {
    decode_u32_list(raw, atom_type, MAX_WINDOW_ATOMS)
}

/// Decode a bounded `CARDINAL/32` vector.
#[must_use]
pub fn decode_cardinals(raw: &RawProperty, cardinal_type: Atom) -> DecodedProperty<Vec<u32>> {
    decode_u32_list(raw, cardinal_type, MAX_WINDOW_ATOMS)
}

/// Decode a bounded `WINDOW/32` vector.
#[must_use]
pub fn decode_window_list(raw: &RawProperty, window_type: Atom) -> DecodedProperty<Vec<Window>> {
    decode_u32_list(raw, window_type, MAX_WINDOW_ATOMS)
}

pub(crate) fn decode_u32_list(
    raw: &RawProperty,
    expected_type: Atom,
    max_values: usize,
) -> DecodedProperty<Vec<u32>> {
    let Some(mut warnings) = validate_shape(raw, expected_type, 32) else {
        return DecodedProperty::absent();
    };
    if warnings.contains(&PropertyWarning::UnexpectedType)
        || warnings.contains(&PropertyWarning::UnexpectedFormat)
    {
        return DecodedProperty {
            value: None,
            warnings,
        };
    }
    if !raw.value.len().is_multiple_of(4) {
        push_warning(&mut warnings, PropertyWarning::Malformed);
    }
    let mut values: Vec<_> = raw
        .value
        .chunks_exact(4)
        .take(max_values)
        .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    if raw.value.len() / 4 > max_values {
        values.truncate(max_values);
        push_warning(&mut warnings, PropertyWarning::Truncated);
    }
    DecodedProperty {
        value: Some(values),
        warnings,
    }
}

/// Decode the two NUL-delimited ICCCM `WM_CLASS` components.
#[must_use]
pub fn decode_wm_class(raw: &RawProperty, string_type: Atom) -> DecodedProperty<WindowClass> {
    let Some(mut warnings) = validate_shape(raw, string_type, 8) else {
        return DecodedProperty::absent();
    };
    if warnings.contains(&PropertyWarning::UnexpectedType)
        || warnings.contains(&PropertyWarning::UnexpectedFormat)
    {
        return DecodedProperty {
            value: None,
            warnings,
        };
    }

    let parts: Vec<_> = raw.value.split(|byte| *byte == 0).take(4).collect();
    let canonical = raw.value.last() == Some(&0)
        && parts.len() == 3
        && parts.last().is_some_and(|part| part.is_empty());
    if !canonical {
        push_warning(&mut warnings, PropertyWarning::Malformed);
    }
    let instance = parts
        .first()
        .and_then(|part| decode_class_component(part, &mut warnings));
    let class = parts
        .get(1)
        .and_then(|part| decode_class_component(part, &mut warnings));
    let value = if instance.is_some() || class.is_some() {
        Some(WindowClass { instance, class })
    } else {
        push_warning(&mut warnings, PropertyWarning::Malformed);
        None
    };
    DecodedProperty { value, warnings }
}

fn decode_class_component(bytes: &[u8], warnings: &mut Vec<PropertyWarning>) -> Option<WindowText> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: String = bytes.iter().map(|byte| char::from(*byte)).collect();
    if truncate_utf8(&mut value, MAX_WINDOW_TEXT_BYTES) {
        push_warning(warnings, PropertyWarning::Truncated);
    }
    WindowText::new(value, false).ok()
}

fn validate_shape(
    raw: &RawProperty,
    expected_type: Atom,
    expected_format: u8,
) -> Option<Vec<PropertyWarning>> {
    if raw.is_absent() {
        return None;
    }
    let mut warnings = Vec::with_capacity(3);
    if raw.bytes_after != 0 || raw.locally_truncated {
        push_warning(&mut warnings, PropertyWarning::Truncated);
    }
    if raw.type_atom != expected_type {
        push_warning(&mut warnings, PropertyWarning::UnexpectedType);
    }
    if raw.format != expected_format {
        push_warning(&mut warnings, PropertyWarning::UnexpectedFormat);
    }
    Some(warnings)
}

fn truncate_utf8(value: &mut String, max_bytes: usize) -> bool {
    if value.len() <= max_bytes {
        return false;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    true
}

fn push_warning(warnings: &mut Vec<PropertyWarning>, warning: PropertyWarning) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTF8: Atom = 1;
    const STRING: Atom = 2;
    const ATOM: Atom = 3;

    #[test]
    fn malformed_u32_tail_is_warned_and_not_read() {
        let raw = RawProperty::new(ATOM, 32, vec![1, 0, 0, 0, 9], 0, 32);
        let decoded = decode_atom_list(&raw, ATOM);
        assert_eq!(decoded.value, Some(vec![1]));
        assert!(decoded.warnings.contains(&PropertyWarning::Malformed));
    }

    #[test]
    fn wrong_type_and_format_are_not_interpreted() {
        let raw = RawProperty::new(STRING, 16, b"not utf8 shape".to_vec(), 0, 64);
        let decoded = decode_utf8_string(&raw, UTF8);
        assert!(decoded.value.is_none());
        assert_eq!(
            decoded.warnings,
            vec![
                PropertyWarning::UnexpectedType,
                PropertyWarning::UnexpectedFormat
            ]
        );
    }

    #[test]
    fn invalid_utf8_is_bounded_and_marked_lossy() {
        let raw = RawProperty::new(UTF8, 8, vec![b'a', 0xff, b'b'], 0, 64);
        let decoded = decode_utf8_string(&raw, UTF8);
        assert!(decoded.value.is_some());
        if let Some(text) = decoded.value {
            assert_eq!(text.value, "a\u{fffd}b");
            assert!(text.lossy);
        }
        assert!(decoded.warnings.contains(&PropertyWarning::LossyText));
    }

    #[test]
    fn oversized_input_is_truncated_before_retention() {
        let raw = RawProperty::new(UTF8, 8, vec![b'x'; 128], 0, 16);
        assert_eq!(raw.value.len(), 16);
        assert!(raw.locally_truncated);
        let decoded = decode_utf8_string(&raw, UTF8);
        assert!(decoded.warnings.contains(&PropertyWarning::Truncated));
    }

    #[test]
    fn wm_class_recovers_components_but_warns_on_missing_terminator() {
        let raw = RawProperty::new(STRING, 8, b"terminal\0XTerm".to_vec(), 0, 64);
        let decoded = decode_wm_class(&raw, STRING);
        assert!(decoded.value.is_some());
        if let Some(class) = decoded.value {
            assert_eq!(
                class.instance.map(|value| value.value).as_deref(),
                Some("terminal")
            );
            assert_eq!(
                class.class.map(|value| value.value).as_deref(),
                Some("XTerm")
            );
        }
        assert!(decoded.warnings.contains(&PropertyWarning::Malformed));
    }

    #[test]
    fn absent_property_is_not_malformed() {
        let decoded = decode_string(&RawProperty::new(0, 0, Vec::new(), 0, 64), STRING);
        assert!(decoded.value.is_none());
        assert!(decoded.warnings.is_empty());
    }
}
