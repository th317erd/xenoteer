//! RFC 9457 response conversion using only bounded, predeclared text.

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use xenoteer_protocol::{DesktopGeneration, EffectStage, ErrorCode, RequestId, RetryAdvice};

/// HTTP problem response with optional bounded retry guidance.
#[derive(Debug)]
pub(crate) struct ApiProblem {
    status: StatusCode,
    code: ErrorCode,
    title: &'static str,
    detail: &'static str,
    retry: RetryAdvice,
    request_id: RequestId,
    retry_after_seconds: Option<u16>,
    authenticate: bool,
    desktop_generation: Option<DesktopGeneration>,
}

#[derive(Serialize)]
struct ProblemBody {
    #[serde(rename = "type")]
    problem_type: String,
    title: &'static str,
    status: u16,
    code: ErrorCode,
    detail: &'static str,
    instance: String,
    retry: RetryAdvice,
    effect_stage: EffectStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    desktop_generation: Option<DesktopGeneration>,
    details: EmptyProblemDetails,
}

#[derive(Serialize)]
struct EmptyProblemDetails {}

impl ApiProblem {
    pub(crate) fn authentication_required(request_id: RequestId) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: ErrorCode::AuthenticationRequired,
            title: "Authentication required",
            detail: "A valid Bearer credential is required.",
            retry: RetryAdvice::Never,
            request_id,
            retry_after_seconds: None,
            authenticate: true,
            desktop_generation: None,
        }
    }

    pub(crate) fn permission_denied(request_id: RequestId) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: ErrorCode::PermissionDenied,
            title: "Permission denied",
            detail: "The authenticated principal lacks the required capability.",
            retry: RetryAdvice::Never,
            request_id,
            retry_after_seconds: None,
            authenticate: false,
            desktop_generation: None,
        }
    }

    pub(crate) fn invalid_request(request_id: RequestId) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: ErrorCode::InvalidRequest,
            title: "Invalid request",
            detail: "The request does not match the versioned protocol shape.",
            retry: RetryAdvice::Never,
            request_id,
            retry_after_seconds: None,
            authenticate: false,
            desktop_generation: None,
        }
    }

    pub(crate) fn not_found(request_id: RequestId) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: ErrorCode::NotFound,
            title: "Not found",
            detail: "The requested versioned resource does not exist.",
            retry: RetryAdvice::Never,
            request_id,
            retry_after_seconds: None,
            authenticate: false,
            desktop_generation: None,
        }
    }

    pub(crate) fn payload_too_large(request_id: RequestId) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: ErrorCode::ResourceExhausted,
            title: "Payload too large",
            detail: "The request body exceeds the configured byte limit.",
            retry: RetryAdvice::Never,
            request_id,
            retry_after_seconds: None,
            authenticate: false,
            desktop_generation: None,
        }
    }

    pub(crate) fn concurrency_exhausted(request_id: RequestId) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: ErrorCode::ResourceExhausted,
            title: "Request capacity exhausted",
            detail: "The bounded request concurrency limit is currently full.",
            retry: RetryAdvice::AfterBackoff,
            request_id,
            retry_after_seconds: Some(1),
            authenticate: false,
            desktop_generation: None,
        }
    }

    pub(crate) fn resource_exhausted(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::ResourceExhausted,
            "Control capacity exhausted",
            "A bounded control queue or quota is currently full.",
            RetryAdvice::AfterBackoff,
            ControlMetadata::new(request_id).with_retry_after(1),
        )
    }

    pub(crate) fn request_timeout(request_id: RequestId, exact_command_retry: bool) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            code: ErrorCode::RequestOutcomeUnknown,
            title: "Request outcome unknown",
            detail: "The response deadline elapsed after dispatch; the operation may still complete.",
            retry: if exact_command_retry {
                RetryAdvice::SameCommandId
            } else {
                RetryAdvice::Never
            },
            request_id,
            retry_after_seconds: None,
            authenticate: false,
            desktop_generation: None,
        }
    }

    pub(crate) fn deadline_before_effect(request_id: RequestId) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            code: ErrorCode::DeadlineExceededBeforeEffect,
            title: "Deadline exceeded before effect",
            detail: "The semantic read deadline elapsed without a mutating effect; retrying the read is safe.",
            retry: RetryAdvice::AfterBackoff,
            request_id,
            retry_after_seconds: Some(1),
            authenticate: false,
            desktop_generation: None,
        }
    }

    pub(crate) fn origin_denied(request_id: RequestId) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: ErrorCode::PermissionDenied,
            title: "WebSocket origin denied",
            detail: "The browser Origin is not in the configured exact allowlist.",
            retry: RetryAdvice::Never,
            request_id,
            retry_after_seconds: None,
            authenticate: false,
            desktop_generation: None,
        }
    }

    pub(crate) fn unsupported_version(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::BAD_REQUEST,
            ErrorCode::UnsupportedVersion,
            "Unsupported protocol version",
            "The requested protocol version is not supported by this endpoint.",
            RetryAdvice::Never,
            ControlMetadata::new(request_id),
        )
    }

    pub(crate) fn stale_reference(
        request_id: RequestId,
        current_generation: Option<DesktopGeneration>,
    ) -> Self {
        Self::control(
            StatusCode::CONFLICT,
            ErrorCode::StaleReference,
            "Stale reference",
            "The request targets an earlier desktop generation.",
            RetryAdvice::AfterResync,
            ControlMetadata::new(request_id).with_generation(current_generation),
        )
    }

    pub(crate) fn accessibility_resync_required(
        request_id: RequestId,
        current_generation: Option<DesktopGeneration>,
    ) -> Self {
        Self::control(
            StatusCode::CONFLICT,
            ErrorCode::ToolkitProtocolError,
            "Accessibility resynchronization required",
            "Accessibility completeness was lost; refresh authoritative state before retrying.",
            RetryAdvice::AfterResync,
            ControlMetadata::new(request_id).with_generation(current_generation),
        )
    }

    pub(crate) fn ambiguous_accessibility_target(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::CONFLICT,
            ErrorCode::AmbiguousTarget,
            "Ambiguous accessibility target",
            "The selector matched more than one accessible element.",
            RetryAdvice::Never,
            ControlMetadata::new(request_id),
        )
    }

    pub(crate) fn accessibility_query_limit_exceeded(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::QueryBudgetExceeded,
            "Accessibility query limit exceeded",
            "The bounded accessibility query could not complete within its declared limits.",
            RetryAdvice::Never,
            ControlMetadata::new(request_id),
        )
    }

    pub(crate) fn accessibility_element_not_found(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::NOT_FOUND,
            ErrorCode::ElementNotFound,
            "Accessible element not found",
            "The exact generation-fenced accessible element does not exist.",
            RetryAdvice::Never,
            ControlMetadata::new(request_id),
        )
    }

    pub(crate) fn accessibility_interface_not_supported(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InterfaceNotSupported,
            "Accessibility interface not supported",
            "The target does not expose the required accessibility interface.",
            RetryAdvice::Never,
            ControlMetadata::new(request_id),
        )
    }

    pub(crate) fn command_id_conflict(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::CONFLICT,
            ErrorCode::CommandIdConflict,
            "Command identifier conflict",
            "The command identifier is already bound to different command content.",
            RetryAdvice::Never,
            ControlMetadata::new(request_id),
        )
    }

    pub(crate) fn lease_conflict(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::CONFLICT,
            ErrorCode::LeaseConflict,
            "Controller lease conflict",
            "The requested controller lease operation conflicts with current state.",
            RetryAdvice::AfterBackoff,
            ControlMetadata::new(request_id).with_retry_after(1),
        )
    }

    pub(crate) fn cancellation_conflict(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::CONFLICT,
            ErrorCode::UnsupportedByTarget,
            "Cancellation is not safe",
            "The command is in an atomic stage that cannot be cancelled safely.",
            RetryAdvice::Never,
            ControlMetadata::new(request_id),
        )
    }

    pub(crate) fn capability_unavailable(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::CapabilityUnavailable,
            "Desktop capability unavailable",
            "The desktop is not ready to accept this operation.",
            RetryAdvice::AfterBackoff,
            ControlMetadata::new(request_id).with_retry_after(1),
        )
    }

    pub(crate) fn unsupported_by_target(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::UnsupportedByTarget,
            "Operation unsupported by target",
            "The target cannot perform this operation.",
            RetryAdvice::Never,
            ControlMetadata::new(request_id),
        )
    }

    pub(crate) fn internal(request_id: RequestId) -> Self {
        Self::control(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            "Internal server error",
            "The server could not complete the request safely.",
            RetryAdvice::Never,
            ControlMetadata::new(request_id),
        )
    }

    fn control(
        status: StatusCode,
        code: ErrorCode,
        title: &'static str,
        detail: &'static str,
        retry: RetryAdvice,
        metadata: ControlMetadata,
    ) -> Self {
        Self {
            status,
            code,
            title,
            detail,
            retry,
            request_id: metadata.request_id,
            retry_after_seconds: metadata.retry_after_seconds,
            authenticate: false,
            desktop_generation: metadata.desktop_generation,
        }
    }

    #[cfg(test)]
    pub(crate) const fn status(&self) -> StatusCode {
        self.status
    }
}

struct ControlMetadata {
    request_id: RequestId,
    retry_after_seconds: Option<u16>,
    desktop_generation: Option<DesktopGeneration>,
}

impl ControlMetadata {
    const fn new(request_id: RequestId) -> Self {
        Self {
            request_id,
            retry_after_seconds: None,
            desktop_generation: None,
        }
    }

    const fn with_retry_after(mut self, seconds: u16) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }

    const fn with_generation(mut self, generation: Option<DesktopGeneration>) -> Self {
        self.desktop_generation = generation;
        self
    }
}

impl IntoResponse for ApiProblem {
    fn into_response(self) -> Response {
        let body = ProblemBody {
            problem_type: format!(
                "https://xenoteer.dev/problems/{}",
                self.code.as_str().replace('_', "-")
            ),
            title: self.title,
            status: self.status.as_u16(),
            code: self.code,
            detail: self.detail,
            instance: format!("urn:xenoteer:request:{}", self.request_id),
            retry: self.retry,
            effect_stage: if self.code == ErrorCode::RequestOutcomeUnknown {
                EffectStage::OutcomeUnknown
            } else {
                EffectStage::None
            },
            desktop_generation: self.desktop_generation,
            details: EmptyProblemDetails {},
        };
        let mut response = (self.status, Json(body)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if let Some(seconds) = self.retry_after_seconds
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        if self.authenticate {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"xenoteer\""),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[tokio::test]
    async fn authentication_problem_has_rfc_content_type_and_challenge()
    -> Result<(), Box<dyn std::error::Error>> {
        let request_id = RequestId::new();
        let problem = ApiProblem::authentication_required(request_id);
        assert_eq!(problem.status(), StatusCode::UNAUTHORIZED);
        let response = problem.into_response();
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("application/problem+json"))
        );
        assert_eq!(
            response.headers().get(header::WWW_AUTHENTICATE),
            Some(&HeaderValue::from_static("Bearer realm=\"xenoteer\""))
        );
        let body = to_bytes(response.into_body(), 4_096).await?;
        let value: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(value["status"], 401);
        assert_eq!(value["code"], "authentication_required");
        assert_eq!(
            value["instance"],
            format!("urn:xenoteer:request:{request_id}")
        );
        assert_eq!(value["effect_stage"], "none");
        assert_eq!(value["details"], serde_json::json!({}));
        let decoded: xenoteer_protocol::Problem = serde_json::from_slice(&body)?;
        decoded.validate()?;
        Ok(())
    }

    #[test]
    fn exhaustion_problem_includes_bounded_retry_after() {
        let response = ApiProblem::concurrency_exhausted(RequestId::new()).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers().get(header::RETRY_AFTER),
            Some(&HeaderValue::from_static("1"))
        );
    }
}
