//! AT-SPI connection probes and platform capability boundaries.

#![forbid(unsafe_code)]

/// Errors from the accessibility backend probe.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AtspiProbeError {
    /// Native live probing was not compiled into this build.
    #[error("AT-SPI live probe feature is disabled")]
    FeatureDisabled,
    /// The session or accessibility bus could not be reached.
    #[error("AT-SPI connection failed: {0}")]
    Connection(String),
    /// The registry or an application returned invalid/unavailable data.
    #[error("AT-SPI registry probe failed: {0}")]
    Registry(String),
    /// A bounded bus operation did not finish before the caller's deadline.
    #[error("AT-SPI operation timed out: {operation}")]
    Timeout {
        /// Static, non-secret operation label.
        operation: &'static str,
    },
    /// Registry evidence exceeded a fixed local resource ceiling.
    #[error("AT-SPI {resource} limit exceeded: {actual} > {max}")]
    LimitExceeded {
        /// Static, non-secret resource label.
        resource: &'static str,
        /// Observed count or byte length.
        actual: usize,
        /// Enforced maximum.
        max: usize,
    },
}

/// Maximum application roots admitted into one registry snapshot.
pub const MAX_ATSPI_ROOTS: usize = 10_000;
/// Maximum UTF-8 bytes admitted for one accessible application name.
pub const MAX_ATSPI_NAME_BYTES: usize = 1_024;
/// Maximum aggregate UTF-8 name bytes admitted into one probe report.
pub const MAX_ATSPI_REPORT_BYTES: usize = 16 * 1_024 * 1_024;

/// Explicit portable-build capability probe.
pub struct DisabledAtspiProbe;

impl DisabledAtspiProbe {
    /// Report that the native backend was deliberately not included.
    pub const fn probe() -> Result<(), AtspiProbeError> {
        Err(AtspiProbeError::FeatureDisabled)
    }
}

/// Bounded evidence returned by the live registry probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AtspiProbeReport {
    /// Number of application roots reported by the registry.
    pub root_child_count: usize,
    /// Application accessible names, bounded to the registry's current roots.
    pub application_names: Vec<String>,
}

#[cfg(any(feature = "live-atspi", test))]
fn validate_root_count(count: usize) -> Result<(), AtspiProbeError> {
    if count > MAX_ATSPI_ROOTS {
        return Err(AtspiProbeError::LimitExceeded {
            resource: "registry root count",
            actual: count,
            max: MAX_ATSPI_ROOTS,
        });
    }
    Ok(())
}

#[cfg(any(feature = "live-atspi", test))]
fn account_name_bytes(name: &str, aggregate: usize) -> Result<usize, AtspiProbeError> {
    let name_bytes = name.len();
    if name_bytes > MAX_ATSPI_NAME_BYTES {
        return Err(AtspiProbeError::LimitExceeded {
            resource: "application name bytes",
            actual: name_bytes,
            max: MAX_ATSPI_NAME_BYTES,
        });
    }
    let total = aggregate
        .checked_add(name_bytes)
        .ok_or(AtspiProbeError::LimitExceeded {
            resource: "aggregate report bytes",
            actual: usize::MAX,
            max: MAX_ATSPI_REPORT_BYTES,
        })?;
    if total > MAX_ATSPI_REPORT_BYTES {
        return Err(AtspiProbeError::LimitExceeded {
            resource: "aggregate report bytes",
            actual: total,
            max: MAX_ATSPI_REPORT_BYTES,
        });
    }
    Ok(total)
}

#[cfg(feature = "live-atspi")]
mod live {
    use std::future::Future;

    use atspi::AccessibilityConnection;
    use atspi::proxy::accessible::ObjectRefExt as _;
    use tokio::time::{Instant, timeout_at};

    use crate::{AtspiProbeError, AtspiProbeReport, account_name_bytes, validate_root_count};

    /// Single-owner connection to the AT-SPI registry.
    pub struct LiveAtspiProbe {
        connection: AccessibilityConnection,
    }

    impl LiveAtspiProbe {
        /// Discover the AT-SPI address on the session bus and connect with
        /// zbus's Tokio runtime integration.
        pub async fn connect(deadline: Instant) -> Result<Self, AtspiProbeError> {
            let connection = connection_operation(
                deadline,
                "accessibility bus connection",
                AccessibilityConnection::new(),
            )
            .await?;
            Ok(Self { connection })
        }

        /// Query the registry root and each current application root by its
        /// unique bus name/object path pair under one terminal deadline.
        pub async fn inspect_registry(
            &self,
            deadline: Instant,
        ) -> Result<AtspiProbeReport, AtspiProbeError> {
            let root = registry_operation(
                deadline,
                "registry root lookup",
                self.connection.root_accessible_on_registry(),
            )
            .await?;
            let children =
                registry_operation(deadline, "registry children fetch", root.get_children())
                    .await?;
            validate_root_count(children.len())?;
            let mut application_names = Vec::new();
            application_names
                .try_reserve_exact(children.len())
                .map_err(|_| {
                    AtspiProbeError::Registry("bounded report allocation failed".to_owned())
                })?;
            let mut aggregate_bytes = 0_usize;
            for child in &children {
                let proxy = registry_operation(
                    deadline,
                    "application proxy creation",
                    child.as_accessible_proxy(self.connection.connection()),
                )
                .await?;
                let name =
                    registry_operation(deadline, "application name fetch", proxy.name()).await?;
                aggregate_bytes = account_name_bytes(&name, aggregate_bytes)?;
                application_names.push(name);
            }
            Ok(AtspiProbeReport {
                root_child_count: children.len(),
                application_names,
            })
        }
    }

    async fn connection_operation<T, E>(
        deadline: Instant,
        operation: &'static str,
        future: impl Future<Output = std::result::Result<T, E>>,
    ) -> Result<T, AtspiProbeError>
    where
        E: std::fmt::Display,
    {
        match timeout_at(deadline, future).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(AtspiProbeError::Connection(error.to_string())),
            Err(_) => Err(AtspiProbeError::Timeout { operation }),
        }
    }

    async fn registry_operation<T, E>(
        deadline: Instant,
        operation: &'static str,
        future: impl Future<Output = std::result::Result<T, E>>,
    ) -> Result<T, AtspiProbeError>
    where
        E: std::fmt::Display,
    {
        match timeout_at(deadline, future).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(AtspiProbeError::Registry(error.to_string())),
            Err(_) => Err(AtspiProbeError::Timeout { operation }),
        }
    }
}

#[cfg(feature = "live-atspi")]
pub use live::LiveAtspiProbe;

#[cfg(test)]
mod tests {
    use crate::{
        AtspiProbeError, MAX_ATSPI_NAME_BYTES, MAX_ATSPI_REPORT_BYTES, MAX_ATSPI_ROOTS,
        account_name_bytes, validate_root_count,
    };

    #[test]
    fn root_count_boundaries_are_enforced_before_iteration() {
        assert!(validate_root_count(MAX_ATSPI_ROOTS).is_ok());
        assert!(matches!(
            validate_root_count(MAX_ATSPI_ROOTS + 1),
            Err(AtspiProbeError::LimitExceeded {
                resource: "registry root count",
                ..
            })
        ));
    }

    #[test]
    fn name_limit_counts_utf8_bytes_not_scalar_values() {
        let exact = "é".repeat(MAX_ATSPI_NAME_BYTES / 2);
        assert_eq!(account_name_bytes(&exact, 0), Ok(MAX_ATSPI_NAME_BYTES));
        let excess = format!("{exact}x");
        assert!(matches!(
            account_name_bytes(&excess, 0),
            Err(AtspiProbeError::LimitExceeded {
                resource: "application name bytes",
                ..
            })
        ));
    }

    #[test]
    fn aggregate_report_boundary_and_overflow_are_enforced() {
        assert_eq!(
            account_name_bytes("x", MAX_ATSPI_REPORT_BYTES - 1),
            Ok(MAX_ATSPI_REPORT_BYTES)
        );
        assert!(matches!(
            account_name_bytes("x", MAX_ATSPI_REPORT_BYTES),
            Err(AtspiProbeError::LimitExceeded {
                resource: "aggregate report bytes",
                ..
            })
        ));
        assert!(matches!(
            account_name_bytes("", usize::MAX),
            Err(AtspiProbeError::LimitExceeded {
                resource: "aggregate report bytes",
                ..
            })
        ));
    }
}
