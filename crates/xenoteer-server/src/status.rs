//! Authenticated status and capability discovery handlers.

use std::{fmt, sync::Arc};

use axum::{
    Extension, Json,
    extract::State,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use xenoteer_protocol::{
    CapabilityReport, DesktopGeneration, DesktopId, ProtocolVersion, RequestId,
};

use crate::{
    ApiState,
    auth::{Grant, Principal},
    problem::ApiProblem,
};

/// Snapshot source for the replaceable capability-probe layer.
pub trait CapabilityProvider: Send + Sync + 'static {
    /// Returns one already validated, deterministic capability report.
    fn capabilities(&self) -> CapabilityReport;
}

/// Immutable capability source useful during composition and tests.
#[derive(Clone)]
pub struct StaticCapabilityProvider {
    report: CapabilityReport,
}

impl StaticCapabilityProvider {
    /// Creates an immutable capability source.
    #[must_use]
    pub fn new(report: CapabilityReport) -> Self {
        Self { report }
    }

    /// Creates an honest empty report until capability adapters are connected.
    pub fn empty() -> Result<Self, xenoteer_protocol::CapabilityReportError> {
        CapabilityReport::checked(Vec::new()).map(Self::new)
    }
}

impl CapabilityProvider for StaticCapabilityProvider {
    fn capabilities(&self) -> CapabilityReport {
        self.report.clone()
    }
}

impl fmt::Debug for StaticCapabilityProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StaticCapabilityProvider")
            .field("capability_count", &self.report.capabilities().len())
            .finish()
    }
}

pub(crate) type SharedCapabilityProvider = Arc<dyn CapabilityProvider>;

#[derive(Serialize)]
struct StatusResponse {
    server_version: &'static str,
    protocol_min: ProtocolVersion,
    protocol_max: ProtocolVersion,
    desktop: DesktopStatus,
    capabilities: CapabilityReport,
}

#[derive(Serialize)]
struct DesktopStatus {
    id: DesktopId,
    generation: Option<DesktopGeneration>,
    state: crate::DesktopReadiness,
    reason_code: Option<String>,
}

pub(crate) async fn status(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    if !principal.has_grant(Grant::DesktopStatus) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let readiness = state.readiness.snapshot();
    Json(StatusResponse {
        server_version: env!("CARGO_PKG_VERSION"),
        protocol_min: ProtocolVersion::V1_0,
        protocol_max: ProtocolVersion::V1_0,
        desktop: DesktopStatus {
            id: state.desktop_id,
            generation: readiness.desktop_generation,
            state: readiness.state,
            reason_code: readiness.reason_code,
        },
        capabilities: state.capabilities.capabilities(),
    })
    .into_response()
}

pub(crate) async fn capabilities(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    if !principal.has_grant(Grant::DesktopStatus) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    Json(state.capabilities.capabilities()).into_response()
}

pub(crate) async fn not_found(Extension(request_id): Extension<RequestId>) -> Response {
    ApiProblem::not_found(request_id).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_provider_returns_valid_independent_snapshots()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = StaticCapabilityProvider::empty()?;
        let first = provider.capabilities();
        let second = provider.capabilities();
        assert!(first.capabilities().is_empty());
        assert_eq!(first, second);
        assert_eq!(
            format!("{provider:?}"),
            "StaticCapabilityProvider { capability_count: 0 }"
        );
        Ok(())
    }
}
