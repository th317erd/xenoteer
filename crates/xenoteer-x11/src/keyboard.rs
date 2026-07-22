//! XKB-backed physical-key model and feature-independent keyboard domain types.
//!
//! The default crate keeps the native dependency disabled. Enabling
//! `native-xkbcommon` constructs the model from the live X server, tracks its
//! mapping generation, and resolves only the closed key vocabulary declared
//! here. Text resolution is deliberately limited to direct, current-layout
//! Unicode keysyms: this module never guesses a Compose/dead-key sequence or
//! changes a locking modifier/group.

use std::{fmt, str::FromStr};

/// Why a keyboard model is unavailable in a portable build.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardModelAvailability {
    /// Native model support was compiled in.
    Available,
    /// The optional native dependency was not compiled in.
    FeatureDisabled,
}

/// Return whether this crate was built with the native server model.
#[must_use]
pub const fn availability() -> KeyboardModelAvailability {
    if cfg!(feature = "native-xkbcommon") {
        KeyboardModelAvailability::Available
    } else {
        KeyboardModelAvailability::FeatureDisabled
    }
}

/// One concrete server keycode/layout/level to keysym mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SymbolMapping {
    /// X server keycode.
    pub keycode: u32,
    /// XKB layout index.
    pub layout: u32,
    /// XKB level index.
    pub level: u32,
    /// Raw X11 keysym.
    pub keysym: u32,
}

/// Closed, versioned physical-key vocabulary accepted by the X11 adapter.
///
/// Both side-specific modifiers and the five canonical generic modifiers are
/// intentional. Generic modifiers are concretized with
/// [`ModifierSideDefaults`] before XKB lookup.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NamedKey {
    /// Backspace.
    Backspace,
    /// Horizontal tab.
    Tab,
    /// Return/enter.
    Enter,
    /// Escape.
    Escape,
    /// Space.
    Space,
    /// Insert.
    Insert,
    /// Delete.
    Delete,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Left arrow.
    ArrowLeft,
    /// Up arrow.
    ArrowUp,
    /// Right arrow.
    ArrowRight,
    /// Down arrow.
    ArrowDown,
    /// Configured default shift side.
    Shift,
    /// Configured default control side.
    Control,
    /// Configured default alt side.
    Alt,
    /// Configured default meta side.
    Meta,
    /// Configured default super side.
    Super,
    /// Left shift.
    ShiftLeft,
    /// Right shift.
    ShiftRight,
    /// Left control.
    ControlLeft,
    /// Right control.
    ControlRight,
    /// Left alt.
    AltLeft,
    /// Right alt.
    AltRight,
    /// Left meta.
    MetaLeft,
    /// Right meta.
    MetaRight,
    /// Left super (Windows/Command-style) key.
    SuperLeft,
    /// Right super (Windows/Command-style) key.
    SuperRight,
    /// Left hyper.
    HyperLeft,
    /// Right hyper.
    HyperRight,
    /// ISO level-three shift (normally AltGr).
    AltGraph,
    /// Caps lock.
    CapsLock,
    /// Num lock.
    NumLock,
    /// Scroll lock.
    ScrollLock,
    /// Print screen.
    PrintScreen,
    /// Pause/break.
    Pause,
    /// Context menu.
    ContextMenu,
    /// Function key 1.
    F1,
    /// Function key 2.
    F2,
    /// Function key 3.
    F3,
    /// Function key 4.
    F4,
    /// Function key 5.
    F5,
    /// Function key 6.
    F6,
    /// Function key 7.
    F7,
    /// Function key 8.
    F8,
    /// Function key 9.
    F9,
    /// Function key 10.
    F10,
    /// Function key 11.
    F11,
    /// Function key 12.
    F12,
    /// Function key 13.
    F13,
    /// Function key 14.
    F14,
    /// Function key 15.
    F15,
    /// Function key 16.
    F16,
    /// Function key 17.
    F17,
    /// Function key 18.
    F18,
    /// Function key 19.
    F19,
    /// Function key 20.
    F20,
    /// Function key 21.
    F21,
    /// Function key 22.
    F22,
    /// Function key 23.
    F23,
    /// Function key 24.
    F24,
}

impl NamedKey {
    /// Canonical protocol spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Backspace => "Backspace",
            Self::Tab => "Tab",
            Self::Enter => "Enter",
            Self::Escape => "Escape",
            Self::Space => "Space",
            Self::Insert => "Insert",
            Self::Delete => "Delete",
            Self::Home => "Home",
            Self::End => "End",
            Self::PageUp => "PageUp",
            Self::PageDown => "PageDown",
            Self::ArrowLeft => "ArrowLeft",
            Self::ArrowUp => "ArrowUp",
            Self::ArrowRight => "ArrowRight",
            Self::ArrowDown => "ArrowDown",
            Self::Shift => "Shift",
            Self::Control => "Control",
            Self::Alt => "Alt",
            Self::Meta => "Meta",
            Self::Super => "Super",
            Self::ShiftLeft => "ShiftLeft",
            Self::ShiftRight => "ShiftRight",
            Self::ControlLeft => "ControlLeft",
            Self::ControlRight => "ControlRight",
            Self::AltLeft => "AltLeft",
            Self::AltRight => "AltRight",
            Self::MetaLeft => "MetaLeft",
            Self::MetaRight => "MetaRight",
            Self::SuperLeft => "SuperLeft",
            Self::SuperRight => "SuperRight",
            Self::HyperLeft => "HyperLeft",
            Self::HyperRight => "HyperRight",
            Self::AltGraph => "AltGraph",
            Self::CapsLock => "CapsLock",
            Self::NumLock => "NumLock",
            Self::ScrollLock => "ScrollLock",
            Self::PrintScreen => "PrintScreen",
            Self::Pause => "Pause",
            Self::ContextMenu => "ContextMenu",
            Self::F1 => "F1",
            Self::F2 => "F2",
            Self::F3 => "F3",
            Self::F4 => "F4",
            Self::F5 => "F5",
            Self::F6 => "F6",
            Self::F7 => "F7",
            Self::F8 => "F8",
            Self::F9 => "F9",
            Self::F10 => "F10",
            Self::F11 => "F11",
            Self::F12 => "F12",
            Self::F13 => "F13",
            Self::F14 => "F14",
            Self::F15 => "F15",
            Self::F16 => "F16",
            Self::F17 => "F17",
            Self::F18 => "F18",
            Self::F19 => "F19",
            Self::F20 => "F20",
            Self::F21 => "F21",
            Self::F22 => "F22",
            Self::F23 => "F23",
            Self::F24 => "F24",
        }
    }

    /// Whether this named key changes modifier state rather than representing
    /// an ordinary non-modifier key.
    #[must_use]
    pub const fn is_modifier(self) -> bool {
        matches!(
            self,
            Self::Shift
                | Self::Control
                | Self::Alt
                | Self::Meta
                | Self::Super
                | Self::ShiftLeft
                | Self::ShiftRight
                | Self::ControlLeft
                | Self::ControlRight
                | Self::AltLeft
                | Self::AltRight
                | Self::MetaLeft
                | Self::MetaRight
                | Self::SuperLeft
                | Self::SuperRight
                | Self::HyperLeft
                | Self::HyperRight
                | Self::AltGraph
                | Self::CapsLock
                | Self::NumLock
                | Self::ScrollLock
        )
    }

    /// Whether this key is a locking modifier that automation must never add
    /// as a temporary level selector.
    #[must_use]
    pub const fn is_lock(self) -> bool {
        matches!(self, Self::CapsLock | Self::NumLock | Self::ScrollLock)
    }

    /// Concretize one of the five generic modifier names to its configured
    /// side. Side-specific names and non-modifiers are returned unchanged.
    #[must_use]
    pub const fn concretize(self, defaults: ModifierSideDefaults) -> Self {
        match self {
            Self::Shift => defaults.shift.choose(Self::ShiftLeft, Self::ShiftRight),
            Self::Control => defaults
                .control
                .choose(Self::ControlLeft, Self::ControlRight),
            Self::Alt => defaults.alt.choose(Self::AltLeft, Self::AltRight),
            Self::Meta => defaults.meta.choose(Self::MetaLeft, Self::MetaRight),
            Self::Super => defaults.super_key.choose(Self::SuperLeft, Self::SuperRight),
            concrete => concrete,
        }
    }
}

/// Side selected when a canonical generic modifier is resolved.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModifierSide {
    /// Prefer the left key.
    #[default]
    Left,
    /// Prefer the right key.
    Right,
}

impl ModifierSide {
    const fn choose<T: Copy>(self, left: T, right: T) -> T {
        match self {
            Self::Left => left,
            Self::Right => right,
        }
    }
}

/// Concrete-side policy for the five canonical generic modifier names.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModifierSideDefaults {
    /// Side used for `Shift`.
    pub shift: ModifierSide,
    /// Side used for `Control`.
    pub control: ModifierSide,
    /// Side used for `Alt`.
    pub alt: ModifierSide,
    /// Side used for `Meta`.
    pub meta: ModifierSide,
    /// Side used for `Super`.
    pub super_key: ModifierSide,
}

impl fmt::Display for NamedKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An unknown named-key spelling.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("unknown named key `{0}`")]
pub struct NamedKeyParseError(String);

impl FromStr for NamedKey {
    type Err = NamedKeyParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let key = match value {
            "Backspace" => Self::Backspace,
            "Tab" => Self::Tab,
            "Enter" => Self::Enter,
            "Escape" => Self::Escape,
            "Space" => Self::Space,
            "Insert" => Self::Insert,
            "Delete" => Self::Delete,
            "Home" => Self::Home,
            "End" => Self::End,
            "PageUp" => Self::PageUp,
            "PageDown" => Self::PageDown,
            "ArrowLeft" => Self::ArrowLeft,
            "ArrowUp" => Self::ArrowUp,
            "ArrowRight" => Self::ArrowRight,
            "ArrowDown" => Self::ArrowDown,
            "Shift" => Self::Shift,
            "Control" => Self::Control,
            "Alt" => Self::Alt,
            "Meta" => Self::Meta,
            "Super" => Self::Super,
            "ShiftLeft" => Self::ShiftLeft,
            "ShiftRight" => Self::ShiftRight,
            "ControlLeft" => Self::ControlLeft,
            "ControlRight" => Self::ControlRight,
            "AltLeft" => Self::AltLeft,
            "AltRight" => Self::AltRight,
            "MetaLeft" => Self::MetaLeft,
            "MetaRight" => Self::MetaRight,
            "SuperLeft" => Self::SuperLeft,
            "SuperRight" => Self::SuperRight,
            "HyperLeft" => Self::HyperLeft,
            "HyperRight" => Self::HyperRight,
            "AltGraph" => Self::AltGraph,
            "CapsLock" => Self::CapsLock,
            "NumLock" => Self::NumLock,
            "ScrollLock" => Self::ScrollLock,
            "PrintScreen" => Self::PrintScreen,
            "Pause" => Self::Pause,
            "ContextMenu" => Self::ContextMenu,
            "F1" => Self::F1,
            "F2" => Self::F2,
            "F3" => Self::F3,
            "F4" => Self::F4,
            "F5" => Self::F5,
            "F6" => Self::F6,
            "F7" => Self::F7,
            "F8" => Self::F8,
            "F9" => Self::F9,
            "F10" => Self::F10,
            "F11" => Self::F11,
            "F12" => Self::F12,
            "F13" => Self::F13,
            "F14" => Self::F14,
            "F15" => Self::F15,
            "F16" => Self::F16,
            "F17" => Self::F17,
            "F18" => Self::F18,
            "F19" => Self::F19,
            "F20" => Self::F20,
            "F21" => Self::F21,
            "F22" => Self::F22,
            "F23" => Self::F23,
            "F24" => Self::F24,
            _ => return Err(NamedKeyParseError(value.to_owned())),
        };
        Ok(key)
    }
}

/// Caller-facing physical key identifier after protocol validation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KeyIdentifier {
    /// One member of the closed named-key vocabulary.
    Named(NamedKey),
    /// One direct Unicode scalar value.
    Scalar(char),
    /// Advanced raw core keycode. Capability checks happen above this crate.
    Raw(u8),
}

impl From<xenoteer_protocol::KeyboardNamedKey> for NamedKey {
    fn from(value: xenoteer_protocol::KeyboardNamedKey) -> Self {
        use xenoteer_protocol::KeyboardNamedKey as Wire;
        match value {
            Wire::Backspace => Self::Backspace,
            Wire::Tab => Self::Tab,
            Wire::Enter => Self::Enter,
            Wire::Escape => Self::Escape,
            Wire::Space => Self::Space,
            Wire::Insert => Self::Insert,
            Wire::Delete => Self::Delete,
            Wire::Home => Self::Home,
            Wire::End => Self::End,
            Wire::PageUp => Self::PageUp,
            Wire::PageDown => Self::PageDown,
            Wire::ArrowLeft => Self::ArrowLeft,
            Wire::ArrowUp => Self::ArrowUp,
            Wire::ArrowRight => Self::ArrowRight,
            Wire::ArrowDown => Self::ArrowDown,
            Wire::Shift => Self::Shift,
            Wire::Control => Self::Control,
            Wire::Alt => Self::Alt,
            Wire::Meta => Self::Meta,
            Wire::Super => Self::Super,
            Wire::ShiftLeft => Self::ShiftLeft,
            Wire::ShiftRight => Self::ShiftRight,
            Wire::ControlLeft => Self::ControlLeft,
            Wire::ControlRight => Self::ControlRight,
            Wire::AltLeft => Self::AltLeft,
            Wire::AltRight => Self::AltRight,
            Wire::MetaLeft => Self::MetaLeft,
            Wire::MetaRight => Self::MetaRight,
            Wire::SuperLeft => Self::SuperLeft,
            Wire::SuperRight => Self::SuperRight,
            Wire::HyperLeft => Self::HyperLeft,
            Wire::HyperRight => Self::HyperRight,
            Wire::AltGraph => Self::AltGraph,
            Wire::CapsLock => Self::CapsLock,
            Wire::NumLock => Self::NumLock,
            Wire::ScrollLock => Self::ScrollLock,
            Wire::PrintScreen => Self::PrintScreen,
            Wire::Pause => Self::Pause,
            Wire::ContextMenu => Self::ContextMenu,
            Wire::F1 => Self::F1,
            Wire::F2 => Self::F2,
            Wire::F3 => Self::F3,
            Wire::F4 => Self::F4,
            Wire::F5 => Self::F5,
            Wire::F6 => Self::F6,
            Wire::F7 => Self::F7,
            Wire::F8 => Self::F8,
            Wire::F9 => Self::F9,
            Wire::F10 => Self::F10,
            Wire::F11 => Self::F11,
            Wire::F12 => Self::F12,
            Wire::F13 => Self::F13,
            Wire::F14 => Self::F14,
            Wire::F15 => Self::F15,
            Wire::F16 => Self::F16,
            Wire::F17 => Self::F17,
            Wire::F18 => Self::F18,
            Wire::F19 => Self::F19,
            Wire::F20 => Self::F20,
            Wire::F21 => Self::F21,
            Wire::F22 => Self::F22,
            Wire::F23 => Self::F23,
            Wire::F24 => Self::F24,
        }
    }
}

impl From<xenoteer_protocol::KeyboardKeyIdentifier> for KeyIdentifier {
    fn from(value: xenoteer_protocol::KeyboardKeyIdentifier) -> Self {
        match value {
            xenoteer_protocol::KeyboardKeyIdentifier::Named { name } => Self::Named(name.into()),
            xenoteer_protocol::KeyboardKeyIdentifier::Scalar { value } => Self::Scalar(value),
            xenoteer_protocol::KeyboardKeyIdentifier::Raw { keycode } => Self::Raw(keycode),
        }
    }
}

/// Semantic promise requested from the keyboard resolver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardResolutionIntent {
    /// Resolve the requested physical key while allowing modifiers that this
    /// actor verifiably owns to remain held as part of a chord.
    PhysicalKey,
    /// Resolve an exact Unicode character. Any active shortcut modifier is a
    /// conflict, including one owned by this actor.
    ExactText,
}

/// Maximum actor-owned keycodes admitted to one resolution context.
pub const MAX_RESOLUTION_OWNED_KEYS: usize = 248;

/// Invalid actor-owned evidence supplied to a resolution context.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ResolutionContextError {
    /// More keys were supplied than can exist in the core keycode range.
    #[error("resolution context contains {actual} owned keys; maximum is 248")]
    TooManyOwnedKeys {
        /// Supplied key count.
        actual: usize,
    },
    /// Core keycodes below eight are structurally invalid.
    #[error("resolution context contains invalid physical keycode {keycode}")]
    InvalidOwnedKeycode {
        /// Invalid keycode.
        keycode: u8,
    },
    /// Duplicate ownership evidence is rejected rather than silently merged.
    #[error("resolution context repeats owned physical keycode {keycode}")]
    DuplicateOwnedKeycode {
        /// Repeated keycode.
        keycode: u8,
    },
}

/// Bounded, non-forgeable-as-bitmap resolution evidence supplied by the input
/// actor from its owned-key state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyboardResolutionContext {
    intent: KeyboardResolutionIntent,
    actor_owned: [u8; 32],
    actor_owned_count: usize,
}

impl KeyboardResolutionContext {
    /// Construct a checked context from the actor's complete owned-key list.
    pub fn new(
        intent: KeyboardResolutionIntent,
        actor_owned_keycodes: &[u8],
    ) -> std::result::Result<Self, ResolutionContextError> {
        if actor_owned_keycodes.len() > MAX_RESOLUTION_OWNED_KEYS {
            return Err(ResolutionContextError::TooManyOwnedKeys {
                actual: actor_owned_keycodes.len(),
            });
        }
        let mut actor_owned = [0_u8; 32];
        for keycode in actor_owned_keycodes {
            if *keycode < 8 {
                return Err(ResolutionContextError::InvalidOwnedKeycode { keycode: *keycode });
            }
            let byte = usize::from(*keycode / 8);
            let bit = 1_u8 << (*keycode % 8);
            if actor_owned[byte] & bit != 0 {
                return Err(ResolutionContextError::DuplicateOwnedKeycode { keycode: *keycode });
            }
            actor_owned[byte] |= bit;
        }
        Ok(Self {
            intent,
            actor_owned,
            actor_owned_count: actor_owned_keycodes.len(),
        })
    }

    /// Exact-text context with no actor-owned keys.
    #[must_use]
    pub const fn exact_text() -> Self {
        Self {
            intent: KeyboardResolutionIntent::ExactText,
            actor_owned: [0; 32],
            actor_owned_count: 0,
        }
    }

    /// Physical-key context with no actor-owned keys.
    #[must_use]
    pub const fn physical_key() -> Self {
        Self {
            intent: KeyboardResolutionIntent::PhysicalKey,
            actor_owned: [0; 32],
            actor_owned_count: 0,
        }
    }

    /// Requested semantic promise.
    #[must_use]
    pub const fn intent(&self) -> KeyboardResolutionIntent {
        self.intent
    }

    /// Number of distinct actor-owned keycodes supplied.
    #[must_use]
    pub const fn actor_owned_count(&self) -> usize {
        self.actor_owned_count
    }

    #[cfg(any(feature = "native-xkbcommon", test))]
    fn is_actor_owned(&self, keycode: u8) -> bool {
        self.actor_owned[usize::from(keycode / 8)] & (1 << (keycode % 8)) != 0
    }
}

/// The eight modifier groups in the core X11 modifier map.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum CoreModifier {
    /// Shift.
    Shift = 0,
    /// Lock.
    Lock = 1,
    /// Control.
    Control = 2,
    /// Mod1.
    Mod1 = 3,
    /// Mod2.
    Mod2 = 4,
    /// Mod3.
    Mod3 = 5,
    /// Mod4.
    Mod4 = 6,
    /// Mod5.
    Mod5 = 7,
}

impl CoreModifier {
    const ALL: [Self; 8] = [
        Self::Shift,
        Self::Lock,
        Self::Control,
        Self::Mod1,
        Self::Mod2,
        Self::Mod3,
        Self::Mod4,
        Self::Mod5,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// Compact set of core X11 modifier groups.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct CoreModifierMask(u8);

impl CoreModifierMask {
    /// Empty modifier set.
    pub const EMPTY: Self = Self(0);

    /// Build from the core X11 eight-bit mask.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// Return the core X11 eight-bit mask.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether this set contains `modifier`.
    #[must_use]
    pub const fn contains(self, modifier: CoreModifier) -> bool {
        self.0 & modifier.bit() != 0
    }

    /// Return this set with `modifier` included.
    #[must_use]
    pub const fn with(self, modifier: CoreModifier) -> Self {
        Self(self.0 | modifier.bit())
    }

    /// Number of modifier groups in the set.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// Whether no modifier group is present.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Return groups present here but absent from `other`.
    #[must_use]
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

/// Parsed `GetModifierMapping` reply, with protocol padding removed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModifierMap {
    groups: [Vec<u8>; 8],
}

/// Malformed modifier-map data returned by an X server or test double.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModifierMapError {
    /// Core protocol replies contain the same number of slots for all eight
    /// groups, so the flattened length must be divisible by eight.
    #[error("modifier mapping length {actual} is not divisible by eight")]
    InvalidLength {
        /// Flattened keycode count.
        actual: usize,
    },
}

impl ModifierMap {
    /// Parse the core protocol's eight equal-width, group-major rows.
    pub fn from_flat_keycodes(keycodes: &[u8]) -> Result<Self, ModifierMapError> {
        if !keycodes.len().is_multiple_of(8) {
            return Err(ModifierMapError::InvalidLength {
                actual: keycodes.len(),
            });
        }
        let width = keycodes.len() / 8;
        let groups = std::array::from_fn(|index| {
            keycodes[index * width..(index + 1) * width]
                .iter()
                .copied()
                .filter(|keycode| *keycode != 0)
                .collect()
        });
        Ok(Self { groups })
    }

    /// Non-padding keycodes assigned to one core modifier group, in server
    /// order.
    #[must_use]
    pub fn keycodes(&self, modifier: CoreModifier) -> &[u8] {
        &self.groups[modifier.index()]
    }

    /// All core modifier groups containing `keycode`.
    #[must_use]
    pub fn modifiers_for_key(&self, keycode: u8) -> CoreModifierMask {
        CoreModifier::ALL
            .into_iter()
            .filter(|modifier| self.keycodes(*modifier).contains(&keycode))
            .fold(CoreModifierMask::EMPTY, CoreModifierMask::with)
    }

    /// Whether the keycode participates in any core modifier group.
    #[must_use]
    pub fn is_modifier_key(&self, keycode: u8) -> bool {
        !self.modifiers_for_key(keycode).is_empty()
    }
}

/// Complete core `QueryKeymap` pressed-key bitmap.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueryKeymapSnapshot {
    bytes: [u8; 32],
}

impl QueryKeymapSnapshot {
    /// Construct from the exact 256-bit core reply.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self { bytes }
    }

    /// Return the unmodified core reply bytes.
    #[must_use]
    pub const fn bytes(self) -> [u8; 32] {
        self.bytes
    }

    /// Whether the server reports `keycode` down.
    #[must_use]
    pub const fn is_pressed(self, keycode: u8) -> bool {
        let byte = self.bytes[(keycode / 8) as usize];
        byte & (1 << (keycode % 8)) != 0
    }

    /// Pressed keycodes in ascending order.
    #[must_use]
    pub fn pressed_keycodes(self) -> Vec<u8> {
        (u8::MIN..=u8::MAX)
            .filter(|keycode| self.is_pressed(*keycode))
            .collect()
    }
}

/// One physical provider selected for a required core modifier group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedModifier {
    /// Core modifier group.
    modifier: CoreModifier,
    /// Deterministically selected safe momentary keycode.
    keycode: u8,
    /// Whether the server already reports this modifier effectively active.
    already_active: bool,
}

impl ResolvedModifier {
    /// Core modifier group.
    #[must_use]
    pub const fn modifier(self) -> CoreModifier {
        self.modifier
    }

    /// Concrete safe momentary provider.
    #[must_use]
    pub const fn keycode(self) -> u8 {
        self.keycode
    }

    /// Whether a freshly observed, exclusively actor-owned depressed provider
    /// already supplies this modifier.
    #[must_use]
    pub const fn already_active(self) -> bool {
        self.already_active
    }
}

/// Modifier ownership/interference facts derived from fresh QueryKeymap, the
/// server modifier map, and XKB depressed/latched/locked state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModifierOwnershipEvidence {
    active_shortcut: CoreModifierMask,
    actor_owned_depressed: CoreModifierMask,
    conflicting: CoreModifierMask,
}

impl ModifierOwnershipEvidence {
    /// Significant shortcut modifier groups currently effective.
    #[must_use]
    pub const fn active_shortcut(self) -> CoreModifierMask {
        self.active_shortcut
    }

    /// Groups exempted only because every down provider is actor-owned,
    /// at least one such provider is down, and the group is neither latched nor
    /// locked.
    #[must_use]
    pub const fn actor_owned_depressed(self) -> CoreModifierMask {
        self.actor_owned_depressed
    }

    /// Active shortcut groups that make this resolution unsafe.
    #[must_use]
    pub const fn conflicting(self) -> CoreModifierMask {
        self.conflicting
    }
}

/// Captured physical binding that remains valid only for its mapping generation.
#[derive(Clone, Eq, PartialEq)]
pub struct ResolvedKeyBinding {
    /// Original identifier.
    identifier: KeyIdentifier,
    /// Concrete configured name for named keys, including the selected side
    /// for a generic modifier.
    concrete_named_key: Option<NamedKey>,
    /// Concrete server keycode.
    keycode: u8,
    /// XKB layout used for validation.
    layout: u32,
    /// XKB level used for validation.
    level: u32,
    /// Exact emitted keysym.
    keysym: u32,
    /// Safe modifier providers needed by this binding.
    required_modifiers: Vec<ResolvedModifier>,
    /// Mapping generation under which resolution occurred.
    generation: u64,
    /// Whether the resolved key itself belongs to a core modifier group.
    is_modifier: bool,
    intent: KeyboardResolutionIntent,
    modifier_evidence: ModifierOwnershipEvidence,
    model_instance: u64,
}

impl fmt::Debug for ResolvedKeyBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identifier_kind = match self.identifier {
            KeyIdentifier::Named(_) => "named",
            KeyIdentifier::Scalar(_) => "scalar",
            KeyIdentifier::Raw(_) => "raw",
        };
        formatter
            .debug_struct("ResolvedKeyBinding")
            .field("identifier_kind", &identifier_kind)
            .field("keycode", &self.keycode)
            .field("layout", &self.layout)
            .field("level", &self.level)
            .field("required_modifiers", &self.required_modifiers)
            .field("generation", &self.generation)
            .field("is_modifier", &self.is_modifier)
            .field("intent", &self.intent)
            .field("modifier_evidence", &self.modifier_evidence)
            .field("model_instance", &self.model_instance)
            .finish_non_exhaustive()
    }
}

impl ResolvedKeyBinding {
    /// Original identifier.
    #[must_use]
    pub const fn identifier(&self) -> KeyIdentifier {
        self.identifier
    }

    /// Concrete configured named key, when applicable.
    #[must_use]
    pub const fn concrete_named_key(&self) -> Option<NamedKey> {
        self.concrete_named_key
    }

    /// Concrete server keycode.
    #[must_use]
    pub const fn keycode(&self) -> u8 {
        self.keycode
    }

    /// Captured XKB layout.
    #[must_use]
    pub const fn layout(&self) -> u32 {
        self.layout
    }

    /// Captured XKB level.
    #[must_use]
    pub const fn level(&self) -> u32 {
        self.level
    }

    /// Exact captured keysym.
    #[must_use]
    pub const fn keysym(&self) -> u32 {
        self.keysym
    }

    /// Required safe physical modifier providers.
    #[must_use]
    pub fn required_modifiers(&self) -> &[ResolvedModifier] {
        &self.required_modifiers
    }

    /// Mapping generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the resolved key itself is a modifier.
    #[must_use]
    pub const fn is_modifier(&self) -> bool {
        self.is_modifier
    }

    /// Semantic promise used during resolution.
    #[must_use]
    pub const fn intent(&self) -> KeyboardResolutionIntent {
        self.intent
    }

    /// Fresh modifier ownership/interference evidence.
    #[must_use]
    pub const fn modifier_evidence(&self) -> ModifierOwnershipEvidence {
        self.modifier_evidence
    }
}

/// Serializable XKB state components used to validate candidate simulations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardStateSnapshot {
    /// Physically depressed XKB modifier mask.
    pub depressed_modifiers: u32,
    /// Latched XKB modifier mask.
    pub latched_modifiers: u32,
    /// Locked XKB modifier mask.
    pub locked_modifiers: u32,
    /// Physically depressed layout/group component.
    pub depressed_layout: u32,
    /// Latched layout/group component.
    pub latched_layout: u32,
    /// Locked layout/group component.
    pub locked_layout: u32,
    /// Effective layout/group after XKB state rules.
    pub effective_layout: u32,
}

/// Summary of pending events consumed from the model's dedicated X connection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyboardEventDrain {
    /// Total events removed from the connection queue.
    pub events: usize,
    /// Relevant core/XKB mapping invalidations observed.
    pub mapping_invalidations: usize,
    /// Exact structural XkbSetMap notifications emitted while Xorg initializes
    /// its XTEST keyboard device on first keyboard use.
    ///
    /// This shape alone is not trusted as proof of origin. The input actor
    /// additionally requires first-keyboard-effect ordering, a single total
    /// invalidation, and an unchanged complete keymap fingerprint.
    pub structural_set_map_invalidations: usize,
    /// Relevant XKB state snapshots applied.
    pub state_updates: usize,
    /// Events not owned by the keyboard model. These are counted rather than
    /// silently presented as keyboard evidence.
    pub unrelated_events: usize,
}

#[cfg(feature = "native-xkbcommon")]
impl KeyboardEventDrain {
    fn merge(&mut self, other: Self) {
        self.events = self.events.saturating_add(other.events);
        self.mapping_invalidations = self
            .mapping_invalidations
            .saturating_add(other.mapping_invalidations);
        self.structural_set_map_invalidations = self
            .structural_set_map_invalidations
            .saturating_add(other.structural_set_map_invalidations);
        self.state_updates = self.state_updates.saturating_add(other.state_updates);
        self.unrelated_events = self.unrelated_events.saturating_add(other.unrelated_events);
    }
}

/// Evidence produced by one authoritative keyboard-model preflight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardPreflight {
    /// Reply/drain rounds needed to reach a clean mapping generation.
    pub rounds: u8,
    /// Model rebuilds performed while converging.
    pub rebuilds: u8,
    /// Events consumed across all rounds.
    pub drained: KeyboardEventDrain,
    /// QueryKeymap evidence from the final reply-producing round trip.
    pub pressed: QueryKeymapSnapshot,
    /// Clean mapping generation at return.
    pub generation: u64,
}

/// Authoritative configured XKB names read from root `_XKB_RULES_NAMES`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyboardConfigurationNames {
    rules: String,
    model: String,
    layout: String,
    variant: String,
    options: String,
}

impl KeyboardConfigurationNames {
    /// XKB rules name.
    #[must_use]
    pub fn rules(&self) -> &str {
        &self.rules
    }

    /// XKB model name.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Configured comma-separated layouts.
    #[must_use]
    pub fn layout(&self) -> &str {
        &self.layout
    }

    /// Configured comma-separated variants.
    #[must_use]
    pub fn variant(&self) -> &str {
        &self.variant
    }

    /// Configured comma-separated options.
    #[must_use]
    pub fn options(&self) -> &str {
        &self.options
    }
}

/// Explicit availability of the conventional root XKB names property.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfiguredKeyboardNames {
    /// Root `_XKB_RULES_NAMES` does not exist. The server-derived keymap still
    /// remains authoritative, but configured names are explicitly unavailable.
    Missing,
    /// Strictly parsed five-field root property.
    Present(KeyboardConfigurationNames),
}

/// Deterministic identity of the complete serialized server keymap.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeymapFingerprint(u64);

impl KeymapFingerprint {
    /// Raw 64-bit FNV-1a value over libxkbcommon's complete TEXT_V1 keymap
    /// serialization, excluding its terminating NUL.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Display for KeymapFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "fnv1a64:{:016x}", self.0)
    }
}

/// Configuration metadata and complete serialized-keymap identity captured
/// for one model generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyboardModelIdentity {
    configured_names: ConfiguredKeyboardNames,
    fingerprint: KeymapFingerprint,
}

impl KeyboardModelIdentity {
    /// Configured root-property metadata, including explicit absence.
    #[must_use]
    pub const fn configured_names(&self) -> &ConfiguredKeyboardNames {
        &self.configured_names
    }

    /// Complete serialized keymap fingerprint.
    #[must_use]
    pub const fn fingerprint(&self) -> KeymapFingerprint {
        self.fingerprint
    }
}

/// Complete read-only snapshot of one keycode's XKB symbols.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeycodeSymbolSnapshot {
    /// Keycode represented by this snapshot.
    keycode: u8,
    /// Layout-major, level-major, then keysym-major symbol rows.
    layouts: Vec<Vec<Vec<u32>>>,
}

impl KeycodeSymbolSnapshot {
    /// Snapshotted keycode.
    #[must_use]
    pub const fn keycode(&self) -> u8 {
        self.keycode
    }

    /// Layout-major, level-major, keysym-major rows.
    #[must_use]
    pub fn layouts(&self) -> &[Vec<Vec<u32>>] {
        &self.layouts
    }

    /// Whether every symbol slot is empty or `NoSymbol`.
    #[must_use]
    pub fn is_completely_unused(&self) -> bool {
        self.layouts
            .iter()
            .flatten()
            .flatten()
            .all(|keysym| *keysym == 0)
    }
}

/// Startup reservation for a deterministic, genuinely unused keycode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnusedKeycodeReservation {
    /// Reserved keycode.
    keycode: u8,
    /// Exact original symbol model used for later read-back comparison.
    original: KeycodeSymbolSnapshot,
    /// Mapping generation at reservation time.
    generation: u64,
    model_instance: u64,
}

impl UnusedKeycodeReservation {
    /// Reserved core keycode.
    #[must_use]
    pub const fn keycode(&self) -> u8 {
        self.keycode
    }

    /// Exact original symbol rows used for restoration/read-back checks.
    #[must_use]
    pub const fn original(&self) -> &KeycodeSymbolSnapshot {
        &self.original
    }

    /// Captured clean mapping generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// Synchronized proof and reservation returned as one indivisible result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SynchronizedUnusedKeycodeReservation {
    preflight: KeyboardPreflight,
    reservation: UnusedKeycodeReservation,
}

impl SynchronizedUnusedKeycodeReservation {
    /// Reply/drain/rebuild evidence that preceded selection.
    #[must_use]
    pub const fn preflight(&self) -> KeyboardPreflight {
        self.preflight
    }

    /// Non-forgeable reservation token.
    #[must_use]
    pub const fn reservation(&self) -> &UnusedKeycodeReservation {
        &self.reservation
    }

    /// Consume the wrapper into the reservation retained by the actor.
    #[must_use]
    pub fn into_reservation(self) -> UnusedKeycodeReservation {
        self.reservation
    }
}

/// Safe resolution or reservation failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum KeyboardResolutionError {
    /// A mapping notification invalidated the current model.
    #[error("keyboard mapping is dirty and must be rebuilt")]
    DirtyKeymap,
    /// A raw keycode falls outside the current server keycode range.
    #[error("raw keycode {keycode} is outside server range {minimum}..={maximum}")]
    RawKeycodeOutOfRange {
        /// Requested keycode.
        keycode: u8,
        /// Server minimum.
        minimum: u8,
        /// Server maximum.
        maximum: u8,
    },
    /// No exact current-layout candidate exists.
    #[error("key is not representable by a direct current-layout keysym")]
    NotRepresentable,
    /// Active shortcut modifiers would alter application semantics even if
    /// xkbcommon still reports the requested keysym.
    #[error(
        "active shortcut modifier mask {active:#04x} conflicts with required mask {required:#04x}"
    )]
    ConflictingModifierState {
        /// Active core groups.
        active: u8,
        /// Groups required by the candidate.
        required: u8,
    },
    /// A candidate requires a lock or an XKB-only modifier that cannot be
    /// produced safely with the core modifier map.
    #[error("candidate requires unsupported or locking modifier mask {mask:#010x}")]
    UnsafeModifierMask {
        /// Unhandled XKB modifier bits.
        mask: u32,
    },
    /// No non-locking physical provider exists for a required core group.
    #[error("no safe momentary key provider exists for {modifier:?}")]
    NoSafeModifierProvider {
        /// Missing group.
        modifier: CoreModifier,
    },
    /// No keycode satisfies the unused/reserved-key safety requirements.
    #[error("no genuinely unused, unpressed, non-modifier keycode is available")]
    NoUnusedKeycode,
    /// The reservation generation, mapping, pressed state, or modifier role
    /// changed since reservation.
    #[error("unused-keycode reservation is no longer valid")]
    ReservationInvalid,
    /// A captured binding belongs to a superseded mapping generation.
    #[error("captured key binding generation {captured} is stale; current generation is {current}")]
    StaleBinding {
        /// Generation captured in the binding.
        captured: u64,
        /// Current model generation.
        current: u64,
    },
    /// A binding was forged, came from another model, no longer resolves to
    /// the same physical key, or was validated under different ownership
    /// evidence.
    #[error("captured key binding does not match the current model and resolution context")]
    BindingInvalid,
}

#[cfg(feature = "native-xkbcommon")]
mod native {
    use std::{
        ffi::CString,
        sync::atomic::{AtomicU64, Ordering},
    };

    use x11rb::{
        connection::{Connection, RequestConnection},
        protocol::{
            Event,
            xkb::{
                self as xkb_protocol, ConnectionExt as _, EventType, ID, MapPart, NKNDetail,
                SelectEventsAux, SelectEventsAuxNewKeyboardNotify, SelectEventsAuxStateNotify,
                StatePart,
            },
            xproto::{AtomEnum, ConnectionExt as _, Mapping, MappingNotifyEvent, Window},
        },
        xcb_ffi::XCBConnection,
    };
    use xkbcommon::xkb;

    use super::{
        ConfiguredKeyboardNames, CoreModifier, CoreModifierMask, KeyIdentifier,
        KeyboardConfigurationNames, KeyboardEventDrain, KeyboardModelIdentity,
        KeyboardResolutionContext, KeyboardResolutionError, KeyboardResolutionIntent,
        KeyboardStateSnapshot, KeycodeSymbolSnapshot, KeymapFingerprint, ModifierMap,
        ModifierOwnershipEvidence, ModifierSideDefaults, NamedKey, QueryKeymapSnapshot,
        ResolvedKeyBinding, ResolvedModifier, SymbolMapping, SynchronizedUnusedKeycodeReservation,
        UnusedKeycodeReservation,
    };
    use crate::{Result, X11Error};

    const CORE_MODIFIER_NAMES: [&str; 8] = [
        "Shift", "Lock", "Control", "Mod1", "Mod2", "Mod3", "Mod4", "Mod5",
    ];
    const MAX_PREFLIGHT_ROUNDS: u8 = 4;
    const XKB_RULES_NAMES_MAX_BYTES: u32 = 4_096;
    static NEXT_MODEL_INSTANCE: AtomicU64 = AtomicU64::new(1);

    /// Native connection/model failure or typed resolution refusal.
    #[derive(Debug, thiserror::Error)]
    pub enum KeyboardModelError {
        /// X11/libxkbcommon model failure.
        #[error(transparent)]
        Platform(#[from] X11Error),
        /// Fail-closed key resolution failure.
        #[error(transparent)]
        Resolution(#[from] KeyboardResolutionError),
    }

    /// A binding returned together with the synchronized preflight that made
    /// its resolution authoritative.
    pub struct SynchronizedKeyResolution {
        preflight: super::KeyboardPreflight,
        binding: ResolvedKeyBinding,
    }

    impl std::fmt::Debug for SynchronizedKeyResolution {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("SynchronizedKeyResolution")
                .field("preflight", &self.preflight)
                .field("binding", &self.binding)
                .finish()
        }
    }

    impl SynchronizedKeyResolution {
        /// Reply/drain/rebuild evidence.
        #[must_use]
        pub const fn preflight(&self) -> super::KeyboardPreflight {
            self.preflight
        }

        /// Binding captured in the final clean generation.
        #[must_use]
        pub const fn binding(&self) -> &ResolvedKeyBinding {
            &self.binding
        }

        /// Consume the synchronized result into its non-forgeable binding.
        #[must_use]
        pub fn into_binding(self) -> ResolvedKeyBinding {
            self.binding
        }
    }

    /// Mapping-generation status for an actor-confined held-key binding.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum HeldBindingGeneration {
        /// The captured mapping generation is still current.
        Current,
        /// The mapping changed while the physical key remained held. The actor
        /// must still release its captured keycode rather than re-resolving.
        Stale {
            /// Generation captured before the key-down effect.
            captured: u64,
            /// Current synchronized model generation.
            current: u64,
        },
    }

    /// Synchronized ownership proof for a previously emitted key-down.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct SynchronizedHeldBindingValidation {
        preflight: super::KeyboardPreflight,
        generation: HeldBindingGeneration,
    }

    impl SynchronizedHeldBindingValidation {
        /// Reply/drain/rebuild evidence proving current pressed state.
        #[must_use]
        pub const fn preflight(self) -> super::KeyboardPreflight {
            self.preflight
        }

        /// Whether the mapping remained current while the key was held.
        #[must_use]
        pub const fn generation(self) -> HeldBindingGeneration {
            self.generation
        }
    }

    /// XKB model compiled from the core keyboard device on the live X server.
    pub struct NativeKeyboardModel {
        connection: XCBConnection,
        context: xkb::Context,
        keymap: xkb::Keymap,
        state: xkb::State,
        modifier_map: ModifierMap,
        pressed: QueryKeymapSnapshot,
        root: Window,
        device_id: i32,
        server_major: u16,
        server_minor: u16,
        xkb_major_opcode: u8,
        base_event: u8,
        base_error: u8,
        modifier_side_defaults: ModifierSideDefaults,
        identity: KeyboardModelIdentity,
        model_instance: u64,
        generation: u64,
        dirty: bool,
    }

    #[derive(Clone, Copy)]
    struct Candidate {
        keycode: u8,
        layout: u32,
        level: u32,
        keysym: u32,
        required_xkb_mask: u32,
        required_core: CoreModifierMask,
        added_core: CoreModifierMask,
    }

    #[derive(Clone, Copy)]
    struct DerivedModifierEvidence {
        public: ModifierOwnershipEvidence,
        exempt_xkb: u32,
    }

    impl NativeKeyboardModel {
        /// Connect to `display`, negotiate XKB, select mapping/state events, and
        /// compile keymap/state from the server's core keyboard device.
        pub fn connect(display: &str) -> Result<Self> {
            Self::connect_with_modifier_defaults(display, ModifierSideDefaults::default())
        }

        /// Connect with an explicit concrete-side policy for generic modifier
        /// names. The ordinary constructor defaults every generic name left.
        pub fn connect_with_modifier_defaults(
            display: &str,
            modifier_side_defaults: ModifierSideDefaults,
        ) -> Result<Self> {
            let display =
                CString::new(display).map_err(|error| X11Error::Keyboard(error.to_string()))?;
            let (connection, screen_index) = XCBConnection::connect(Some(&display))
                .map_err(|error| X11Error::Keyboard(error.to_string()))?;
            let mut server_major = 0;
            let mut server_minor = 0;
            let mut base_event = 0;
            let mut base_error = 0;
            if !xkb::x11::setup_xkb_extension(
                &connection,
                xkb::x11::MIN_MAJOR_XKB_VERSION,
                xkb::x11::MIN_MINOR_XKB_VERSION,
                xkb::x11::SetupXkbExtensionFlags::NoFlags,
                &mut server_major,
                &mut server_minor,
                &mut base_event,
                &mut base_error,
            ) {
                return Err(X11Error::Keyboard(
                    "server rejected the minimum XKB extension version".to_owned(),
                ));
            }
            let xkb_major_opcode = connection
                .extension_information(xkb_protocol::X11_EXTENSION_NAME)
                .map_err(|error| X11Error::Connection(error.to_string()))?
                .ok_or_else(|| {
                    X11Error::Keyboard(
                        "XKB extension vanished after successful negotiation".to_owned(),
                    )
                })?
                .major_opcode;

            // Subscribe before the first authoritative build. A checked
            // post-subscription preflight below closes the remaining window by
            // draining every notification ordered before its QueryKeymap reply.
            Self::select_events(&connection)?;

            let root = connection
                .setup()
                .roots
                .get(screen_index)
                .ok_or(X11Error::Keyboard(
                    "XKB connection selected a missing screen".to_owned(),
                ))?
                .root;

            let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
            if context.get_raw_ptr().is_null() {
                return Err(X11Error::Keyboard(
                    "libxkbcommon returned a null context".to_owned(),
                ));
            }
            let device_id = xkb::x11::get_core_keyboard_device_id(&connection);
            if device_id < 0 {
                return Err(X11Error::Keyboard(
                    "server returned no core keyboard device".to_owned(),
                ));
            }
            let keymap = Self::new_keymap(&context, &connection, device_id)?;
            let state = Self::new_state(&keymap, &connection, device_id)?;
            let modifier_map = Self::query_modifier_map(&connection)?;
            let pressed = Self::query_pressed(&connection)?;
            let identity = Self::query_identity(&connection, root, &keymap)?;

            ensure_required_named_symbols(&keymap)?;
            let mut model = Self {
                connection,
                context,
                keymap,
                state,
                modifier_map,
                pressed,
                root,
                device_id,
                server_major,
                server_minor,
                xkb_major_opcode,
                base_event,
                base_error,
                modifier_side_defaults,
                identity,
                model_instance: NEXT_MODEL_INSTANCE.fetch_add(1, Ordering::Relaxed),
                generation: 1,
                dirty: false,
            };
            let _startup_preflight = model.synchronize_preflight()?;
            Ok(model)
        }

        fn new_keymap(
            context: &xkb::Context,
            connection: &XCBConnection,
            device_id: i32,
        ) -> Result<xkb::Keymap> {
            let keymap = xkb::x11::keymap_new_from_device(
                context,
                connection,
                device_id,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            );
            if keymap.get_raw_ptr().is_null() {
                return Err(X11Error::Keyboard(
                    "libxkbcommon returned a null server keymap".to_owned(),
                ));
            }
            Ok(keymap)
        }

        fn new_state(
            keymap: &xkb::Keymap,
            connection: &XCBConnection,
            device_id: i32,
        ) -> Result<xkb::State> {
            let state = xkb::x11::state_new_from_device(keymap, connection, device_id);
            if state.get_raw_ptr().is_null() {
                return Err(X11Error::Keyboard(
                    "libxkbcommon returned a null server state".to_owned(),
                ));
            }
            Ok(state)
        }

        fn select_events(connection: &XCBConnection) -> Result<()> {
            let map = selected_map_parts();
            let new_keyboard = NKNDetail::KEYCODES | NKNDetail::DEVICE_ID;
            let state = StatePart::MODIFIER_BASE
                | StatePart::MODIFIER_LATCH
                | StatePart::MODIFIER_LOCK
                | StatePart::GROUP_BASE
                | StatePart::GROUP_LATCH
                | StatePart::GROUP_LOCK;
            let details = SelectEventsAux::new()
                .new_keyboard_notify(SelectEventsAuxNewKeyboardNotify {
                    affect_new_keyboard: new_keyboard,
                    new_keyboard_details: new_keyboard,
                })
                .state_notify(SelectEventsAuxStateNotify {
                    affect_state: state,
                    state_details: state,
                });
            // x11rb derives the wire `affectWhich` mask from the populated
            // `SelectEventsAux` arms, plus `clear` and `select_all`. There is no
            // separate affectWhich argument in this generated API.
            connection
                .xkb_select_events(
                    ID::USE_CORE_KBD.into(),
                    EventType::default(),
                    EventType::MAP_NOTIFY,
                    map,
                    map,
                    &details,
                )
                .map_err(|error| X11Error::Connection(error.to_string()))?
                .check()
                .map_err(|error| X11Error::Reply(format!("{error:?}")))?;
            Ok(())
        }

        fn query_modifier_map(connection: &XCBConnection) -> Result<ModifierMap> {
            let reply = connection
                .get_modifier_mapping()
                .map_err(|error| X11Error::Connection(error.to_string()))?
                .reply()
                .map_err(|error| X11Error::Reply(format!("{error:?}")))?;
            ModifierMap::from_flat_keycodes(&reply.keycodes)
                .map_err(|error| X11Error::Keyboard(error.to_string()))
        }

        fn query_pressed(connection: &XCBConnection) -> Result<QueryKeymapSnapshot> {
            let reply = connection
                .query_keymap()
                .map_err(|error| X11Error::Connection(error.to_string()))?
                .reply()
                .map_err(|error| X11Error::Reply(format!("{error:?}")))?;
            Ok(QueryKeymapSnapshot::new(reply.keys))
        }

        fn query_identity(
            connection: &XCBConnection,
            root: Window,
            keymap: &xkb::Keymap,
        ) -> Result<KeyboardModelIdentity> {
            let atom = connection
                .intern_atom(true, b"_XKB_RULES_NAMES")
                .map_err(|error| X11Error::Connection(error.to_string()))?
                .reply()
                .map_err(|error| X11Error::Reply(format!("{error:?}")))?
                .atom;
            let configured_names = if atom == 0 {
                ConfiguredKeyboardNames::Missing
            } else {
                let reply = connection
                    .get_property(
                        false,
                        root,
                        atom,
                        AtomEnum::STRING,
                        0,
                        XKB_RULES_NAMES_MAX_BYTES.div_ceil(4),
                    )
                    .map_err(|error| X11Error::Connection(error.to_string()))?
                    .reply()
                    .map_err(|error| X11Error::Reply(format!("{error:?}")))?;
                if reply.type_ == 0 && reply.value.is_empty() {
                    ConfiguredKeyboardNames::Missing
                } else {
                    if reply.type_ != u32::from(AtomEnum::STRING)
                        || reply.format != 8
                        || reply.bytes_after != 0
                        || reply.value.len() > XKB_RULES_NAMES_MAX_BYTES as usize
                    {
                        return Err(X11Error::Keyboard(
                            "root _XKB_RULES_NAMES is malformed or exceeds 4096 bytes".to_owned(),
                        ));
                    }
                    ConfiguredKeyboardNames::Present(parse_configured_names(&reply.value)?)
                }
            };
            let serialized = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
            if serialized.is_empty() {
                return Err(X11Error::Keyboard(
                    "libxkbcommon returned an empty serialized server keymap".to_owned(),
                ));
            }
            Ok(KeyboardModelIdentity {
                configured_names,
                fingerprint: KeymapFingerprint(fnv1a64(serialized.as_bytes())),
            })
        }

        /// Minimum keycode in the server-derived keymap.
        #[must_use]
        pub fn min_keycode(&self) -> u32 {
            self.keymap.min_keycode().raw()
        }

        /// Maximum keycode in the server-derived keymap.
        #[must_use]
        pub fn max_keycode(&self) -> u32 {
            self.keymap.max_keycode().raw()
        }

        /// Negotiated XKB protocol version.
        #[must_use]
        pub const fn server_version(&self) -> (u16, u16) {
            (self.server_major, self.server_minor)
        }

        /// Extension event/error bases negotiated by libxkbcommon-x11.
        #[must_use]
        pub const fn extension_bases(&self) -> (u8, u8) {
            (self.base_event, self.base_error)
        }

        /// Concrete-side policy applied to generic modifier names.
        #[must_use]
        pub const fn modifier_side_defaults(&self) -> ModifierSideDefaults {
            self.modifier_side_defaults
        }

        /// Configured root-property names and serialized keymap fingerprint for
        /// the current generation.
        #[must_use]
        pub const fn identity(&self) -> &KeyboardModelIdentity {
            &self.identity
        }

        /// Mapping generation captured by newly resolved bindings.
        #[must_use]
        pub const fn generation(&self) -> u64 {
            self.generation
        }

        /// Whether a mapping notification requires rebuilding before the next
        /// resolution-dependent action.
        #[must_use]
        pub const fn is_dirty(&self) -> bool {
            self.dirty
        }

        /// Latest complete `QueryKeymap` snapshot owned by the model.
        #[must_use]
        pub const fn pressed_keys(&self) -> QueryKeymapSnapshot {
            self.pressed
        }

        /// Current serialized XKB state used by deterministic resolution.
        #[must_use]
        pub fn state_snapshot(&self) -> KeyboardStateSnapshot {
            KeyboardStateSnapshot {
                depressed_modifiers: self.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                latched_modifiers: self.state.serialize_mods(xkb::STATE_MODS_LATCHED),
                locked_modifiers: self.state.serialize_mods(xkb::STATE_MODS_LOCKED),
                depressed_layout: self.state.serialize_layout(xkb::STATE_LAYOUT_DEPRESSED),
                latched_layout: self.state.serialize_layout(xkb::STATE_LAYOUT_LATCHED),
                locked_layout: self.state.serialize_layout(xkb::STATE_LAYOUT_LOCKED),
                effective_layout: self.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
            }
        }

        /// Latest core modifier map.
        #[must_use]
        pub const fn modifier_map(&self) -> &ModifierMap {
            &self.modifier_map
        }

        /// Refresh pressed-key evidence through a reply-producing request.
        pub fn refresh_pressed_keys(&mut self) -> Result<QueryKeymapSnapshot> {
            self.pressed = Self::query_pressed(&self.connection)?;
            Ok(self.pressed)
        }

        /// Drain and apply every currently pending event from the model's
        /// dedicated connection. The input actor must call this before every
        /// resolution-dependent command; otherwise mapping generations can be
        /// stale even though the server already emitted a notification.
        pub fn drain_pending_events(&mut self) -> Result<KeyboardEventDrain> {
            let mut report = KeyboardEventDrain::default();
            loop {
                let event = self
                    .connection
                    .poll_for_event()
                    .map_err(|error| X11Error::Connection(error.to_string()))?;
                let Some(event) = event else {
                    return Ok(report);
                };
                report.events = report.events.saturating_add(1);
                match event {
                    Event::Error(error) => {
                        return Err(X11Error::Reply(format!("{error:?}")));
                    }
                    Event::MappingNotify(event)
                        if matches!(event.request, Mapping::KEYBOARD | Mapping::MODIFIER) =>
                    {
                        tracing::debug!(
                            request = ?event.request,
                            first_keycode = event.first_keycode,
                            count = event.count,
                            "core keyboard mapping invalidation observed"
                        );
                        self.note_core_mapping_notify(&event);
                        report.mapping_invalidations =
                            report.mapping_invalidations.saturating_add(1);
                    }
                    Event::XkbNewKeyboardNotify(event)
                        if i32::from(event.device_id) == self.device_id
                            || i32::from(event.old_device_id) == self.device_id =>
                    {
                        tracing::debug!(
                            device_id = event.device_id,
                            old_device_id = event.old_device_id,
                            changed = ?event.changed,
                            request_major = event.request_major,
                            request_minor = event.request_minor,
                            event_time = event.time,
                            sequence = event.sequence,
                            "XKB keyboard identity invalidation observed"
                        );
                        self.note_new_keyboard_notify(&event);
                        report.mapping_invalidations =
                            report.mapping_invalidations.saturating_add(1);
                        let structural_parts = NKNDetail::KEYCODES | NKNDetail::GEOMETRY;
                        if event.device_id == event.old_device_id
                            && i32::from(event.device_id) == self.device_id
                            && event.min_key_code == event.old_min_key_code
                            && event.max_key_code == event.old_max_key_code
                            && event.changed == structural_parts
                            && event.request_major == self.xkb_major_opcode
                            && event.request_minor == xkb_protocol::SET_MAP_REQUEST
                        {
                            report.structural_set_map_invalidations =
                                report.structural_set_map_invalidations.saturating_add(1);
                        }
                    }
                    Event::XkbMapNotify(event) if i32::from(event.device_id) == self.device_id => {
                        tracing::debug!(
                            device_id = event.device_id,
                            changed = ?event.changed,
                            first_key_sym = event.first_key_sym,
                            key_sym_count = event.n_key_syms,
                            first_modifier_key = event.first_mod_map_key,
                            modifier_key_count = event.n_mod_map_keys,
                            "XKB keymap invalidation observed"
                        );
                        self.note_map_notify(&event);
                        report.mapping_invalidations =
                            report.mapping_invalidations.saturating_add(1);
                    }
                    Event::XkbStateNotify(event)
                        if i32::from(event.device_id) == self.device_id =>
                    {
                        self.note_state_notify(&event)?;
                        report.state_updates = report.state_updates.saturating_add(1);
                    }
                    _ => {
                        report.unrelated_events = report.unrelated_events.saturating_add(1);
                    }
                }
            }
        }

        /// Establish an authoritative clean-generation preflight.
        ///
        /// Each round obtains fresh `GetModifierMapping` and `QueryKeymap`
        /// replies. X11 connection ordering guarantees that earlier
        /// notifications have been read or queued before the latter reply
        /// completes. The method then drains those notifications and rebuilds
        /// a dirty model. A bounded retry prevents an attacker or unstable
        /// desktop from keeping preflight alive indefinitely.
        pub fn synchronize_preflight(&mut self) -> Result<super::KeyboardPreflight> {
            let mut drained = KeyboardEventDrain::default();
            let mut rebuilds = 0_u8;
            for rounds in 1..=MAX_PREFLIGHT_ROUNDS {
                self.modifier_map = Self::query_modifier_map(&self.connection)?;
                self.refresh_pressed_keys()?;
                drained.merge(self.drain_pending_events()?);
                if self.dirty {
                    self.rebuild_if_dirty()?;
                    rebuilds = rebuilds.saturating_add(1);
                    continue;
                }
                return Ok(super::KeyboardPreflight {
                    rounds,
                    rebuilds,
                    drained,
                    pressed: self.pressed,
                    generation: self.generation,
                });
            }
            Err(X11Error::Keyboard(format!(
                "keyboard mapping did not stabilize within {MAX_PREFLIGHT_ROUNDS} synchronized rounds"
            )))
        }

        /// Synchronize the dedicated model connection and resolve only from
        /// the resulting clean generation.
        ///
        /// Resolution is the first bracket, not permission to serialize at an
        /// arbitrary later time. The input actor must run
        /// [`Self::validate_binding_synchronized`] immediately before XTEST
        /// serialization. After the XTEST reply barrier it must run
        /// [`Self::synchronize_preflight`] and compare that generation with the
        /// binding's captured generation. Re-resolving after the effect would
        /// be incorrect for modifier key-down because the intended effect
        /// changes state. A stale post-barrier generation is a
        /// mapping-changed-after-effect result, not a retryable preflight
        /// failure.
        pub fn resolve_synchronized(
            &mut self,
            identifier: KeyIdentifier,
            context: &KeyboardResolutionContext,
        ) -> std::result::Result<SynchronizedKeyResolution, KeyboardModelError> {
            let preflight = self.synchronize_preflight()?;
            let binding = self.resolve(identifier, context)?;
            Ok(SynchronizedKeyResolution { preflight, binding })
        }

        /// Synchronize, then prove that every private binding field still
        /// matches a fresh resolution under the actor's current owned-key
        /// evidence.
        pub fn validate_binding_synchronized(
            &mut self,
            binding: &ResolvedKeyBinding,
            context: &KeyboardResolutionContext,
        ) -> std::result::Result<super::KeyboardPreflight, KeyboardModelError> {
            let preflight = self.synchronize_preflight()?;
            self.validate_binding(binding, context)?;
            Ok(preflight)
        }

        /// Synchronize and validate actor ownership of a captured key-down
        /// without re-resolving under state changed by that intended effect.
        ///
        /// A stale mapping is returned as evidence, not as permission to leave
        /// the key held. The actor must release the private captured keycode in
        /// either generation state and report the stale mapping separately.
        pub fn validate_held_binding_synchronized(
            &mut self,
            binding: &ResolvedKeyBinding,
            expected_identifier: KeyIdentifier,
            context: &KeyboardResolutionContext,
        ) -> std::result::Result<SynchronizedHeldBindingValidation, KeyboardModelError> {
            let preflight = self.synchronize_preflight()?;
            let (minimum, maximum) = self.server_keycode_bounds();
            if binding.model_instance != self.model_instance
                || binding.intent != KeyboardResolutionIntent::PhysicalKey
                || context.intent != KeyboardResolutionIntent::PhysicalKey
                || binding.identifier != expected_identifier
                || !(minimum..=maximum).contains(&binding.keycode)
                || !context.is_actor_owned(binding.keycode)
                || !self.pressed.is_pressed(binding.keycode)
            {
                return Err(KeyboardResolutionError::BindingInvalid.into());
            }
            let generation = if binding.generation == self.generation {
                HeldBindingGeneration::Current
            } else {
                HeldBindingGeneration::Stale {
                    captured: binding.generation,
                    current: self.generation,
                }
            };
            Ok(SynchronizedHeldBindingValidation {
                preflight,
                generation,
            })
        }

        /// Mark the model dirty for a core keyboard/modifier mapping event.
        /// Pointer mapping changes intentionally do not invalidate XKB.
        pub fn note_core_mapping_notify(&mut self, event: &MappingNotifyEvent) {
            if matches!(event.request, Mapping::KEYBOARD | Mapping::MODIFIER) {
                self.mark_dirty();
            }
        }

        /// Mark the model dirty for an XKB new-keyboard event concerning the
        /// model's device.
        pub fn note_new_keyboard_notify(&mut self, event: &xkb_protocol::NewKeyboardNotifyEvent) {
            if i32::from(event.device_id) == self.device_id
                || i32::from(event.old_device_id) == self.device_id
            {
                self.mark_dirty();
            }
        }

        /// Mark the model dirty for an XKB map event concerning the model's
        /// device.
        pub fn note_map_notify(&mut self, event: &xkb_protocol::MapNotifyEvent) {
            if i32::from(event.device_id) == self.device_id {
                self.mark_dirty();
            }
        }

        /// Apply an XKB state notification without rebuilding the immutable
        /// keymap. A negative serialized group is rejected and leaves the model
        /// dirty so callers rebuild from authoritative server state.
        pub fn note_state_notify(&mut self, event: &xkb_protocol::StateNotifyEvent) -> Result<()> {
            if i32::from(event.device_id) != self.device_id {
                return Ok(());
            }
            let base_group = u32::try_from(event.base_group).map_err(|_| {
                self.mark_dirty();
                X11Error::Keyboard("XKB state event contained negative base group".to_owned())
            })?;
            let latched_group = u32::try_from(event.latched_group).map_err(|_| {
                self.mark_dirty();
                X11Error::Keyboard("XKB state event contained negative latched group".to_owned())
            })?;
            let locked_group = u32::from(u8::from(event.locked_group));
            self.state.update_mask(
                u32::from(u16::from(event.base_mods)),
                u32::from(u16::from(event.latched_mods)),
                u32::from(u16::from(event.locked_mods)),
                base_group,
                latched_group,
                locked_group,
            );
            Ok(())
        }

        fn mark_dirty(&mut self) {
            if !self.dirty {
                self.generation = self.generation.saturating_add(1);
            }
            self.dirty = true;
        }

        /// Rebuild the immutable server keymap/state and refresh core modifier
        /// and key-press snapshots. Generation was already advanced by the
        /// event that made the model dirty.
        pub fn rebuild_if_dirty(&mut self) -> Result<bool> {
            if !self.dirty {
                return Ok(false);
            }
            let device_id = xkb::x11::get_core_keyboard_device_id(&self.connection);
            if device_id < 0 {
                return Err(X11Error::Keyboard(
                    "server returned no core keyboard device during rebuild".to_owned(),
                ));
            }
            // A new core device may discard per-device selections. Re-select
            // through the core-keyboard alias before deriving any replacement
            // model, then let the next preflight round drain changes ordered
            // after this checked request.
            Self::select_events(&self.connection)?;
            let keymap = Self::new_keymap(&self.context, &self.connection, device_id)?;
            let state = Self::new_state(&keymap, &self.connection, device_id)?;
            ensure_required_named_symbols(&keymap)?;
            let modifier_map = Self::query_modifier_map(&self.connection)?;
            let pressed = Self::query_pressed(&self.connection)?;
            let identity = Self::query_identity(&self.connection, self.root, &keymap)?;
            self.keymap = keymap;
            self.state = state;
            self.modifier_map = modifier_map;
            self.pressed = pressed;
            self.identity = identity;
            self.device_id = device_id;
            self.dirty = false;
            Ok(true)
        }

        /// Resolve a named, scalar, or raw identifier against the current
        /// authoritative model.
        fn resolve(
            &self,
            identifier: KeyIdentifier,
            context: &KeyboardResolutionContext,
        ) -> std::result::Result<ResolvedKeyBinding, KeyboardResolutionError> {
            if self.dirty {
                return Err(KeyboardResolutionError::DirtyKeymap);
            }
            if let KeyIdentifier::Raw(keycode) = identifier {
                if context.intent == KeyboardResolutionIntent::ExactText {
                    return Err(KeyboardResolutionError::NotRepresentable);
                }
                return self.resolve_raw(identifier, keycode, context);
            }
            let (target_keysym, target_scalar) = match identifier {
                KeyIdentifier::Named(named) => (
                    named_keysym(named.concretize(self.modifier_side_defaults)),
                    None,
                ),
                KeyIdentifier::Scalar(scalar) => {
                    let keysym = xkb::utf32_to_keysym(u32::from(scalar)).raw();
                    if keysym == 0 {
                        return Err(KeyboardResolutionError::NotRepresentable);
                    }
                    (keysym, Some(u32::from(scalar)))
                }
                KeyIdentifier::Raw(_) => return Err(KeyboardResolutionError::NotRepresentable),
            };
            let evidence = self.derive_modifier_evidence(context);
            if !evidence.public.conflicting.is_empty() {
                return Err(KeyboardResolutionError::ConflictingModifierState {
                    active: evidence.public.active_shortcut.bits(),
                    required: 0,
                });
            }
            let candidate = self.find_candidate(target_keysym, target_scalar, context, evidence)?;
            self.binding_from_candidate(identifier, candidate, context, evidence)
        }

        /// Prove that every private field in a captured binding still matches
        /// this model and a fresh resolution under the same actor evidence.
        fn validate_binding(
            &self,
            binding: &ResolvedKeyBinding,
            context: &KeyboardResolutionContext,
        ) -> std::result::Result<(), KeyboardResolutionError> {
            if self.dirty {
                return Err(KeyboardResolutionError::DirtyKeymap);
            }
            if binding.model_instance != self.model_instance || binding.intent != context.intent {
                return Err(KeyboardResolutionError::BindingInvalid);
            }
            if binding.generation != self.generation {
                return Err(KeyboardResolutionError::StaleBinding {
                    captured: binding.generation,
                    current: self.generation,
                });
            }
            let current = self
                .resolve(binding.identifier, context)
                .map_err(|_| KeyboardResolutionError::BindingInvalid)?;
            if &current != binding {
                return Err(KeyboardResolutionError::BindingInvalid);
            }
            Ok(())
        }

        fn resolve_raw(
            &self,
            identifier: KeyIdentifier,
            keycode: u8,
            context: &KeyboardResolutionContext,
        ) -> std::result::Result<ResolvedKeyBinding, KeyboardResolutionError> {
            let minimum = u8::try_from(self.min_keycode()).unwrap_or(u8::MAX);
            let maximum = u8::try_from(self.max_keycode()).unwrap_or(u8::MIN);
            if !(minimum..=maximum).contains(&keycode) {
                return Err(KeyboardResolutionError::RawKeycodeOutOfRange {
                    keycode,
                    minimum,
                    maximum,
                });
            }
            let xkb_keycode = xkb::Keycode::new(u32::from(keycode));
            let layout = self.state.key_get_layout(xkb_keycode);
            let level = self.state.key_get_level(xkb_keycode, layout);
            let evidence = self.derive_modifier_evidence(context);
            if !evidence.public.conflicting.is_empty() {
                return Err(KeyboardResolutionError::ConflictingModifierState {
                    active: evidence.public.active_shortcut.bits(),
                    required: 0,
                });
            }
            Ok(ResolvedKeyBinding {
                identifier,
                concrete_named_key: None,
                keycode,
                layout,
                level,
                keysym: self.state.key_get_one_sym(xkb_keycode).raw(),
                required_modifiers: Vec::new(),
                generation: self.generation,
                is_modifier: self.modifier_map.is_modifier_key(keycode),
                intent: context.intent,
                modifier_evidence: evidence.public,
                model_instance: self.model_instance,
            })
        }

        fn find_candidate(
            &self,
            target_keysym: u32,
            target_scalar: Option<u32>,
            context: &KeyboardResolutionContext,
            evidence: DerivedModifierEvidence,
        ) -> std::result::Result<Candidate, KeyboardResolutionError> {
            let active_xkb = self.state.serialize_mods(xkb::STATE_MODS_EFFECTIVE);
            let active_core = self.xkb_to_core_mask(active_xkb).0;
            let baseline_xkb = active_xkb & !evidence.exempt_xkb;
            let mut unsafe_mask = 0;
            let mut missing_provider = None;
            let mut best = None;
            for raw_keycode in self.min_keycode()..=self.max_keycode() {
                let Ok(keycode) = u8::try_from(raw_keycode) else {
                    continue;
                };
                let xkb_keycode = xkb::Keycode::new(raw_keycode);
                let current_layout = self.state.key_get_layout(xkb_keycode);
                if current_layout == xkb::LAYOUT_INVALID {
                    continue;
                }
                for level in 0..self.keymap.num_levels_for_key(xkb_keycode, current_layout) {
                    if self
                        .keymap
                        .key_get_syms_by_level(xkb_keycode, current_layout, level)
                        != [xkb::Keysym::new(target_keysym)]
                    {
                        continue;
                    }
                    let mut masks = [0; 64];
                    let count = self.keymap.key_get_mods_for_level(
                        xkb_keycode,
                        current_layout,
                        level,
                        &mut masks,
                    );
                    if count > masks.len() {
                        unsafe_mask = u32::MAX;
                        continue;
                    }
                    for required_xkb in masks.into_iter().take(count) {
                        let (required_core, unhandled) = self.xkb_to_core_mask(required_xkb);
                        if unhandled != 0 || required_core.contains(CoreModifier::Lock) {
                            unsafe_mask |= unhandled | required_xkb;
                            continue;
                        }
                        if let Some(modifier) = CoreModifier::ALL.into_iter().find(|modifier| {
                            required_core.contains(*modifier)
                                && if evidence.public.actor_owned_depressed.contains(*modifier) {
                                    self.actor_owned_down_provider(*modifier, context).is_none()
                                } else {
                                    self.available_modifier_provider(*modifier).is_none()
                                }
                        }) {
                            missing_provider.get_or_insert(modifier);
                            continue;
                        }
                        let added_core = required_core.difference(active_core);
                        let added_xkb = required_xkb & !baseline_xkb;
                        if !self.simulates_exact(
                            xkb_keycode,
                            target_keysym,
                            target_scalar,
                            evidence.exempt_xkb,
                            added_xkb,
                        ) {
                            continue;
                        }
                        let candidate = Candidate {
                            keycode,
                            layout: current_layout,
                            level,
                            keysym: target_keysym,
                            required_xkb_mask: required_xkb,
                            required_core,
                            added_core,
                        };
                        let rank = candidate_rank(candidate);
                        if best.is_none_or(|current| rank < candidate_rank(current)) {
                            best = Some(candidate);
                        }
                    }
                }
            }
            if let Some(candidate) = best {
                return Ok(candidate);
            }
            if let Some(modifier) = missing_provider {
                return Err(KeyboardResolutionError::NoSafeModifierProvider { modifier });
            }
            if unsafe_mask != 0 {
                return Err(KeyboardResolutionError::UnsafeModifierMask { mask: unsafe_mask });
            }
            Err(KeyboardResolutionError::NotRepresentable)
        }

        fn simulates_exact(
            &self,
            keycode: xkb::Keycode,
            target_keysym: u32,
            target_scalar: Option<u32>,
            exempt_xkb: u32,
            added_xkb: u32,
        ) -> bool {
            let mut state = xkb::State::new(&self.keymap);
            if state.get_raw_ptr().is_null() {
                return false;
            }
            state.update_mask(
                (self.state.serialize_mods(xkb::STATE_MODS_DEPRESSED) & !exempt_xkb) | added_xkb,
                self.state.serialize_mods(xkb::STATE_MODS_LATCHED),
                self.state.serialize_mods(xkb::STATE_MODS_LOCKED),
                self.state.serialize_layout(xkb::STATE_LAYOUT_DEPRESSED),
                self.state.serialize_layout(xkb::STATE_LAYOUT_LATCHED),
                self.state.serialize_layout(xkb::STATE_LAYOUT_LOCKED),
            );
            state.key_get_one_sym(keycode).raw() == target_keysym
                && target_scalar.is_none_or(|scalar| state.key_get_utf32(keycode) == scalar)
        }

        fn binding_from_candidate(
            &self,
            identifier: KeyIdentifier,
            candidate: Candidate,
            context: &KeyboardResolutionContext,
            evidence: DerivedModifierEvidence,
        ) -> std::result::Result<ResolvedKeyBinding, KeyboardResolutionError> {
            let mut required_modifiers = Vec::new();
            for modifier in CoreModifier::ALL {
                if !candidate.required_core.contains(modifier) {
                    continue;
                }
                let already_active = evidence.public.actor_owned_depressed.contains(modifier);
                let keycode = if already_active {
                    self.actor_owned_down_provider(modifier, context)
                } else {
                    self.available_modifier_provider(modifier)
                }
                .ok_or(KeyboardResolutionError::NoSafeModifierProvider { modifier })?;
                required_modifiers.push(ResolvedModifier {
                    modifier,
                    keycode,
                    already_active,
                });
            }
            Ok(ResolvedKeyBinding {
                identifier,
                concrete_named_key: match identifier {
                    KeyIdentifier::Named(named) => {
                        Some(named.concretize(self.modifier_side_defaults))
                    }
                    KeyIdentifier::Scalar(_) | KeyIdentifier::Raw(_) => None,
                },
                keycode: candidate.keycode,
                layout: candidate.layout,
                level: candidate.level,
                keysym: candidate.keysym,
                required_modifiers,
                generation: self.generation,
                is_modifier: self.modifier_map.is_modifier_key(candidate.keycode),
                intent: context.intent,
                modifier_evidence: evidence.public,
                model_instance: self.model_instance,
            })
        }

        fn derive_modifier_evidence(
            &self,
            context: &KeyboardResolutionContext,
        ) -> DerivedModifierEvidence {
            let depressed = self
                .xkb_to_core_mask(self.state.serialize_mods(xkb::STATE_MODS_DEPRESSED))
                .0;
            let latched = self
                .xkb_to_core_mask(self.state.serialize_mods(xkb::STATE_MODS_LATCHED))
                .0;
            let locked = self
                .xkb_to_core_mask(self.state.serialize_mods(xkb::STATE_MODS_LOCKED))
                .0;
            // Every depressed or latched non-Lock core group is significant
            // to application shortcut semantics, regardless of the keysym on
            // its provider. Locked-only groups (most notably NumLock on Mod2)
            // remain outside this mask and are still included in exact XKB
            // symbol simulation below.
            let depressed_or_latched =
                CoreModifierMask::from_bits(depressed.bits() | latched.bits());
            let active_shortcut = Self::shortcut_modifier_mask(depressed_or_latched);
            let mut exclusively_owned_depressed = CoreModifierMask::EMPTY;
            for modifier in CoreModifier::ALL {
                if !active_shortcut.contains(modifier)
                    || !depressed.contains(modifier)
                    || latched.contains(modifier)
                    || locked.contains(modifier)
                {
                    continue;
                }
                let mut actor_owned_down = false;
                let mut unowned_down = false;
                for keycode in self.modifier_map.keycodes(modifier) {
                    if !self.pressed.is_pressed(*keycode) {
                        continue;
                    }
                    if context.is_actor_owned(*keycode) {
                        actor_owned_down = true;
                    } else {
                        unowned_down = true;
                    }
                }
                if actor_owned_down && !unowned_down {
                    exclusively_owned_depressed = exclusively_owned_depressed.with(modifier);
                }
            }
            let exempt = match context.intent {
                KeyboardResolutionIntent::PhysicalKey => exclusively_owned_depressed,
                KeyboardResolutionIntent::ExactText => CoreModifierMask::EMPTY,
            };
            let conflicting = active_shortcut.difference(exempt);
            DerivedModifierEvidence {
                public: ModifierOwnershipEvidence {
                    active_shortcut,
                    actor_owned_depressed: exclusively_owned_depressed,
                    conflicting,
                },
                exempt_xkb: self.core_to_xkb_mask(exempt),
            }
        }

        fn xkb_to_core_mask(&self, xkb_mask: u32) -> (CoreModifierMask, u32) {
            let mut core = CoreModifierMask::EMPTY;
            let mut handled = 0;
            for (index, name) in CORE_MODIFIER_NAMES.into_iter().enumerate() {
                let xkb_index = self.keymap.mod_get_index(name);
                if xkb_index == xkb::MOD_INVALID || xkb_index >= u32::BITS {
                    continue;
                }
                let bit = 1_u32 << xkb_index;
                if xkb_mask & bit != 0 {
                    core = core.with(CoreModifier::ALL[index]);
                    handled |= bit;
                }
            }
            (core, xkb_mask & !handled)
        }

        fn core_to_xkb_mask(&self, core_mask: CoreModifierMask) -> u32 {
            let mut xkb_mask = 0_u32;
            for (index, name) in CORE_MODIFIER_NAMES.into_iter().enumerate() {
                if !core_mask.contains(CoreModifier::ALL[index]) {
                    continue;
                }
                let xkb_index = self.keymap.mod_get_index(name);
                if xkb_index != xkb::MOD_INVALID && xkb_index < u32::BITS {
                    xkb_mask |= 1_u32 << xkb_index;
                }
            }
            xkb_mask
        }

        fn shortcut_modifier_mask(mask: CoreModifierMask) -> CoreModifierMask {
            CoreModifier::ALL
                .into_iter()
                .filter(|modifier| *modifier != CoreModifier::Lock && mask.contains(*modifier))
                .fold(CoreModifierMask::EMPTY, CoreModifierMask::with)
        }

        fn actor_owned_down_provider(
            &self,
            modifier: CoreModifier,
            context: &KeyboardResolutionContext,
        ) -> Option<u8> {
            self.modifier_map
                .keycodes(modifier)
                .iter()
                .copied()
                .filter(|keycode| self.pressed.is_pressed(*keycode))
                .filter(|keycode| context.is_actor_owned(*keycode))
                .min()
        }

        fn available_modifier_provider(&self, modifier: CoreModifier) -> Option<u8> {
            self.modifier_map
                .keycodes(modifier)
                .iter()
                .copied()
                .filter(|keycode| !self.pressed.is_pressed(*keycode))
                .filter(|keycode| self.keycode_is_safe_momentary_modifier(*keycode))
                .min()
        }

        fn keycode_is_safe_momentary_modifier(&self, keycode: u8) -> bool {
            let keycode = xkb::Keycode::new(u32::from(keycode));
            let layouts = self.keymap.num_layouts_for_key(keycode);
            (0..layouts).any(|layout| {
                self.keymap
                    .key_get_syms_by_level(keycode, layout, 0)
                    .iter()
                    .any(|keysym| is_safe_momentary_modifier_keysym(keysym.raw()))
            })
        }

        /// Return the first nonzero mapping in deterministic
        /// keycode/layout/level order.
        #[must_use]
        pub fn first_symbol_mapping(&self) -> Option<SymbolMapping> {
            for raw_keycode in self.min_keycode()..=self.max_keycode() {
                let keycode = xkb::Keycode::new(raw_keycode);
                for layout in 0..self.keymap.num_layouts_for_key(keycode) {
                    for level in 0..self.keymap.num_levels_for_key(keycode, layout) {
                        if let Some(keysym) = self
                            .keymap
                            .key_get_syms_by_level(keycode, layout, level)
                            .iter()
                            .find(|keysym| keysym.raw() != 0)
                        {
                            return Some(SymbolMapping {
                                keycode: raw_keycode,
                                layout,
                                level,
                                keysym: keysym.raw(),
                            });
                        }
                    }
                }
            }
            None
        }

        fn symbol_snapshot_current(
            &self,
            keycode: u8,
        ) -> std::result::Result<KeycodeSymbolSnapshot, KeyboardResolutionError> {
            let (minimum, maximum) = self.server_keycode_bounds();
            if !(minimum..=maximum).contains(&keycode) {
                return Err(KeyboardResolutionError::RawKeycodeOutOfRange {
                    keycode,
                    minimum,
                    maximum,
                });
            }
            let xkb_keycode = xkb::Keycode::new(u32::from(keycode));
            let layouts = (0..self.keymap.num_layouts_for_key(xkb_keycode))
                .map(|layout| {
                    (0..self.keymap.num_levels_for_key(xkb_keycode, layout))
                        .map(|level| {
                            self.keymap
                                .key_get_syms_by_level(xkb_keycode, layout, level)
                                .iter()
                                .map(|keysym| keysym.raw())
                                .collect()
                        })
                        .collect()
                })
                .collect();
            Ok(KeycodeSymbolSnapshot { keycode, layouts })
        }

        /// Synchronize with the server, then reserve the highest genuinely
        /// unused, unpressed, non-modifier keycode.
        pub fn reserve_unused_keycode(
            &mut self,
        ) -> std::result::Result<SynchronizedUnusedKeycodeReservation, KeyboardModelError> {
            let preflight = self.synchronize_preflight()?;
            let reservation = self.reserve_unused_keycode_current()?;
            Ok(SynchronizedUnusedKeycodeReservation {
                preflight,
                reservation,
            })
        }

        fn reserve_unused_keycode_current(
            &self,
        ) -> std::result::Result<UnusedKeycodeReservation, KeyboardResolutionError> {
            if self.dirty {
                return Err(KeyboardResolutionError::DirtyKeymap);
            }
            for raw_keycode in (self.min_keycode()..=self.max_keycode()).rev() {
                let Ok(keycode) = u8::try_from(raw_keycode) else {
                    continue;
                };
                let snapshot = self.symbol_snapshot_current(keycode)?;
                if snapshot.is_completely_unused()
                    && !self.modifier_map.is_modifier_key(keycode)
                    && !self.pressed.is_pressed(keycode)
                {
                    return Ok(UnusedKeycodeReservation {
                        keycode,
                        original: snapshot,
                        generation: self.generation,
                        model_instance: self.model_instance,
                    });
                }
            }
            Err(KeyboardResolutionError::NoUnusedKeycode)
        }

        /// Synchronize fresh mapping/pressed evidence and prove that a
        /// reservation still exactly matches this model.
        pub fn validate_reservation(
            &mut self,
            reservation: &UnusedKeycodeReservation,
        ) -> std::result::Result<super::KeyboardPreflight, KeyboardModelError> {
            let preflight = self.synchronize_preflight()?;
            self.validate_reservation_current(reservation)?;
            Ok(preflight)
        }

        fn validate_reservation_current(
            &self,
            reservation: &UnusedKeycodeReservation,
        ) -> std::result::Result<(), KeyboardResolutionError> {
            let (minimum, maximum) = self.server_keycode_bounds();
            if self.dirty
                || reservation.model_instance != self.model_instance
                || reservation.generation != self.generation
                || !(minimum..=maximum).contains(&reservation.keycode)
                || reservation.original.keycode != reservation.keycode
                || self.modifier_map.is_modifier_key(reservation.keycode)
                || self.pressed.is_pressed(reservation.keycode)
                || !reservation.original.is_completely_unused()
            {
                return Err(KeyboardResolutionError::ReservationInvalid);
            }
            let snapshot = self
                .symbol_snapshot_current(reservation.keycode)
                .map_err(|_| KeyboardResolutionError::ReservationInvalid)?;
            if snapshot != reservation.original {
                return Err(KeyboardResolutionError::ReservationInvalid);
            }
            Ok(())
        }

        fn server_keycode_bounds(&self) -> (u8, u8) {
            (
                u8::try_from(self.min_keycode()).unwrap_or(u8::MAX),
                u8::try_from(self.max_keycode()).unwrap_or(u8::MIN),
            )
        }
    }

    fn selected_map_parts() -> MapPart {
        MapPart::KEY_TYPES
            | MapPart::KEY_SYMS
            | MapPart::MODIFIER_MAP
            | MapPart::EXPLICIT_COMPONENTS
            | MapPart::KEY_ACTIONS
            | MapPart::KEY_BEHAVIORS
            | MapPart::VIRTUAL_MODS
            | MapPart::VIRTUAL_MOD_MAP
    }

    fn candidate_rank(candidate: Candidate) -> (u32, u8, u32, u8, u32) {
        (
            candidate.added_core.len(),
            candidate.keycode,
            candidate.level,
            candidate.required_core.bits(),
            candidate.required_xkb_mask,
        )
    }

    fn parse_configured_names(value: &[u8]) -> Result<KeyboardConfigurationNames> {
        let mut fields: Vec<&[u8]> = value.split(|byte| *byte == 0).collect();
        if fields.last().is_some_and(|field| field.is_empty()) {
            let _terminator = fields.pop();
        } else {
            return Err(X11Error::Keyboard(
                "root _XKB_RULES_NAMES is missing its final NUL terminator".to_owned(),
            ));
        }
        if fields.len() != 5 {
            return Err(X11Error::Keyboard(format!(
                "root _XKB_RULES_NAMES has {} fields instead of five",
                fields.len()
            )));
        }
        let decode = |field: &[u8]| {
            std::str::from_utf8(field).map(str::to_owned).map_err(|_| {
                X11Error::Keyboard("root _XKB_RULES_NAMES contains invalid UTF-8".to_owned())
            })
        };
        Ok(KeyboardConfigurationNames {
            rules: decode(fields[0])?,
            model: decode(fields[1])?,
            layout: decode(fields[2])?,
            variant: decode(fields[3])?,
            options: decode(fields[4])?,
        })
    }

    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    fn named_keysym(key: NamedKey) -> u32 {
        use xkb::keysyms;

        match key {
            NamedKey::Backspace => keysyms::KEY_BackSpace,
            NamedKey::Tab => keysyms::KEY_Tab,
            NamedKey::Enter => keysyms::KEY_Return,
            NamedKey::Escape => keysyms::KEY_Escape,
            NamedKey::Space => keysyms::KEY_space,
            NamedKey::Insert => keysyms::KEY_Insert,
            NamedKey::Delete => keysyms::KEY_Delete,
            NamedKey::Home => keysyms::KEY_Home,
            NamedKey::End => keysyms::KEY_End,
            NamedKey::PageUp => keysyms::KEY_Page_Up,
            NamedKey::PageDown => keysyms::KEY_Page_Down,
            NamedKey::ArrowLeft => keysyms::KEY_Left,
            NamedKey::ArrowUp => keysyms::KEY_Up,
            NamedKey::ArrowRight => keysyms::KEY_Right,
            NamedKey::ArrowDown => keysyms::KEY_Down,
            NamedKey::Shift => keysyms::KEY_Shift_L,
            NamedKey::Control => keysyms::KEY_Control_L,
            NamedKey::Alt => keysyms::KEY_Alt_L,
            NamedKey::Meta => keysyms::KEY_Meta_L,
            NamedKey::Super => keysyms::KEY_Super_L,
            NamedKey::ShiftLeft => keysyms::KEY_Shift_L,
            NamedKey::ShiftRight => keysyms::KEY_Shift_R,
            NamedKey::ControlLeft => keysyms::KEY_Control_L,
            NamedKey::ControlRight => keysyms::KEY_Control_R,
            NamedKey::AltLeft => keysyms::KEY_Alt_L,
            NamedKey::AltRight => keysyms::KEY_Alt_R,
            NamedKey::MetaLeft => keysyms::KEY_Meta_L,
            NamedKey::MetaRight => keysyms::KEY_Meta_R,
            NamedKey::SuperLeft => keysyms::KEY_Super_L,
            NamedKey::SuperRight => keysyms::KEY_Super_R,
            NamedKey::HyperLeft => keysyms::KEY_Hyper_L,
            NamedKey::HyperRight => keysyms::KEY_Hyper_R,
            NamedKey::AltGraph => keysyms::KEY_ISO_Level3_Shift,
            NamedKey::CapsLock => keysyms::KEY_Caps_Lock,
            NamedKey::NumLock => keysyms::KEY_Num_Lock,
            NamedKey::ScrollLock => keysyms::KEY_Scroll_Lock,
            NamedKey::PrintScreen => keysyms::KEY_Print,
            NamedKey::Pause => keysyms::KEY_Pause,
            NamedKey::ContextMenu => keysyms::KEY_Menu,
            NamedKey::F1 => keysyms::KEY_F1,
            NamedKey::F2 => keysyms::KEY_F2,
            NamedKey::F3 => keysyms::KEY_F3,
            NamedKey::F4 => keysyms::KEY_F4,
            NamedKey::F5 => keysyms::KEY_F5,
            NamedKey::F6 => keysyms::KEY_F6,
            NamedKey::F7 => keysyms::KEY_F7,
            NamedKey::F8 => keysyms::KEY_F8,
            NamedKey::F9 => keysyms::KEY_F9,
            NamedKey::F10 => keysyms::KEY_F10,
            NamedKey::F11 => keysyms::KEY_F11,
            NamedKey::F12 => keysyms::KEY_F12,
            NamedKey::F13 => keysyms::KEY_F13,
            NamedKey::F14 => keysyms::KEY_F14,
            NamedKey::F15 => keysyms::KEY_F15,
            NamedKey::F16 => keysyms::KEY_F16,
            NamedKey::F17 => keysyms::KEY_F17,
            NamedKey::F18 => keysyms::KEY_F18,
            NamedKey::F19 => keysyms::KEY_F19,
            NamedKey::F20 => keysyms::KEY_F20,
            NamedKey::F21 => keysyms::KEY_F21,
            NamedKey::F22 => keysyms::KEY_F22,
            NamedKey::F23 => keysyms::KEY_F23,
            NamedKey::F24 => keysyms::KEY_F24,
        }
    }

    fn ensure_required_named_symbols(keymap: &xkb::Keymap) -> Result<()> {
        const REQUIRED: [NamedKey; 9] = [
            NamedKey::Backspace,
            NamedKey::Tab,
            NamedKey::Enter,
            NamedKey::Escape,
            NamedKey::Space,
            NamedKey::ArrowLeft,
            NamedKey::ArrowUp,
            NamedKey::ArrowRight,
            NamedKey::ArrowDown,
        ];
        for named in REQUIRED {
            let target = named_keysym(named);
            let mut found = false;
            for raw_keycode in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
                let keycode = xkb::Keycode::new(raw_keycode);
                for layout in 0..keymap.num_layouts_for_key(keycode) {
                    for level in 0..keymap.num_levels_for_key(keycode, layout) {
                        if keymap
                            .key_get_syms_by_level(keycode, layout, level)
                            .iter()
                            .any(|keysym| keysym.raw() == target)
                        {
                            found = true;
                            break;
                        }
                    }
                    if found {
                        break;
                    }
                }
                if found {
                    break;
                }
            }
            if !found {
                return Err(X11Error::Keyboard(format!(
                    "server keymap is missing required named key {named}"
                )));
            }
        }
        Ok(())
    }

    fn is_safe_momentary_modifier_keysym(keysym: u32) -> bool {
        use xkb::keysyms;

        matches!(
            keysym,
            keysyms::KEY_Shift_L
                | keysyms::KEY_Shift_R
                | keysyms::KEY_Control_L
                | keysyms::KEY_Control_R
                | keysyms::KEY_Alt_L
                | keysyms::KEY_Alt_R
                | keysyms::KEY_Meta_L
                | keysyms::KEY_Meta_R
                | keysyms::KEY_Super_L
                | keysyms::KEY_Super_R
                | keysyms::KEY_Hyper_L
                | keysyms::KEY_Hyper_R
                | keysyms::KEY_ISO_Level3_Shift
                | keysyms::KEY_ISO_Level5_Shift
        )
    }

    #[cfg(test)]
    mod tests {
        use std::{process::Command, sync::Mutex};

        use x11rb::protocol::{
            xkb::{ConnectionExt as _, Group, ID, MapPart},
            xproto::{
                ConnectionExt as _, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Mapping,
                MappingNotifyEvent, ModMask,
            },
            xtest::ConnectionExt as _,
        };
        use xkbcommon::xkb;

        use super::{
            HeldBindingGeneration, KeyboardModelError, NativeKeyboardModel,
            SynchronizedKeyResolution, fnv1a64, is_safe_momentary_modifier_keysym, named_keysym,
            parse_configured_names, selected_map_parts,
        };
        use crate::keyboard::{
            ConfiguredKeyboardNames, CoreModifier, CoreModifierMask, KeyIdentifier,
            KeyboardConfigurationNames, KeyboardEventDrain, KeyboardPreflight,
            KeyboardResolutionContext, KeyboardResolutionError, KeyboardResolutionIntent,
            ModifierOwnershipEvidence, NamedKey, QueryKeymapSnapshot, ResolvedKeyBinding,
        };

        static LIVE_KEYBOARD_TEST: Mutex<()> = Mutex::new(());

        fn fake_key(
            model: &NativeKeyboardModel,
            event_type: u8,
            keycode: u8,
        ) -> Result<(), Box<dyn std::error::Error>> {
            model
                .connection
                .xtest_fake_input(event_type, keycode, 0, model.root, 0, 0, 0)?
                .check()?;
            // A reply on the same connection is the server-processing barrier.
            let _reply = model.connection.get_input_focus()?.reply()?;
            Ok(())
        }

        fn lock_group(
            model: &NativeKeyboardModel,
            group: Group,
        ) -> Result<(), Box<dyn std::error::Error>> {
            model
                .connection
                .xkb_latch_lock_state(
                    ID::USE_CORE_KBD.into(),
                    ModMask::default(),
                    ModMask::default(),
                    true,
                    group,
                    ModMask::default(),
                    false,
                    0,
                )?
                .check()?;
            let _reply = model.connection.get_input_focus()?.reply()?;
            Ok(())
        }

        fn run_setxkbmap(
            display: &str,
            configure: impl FnOnce(&mut Command),
        ) -> std::io::Result<()> {
            let mut command = Command::new("setxkbmap");
            command.env("DISPLAY", display);
            configure(&mut command);
            let output = command.output()?;
            if output.status.success() {
                return Ok(());
            }
            Err(std::io::Error::other(format!(
                "setxkbmap failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }

        fn set_layouts(display: &str, layouts: &str) -> std::io::Result<()> {
            run_setxkbmap(display, |command| {
                command
                    .arg("-layout")
                    .arg(layouts)
                    .arg("-variant")
                    .arg("")
                    .arg("-option")
                    .arg("");
            })
        }

        fn apply_names(display: &str, names: &KeyboardConfigurationNames) -> std::io::Result<()> {
            run_setxkbmap(display, |command| {
                if !names.rules().is_empty() {
                    command.arg("-rules").arg(names.rules());
                }
                if !names.model().is_empty() {
                    command.arg("-model").arg(names.model());
                }
                command
                    .arg("-layout")
                    .arg(names.layout())
                    .arg("-variant")
                    .arg(names.variant())
                    .arg("-option")
                    .arg("");
                if !names.options().is_empty() {
                    command.arg("-option").arg(names.options());
                }
            })
        }

        struct KeyboardConfigRestore {
            display: String,
            names: KeyboardConfigurationNames,
            armed: bool,
        }

        impl KeyboardConfigRestore {
            fn restore(&mut self) -> std::io::Result<()> {
                apply_names(&self.display, &self.names)?;
                self.armed = false;
                Ok(())
            }
        }

        impl Drop for KeyboardConfigRestore {
            fn drop(&mut self) {
                if self.armed {
                    let _ignored = apply_names(&self.display, &self.names);
                }
            }
        }

        #[test]
        fn named_function_key_range_is_exact() {
            assert_eq!(named_keysym(NamedKey::F1), xkb::keysyms::KEY_F1);
            assert_eq!(named_keysym(NamedKey::F24), xkb::keysyms::KEY_F24);
            assert_eq!(named_keysym(NamedKey::F24) - named_keysym(NamedKey::F1), 23);
        }

        #[test]
        fn generic_modifier_keysyms_have_documented_left_fallbacks() {
            assert_eq!(named_keysym(NamedKey::Shift), xkb::keysyms::KEY_Shift_L);
            assert_eq!(named_keysym(NamedKey::Control), xkb::keysyms::KEY_Control_L);
            assert_eq!(named_keysym(NamedKey::Alt), xkb::keysyms::KEY_Alt_L);
            assert_eq!(named_keysym(NamedKey::Meta), xkb::keysyms::KEY_Meta_L);
            assert_eq!(named_keysym(NamedKey::Super), xkb::keysyms::KEY_Super_L);
        }

        #[test]
        fn locks_are_not_safe_temporary_modifier_providers() {
            assert!(!is_safe_momentary_modifier_keysym(
                xkb::keysyms::KEY_Caps_Lock
            ));
            assert!(!is_safe_momentary_modifier_keysym(
                xkb::keysyms::KEY_Num_Lock
            ));
            assert!(is_safe_momentary_modifier_keysym(xkb::keysyms::KEY_Shift_L));
        }

        #[test]
        fn synchronized_resolution_debug_does_not_reintroduce_scalar_content() {
            let secret_scalar = '🫆';
            let resolution = SynchronizedKeyResolution {
                preflight: KeyboardPreflight {
                    rounds: 1,
                    rebuilds: 0,
                    drained: KeyboardEventDrain::default(),
                    pressed: QueryKeymapSnapshot::default(),
                    generation: 3,
                },
                binding: ResolvedKeyBinding {
                    identifier: KeyIdentifier::Scalar(secret_scalar),
                    concrete_named_key: None,
                    keycode: 38,
                    layout: 0,
                    level: 0,
                    keysym: u32::from(secret_scalar),
                    required_modifiers: Vec::new(),
                    generation: 3,
                    is_modifier: false,
                    intent: KeyboardResolutionIntent::ExactText,
                    modifier_evidence: ModifierOwnershipEvidence::default(),
                    model_instance: 5,
                },
            };

            let debug = format!("{resolution:?}");
            assert!(debug.contains("SynchronizedKeyResolution"));
            assert!(!debug.contains(secret_scalar));
            assert!(!debug.contains(&u32::from(secret_scalar).to_string()));
            assert!(!debug.contains("keysym"));
        }

        #[test]
        fn every_depressed_or_latched_nonlock_core_group_is_shortcut_significant() {
            let input = CoreModifierMask::from_bits(u8::MAX);
            let significant = NativeKeyboardModel::shortcut_modifier_mask(input);
            assert!(!significant.contains(CoreModifier::Lock));
            for modifier in [
                CoreModifier::Shift,
                CoreModifier::Control,
                CoreModifier::Mod1,
                CoreModifier::Mod2,
                CoreModifier::Mod3,
                CoreModifier::Mod4,
                CoreModifier::Mod5,
            ] {
                assert!(significant.contains(modifier));
            }
        }

        #[test]
        fn subscription_includes_every_keymap_part_that_changes_resolution() {
            let selected = u16::from(selected_map_parts());
            for required in [
                MapPart::KEY_TYPES,
                MapPart::KEY_SYMS,
                MapPart::MODIFIER_MAP,
                MapPart::EXPLICIT_COMPONENTS,
                MapPart::KEY_ACTIONS,
                MapPart::KEY_BEHAVIORS,
                MapPart::VIRTUAL_MODS,
                MapPart::VIRTUAL_MOD_MAP,
            ] {
                assert_ne!(selected & u16::from(required), 0, "missing {required:?}");
            }
        }

        #[test]
        fn configured_names_parser_requires_exactly_five_nul_terminated_utf8_fields()
        -> Result<(), Box<dyn std::error::Error>> {
            let parsed = parse_configured_names(b"evdev\0pc105\0us,de\0,\0grp:alt_shift_toggle\0")?;
            assert_eq!(parsed.rules(), "evdev");
            assert_eq!(parsed.model(), "pc105");
            assert_eq!(parsed.layout(), "us,de");
            assert_eq!(parsed.variant(), ",");
            assert_eq!(parsed.options(), "grp:alt_shift_toggle");

            assert!(parse_configured_names(b"evdev\0pc105\0us\0\0").is_err());
            assert!(parse_configured_names(b"evdev\0pc105\0us\0\0\0extra\0").is_err());
            assert!(parse_configured_names(b"evdev\0pc105\0us\0\0\0\0").is_err());
            assert!(parse_configured_names(b"evdev\0pc105\0\xff\0\0\0").is_err());
            Ok(())
        }

        #[test]
        fn complete_keymap_fingerprint_algorithm_is_stable() {
            assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
            assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
            assert_eq!(fnv1a64(b"complete keymap"), fnv1a64(b"complete keymap"));
            assert_ne!(fnv1a64(b"complete keymap"), fnv1a64(b"complete keymap\0"));
        }

        #[test]
        #[ignore = "requires an authenticated X server and libxkbcommon-x11"]
        fn live_model_enforces_owned_modifier_and_token_invariants()
        -> Result<(), Box<dyn std::error::Error>> {
            let _serial = LIVE_KEYBOARD_TEST
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let display = std::env::var("DISPLAY")?;
            let mut model = NativeKeyboardModel::connect(&display)?;
            let physical = KeyboardResolutionContext::physical_key();
            let exact = KeyboardResolutionContext::exact_text();
            let drained = model.drain_pending_events()?;
            assert_eq!(
                drained.events,
                drained.mapping_invalidations + drained.state_updates + drained.unrelated_events
            );
            let _rebuilt = model.rebuild_if_dirty()?;
            let synchronized =
                model.resolve_synchronized(KeyIdentifier::Named(NamedKey::Control), &physical)?;
            assert_eq!(
                synchronized.preflight().generation,
                synchronized.binding().generation()
            );
            let generic = synchronized.into_binding();
            let concrete = model.resolve(KeyIdentifier::Named(NamedKey::ControlLeft), &physical)?;
            assert_eq!(generic.keycode(), concrete.keycode());
            assert_eq!(generic.concrete_named_key(), Some(NamedKey::ControlLeft));
            let _binding_preflight = model.validate_binding_synchronized(&generic, &physical)?;
            let letter = model.resolve(KeyIdentifier::Scalar('A'), &exact)?;
            assert_eq!(letter.keysym(), xkb::utf32_to_keysym(u32::from('A')).raw());

            let mut forged_binding = generic.clone();
            forged_binding.model_instance = forged_binding.model_instance.wrapping_add(1);
            assert!(matches!(
                model.validate_binding_synchronized(&forged_binding, &physical),
                Err(KeyboardModelError::Resolution(
                    KeyboardResolutionError::BindingInvalid
                ))
            ));
            let mut altered_binding = generic.clone();
            altered_binding.keycode = altered_binding.keycode.wrapping_add(1);
            assert!(matches!(
                model.validate_binding_synchronized(&altered_binding, &physical),
                Err(KeyboardModelError::Resolution(
                    KeyboardResolutionError::BindingInvalid
                ))
            ));
            assert!(matches!(
                model.validate_binding_synchronized(&generic, &exact),
                Err(KeyboardModelError::Resolution(
                    KeyboardResolutionError::BindingInvalid
                ))
            ));

            let synchronized_reservation = model.reserve_unused_keycode()?;
            assert_eq!(
                synchronized_reservation.preflight().generation,
                synchronized_reservation.reservation().generation()
            );
            let reservation = synchronized_reservation.into_reservation();
            let _reservation_preflight = model.validate_reservation(&reservation)?;
            assert!(
                !model
                    .refresh_pressed_keys()?
                    .is_pressed(reservation.keycode())
            );

            let mut forged_reservation = reservation.clone();
            forged_reservation.model_instance = forged_reservation.model_instance.wrapping_add(1);
            assert_eq!(
                model.validate_reservation_current(&forged_reservation),
                Err(KeyboardResolutionError::ReservationInvalid)
            );
            let mut stale_reservation = reservation.clone();
            stale_reservation.generation = stale_reservation.generation.wrapping_add(1);
            assert_eq!(
                model.validate_reservation_current(&stale_reservation),
                Err(KeyboardResolutionError::ReservationInvalid)
            );
            let mut mismatched_reservation = reservation.clone();
            mismatched_reservation.original.keycode =
                mismatched_reservation.original.keycode.wrapping_sub(1);
            assert_eq!(
                model.validate_reservation_current(&mismatched_reservation),
                Err(KeyboardResolutionError::ReservationInvalid)
            );
            let mut out_of_range_reservation = reservation.clone();
            out_of_range_reservation.keycode = 0;
            out_of_range_reservation.original.keycode = 0;
            assert_eq!(
                model.validate_reservation_current(&out_of_range_reservation),
                Err(KeyboardResolutionError::ReservationInvalid)
            );
            let mut forged_symbol_reservation = reservation.clone();
            forged_symbol_reservation.original.layouts = vec![vec![vec![xkb::keysyms::KEY_a]]];
            assert_eq!(
                model.validate_reservation_current(&forged_symbol_reservation),
                Err(KeyboardResolutionError::ReservationInvalid)
            );
            let saved_modifier_map = model.modifier_map.clone();
            model.modifier_map.groups[CoreModifier::Shift.index()].push(reservation.keycode());
            let modifier_reservation_result = model.validate_reservation_current(&reservation);
            model.modifier_map = saved_modifier_map;
            assert_eq!(
                modifier_reservation_result,
                Err(KeyboardResolutionError::ReservationInvalid)
            );

            fake_key(&model, KEY_PRESS_EVENT, reservation.keycode())?;
            let pressed_reservation_result = model.validate_reservation(&reservation);
            fake_key(&model, KEY_RELEASE_EVENT, reservation.keycode())?;
            let _released_preflight = model.synchronize_preflight()?;
            assert!(matches!(
                pressed_reservation_result,
                Err(KeyboardModelError::Resolution(
                    KeyboardResolutionError::ReservationInvalid
                ))
            ));

            // Recapture after the reservation interference checks. Xvfb may
            // deliver a legitimate deferred mapping invalidation while those
            // checks synchronize; the held-key Current assertion below is
            // specifically about a binding from the final clean generation.
            let generic = model
                .resolve_synchronized(KeyIdentifier::Named(NamedKey::Control), &physical)?
                .into_binding();
            let ctrl_keycode = generic.keycode();
            fake_key(&model, KEY_PRESS_EVENT, ctrl_keycode)?;
            let owned_ctrl = KeyboardResolutionContext::new(
                KeyboardResolutionIntent::PhysicalKey,
                &[ctrl_keycode],
            )?;
            let exact_with_owned_ctrl = KeyboardResolutionContext::new(
                KeyboardResolutionIntent::ExactText,
                &[ctrl_keycode],
            )?;
            let held_validation = model.validate_held_binding_synchronized(
                &generic,
                KeyIdentifier::Named(NamedKey::Control),
                &owned_ctrl,
            )?;
            assert_eq!(held_validation.generation(), HeldBindingGeneration::Current);
            assert!(matches!(
                model.validate_binding_synchronized(&generic, &owned_ctrl),
                Err(KeyboardModelError::Resolution(
                    KeyboardResolutionError::BindingInvalid
                ))
            ));
            let owned_ctrl_physical =
                model.resolve_synchronized(KeyIdentifier::Scalar('c'), &owned_ctrl);
            let owned_ctrl_exact =
                model.resolve_synchronized(KeyIdentifier::Scalar('c'), &exact_with_owned_ctrl);
            let unowned_ctrl_physical =
                model.resolve_synchronized(KeyIdentifier::Scalar('c'), &physical);
            fake_key(&model, KEY_RELEASE_EVENT, ctrl_keycode)?;
            let _ctrl_release_preflight = model.synchronize_preflight()?;
            assert!(matches!(
                owned_ctrl_exact,
                Err(KeyboardModelError::Resolution(
                    KeyboardResolutionError::ConflictingModifierState { .. }
                ))
            ));
            let owned_ctrl_physical = owned_ctrl_physical?.into_binding();
            let ctrl_evidence = owned_ctrl_physical.modifier_evidence();
            assert!(
                ctrl_evidence
                    .active_shortcut()
                    .contains(CoreModifier::Control)
            );
            assert!(
                ctrl_evidence
                    .actor_owned_depressed()
                    .contains(CoreModifier::Control)
            );
            assert!(ctrl_evidence.conflicting().is_empty());
            assert!(matches!(
                unowned_ctrl_physical,
                Err(KeyboardModelError::Resolution(
                    KeyboardResolutionError::ConflictingModifierState { .. }
                ))
            ));

            let lower_without_shift = model.resolve(KeyIdentifier::Scalar('c'), &physical)?;
            let shift_keycode = model
                .resolve(KeyIdentifier::Named(NamedKey::ShiftLeft), &physical)?
                .keycode();
            fake_key(&model, KEY_PRESS_EVENT, shift_keycode)?;
            let owned_shift = KeyboardResolutionContext::new(
                KeyboardResolutionIntent::PhysicalKey,
                &[shift_keycode],
            )?;
            let lower_with_owned_shift =
                model.resolve_synchronized(KeyIdentifier::Scalar('c'), &owned_shift);
            fake_key(&model, KEY_RELEASE_EVENT, shift_keycode)?;
            let _shift_release_preflight = model.synchronize_preflight()?;
            let lower_with_owned_shift = lower_with_owned_shift?.into_binding();
            assert_eq!(
                lower_with_owned_shift.keycode(),
                lower_without_shift.keycode()
            );
            assert_eq!(
                lower_with_owned_shift.keysym(),
                lower_without_shift.keysym()
            );
            assert!(
                lower_with_owned_shift
                    .modifier_evidence()
                    .actor_owned_depressed()
                    .contains(CoreModifier::Shift)
            );

            let old_generation = model.generation();
            model.note_core_mapping_notify(&MappingNotifyEvent {
                request: Mapping::KEYBOARD,
                ..MappingNotifyEvent::default()
            });
            assert!(model.is_dirty());
            assert_eq!(model.generation(), old_generation + 1);
            assert_eq!(
                model.resolve(KeyIdentifier::Named(NamedKey::Enter), &physical),
                Err(KeyboardResolutionError::DirtyKeymap)
            );
            assert!(model.rebuild_if_dirty()?);
            assert!(!model.is_dirty());
            fake_key(&model, KEY_PRESS_EVENT, ctrl_keycode)?;
            let stale_held = model.validate_held_binding_synchronized(
                &generic,
                KeyIdentifier::Named(NamedKey::Control),
                &owned_ctrl,
            )?;
            assert_eq!(
                stale_held.generation(),
                HeldBindingGeneration::Stale {
                    captured: generic.generation(),
                    current: old_generation + 1,
                }
            );
            fake_key(&model, KEY_RELEASE_EVENT, ctrl_keycode)?;
            let _stale_release_preflight = model.synchronize_preflight()?;
            assert_eq!(
                model.validate_binding(&generic, &physical),
                Err(KeyboardResolutionError::StaleBinding {
                    captured: generic.generation(),
                    current: old_generation + 1,
                })
            );
            assert_eq!(
                model.validate_reservation_current(&reservation),
                Err(KeyboardResolutionError::ReservationInvalid)
            );
            Ok(())
        }

        #[test]
        #[ignore = "requires setxkbmap, an authenticated X server, and libxkbcommon-x11"]
        fn live_subscription_tracks_us_non_us_altgr_and_current_group()
        -> Result<(), Box<dyn std::error::Error>> {
            let _serial = LIVE_KEYBOARD_TEST
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let display = std::env::var("DISPLAY")?;
            let mut model = NativeKeyboardModel::connect(&display)?;
            let original_names = match model.identity().configured_names() {
                ConfiguredKeyboardNames::Present(names) => names.clone(),
                ConfiguredKeyboardNames::Missing => {
                    return Err(std::io::Error::other(
                        "live remap test requires root _XKB_RULES_NAMES for exact restoration",
                    )
                    .into());
                }
            };
            let mut restore = KeyboardConfigRestore {
                display: display.clone(),
                names: original_names,
                armed: true,
            };

            // Collect all fallible evidence before asserting, so the original
            // user's mapping and group are restored on every ordinary failure.
            let evidence = (|| -> Result<_, Box<dyn std::error::Error>> {
                set_layouts(&display, "us")?;
                lock_group(&model, Group::M1)?;
                let us_preflight = model.synchronize_preflight()?;
                let us_fingerprint = model.identity().fingerprint();
                let us_at = model.resolve(
                    KeyIdentifier::Scalar('@'),
                    &KeyboardResolutionContext::exact_text(),
                )?;

                let generation_before_multi_layout = model.generation();
                set_layouts(&display, "us,de")?;
                let remap_preflight = model.synchronize_preflight()?;
                let multi_fingerprint = model.identity().fingerprint();
                let configured_layout = match model.identity().configured_names() {
                    ConfiguredKeyboardNames::Present(names) => names.layout().to_owned(),
                    ConfiguredKeyboardNames::Missing => String::new(),
                };

                lock_group(&model, Group::M2)?;
                let group_preflight = model.synchronize_preflight()?;
                let state = model.state_snapshot();
                let de_at = model.resolve(
                    KeyIdentifier::Scalar('@'),
                    &KeyboardResolutionContext::exact_text(),
                )?;
                Ok((
                    us_preflight,
                    us_fingerprint,
                    us_at,
                    generation_before_multi_layout,
                    remap_preflight,
                    multi_fingerprint,
                    configured_layout,
                    group_preflight,
                    state,
                    de_at,
                ))
            })();

            let restore_names_result = restore.restore();
            let restore_group_result = lock_group(&model, Group::M1);
            let restore_preflight_result = model.synchronize_preflight();
            let (
                _us_preflight,
                us_fingerprint,
                us_at,
                generation_before_multi_layout,
                remap_preflight,
                multi_fingerprint,
                configured_layout,
                _group_preflight,
                state,
                de_at,
            ) = evidence?;
            restore_names_result?;
            restore_group_result?;
            let _restore_preflight = restore_preflight_result?;

            assert!(
                us_at
                    .required_modifiers()
                    .iter()
                    .any(|modifier| modifier.modifier() == CoreModifier::Shift),
                "US @ must use Shift"
            );
            assert!(model.generation() > generation_before_multi_layout);
            assert!(remap_preflight.rebuilds > 0);
            assert!(remap_preflight.drained.mapping_invalidations > 0);
            assert_ne!(us_fingerprint, multi_fingerprint);
            assert_eq!(configured_layout, "us,de");
            assert_eq!(state.effective_layout, 1);
            assert_eq!(de_at.layout(), 1);
            assert!(
                de_at
                    .required_modifiers()
                    .iter()
                    .any(|modifier| modifier.modifier() == CoreModifier::Mod5),
                "German @ must use the AltGr/Mod5 provider"
            );
            Ok(())
        }
    }
}

#[cfg(feature = "native-xkbcommon")]
pub use native::{
    HeldBindingGeneration, KeyboardModelError, NativeKeyboardModel,
    SynchronizedHeldBindingValidation, SynchronizedKeyResolution,
};

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{
        CoreModifier, CoreModifierMask, KeyIdentifier, KeyboardResolutionContext,
        KeyboardResolutionIntent, KeycodeSymbolSnapshot, MAX_RESOLUTION_OWNED_KEYS, ModifierMap,
        ModifierMapError, ModifierOwnershipEvidence, ModifierSide, ModifierSideDefaults, NamedKey,
        QueryKeymapSnapshot, ResolutionContextError, ResolvedKeyBinding,
    };

    #[test]
    fn named_key_parser_is_closed_and_round_trips() {
        let accepted = [
            NamedKey::Enter,
            NamedKey::Space,
            NamedKey::ArrowLeft,
            NamedKey::Control,
            NamedKey::ControlLeft,
            NamedKey::CapsLock,
            NamedKey::PrintScreen,
            NamedKey::ContextMenu,
            NamedKey::F24,
        ];
        for key in accepted {
            assert_eq!(NamedKey::from_str(key.as_str()), Ok(key));
        }
        assert!(NamedKey::from_str("XF86AudioRaiseVolume").is_err());
        assert!(NamedKey::from_str("controlleft").is_err());
        assert!(NamedKey::from_str("Menu").is_err());
    }

    #[test]
    fn resolution_context_is_bounded_and_rejects_ambiguous_ownership()
    -> Result<(), Box<dyn std::error::Error>> {
        let context =
            KeyboardResolutionContext::new(KeyboardResolutionIntent::PhysicalKey, &[8, 37, 255])?;
        assert_eq!(context.intent(), KeyboardResolutionIntent::PhysicalKey);
        assert_eq!(context.actor_owned_count(), 3);
        assert!(context.is_actor_owned(8));
        assert!(context.is_actor_owned(37));
        assert!(context.is_actor_owned(255));
        assert!(!context.is_actor_owned(36));

        assert_eq!(
            KeyboardResolutionContext::new(KeyboardResolutionIntent::PhysicalKey, &[7]),
            Err(ResolutionContextError::InvalidOwnedKeycode { keycode: 7 })
        );
        assert_eq!(
            KeyboardResolutionContext::new(KeyboardResolutionIntent::PhysicalKey, &[37, 37]),
            Err(ResolutionContextError::DuplicateOwnedKeycode { keycode: 37 })
        );
        assert_eq!(
            KeyboardResolutionContext::new(
                KeyboardResolutionIntent::PhysicalKey,
                &vec![8; MAX_RESOLUTION_OWNED_KEYS + 1],
            ),
            Err(ResolutionContextError::TooManyOwnedKeys {
                actual: MAX_RESOLUTION_OWNED_KEYS + 1,
            })
        );
        Ok(())
    }

    #[test]
    fn convenience_resolution_contexts_have_no_ownership_claims() {
        let exact = KeyboardResolutionContext::exact_text();
        assert_eq!(exact.intent(), KeyboardResolutionIntent::ExactText);
        assert_eq!(exact.actor_owned_count(), 0);
        let physical = KeyboardResolutionContext::physical_key();
        assert_eq!(physical.intent(), KeyboardResolutionIntent::PhysicalKey);
        assert_eq!(physical.actor_owned_count(), 0);
    }

    #[test]
    fn modifier_and_lock_classification_is_explicit() {
        assert!(NamedKey::ControlRight.is_modifier());
        assert!(NamedKey::Control.is_modifier());
        assert!(NamedKey::AltGraph.is_modifier());
        assert!(!NamedKey::Enter.is_modifier());
        assert!(NamedKey::NumLock.is_lock());
        assert!(!NamedKey::ShiftLeft.is_lock());
    }

    #[test]
    fn generic_modifiers_default_left_and_honor_explicit_side_policy() {
        assert_eq!(
            NamedKey::Control.concretize(ModifierSideDefaults::default()),
            NamedKey::ControlLeft
        );
        let policy = ModifierSideDefaults {
            shift: ModifierSide::Right,
            control: ModifierSide::Right,
            alt: ModifierSide::Right,
            meta: ModifierSide::Right,
            super_key: ModifierSide::Right,
        };
        assert_eq!(NamedKey::Shift.concretize(policy), NamedKey::ShiftRight);
        assert_eq!(NamedKey::Control.concretize(policy), NamedKey::ControlRight);
        assert_eq!(NamedKey::Alt.concretize(policy), NamedKey::AltRight);
        assert_eq!(NamedKey::Meta.concretize(policy), NamedKey::MetaRight);
        assert_eq!(NamedKey::Super.concretize(policy), NamedKey::SuperRight);
        assert_eq!(NamedKey::Enter.concretize(policy), NamedKey::Enter);
    }

    #[test]
    fn modifier_map_removes_padding_and_preserves_group_order() {
        let flat = [
            50, 62, 0, // Shift
            66, 0, 0, // Lock
            37, 105, 0, // Control
            64, 0, 0, // Mod1
            77, 0, 0, // Mod2
            0, 0, 0, // Mod3
            133, 134, 0, // Mod4
            108, 0, 0, // Mod5
        ];
        let map = ModifierMap::from_flat_keycodes(&flat);
        assert!(map.is_ok());
        let map = map.unwrap_or_else(|_| unreachable!());
        assert_eq!(map.keycodes(CoreModifier::Shift), [50, 62]);
        assert!(map.keycodes(CoreModifier::Mod3).is_empty());
        assert_eq!(map.modifiers_for_key(134).bits(), 1 << 6);
        assert!(map.is_modifier_key(108));
        assert!(!map.is_modifier_key(38));
    }

    #[test]
    fn malformed_modifier_map_is_rejected() {
        assert_eq!(
            ModifierMap::from_flat_keycodes(&[1, 2, 3]),
            Err(ModifierMapError::InvalidLength { actual: 3 })
        );
    }

    #[test]
    fn query_keymap_uses_core_bit_numbering_at_boundaries() {
        let mut bytes = [0; 32];
        bytes[0] = 0b1000_0001;
        bytes[1] = 0b0000_0011;
        bytes[31] = 0b1000_0000;
        let snapshot = QueryKeymapSnapshot::new(bytes);
        assert!(snapshot.is_pressed(0));
        assert!(snapshot.is_pressed(7));
        assert!(snapshot.is_pressed(8));
        assert!(snapshot.is_pressed(9));
        assert!(snapshot.is_pressed(255));
        assert!(!snapshot.is_pressed(254));
        assert_eq!(snapshot.pressed_keycodes(), [0, 7, 8, 9, 255]);
    }

    #[test]
    fn completely_unused_snapshot_accepts_empty_or_nosymbol_rows() {
        let unused = KeycodeSymbolSnapshot {
            keycode: 255,
            layouts: vec![vec![vec![], vec![0]], vec![vec![0, 0]]],
        };
        assert!(unused.is_completely_unused());
        let used = KeycodeSymbolSnapshot {
            keycode: 255,
            layouts: vec![vec![vec![0x61]]],
        };
        assert!(!used.is_completely_unused());
    }

    #[test]
    fn core_modifier_mask_operations_are_bounded_to_eight_bits() {
        let mask = CoreModifierMask::EMPTY
            .with(CoreModifier::Shift)
            .with(CoreModifier::Mod5);
        assert_eq!(mask.bits(), 0b1000_0001);
        assert_eq!(mask.len(), 2);
        assert!(mask.contains(CoreModifier::Mod5));
    }

    #[test]
    fn resolved_binding_debug_redacts_scalar_and_keysym() {
        let secret_scalar = '🛸';
        let secret_keysym = u32::from(secret_scalar);
        let binding = ResolvedKeyBinding {
            identifier: KeyIdentifier::Scalar(secret_scalar),
            concrete_named_key: None,
            keycode: 38,
            layout: 0,
            level: 0,
            keysym: secret_keysym,
            required_modifiers: Vec::new(),
            generation: 7,
            is_modifier: false,
            intent: KeyboardResolutionIntent::ExactText,
            modifier_evidence: ModifierOwnershipEvidence::default(),
            model_instance: 11,
        };

        let debug = format!("{binding:?}");
        assert!(debug.contains("identifier_kind: \"scalar\""));
        assert!(!debug.contains(secret_scalar));
        assert!(!debug.contains(&secret_keysym.to_string()));
        assert!(!debug.contains("keysym"));
    }
}
