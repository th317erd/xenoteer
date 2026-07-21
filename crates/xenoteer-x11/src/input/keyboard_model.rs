//! Actor-local abstraction over the frozen native keyboard model contract.

use xenoteer_core::input::PhysicalKey;

use crate::keyboard::{
    KeyIdentifier, KeyboardModelAvailability, KeyboardResolutionContext, NamedKey,
};

#[cfg(feature = "native-xkbcommon")]
use super::backend::{BackendFault, BackendFaultKind};
use super::{KeyboardBindingEvidence, KeyboardModelDiagnostics};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyboardModelFaultKind {
    Unavailable,
    #[cfg(any(test, feature = "native-xkbcommon"))]
    Conflict,
    #[cfg(any(test, feature = "native-xkbcommon"))]
    NotRepresentable,
    #[cfg(any(test, feature = "native-xkbcommon"))]
    MappingChanged,
    #[cfg(any(test, feature = "native-xkbcommon"))]
    Unsafe,
    #[cfg(feature = "native-xkbcommon")]
    Connection,
    #[cfg(any(test, feature = "native-xkbcommon"))]
    Platform,
}

impl KeyboardModelFaultKind {
    pub(super) const fn is_connection(self) -> bool {
        #[cfg(feature = "native-xkbcommon")]
        {
            matches!(self, Self::Connection)
        }
        #[cfg(not(feature = "native-xkbcommon"))]
        {
            false
        }
    }

    pub(super) const fn is_not_representable(self) -> bool {
        #[cfg(any(test, feature = "native-xkbcommon"))]
        {
            matches!(self, Self::NotRepresentable)
        }
        #[cfg(not(any(test, feature = "native-xkbcommon")))]
        {
            false
        }
    }
}

#[derive(Debug)]
pub(super) struct KeyboardModelFault {
    pub(super) kind: KeyboardModelFaultKind,
}

impl KeyboardModelFault {
    pub(super) const fn new(kind: KeyboardModelFaultKind) -> Self {
        Self { kind }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ModelPreflight {
    pub(super) generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeldBindingGeneration {
    #[cfg(any(test, feature = "native-xkbcommon"))]
    Current,
    Stale {
        captured: u64,
        current: u64,
    },
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RequiredModifierBinding {
    pub(super) key: PhysicalKey,
    pub(super) already_active: bool,
}

pub(super) struct CapturedKeyBinding {
    pub(super) identifier: KeyIdentifier,
    pub(super) key: PhysicalKey,
    pub(super) concrete_named_key: Option<NamedKey>,
    pub(super) layout: u32,
    pub(super) level: u32,
    pub(super) generation: u64,
    pub(super) is_modifier: bool,
    pub(super) required_modifiers: Vec<RequiredModifierBinding>,
    #[cfg(any(test, feature = "native-xkbcommon"))]
    token: BindingToken,
}

impl CapturedKeyBinding {
    pub(super) fn evidence(&self) -> KeyboardBindingEvidence {
        KeyboardBindingEvidence {
            key: self.key,
            concrete_named_key: self.concrete_named_key,
            layout: self.layout,
            level: self.level,
            generation: self.generation,
            is_modifier: self.is_modifier,
            required_modifiers: self
                .required_modifiers
                .iter()
                .map(|modifier| modifier.key)
                .collect(),
        }
    }

    pub(super) fn physically_equivalent(&self, other: &Self) -> bool {
        self.identifier == other.identifier
            && self.key == other.key
            && self.concrete_named_key == other.concrete_named_key
            && self.layout == other.layout
            && self.level == other.level
            && self.generation == other.generation
            && self.is_modifier == other.is_modifier
            && self.required_modifiers.len() == other.required_modifiers.len()
            && self
                .required_modifiers
                .iter()
                .zip(&other.required_modifiers)
                .all(|(left, right)| left.key == right.key)
    }

    #[cfg(test)]
    pub(super) fn for_test(
        identifier: KeyIdentifier,
        key: PhysicalKey,
        generation: u64,
        is_modifier: bool,
        required_modifiers: Vec<RequiredModifierBinding>,
        token: u64,
    ) -> Self {
        Self {
            identifier,
            key,
            concrete_named_key: match identifier {
                KeyIdentifier::Named(named) => Some(named),
                KeyIdentifier::Scalar(_) | KeyIdentifier::Raw(_) => None,
            },
            layout: 0,
            level: 0,
            generation,
            is_modifier,
            required_modifiers,
            token: BindingToken::Test(token),
        }
    }

    #[cfg(test)]
    pub(super) fn test_token(&self) -> Option<u64> {
        match self.token {
            BindingToken::Test(token) => Some(token),
            _ => None,
        }
    }
}

#[cfg(any(test, feature = "native-xkbcommon"))]
enum BindingToken {
    #[cfg(feature = "native-xkbcommon")]
    Native(crate::keyboard::ResolvedKeyBinding),
    #[cfg(test)]
    Test(u64),
    #[allow(dead_code)]
    Invalid,
}

pub(super) struct KeyboardReservation {
    pub(super) key: PhysicalKey,
    #[cfg(any(test, feature = "native-xkbcommon"))]
    token: ReservationToken,
}

impl KeyboardReservation {
    #[cfg(test)]
    pub(super) fn for_test(key: PhysicalKey, token: u64) -> Self {
        Self {
            key,
            token: ReservationToken::Test(token),
        }
    }

    #[cfg(test)]
    pub(super) fn test_token(&self) -> Option<u64> {
        match self.token {
            ReservationToken::Test(token) => Some(token),
            _ => None,
        }
    }
}

#[cfg(any(test, feature = "native-xkbcommon"))]
enum ReservationToken {
    #[cfg(feature = "native-xkbcommon")]
    Native(crate::keyboard::UnusedKeycodeReservation),
    #[cfg(test)]
    Test(u64),
    #[allow(dead_code)]
    Invalid,
}

pub(super) trait ActorKeyboardModel {
    fn diagnostics(&self) -> KeyboardModelDiagnostics;

    fn synchronize_preflight(&mut self) -> Result<ModelPreflight, KeyboardModelFault>;

    fn resolve_synchronized(
        &mut self,
        identifier: KeyIdentifier,
        context: &KeyboardResolutionContext,
    ) -> Result<CapturedKeyBinding, KeyboardModelFault>;

    fn validate_binding_synchronized(
        &mut self,
        binding: &CapturedKeyBinding,
        context: &KeyboardResolutionContext,
    ) -> Result<ModelPreflight, KeyboardModelFault>;

    fn validate_held_binding_synchronized(
        &mut self,
        binding: &CapturedKeyBinding,
        expected_identifier: KeyIdentifier,
        context: &KeyboardResolutionContext,
    ) -> Result<HeldBindingGeneration, KeyboardModelFault>;

    fn reserve_unused_keycode(&mut self) -> Result<KeyboardReservation, KeyboardModelFault>;

    fn validate_reservation(
        &mut self,
        reservation: &KeyboardReservation,
    ) -> Result<ModelPreflight, KeyboardModelFault>;
}

#[cfg(any(test, not(feature = "native-xkbcommon")))]
pub(super) struct UnavailableKeyboardModel;

#[cfg(any(test, not(feature = "native-xkbcommon")))]
impl ActorKeyboardModel for UnavailableKeyboardModel {
    fn diagnostics(&self) -> KeyboardModelDiagnostics {
        KeyboardModelDiagnostics {
            availability: KeyboardModelAvailability::FeatureDisabled,
            generation: None,
            keymap_fingerprint: None,
        }
    }

    fn synchronize_preflight(&mut self) -> Result<ModelPreflight, KeyboardModelFault> {
        Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unavailable))
    }

    fn resolve_synchronized(
        &mut self,
        _identifier: KeyIdentifier,
        _context: &KeyboardResolutionContext,
    ) -> Result<CapturedKeyBinding, KeyboardModelFault> {
        Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unavailable))
    }

    fn validate_binding_synchronized(
        &mut self,
        _binding: &CapturedKeyBinding,
        _context: &KeyboardResolutionContext,
    ) -> Result<ModelPreflight, KeyboardModelFault> {
        Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unavailable))
    }

    fn reserve_unused_keycode(&mut self) -> Result<KeyboardReservation, KeyboardModelFault> {
        Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unavailable))
    }

    fn validate_held_binding_synchronized(
        &mut self,
        _binding: &CapturedKeyBinding,
        _expected_identifier: KeyIdentifier,
        _context: &KeyboardResolutionContext,
    ) -> Result<HeldBindingGeneration, KeyboardModelFault> {
        Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unavailable))
    }

    fn validate_reservation(
        &mut self,
        _reservation: &KeyboardReservation,
    ) -> Result<ModelPreflight, KeyboardModelFault> {
        Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unavailable))
    }
}

#[cfg(feature = "native-xkbcommon")]
pub(super) struct NativeActorKeyboardModel {
    model: crate::keyboard::NativeKeyboardModel,
}

#[cfg(feature = "native-xkbcommon")]
impl NativeActorKeyboardModel {
    pub(super) fn connect(display: &str) -> Result<Self, BackendFault> {
        crate::keyboard::NativeKeyboardModel::connect(display)
            .map(|model| Self { model })
            .map_err(|error| BackendFault::new(BackendFaultKind::Capability, error.to_string()))
    }
}

#[cfg(feature = "native-xkbcommon")]
impl ActorKeyboardModel for NativeActorKeyboardModel {
    fn diagnostics(&self) -> KeyboardModelDiagnostics {
        KeyboardModelDiagnostics {
            availability: KeyboardModelAvailability::Available,
            generation: Some(self.model.generation()),
            keymap_fingerprint: Some(self.model.identity().fingerprint().value()),
        }
    }

    fn synchronize_preflight(&mut self) -> Result<ModelPreflight, KeyboardModelFault> {
        self.model
            .synchronize_preflight()
            .map(|preflight| ModelPreflight {
                generation: preflight.generation,
            })
            .map_err(map_native_x11_error)
    }

    fn resolve_synchronized(
        &mut self,
        identifier: KeyIdentifier,
        context: &KeyboardResolutionContext,
    ) -> Result<CapturedKeyBinding, KeyboardModelFault> {
        let resolved = self
            .model
            .resolve_synchronized(identifier, context)
            .map_err(map_native_model_error)?;
        let binding = resolved.into_binding();
        let key = PhysicalKey::new(binding.keycode())
            .map_err(|_| KeyboardModelFault::new(KeyboardModelFaultKind::Unsafe))?;
        let required_modifiers = binding
            .required_modifiers()
            .iter()
            .map(|modifier| {
                PhysicalKey::new(modifier.keycode())
                    .map(|key| RequiredModifierBinding {
                        key,
                        already_active: modifier.already_active(),
                    })
                    .map_err(|_| KeyboardModelFault::new(KeyboardModelFaultKind::Unsafe))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CapturedKeyBinding {
            identifier,
            key,
            concrete_named_key: binding.concrete_named_key(),
            layout: binding.layout(),
            level: binding.level(),
            generation: binding.generation(),
            is_modifier: binding.is_modifier(),
            required_modifiers,
            token: BindingToken::Native(binding),
        })
    }

    fn validate_binding_synchronized(
        &mut self,
        binding: &CapturedKeyBinding,
        context: &KeyboardResolutionContext,
    ) -> Result<ModelPreflight, KeyboardModelFault> {
        let BindingToken::Native(binding) = &binding.token else {
            return Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unsafe));
        };
        self.model
            .validate_binding_synchronized(binding, context)
            .map(|preflight| ModelPreflight {
                generation: preflight.generation,
            })
            .map_err(map_native_model_error)
    }

    fn reserve_unused_keycode(&mut self) -> Result<KeyboardReservation, KeyboardModelFault> {
        let reservation = self
            .model
            .reserve_unused_keycode()
            .map_err(map_native_model_error)?
            .into_reservation();
        let key = PhysicalKey::new(reservation.keycode())
            .map_err(|_| KeyboardModelFault::new(KeyboardModelFaultKind::Unsafe))?;
        Ok(KeyboardReservation {
            key,
            token: ReservationToken::Native(reservation),
        })
    }

    fn validate_held_binding_synchronized(
        &mut self,
        binding: &CapturedKeyBinding,
        expected_identifier: KeyIdentifier,
        context: &KeyboardResolutionContext,
    ) -> Result<HeldBindingGeneration, KeyboardModelFault> {
        let BindingToken::Native(binding) = &binding.token else {
            return Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unsafe));
        };
        self.model
            .validate_held_binding_synchronized(binding, expected_identifier, context)
            .map(|validation| match validation.generation() {
                crate::keyboard::HeldBindingGeneration::Current => HeldBindingGeneration::Current,
                crate::keyboard::HeldBindingGeneration::Stale { captured, current } => {
                    HeldBindingGeneration::Stale { captured, current }
                }
            })
            .map_err(map_native_model_error)
    }

    fn validate_reservation(
        &mut self,
        reservation: &KeyboardReservation,
    ) -> Result<ModelPreflight, KeyboardModelFault> {
        let ReservationToken::Native(reservation) = &reservation.token else {
            return Err(KeyboardModelFault::new(KeyboardModelFaultKind::Unsafe));
        };
        self.model
            .validate_reservation(reservation)
            .map(|preflight| ModelPreflight {
                generation: preflight.generation,
            })
            .map_err(map_native_model_error)
    }
}

#[cfg(feature = "native-xkbcommon")]
fn map_native_model_error(error: crate::keyboard::KeyboardModelError) -> KeyboardModelFault {
    match error {
        crate::keyboard::KeyboardModelError::Resolution(error) => map_resolution_error(error),
        crate::keyboard::KeyboardModelError::Platform(crate::X11Error::Connection(_)) => {
            KeyboardModelFault::new(KeyboardModelFaultKind::Connection)
        }
        crate::keyboard::KeyboardModelError::Platform(_) => {
            KeyboardModelFault::new(KeyboardModelFaultKind::Platform)
        }
    }
}

#[cfg(feature = "native-xkbcommon")]
fn map_native_x11_error(error: crate::X11Error) -> KeyboardModelFault {
    if matches!(error, crate::X11Error::Connection(_)) {
        KeyboardModelFault::new(KeyboardModelFaultKind::Connection)
    } else {
        KeyboardModelFault::new(KeyboardModelFaultKind::Platform)
    }
}

#[cfg(feature = "native-xkbcommon")]
fn map_resolution_error(error: crate::keyboard::KeyboardResolutionError) -> KeyboardModelFault {
    let kind = match error {
        crate::keyboard::KeyboardResolutionError::ConflictingModifierState { .. } => {
            KeyboardModelFaultKind::Conflict
        }
        crate::keyboard::KeyboardResolutionError::NotRepresentable
        | crate::keyboard::KeyboardResolutionError::NoUnusedKeycode => {
            KeyboardModelFaultKind::NotRepresentable
        }
        crate::keyboard::KeyboardResolutionError::DirtyKeymap
        | crate::keyboard::KeyboardResolutionError::StaleBinding { .. }
        | crate::keyboard::KeyboardResolutionError::BindingInvalid
        | crate::keyboard::KeyboardResolutionError::ReservationInvalid => {
            KeyboardModelFaultKind::MappingChanged
        }
        crate::keyboard::KeyboardResolutionError::RawKeycodeOutOfRange { .. }
        | crate::keyboard::KeyboardResolutionError::UnsafeModifierMask { .. }
        | crate::keyboard::KeyboardResolutionError::NoSafeModifierProvider { .. } => {
            KeyboardModelFaultKind::Unsafe
        }
    };
    KeyboardModelFault::new(kind)
}

#[cfg(any(test, not(feature = "native-xkbcommon")))]
pub(super) fn unavailable_keyboard_model() -> Box<dyn ActorKeyboardModel> {
    // Portable actors carry no hidden native connection or shell fallback.
    Box::new(UnavailableKeyboardModel)
}
