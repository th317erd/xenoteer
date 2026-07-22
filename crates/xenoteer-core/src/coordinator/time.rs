//! Caller-supplied monotonic time values.

/// Milliseconds from an arbitrary monotonic epoch.
///
/// Values may be compared only within the lifetime of the component that
/// receives them. Wall-clock time must never be converted into this type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonotonicMillis(u64);

impl MonotonicMillis {
    /// Creates a value from a caller-owned monotonic clock reading.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw millisecond reading.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Adds a duration without wrapping.
    #[must_use]
    pub const fn checked_add(self, duration_ms: u64) -> Option<Self> {
        match self.0.checked_add(duration_ms) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns elapsed milliseconds, or `None` if `earlier` is in the future.
    #[must_use]
    pub const fn elapsed_since(self, earlier: Self) -> Option<u64> {
        self.0.checked_sub(earlier.0)
    }
}
