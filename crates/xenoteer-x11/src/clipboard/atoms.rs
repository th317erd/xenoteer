//! Fixed clipboard atom inventory; no caller-directed interning.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _};
use xenoteer_protocol::SelectionName;

use super::RawClipboardTarget;
use crate::{Result, X11Error};

pub(super) const PRIVATE_PROPERTY_COUNT: usize = 8;

#[derive(Clone, Debug)]
pub(super) struct ClipboardAtoms {
    pub clipboard: Atom,
    pub primary: Atom,
    pub targets: Atom,
    pub timestamp: Atom,
    pub multiple: Atom,
    pub incr: Atom,
    pub atom_pair: Atom,
    pub utf8_string: Atom,
    pub text_plain_utf8: Atom,
    pub text_plain: Atom,
    pub string: Atom,
    pub atom: Atom,
    pub cardinal: Atom,
    pub image_png: Atom,
    pub application_octet_stream: Atom,
    pub time_probe: Atom,
    pub private_properties: [Atom; PRIVATE_PROPERTY_COUNT],
}

impl ClipboardAtoms {
    pub fn intern<C: Connection>(connection: &C) -> Result<Self> {
        let mut private_properties = [0; PRIVATE_PROPERTY_COUNT];
        for (slot, name) in private_properties.iter_mut().zip([
            b"_XENOTEER_SELECTION_0".as_slice(),
            b"_XENOTEER_SELECTION_1".as_slice(),
            b"_XENOTEER_SELECTION_2".as_slice(),
            b"_XENOTEER_SELECTION_3".as_slice(),
            b"_XENOTEER_SELECTION_4".as_slice(),
            b"_XENOTEER_SELECTION_5".as_slice(),
            b"_XENOTEER_SELECTION_6".as_slice(),
            b"_XENOTEER_SELECTION_7".as_slice(),
        ]) {
            *slot = intern(connection, name)?;
        }
        Ok(Self {
            clipboard: intern(connection, b"CLIPBOARD")?,
            primary: u32::from(AtomEnum::PRIMARY),
            targets: intern(connection, b"TARGETS")?,
            timestamp: intern(connection, b"TIMESTAMP")?,
            multiple: intern(connection, b"MULTIPLE")?,
            incr: intern(connection, b"INCR")?,
            atom_pair: intern(connection, b"ATOM_PAIR")?,
            utf8_string: intern(connection, b"UTF8_STRING")?,
            text_plain_utf8: intern(connection, b"text/plain;charset=utf-8")?,
            text_plain: intern(connection, b"text/plain")?,
            string: u32::from(AtomEnum::STRING),
            atom: u32::from(AtomEnum::ATOM),
            cardinal: u32::from(AtomEnum::CARDINAL),
            image_png: intern(connection, b"image/png")?,
            application_octet_stream: intern(connection, b"application/octet-stream")?,
            time_probe: intern(connection, b"_XENOTEER_SERVER_TIME")?,
            private_properties,
        })
    }

    pub const fn selection(&self, selection: SelectionName) -> Atom {
        match selection {
            SelectionName::Clipboard => self.clipboard,
            SelectionName::Primary => self.primary,
        }
    }

    pub const fn target(&self, target: RawClipboardTarget) -> Atom {
        match target {
            RawClipboardTarget::Targets => self.targets,
            RawClipboardTarget::Timestamp => self.timestamp,
            RawClipboardTarget::Multiple => self.multiple,
            RawClipboardTarget::Utf8String => self.utf8_string,
            RawClipboardTarget::TextPlainUtf8 => self.text_plain_utf8,
            RawClipboardTarget::TextPlain => self.text_plain,
            RawClipboardTarget::String => self.string,
            RawClipboardTarget::ImagePng => self.image_png,
            RawClipboardTarget::ApplicationOctetStream => self.application_octet_stream,
        }
    }

    pub fn identify_target(&self, atom: Atom) -> Option<RawClipboardTarget> {
        [
            RawClipboardTarget::Targets,
            RawClipboardTarget::Timestamp,
            RawClipboardTarget::Multiple,
            RawClipboardTarget::Utf8String,
            RawClipboardTarget::TextPlainUtf8,
            RawClipboardTarget::TextPlain,
            RawClipboardTarget::String,
            RawClipboardTarget::ImagePng,
            RawClipboardTarget::ApplicationOctetStream,
        ]
        .into_iter()
        .find(|target| self.target(*target) == atom)
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            clipboard: 100,
            primary: 101,
            targets: 102,
            timestamp: 103,
            multiple: 104,
            incr: 105,
            atom_pair: 106,
            utf8_string: 107,
            text_plain_utf8: 108,
            text_plain: 109,
            string: 110,
            atom: 111,
            cardinal: 112,
            image_png: 113,
            application_octet_stream: 114,
            time_probe: 115,
            private_properties: [200, 201, 202, 203, 204, 205, 206, 207],
        }
    }
}

fn intern<C: Connection>(connection: &C, name: &[u8]) -> Result<Atom> {
    connection
        .intern_atom(false, name)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| X11Error::Reply(error.to_string()))
}
