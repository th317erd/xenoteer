//! Immutable, owner-scoped storage for bounded Xenoteer artifacts.
//!
//! This crate deliberately exposes no listing API and no filesystem paths. An
//! [`ArtifactId`] is only a lookup key: callers must still supply the matching
//! owner and desktop generation and enforce the purpose-specific capability at
//! the API boundary.
#![cfg_attr(not(unix), allow(dead_code))]

#[cfg(not(unix))]
compile_error!("xenoteer-artifacts requires Unix filesystem semantics");

mod store;
mod types;

pub use store::{
    ArtifactCorruption, ArtifactLimits, ArtifactStore, CleanupReport, Clock, OpenedArtifact,
    StoreError, SystemClock,
};
pub use types::{
    ArtifactCreate, ArtifactId, ArtifactMetadata, ArtifactOwner, ArtifactPurpose, ArtifactScope,
    CapabilityProvenance, DesktopGeneration, RedactionMetadata, Sha256Digest, TimestampMillis,
    ValidationError,
};

#[cfg(test)]
mod tests;
