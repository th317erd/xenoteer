//! Authenticated status and capability discovery handlers.

use std::{fmt, sync::Arc, time::SystemTime};

use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use xenoteer_protocol::{CapabilityReport, DesktopStatus, RequestId, StatusResponse, Timestamp};

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

pub(crate) async fn status(
    State(state): State<ApiState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    if !principal.has_grant(Grant::DesktopStatus) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let readiness = state.readiness.snapshot();
    let server_time = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(elapsed) => match Timestamp::from_unix_timestamp_nanos(
            i128::try_from(elapsed.as_nanos()).unwrap_or(i128::MAX),
        ) {
            Ok(timestamp) => timestamp,
            Err(_) => return ApiProblem::internal(request_id).into_response(),
        },
        Err(_) => return ApiProblem::internal(request_id).into_response(),
    };
    let protocol_range = crate::protocol_version::SERVER_PROTOCOL_RANGE;
    let response = StatusResponse {
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol_min: xenoteer_protocol::ProtocolVersion::new(
            protocol_range.major(),
            protocol_range.min_minor(),
        ),
        protocol_max: xenoteer_protocol::ProtocolVersion::new(
            protocol_range.major(),
            protocol_range.max_minor(),
        ),
        server_time,
        desktop: DesktopStatus {
            id: state.desktop_id,
            generation: readiness.desktop_generation,
            state: readiness.state.into(),
            reason_code: readiness.reason_code,
        },
        capabilities: state.capabilities.capabilities(),
    };
    let mut response = Json(response).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
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
