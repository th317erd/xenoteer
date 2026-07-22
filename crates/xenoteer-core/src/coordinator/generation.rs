//! Desktop-generation fencing.

use xenoteer_protocol::{DesktopGeneration, DesktopId};

/// An unforgeable-at-the-domain-boundary snapshot of the current desktop lifetime.
///
/// The UUID fences external references. The process-local epoch additionally
/// prevents a mistakenly reused UUID from making an old in-memory capability valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationToken {
    desktop_id: DesktopId,
    generation: DesktopGeneration,
    epoch: u64,
}

impl GenerationToken {
    /// Returns the desktop bound to this token.
    #[must_use]
    pub const fn desktop_id(self) -> DesktopId {
        self.desktop_id
    }

    /// Returns the externally visible desktop generation.
    #[must_use]
    pub const fn generation(self) -> DesktopGeneration {
        self.generation
    }

    /// Returns the process-local fencing epoch.
    #[must_use]
    pub const fn epoch(self) -> u64 {
        self.epoch
    }
}

/// Owns the authoritative desktop generation and issues comparable snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerationFence {
    current: GenerationToken,
}

impl GenerationFence {
    /// Creates the first process-local epoch for a desktop generation.
    #[must_use]
    pub const fn new(desktop_id: DesktopId, generation: DesktopGeneration) -> Self {
        Self {
            current: GenerationToken {
                desktop_id,
                generation,
                epoch: 1,
            },
        }
    }

    /// Captures the current generation token for a subsequent operation.
    #[must_use]
    pub const fn capture(self) -> GenerationToken {
        self.current
    }

    /// Requires exact desktop, generation UUID, and local-epoch equality.
    pub fn validate(self, supplied: GenerationToken) -> Result<(), GenerationFenceError> {
        if supplied.desktop_id != self.current.desktop_id {
            return Err(GenerationFenceError::WrongDesktop);
        }
        if supplied != self.current {
            return Err(GenerationFenceError::Stale {
                expected: self.current,
                supplied,
            });
        }
        Ok(())
    }

    /// Advances the fence to a newly created desktop generation.
    ///
    /// A generation UUID may not be reused, and the local epoch never wraps.
    pub fn rotate(
        &mut self,
        generation: DesktopGeneration,
    ) -> Result<GenerationToken, GenerationFenceError> {
        if generation == self.current.generation {
            return Err(GenerationFenceError::ReusedGeneration);
        }
        let epoch = self
            .current
            .epoch
            .checked_add(1)
            .ok_or(GenerationFenceError::EpochExhausted)?;
        self.current = GenerationToken {
            desktop_id: self.current.desktop_id,
            generation,
            epoch,
        };
        Ok(self.current)
    }
}

/// A generation-fence validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GenerationFenceError {
    /// The token belongs to another desktop.
    #[error("generation token belongs to another desktop")]
    WrongDesktop,
    /// The token does not match the active generation and local epoch.
    #[error("generation token is stale")]
    Stale {
        /// Authoritative generation token.
        expected: GenerationToken,
        /// Caller-supplied generation token.
        supplied: GenerationToken,
    },
    /// A caller attempted to rotate to the same external generation UUID.
    #[error("desktop generation UUID must not be reused")]
    ReusedGeneration,
    /// The process-local epoch cannot advance safely.
    #[error("generation fencing epoch exhausted")]
    EpochExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_permanently_fences_prior_tokens() -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let mut fence = GenerationFence::new(desktop_id, DesktopGeneration::new());
        let stale = fence.capture();
        let current = fence.rotate(DesktopGeneration::new())?;

        assert!(matches!(
            fence.validate(stale),
            Err(GenerationFenceError::Stale { .. })
        ));
        assert_eq!(fence.validate(current), Ok(()));
        assert_eq!(current.epoch(), stale.epoch() + 1);
        Ok(())
    }
}
