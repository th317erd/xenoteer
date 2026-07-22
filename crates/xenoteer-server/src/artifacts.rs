//! Authenticated, purpose-authorized streaming artifact HTTP transport.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    Extension, Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use xenoteer_protocol::{
    ArtifactContentType, ArtifactId, ArtifactPurpose, ArtifactRef, DesktopGeneration, DesktopId,
    RequestId, Sha256Digest,
};

use crate::{
    ApiState,
    auth::{Grant, Principal},
    control::{ControlPlaneError, control_problem},
    problem::ApiProblem,
};

/// Optional request digest and mandatory download digest response header.
pub const ARTIFACT_SHA256_HEADER: &str = "x-content-sha256";

const CACHE_CONTROL_PRIVATE_NO_STORE: &str = "private, no-store";

/// Boxed future used by the object-safe artifact service boundary.
pub type ArtifactFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Explicit closed set of artifact purposes authorized for one service call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactPurposeSet(u8);

impl ArtifactPurposeSet {
    /// Returns an empty purpose set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Returns a set containing one purpose.
    #[must_use]
    pub const fn only(purpose: ArtifactPurpose) -> Self {
        Self(Self::bit(purpose))
    }

    /// Returns this set with one purpose added.
    #[must_use]
    pub const fn with(mut self, purpose: ArtifactPurpose) -> Self {
        self.0 |= Self::bit(purpose);
        self
    }

    /// Returns whether the set contains no purposes.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns whether one purpose is explicitly authorized.
    #[must_use]
    pub const fn contains(self, purpose: ArtifactPurpose) -> bool {
        self.0 & Self::bit(purpose) != 0
    }

    const fn bit(purpose: ArtifactPurpose) -> u8 {
        match purpose {
            ArtifactPurpose::ClipboardInput => 1 << 0,
            ArtifactPurpose::ClipboardOutput => 1 << 1,
            ArtifactPurpose::Screenshot => 1 << 2,
            ArtifactPurpose::ActionTrace => 1 << 3,
            ArtifactPurpose::SupportBundle => 1 << 4,
        }
    }
}

/// Authenticated and generation-fenced context supplied to the artifact service.
///
/// The HTTP layer computes `allowed_purposes` from grants for the exact operation.
/// The service must still authorize stored owner/share policy, scope, purpose,
/// expiry, and immutable digest before publishing or opening any bytes.
#[derive(Debug, Clone)]
pub struct ArtifactRequestContext {
    principal: Principal,
    request_id: RequestId,
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
    allowed_purposes: ArtifactPurposeSet,
}

impl ArtifactRequestContext {
    fn new(
        principal: Principal,
        request_id: RequestId,
        desktop_id: DesktopId,
        desktop_generation: DesktopGeneration,
        allowed_purposes: ArtifactPurposeSet,
    ) -> Self {
        Self {
            principal,
            request_id,
            desktop_id,
            desktop_generation,
            allowed_purposes,
        }
    }

    /// Returns the authenticated principal, including its prevalidated grants.
    #[must_use]
    pub const fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Returns the transport request correlation identifier.
    #[must_use]
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Returns the route-fenced desktop identifier.
    #[must_use]
    pub const fn desktop_id(&self) -> DesktopId {
        self.desktop_id
    }

    /// Returns the route-fenced desktop lifetime.
    #[must_use]
    pub const fn desktop_generation(&self) -> DesktopGeneration {
        self.desktop_generation
    }

    /// Returns the purpose set derived from grants for this operation.
    #[must_use]
    pub const fn allowed_purposes(&self) -> ArtifactPurposeSet {
        self.allowed_purposes
    }

    /// Revalidates stored reference shape, desktop lifetime, and purpose.
    ///
    /// This deliberately cannot prove ownership, sharing, expiry, or bytes;
    /// service implementations must check those properties in their live store.
    pub fn authorize_reference(&self, artifact: &ArtifactRef) -> Result<(), ControlPlaneError> {
        artifact
            .validate()
            .map_err(|_| ControlPlaneError::Internal)?;
        if artifact.desktop_id != self.desktop_id
            || artifact.desktop_generation != self.desktop_generation
            || !self.allowed_purposes.contains(artifact.purpose)
        {
            return Err(ControlPlaneError::PermissionDenied);
        }
        Ok(())
    }
}

/// Validated metadata accompanying one streaming clipboard-input upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactUploadRequest {
    /// Release one accepts only `clipboard_input`.
    pub purpose: ArtifactPurpose,
    /// Validated media type from the single `Content-Type` header.
    pub content_type: ArtifactContentType,
    /// Exact declared body length, checked again while streaming.
    pub content_length: u64,
    /// Optional caller digest, checked incrementally while streaming.
    pub expected_sha256: Option<Sha256Digest>,
}

/// Generation-fenced lookup or deletion request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactAccessRequest {
    /// Opaque artifact identifier; possession grants no authority.
    pub artifact_id: ArtifactId,
}

/// Validated artifact metadata plus a raw streaming response body.
pub struct ArtifactDownload {
    /// Metadata for the exact immutable bytes in `body`.
    pub artifact: ArtifactRef,
    /// Raw bytes; the HTTP handler never collects them into memory.
    pub body: Body,
}

/// Object-safe seam between HTTP and the private artifact store.
///
/// Implementations must treat `ArtifactRequestContext` as necessary but not
/// sufficient authorization. Uploads must consume `Body` incrementally, read no
/// more than the declared length plus one byte, enforce purpose/global quota,
/// hash while writing, compare the optional digest, and discard every partial or
/// mismatched object. Downloads and deletes must resolve metadata first and
/// recheck principal owner/share policy, desktop/generation, allowed purpose,
/// expiry, length, and digest before returning or opening bytes. In particular,
/// clipboard-input deletion requires the exact creating principal. Deleting a
/// clipboard-output or screenshot artifact requires both `artifact:delete` and
/// its originating read grant; trace/support deletion requires `artifact:delete`.
pub trait ArtifactService: Send + Sync + 'static {
    /// Streams one immutable clipboard-input upload into private storage.
    fn upload<'a>(
        &'a self,
        context: ArtifactRequestContext,
        request: ArtifactUploadRequest,
        body: Body,
    ) -> ArtifactFuture<'a, Result<ArtifactRef, ControlPlaneError>>;

    /// Opens one already authorized immutable object as a streaming body.
    fn download<'a>(
        &'a self,
        context: ArtifactRequestContext,
        request: ArtifactAccessRequest,
    ) -> ArtifactFuture<'a, Result<ArtifactDownload, ControlPlaneError>>;

    /// Deletes one already authorized object and returns its former metadata.
    fn delete<'a>(
        &'a self,
        context: ArtifactRequestContext,
        request: ArtifactAccessRequest,
    ) -> ArtifactFuture<'a, Result<ArtifactRef, ControlPlaneError>>;
}

pub(crate) type SharedArtifactService = Arc<dyn ArtifactService>;

#[derive(Debug)]
pub(crate) struct UnavailableArtifactService;

impl ArtifactService for UnavailableArtifactService {
    fn upload<'a>(
        &'a self,
        _: ArtifactRequestContext,
        _: ArtifactUploadRequest,
        _: Body,
    ) -> ArtifactFuture<'a, Result<ArtifactRef, ControlPlaneError>> {
        unavailable()
    }

    fn download<'a>(
        &'a self,
        _: ArtifactRequestContext,
        _: ArtifactAccessRequest,
    ) -> ArtifactFuture<'a, Result<ArtifactDownload, ControlPlaneError>> {
        unavailable()
    }

    fn delete<'a>(
        &'a self,
        _: ArtifactRequestContext,
        _: ArtifactAccessRequest,
    ) -> ArtifactFuture<'a, Result<ArtifactRef, ControlPlaneError>> {
        unavailable()
    }
}

fn unavailable<'a, T>() -> ArtifactFuture<'a, Result<T, ControlPlaneError>> {
    Box::pin(async { Err(ControlPlaneError::CapabilityUnavailable) })
}

#[derive(Clone)]
struct ArtifactServiceState(SharedArtifactService);

pub(crate) fn routes(service: SharedArtifactService) -> Router<ApiState> {
    Router::new()
        .route(
            "/v1/artifacts",
            post(upload).layer(axum::extract::DefaultBodyLimit::max(
                xenoteer_protocol::MAX_CLIPBOARD_ARTIFACT_BYTES as usize,
            )),
        )
        .route("/v1/artifacts/{artifact_id}", get(download).delete(remove))
        .layer(Extension(ArtifactServiceState(service)))
}

#[derive(Debug, Clone, Copy)]
enum DeclaredContentLength {
    Missing,
    Invalid,
    Value(u64),
}

/// Preserves artifact upload length metadata while bypassing only the ordinary
/// JSON-size check. Shared concurrency and timeout admission still runs.
pub(crate) async fn preserve_upload_content_length(
    mut request: axum::extract::Request<Body>,
    next: Next,
) -> Response {
    if request.method() == Method::POST && request.uri().path() == "/v1/artifacts" {
        let declared = parse_content_length(&request.headers().get_all(header::CONTENT_LENGTH));
        request.extensions_mut().insert(declared);
        if request.uri().query() == Some("purpose=clipboard_input")
            && matches!(
                declared,
                DeclaredContentLength::Value(1..=xenoteer_protocol::MAX_CLIPBOARD_ARTIFACT_BYTES)
            )
        {
            request.headers_mut().remove(header::CONTENT_LENGTH);
        }
    }
    next.run(request).await
}

fn parse_content_length(
    values: &axum::http::header::GetAll<'_, HeaderValue>,
) -> DeclaredContentLength {
    let mut values = values.iter();
    let Some(value) = values.next() else {
        return DeclaredContentLength::Missing;
    };
    if values.next().is_some() {
        return DeclaredContentLength::Invalid;
    }
    let Ok(value) = value.to_str() else {
        return DeclaredContentLength::Invalid;
    };
    let Ok(length) = value.parse::<u64>() else {
        return DeclaredContentLength::Invalid;
    };
    if length.to_string() != value {
        return DeclaredContentLength::Invalid;
    }
    DeclaredContentLength::Value(length)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactUploadQuery {
    purpose: ArtifactPurpose,
}

async fn upload(
    State(state): State<ApiState>,
    Extension(service): Extension<ArtifactServiceState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    Extension(declared_length): Extension<DeclaredContentLength>,
    query: Result<Query<ArtifactUploadQuery>, axum::extract::rejection::QueryRejection>,
    request: axum::extract::Request<Body>,
) -> Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    if !principal.has_grant(Grant::ClipboardWrite) {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let Ok(Query(query)) = query else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if query.purpose != ArtifactPurpose::ClipboardInput
        || headers.contains_key(header::TRANSFER_ENCODING)
        || headers.contains_key(header::CONTENT_ENCODING)
    {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    let content_length = match declared_length {
        DeclaredContentLength::Value(length)
            if length > 0 && length <= ArtifactPurpose::ClipboardInput.maximum_bytes() =>
        {
            length
        }
        DeclaredContentLength::Value(0) => {
            return ApiProblem::invalid_request(request_id).into_response();
        }
        DeclaredContentLength::Value(_) => {
            return ApiProblem::payload_too_large(request_id).into_response();
        }
        DeclaredContentLength::Missing | DeclaredContentLength::Invalid => {
            return ApiProblem::invalid_request(request_id).into_response();
        }
    };
    let Some(content_type) = single_header(&headers, &header::CONTENT_TYPE) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    let Ok(content_type) = content_type
        .to_str()
        .map_err(|_| ())
        .and_then(|value| ArtifactContentType::new(value).map_err(|_| ()))
    else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if content_type
        .as_str()
        .split_once('/')
        .is_some_and(|(type_, _)| type_.eq_ignore_ascii_case("multipart"))
    {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    let digest_header = HeaderName::from_static(ARTIFACT_SHA256_HEADER);
    let expected_sha256 = match optional_single_header(&headers, &digest_header) {
        Ok(Some(value)) => match value
            .to_str()
            .map_err(|_| ())
            .and_then(|value| Sha256Digest::new(value).map_err(|_| ()))
        {
            Ok(digest) => Some(digest),
            Err(()) => return ApiProblem::invalid_request(request_id).into_response(),
        },
        Ok(None) => None,
        Err(()) => return ApiProblem::invalid_request(request_id).into_response(),
    };
    let readiness = state.readiness.snapshot();
    if !readiness.is_ready() {
        return ApiProblem::capability_unavailable(request_id).into_response();
    }
    let Some(desktop_generation) = readiness.desktop_generation else {
        return ApiProblem::capability_unavailable(request_id).into_response();
    };
    let context = ArtifactRequestContext::new(
        principal,
        request_id,
        state.desktop_id,
        desktop_generation,
        ArtifactPurposeSet::only(ArtifactPurpose::ClipboardInput),
    );
    let request = ArtifactUploadRequest {
        purpose: query.purpose,
        content_type,
        content_length,
        expected_sha256,
    };
    let expected = request.clone();
    match service.0.upload(context, request, body).await {
        Ok(artifact)
            if artifact.validate().is_ok()
                && artifact.purpose == expected.purpose
                && artifact.desktop_id == state.desktop_id
                && artifact.desktop_generation == desktop_generation
                && artifact.content_type == expected.content_type
                && artifact.content_length == expected.content_length
                && expected
                    .expected_sha256
                    .as_ref()
                    .is_none_or(|digest| digest == &artifact.sha256) =>
        {
            artifact_created(artifact)
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactAccessQuery {
    desktop_id: DesktopId,
    desktop_generation: DesktopGeneration,
}

async fn download(
    State(state): State<ApiState>,
    Extension(service): Extension<ArtifactServiceState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    path: Result<Path<ArtifactId>, axum::extract::rejection::PathRejection>,
    query: Result<Query<ArtifactAccessQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let allowed_purposes = download_purposes(&principal);
    if allowed_purposes.is_empty() {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(artifact_id)), Ok(Query(query))) = (path, query) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if artifact_id.as_uuid().is_nil() {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    if let Err(problem) = crate::control::validate_generation(
        &state,
        query.desktop_id,
        query.desktop_generation,
        request_id,
    ) {
        return problem.into_response();
    }
    let context = ArtifactRequestContext::new(
        principal,
        request_id,
        query.desktop_id,
        query.desktop_generation,
        allowed_purposes,
    );
    let requested_range = requested_byte_range(&headers);
    let request = ArtifactAccessRequest { artifact_id };
    match service.0.download(context.clone(), request).await {
        Ok(download)
            if download.artifact.artifact_id == artifact_id
                && context.authorize_reference(&download.artifact).is_ok() =>
        {
            match artifact_body(download, requested_range) {
                Ok(response) => response,
                Err(()) => ApiProblem::internal(request_id).into_response(),
            }
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

async fn remove(
    State(state): State<ApiState>,
    Extension(service): Extension<ArtifactServiceState>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<ArtifactId>, axum::extract::rejection::PathRejection>,
    query: Result<Query<ArtifactAccessQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let allowed_purposes = delete_purposes(&principal);
    if allowed_purposes.is_empty() {
        return ApiProblem::permission_denied(request_id).into_response();
    }
    let (Ok(Path(artifact_id)), Ok(Query(query))) = (path, query) else {
        return ApiProblem::invalid_request(request_id).into_response();
    };
    if artifact_id.as_uuid().is_nil() {
        return ApiProblem::invalid_request(request_id).into_response();
    }
    if let Err(problem) = crate::control::validate_generation(
        &state,
        query.desktop_id,
        query.desktop_generation,
        request_id,
    ) {
        return problem.into_response();
    }
    let context = ArtifactRequestContext::new(
        principal,
        request_id,
        query.desktop_id,
        query.desktop_generation,
        allowed_purposes,
    );
    let request = ArtifactAccessRequest { artifact_id };
    match service.0.delete(context.clone(), request).await {
        Ok(artifact)
            if artifact.artifact_id == artifact_id
                && context.authorize_reference(&artifact).is_ok() =>
        {
            no_content()
        }
        Ok(_) => ApiProblem::internal(request_id).into_response(),
        Err(error) => control_problem(error, request_id).into_response(),
    }
}

fn download_purposes(principal: &Principal) -> ArtifactPurposeSet {
    let mut allowed = ArtifactPurposeSet::empty();
    if principal.has_grant(Grant::ClipboardRead) {
        allowed = allowed.with(ArtifactPurpose::ClipboardOutput);
    }
    if principal.has_grant(Grant::CaptureRead) {
        allowed = allowed.with(ArtifactPurpose::Screenshot);
    }
    if principal.has_grant(Grant::ArtifactRead) {
        allowed = allowed
            .with(ArtifactPurpose::ActionTrace)
            .with(ArtifactPurpose::SupportBundle);
    }
    allowed
}

fn delete_purposes(principal: &Principal) -> ArtifactPurposeSet {
    let mut allowed = ArtifactPurposeSet::empty();
    if principal.has_grant(Grant::ClipboardWrite) {
        allowed = allowed.with(ArtifactPurpose::ClipboardInput);
    }
    if principal.has_grant(Grant::ArtifactDelete) {
        allowed = allowed
            .with(ArtifactPurpose::ActionTrace)
            .with(ArtifactPurpose::SupportBundle);
        if principal.has_grant(Grant::ClipboardRead) {
            allowed = allowed.with(ArtifactPurpose::ClipboardOutput);
        }
        if principal.has_grant(Grant::CaptureRead) {
            allowed = allowed.with(ArtifactPurpose::Screenshot);
        }
    }
    allowed
}

fn single_header<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a HeaderValue> {
    optional_single_header(headers, name).ok().flatten()
}

fn optional_single_header<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
) -> Result<Option<&'a HeaderValue>, ()> {
    let mut values = headers.get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    Ok(first)
}

#[derive(Debug, Clone)]
enum RequestedByteRange {
    Absent,
    Header(HeaderValue),
    Invalid,
}

fn requested_byte_range(headers: &HeaderMap) -> RequestedByteRange {
    match optional_single_header(headers, &header::RANGE) {
        Ok(Some(value)) => RequestedByteRange::Header(value.clone()),
        Ok(None) => RequestedByteRange::Absent,
        Err(()) => RequestedByteRange::Invalid,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedByteRange {
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

fn resolve_byte_range(requested: &RequestedByteRange, complete_length: u64) -> ResolvedByteRange {
    let RequestedByteRange::Header(value) = requested else {
        return if matches!(requested, RequestedByteRange::Absent) {
            ResolvedByteRange::Full
        } else {
            ResolvedByteRange::Unsatisfiable
        };
    };
    let Ok(value) = value.to_str() else {
        return ResolvedByteRange::Unsatisfiable;
    };
    let Some((unit, range)) = value.split_once('=') else {
        return ResolvedByteRange::Unsatisfiable;
    };
    if !unit.eq_ignore_ascii_case("bytes") || range.contains(',') || complete_length == 0 {
        return ResolvedByteRange::Unsatisfiable;
    }
    let Some((start, end)) = range.split_once('-') else {
        return ResolvedByteRange::Unsatisfiable;
    };
    if start.is_empty() {
        let Ok(suffix_length) = end.parse::<u64>() else {
            return ResolvedByteRange::Unsatisfiable;
        };
        if suffix_length == 0 {
            return ResolvedByteRange::Unsatisfiable;
        }
        let selected_length = suffix_length.min(complete_length);
        return ResolvedByteRange::Partial {
            start: complete_length - selected_length,
            end: complete_length - 1,
        };
    }
    let Ok(start) = start.parse::<u64>() else {
        return ResolvedByteRange::Unsatisfiable;
    };
    if start >= complete_length {
        return ResolvedByteRange::Unsatisfiable;
    }
    let end = if end.is_empty() {
        complete_length - 1
    } else {
        let Ok(end) = end.parse::<u64>() else {
            return ResolvedByteRange::Unsatisfiable;
        };
        if end < start {
            return ResolvedByteRange::Unsatisfiable;
        }
        end.min(complete_length - 1)
    };
    ResolvedByteRange::Partial { start, end }
}

fn artifact_created(artifact: ArtifactRef) -> Response {
    let location = format!(
        "/v1/artifacts/{}?desktop_id={}&desktop_generation={}",
        artifact.artifact_id, artifact.desktop_id, artifact.desktop_generation
    );
    let mut response = (StatusCode::CREATED, Json(artifact)).into_response();
    add_no_store(response.headers_mut());
    if let Ok(location) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

fn artifact_body(
    download: ArtifactDownload,
    requested_range: RequestedByteRange,
) -> Result<Response, ()> {
    let ArtifactDownload { artifact, body } = download;
    let resolved_range = resolve_byte_range(&requested_range, artifact.content_length);
    let (status, response_body, response_length, content_range) = match resolved_range {
        ResolvedByteRange::Full => (StatusCode::OK, body, artifact.content_length, None),
        ResolvedByteRange::Partial { start, end } => {
            let response_length = end
                .checked_sub(start)
                .and_then(|length| length.checked_add(1))
                .ok_or(())?;
            (
                StatusCode::PARTIAL_CONTENT,
                slice_body(body, start, response_length),
                response_length,
                Some(format!("bytes {start}-{end}/{}", artifact.content_length)),
            )
        }
        ResolvedByteRange::Unsatisfiable => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            Body::empty(),
            0,
            Some(format!("bytes */{}", artifact.content_length)),
        ),
    };
    let mut response = Response::new(response_body);
    *response.status_mut() = status;
    let content_type = HeaderValue::from_str(artifact.content_type.as_str()).map_err(|_| ())?;
    let content_length = HeaderValue::from_str(&response_length.to_string()).map_err(|_| ())?;
    let sha256 = HeaderValue::from_str(artifact.sha256.as_str()).map_err(|_| ())?;
    let disposition = HeaderValue::from_str(&format!(
        "attachment; filename=\"artifact-{}\"",
        artifact.artifact_id
    ))
    .map_err(|_| ())?;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, content_length);
    response
        .headers_mut()
        .insert(HeaderName::from_static(ARTIFACT_SHA256_HEADER), sha256);
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition);
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    if let Some(content_range) = content_range {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&content_range).map_err(|_| ())?,
        );
    }
    add_no_store(response.headers_mut());
    Ok(response)
}

#[derive(Debug, thiserror::Error)]
#[error("artifact byte-range stream failed safely")]
struct ArtifactRangeBodyError;

fn slice_body(body: Body, start: u64, length: u64) -> Body {
    let source = body.into_data_stream();
    let ranged = stream::unfold(
        (source, start, length),
        |(mut source, mut skipped, mut remaining)| async move {
            if remaining == 0 {
                return None;
            }
            loop {
                let next = source.next().await;
                let bytes = match next {
                    Some(Ok(bytes)) => bytes,
                    Some(Err(_)) | None => {
                        return Some((
                            Err::<Bytes, ArtifactRangeBodyError>(ArtifactRangeBodyError),
                            (source, 0, 0),
                        ));
                    }
                };
                let bytes_length = bytes.len() as u64;
                if skipped >= bytes_length {
                    skipped -= bytes_length;
                    continue;
                }
                let start_index = usize::try_from(skipped).unwrap_or(bytes.len());
                let available = bytes.len().saturating_sub(start_index);
                let selected = usize::try_from(remaining)
                    .unwrap_or(usize::MAX)
                    .min(available);
                let output = bytes.slice(start_index..start_index + selected);
                remaining -= selected as u64;
                return Some((Ok(output), (source, 0, remaining)));
            }
        },
    );
    Body::from_stream(ranged)
}

fn no_content() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    add_no_store(response.headers_mut());
    response
}

fn add_no_store(headers: &mut HeaderMap) {
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_PRIVATE_NO_STORE),
    );
}

#[cfg(test)]
mod tests {
    use core::fmt::Write as _;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{body::to_bytes, http::Request};
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;
    use xenoteer_protocol::Timestamp;

    use super::*;
    use crate::{
        AllowedOrigins, ApiServices, Authentication, DesktopReadiness, ReadinessHandle,
        ReadinessSnapshot, StaticCapabilityProvider, StaticTokenProvider, TransportLimits,
        api_router_with_services, control::UnavailableControlPlane,
        observation::UnavailableObservationPlane,
    };

    const TOKEN: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
    const BODY_SHA256: &str = "3a6eb0790f39ac87c94f3856b2dd2c5d110e6811602261a9a923d3bb23adc8b7";

    struct FakeArtifactService {
        artifact: ArtifactRef,
        owner: &'static str,
        upload_calls: AtomicUsize,
        download_calls: AtomicUsize,
        delete_calls: AtomicUsize,
    }

    impl FakeArtifactService {
        fn new(
            desktop_id: DesktopId,
            generation: DesktopGeneration,
            purpose: ArtifactPurpose,
        ) -> Result<Self, Box<dyn std::error::Error>> {
            Ok(Self {
                artifact: ArtifactRef {
                    artifact_id: ArtifactId::new(),
                    purpose,
                    desktop_id,
                    desktop_generation: generation,
                    content_type: ArtifactContentType::new(match purpose {
                        ArtifactPurpose::Screenshot => "image/png",
                        _ => "application/octet-stream",
                    })?,
                    content_length: 4,
                    sha256: Sha256Digest::new(BODY_SHA256)?,
                    created_at: Timestamp::parse("2026-07-21T00:00:00Z")?,
                    expires_at: Timestamp::parse("2026-07-21T01:00:00Z")?,
                },
                owner: "artifact-owner",
                upload_calls: AtomicUsize::new(0),
                download_calls: AtomicUsize::new(0),
                delete_calls: AtomicUsize::new(0),
            })
        }

        fn authorize(&self, context: &ArtifactRequestContext) -> Result<(), ControlPlaneError> {
            context.authorize_reference(&self.artifact)?;
            if context.principal().id() != self.owner {
                return Err(ControlPlaneError::PermissionDenied);
            }
            Ok(())
        }
    }

    impl ArtifactService for FakeArtifactService {
        fn upload<'a>(
            &'a self,
            context: ArtifactRequestContext,
            request: ArtifactUploadRequest,
            body: Body,
        ) -> ArtifactFuture<'a, Result<ArtifactRef, ControlPlaneError>> {
            self.upload_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                if context.principal().id() != self.owner
                    || !context.allowed_purposes().contains(request.purpose)
                {
                    return Err(ControlPlaneError::PermissionDenied);
                }
                let mut stream = body.into_data_stream();
                let mut received = 0_u64;
                let mut hasher = Sha256::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|_| ControlPlaneError::InvalidRequest)?;
                    received = received
                        .checked_add(
                            u64::try_from(chunk.len()).map_err(|_| ControlPlaneError::Internal)?,
                        )
                        .ok_or(ControlPlaneError::InvalidRequest)?;
                    if received > request.content_length {
                        return Err(ControlPlaneError::InvalidRequest);
                    }
                    hasher.update(&chunk);
                }
                if received != request.content_length {
                    return Err(ControlPlaneError::InvalidRequest);
                }
                let mut encoded = String::with_capacity(64);
                for byte in hasher.finalize() {
                    write!(&mut encoded, "{byte:02x}").map_err(|_| ControlPlaneError::Internal)?;
                }
                let actual = Sha256Digest::new(encoded).map_err(|_| ControlPlaneError::Internal)?;
                if request
                    .expected_sha256
                    .as_ref()
                    .is_some_and(|expected| expected != &actual)
                {
                    return Err(ControlPlaneError::InvalidRequest);
                }
                let mut artifact = self.artifact.clone();
                artifact.content_type = request.content_type;
                artifact.content_length = request.content_length;
                artifact.sha256 = actual;
                Ok(artifact)
            })
        }

        fn download<'a>(
            &'a self,
            context: ArtifactRequestContext,
            _: ArtifactAccessRequest,
        ) -> ArtifactFuture<'a, Result<ArtifactDownload, ControlPlaneError>> {
            self.download_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                self.authorize(&context)?;
                Ok(ArtifactDownload {
                    artifact: self.artifact.clone(),
                    body: Body::from("data"),
                })
            })
        }

        fn delete<'a>(
            &'a self,
            context: ArtifactRequestContext,
            _: ArtifactAccessRequest,
        ) -> ArtifactFuture<'a, Result<ArtifactRef, ControlPlaneError>> {
            self.delete_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                self.authorize(&context)?;
                Ok(self.artifact.clone())
            })
        }
    }

    fn application(
        desktop_id: DesktopId,
        generation: DesktopGeneration,
        grants: impl IntoIterator<Item = Grant>,
        service: Arc<FakeArtifactService>,
    ) -> Result<Router, Box<dyn std::error::Error>> {
        let readiness = ReadinessHandle::new(ReadinessSnapshot::new(
            DesktopReadiness::Ready,
            Some(generation),
            None::<String>,
        ));
        let provider =
            StaticTokenProvider::single(TOKEN, Principal::new("artifact-owner", grants)?)?;
        Ok(api_router_with_services(
            readiness,
            desktop_id,
            Authentication::bearer(provider),
            StaticCapabilityProvider::empty()?,
            TransportLimits::default(),
            AllowedOrigins::default(),
            ApiServices::new(
                Arc::new(UnavailableControlPlane),
                Arc::new(UnavailableObservationPlane),
            )
            .with_artifact_service(service),
        ))
    }

    fn authorized(request: axum::http::request::Builder) -> axum::http::request::Builder {
        request.header(
            header::AUTHORIZATION,
            "Bearer 0123456789abcdef0123456789abcdef",
        )
    }

    #[tokio::test]
    async fn upload_grant_denial_happens_before_service_dispatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let service = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::ClipboardInput,
        )?);
        let response = application(
            desktop_id,
            generation,
            [Grant::DesktopStatus],
            Arc::clone(&service),
        )?
        .oneshot(
            authorized(Request::post("/v1/artifacts?purpose=clipboard_input"))
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CONTENT_LENGTH, "4")
                .body(Body::from("data"))?,
        )
        .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(service.upload_calls.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn upload_requires_one_bounded_content_length_before_dispatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let service = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::ClipboardInput,
        )?);
        let app = application(
            desktop_id,
            generation,
            [Grant::ClipboardWrite],
            Arc::clone(&service),
        )?;
        let missing = app
            .clone()
            .oneshot(
                authorized(Request::post("/v1/artifacts?purpose=clipboard_input"))
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from("data"))?,
            )
            .await?;
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

        let excessive = app
            .clone()
            .oneshot(
                authorized(Request::post("/v1/artifacts?purpose=clipboard_input"))
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(
                        header::CONTENT_LENGTH,
                        ArtifactPurpose::ClipboardInput.maximum_bytes() + 1,
                    )
                    .body(Body::from("data"))?,
            )
            .await?;
        assert_eq!(excessive.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let mut duplicate_request =
            authorized(Request::post("/v1/artifacts?purpose=clipboard_input"))
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CONTENT_LENGTH, "4")
                .body(Body::from("data"))?;
        duplicate_request
            .headers_mut()
            .append(header::CONTENT_LENGTH, HeaderValue::from_static("4"));
        let duplicate = app.oneshot(duplicate_request).await?;
        assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);
        assert_eq!(service.upload_calls.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn upload_rejects_noncanonical_digest_and_multipart_before_dispatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let service = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::ClipboardInput,
        )?);
        let app = application(
            desktop_id,
            generation,
            [Grant::ClipboardWrite],
            Arc::clone(&service),
        )?;
        let invalid_digest = app
            .clone()
            .oneshot(
                authorized(Request::post("/v1/artifacts?purpose=clipboard_input"))
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, "4")
                    .header(ARTIFACT_SHA256_HEADER, BODY_SHA256.to_ascii_uppercase())
                    .body(Body::from("data"))?,
            )
            .await?;
        assert_eq!(invalid_digest.status(), StatusCode::BAD_REQUEST);

        let multipart = app
            .oneshot(
                authorized(Request::post("/v1/artifacts?purpose=clipboard_input"))
                    .header(header::CONTENT_TYPE, "multipart/form-data;boundary=x")
                    .header(header::CONTENT_LENGTH, "4")
                    .body(Body::from("data"))?,
            )
            .await?;
        assert_eq!(multipart.status(), StatusCode::BAD_REQUEST);
        assert_eq!(service.upload_calls.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn clipboard_upload_is_the_only_large_body_exception()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let service = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::ClipboardInput,
        )?);
        let body = vec![b'x'; crate::limits::DEFAULT_MAX_BODY_BYTES + 1];
        let response = application(
            desktop_id,
            generation,
            [Grant::ClipboardWrite],
            Arc::clone(&service),
        )?
        .oneshot(
            authorized(Request::post("/v1/artifacts?purpose=clipboard_input"))
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CONTENT_LENGTH, body.len())
                .body(Body::from(body))?,
        )
        .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static(CACHE_CONTROL_PRIVATE_NO_STORE))
        );
        assert_eq!(service.upload_calls.load(Ordering::Relaxed), 1);
        Ok(())
    }

    #[tokio::test]
    async fn stale_generation_is_denied_before_download_dispatch()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let service = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::ClipboardOutput,
        )?);
        let uri = format!(
            "/v1/artifacts/{}?desktop_id={desktop_id}&desktop_generation={}",
            service.artifact.artifact_id,
            DesktopGeneration::new()
        );
        let response = application(
            desktop_id,
            generation,
            [Grant::ClipboardRead],
            Arc::clone(&service),
        )?
        .oneshot(authorized(Request::get(uri)).body(Body::empty())?)
        .await?;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(service.download_calls.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn download_grants_are_translated_to_an_explicit_purpose_set()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let service = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::Screenshot,
        )?);
        let uri = format!(
            "/v1/artifacts/{}?desktop_id={desktop_id}&desktop_generation={generation}",
            service.artifact.artifact_id
        );
        let no_grant = application(
            desktop_id,
            generation,
            [Grant::DesktopStatus],
            Arc::clone(&service),
        )?
        .oneshot(authorized(Request::get(&uri)).body(Body::empty())?)
        .await?;
        assert_eq!(no_grant.status(), StatusCode::FORBIDDEN);
        assert_eq!(service.download_calls.load(Ordering::Relaxed), 0);

        let denied = application(
            desktop_id,
            generation,
            [Grant::ClipboardRead],
            Arc::clone(&service),
        )?
        .oneshot(authorized(Request::get(&uri)).body(Body::empty())?)
        .await?;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let allowed = application(
            desktop_id,
            generation,
            [Grant::CaptureRead],
            Arc::clone(&service),
        )?
        .oneshot(authorized(Request::get(uri)).body(Body::empty())?)
        .await?;
        assert_eq!(allowed.status(), StatusCode::OK);
        assert_eq!(
            allowed.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static(CACHE_CONTROL_PRIVATE_NO_STORE))
        );
        assert_eq!(
            allowed.headers().get(ARTIFACT_SHA256_HEADER),
            Some(&HeaderValue::from_static(BODY_SHA256))
        );
        let body = to_bytes(allowed.into_body(), 16).await?;
        assert_eq!(&body[..], b"data");
        assert_eq!(service.download_calls.load(Ordering::Relaxed), 2);
        Ok(())
    }

    #[tokio::test]
    async fn download_supports_one_bounded_byte_range_with_hardened_headers()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let service = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::Screenshot,
        )?);
        let uri = format!(
            "/v1/artifacts/{}?desktop_id={desktop_id}&desktop_generation={generation}",
            service.artifact.artifact_id
        );
        let response = application(
            desktop_id,
            generation,
            [Grant::CaptureRead],
            Arc::clone(&service),
        )?
        .oneshot(
            authorized(Request::get(uri))
                .header(header::RANGE, "bytes=1-2")
                .body(Body::empty())?,
        )
        .await?;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE),
            Some(&HeaderValue::from_static("bytes 1-2/4"))
        );
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH),
            Some(&HeaderValue::from_static("2"))
        );
        assert_eq!(
            response.headers().get(header::ACCEPT_RANGES),
            Some(&HeaderValue::from_static("bytes"))
        );
        assert_eq!(
            response.headers().get("x-content-type-options"),
            Some(&HeaderValue::from_static("nosniff"))
        );
        assert!(response.headers().contains_key(header::CONTENT_DISPOSITION));
        let body = to_bytes(response.into_body(), 16).await?;
        assert_eq!(&body[..], b"at");
        Ok(())
    }

    #[tokio::test]
    async fn invalid_duplicate_and_unsatisfiable_ranges_return_empty_416()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let service = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::Screenshot,
        )?);
        let uri = format!(
            "/v1/artifacts/{}?desktop_id={desktop_id}&desktop_generation={generation}",
            service.artifact.artifact_id
        );
        for ranges in [
            vec!["bytes=4-5"],
            vec!["bytes=0-1,2-3"],
            vec!["bytes=0-1", "bytes=2-3"],
        ] {
            let mut request = authorized(Request::get(&uri));
            for range in ranges {
                request = request.header(header::RANGE, range);
            }
            let response = application(
                desktop_id,
                generation,
                [Grant::CaptureRead],
                Arc::clone(&service),
            )?
            .oneshot(request.body(Body::empty())?)
            .await?;
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(
                response.headers().get(header::CONTENT_RANGE),
                Some(&HeaderValue::from_static("bytes */4"))
            );
            assert_eq!(
                response.headers().get(header::CONTENT_LENGTH),
                Some(&HeaderValue::from_static("0"))
            );
            assert!(to_bytes(response.into_body(), 1).await?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn byte_range_parser_covers_open_suffix_clamped_and_invalid_boundaries() {
        let header =
            |value: &'static str| RequestedByteRange::Header(HeaderValue::from_static(value));
        assert_eq!(
            resolve_byte_range(&RequestedByteRange::Absent, 4),
            ResolvedByteRange::Full
        );
        assert_eq!(
            resolve_byte_range(&header("bytes=1-"), 4),
            ResolvedByteRange::Partial { start: 1, end: 3 }
        );
        assert_eq!(
            resolve_byte_range(&header("bytes=-2"), 4),
            ResolvedByteRange::Partial { start: 2, end: 3 }
        );
        assert_eq!(
            resolve_byte_range(&header("Bytes=-99"), 4),
            ResolvedByteRange::Partial { start: 0, end: 3 }
        );
        assert_eq!(
            resolve_byte_range(&header("bytes=1-99"), 4),
            ResolvedByteRange::Partial { start: 1, end: 3 }
        );
        for invalid in [
            "items=0-1",
            "bytes=",
            "bytes=-0",
            "bytes=3-2",
            "bytes=x-2",
            "bytes=0-x",
            "bytes=0-1,2-3",
        ] {
            assert_eq!(
                resolve_byte_range(&header(invalid), 4),
                ResolvedByteRange::Unsatisfiable
            );
        }
        assert_eq!(
            resolve_byte_range(&header("bytes=0-0"), 0),
            ResolvedByteRange::Unsatisfiable
        );
    }

    #[tokio::test]
    async fn byte_range_stream_crosses_chunks_and_fails_on_truncation()
    -> Result<(), Box<dyn std::error::Error>> {
        let chunks = stream::iter([
            Ok::<Bytes, ArtifactRangeBodyError>(Bytes::from_static(b"da")),
            Ok::<Bytes, ArtifactRangeBodyError>(Bytes::from_static(b"ta")),
        ]);
        let selected = slice_body(Body::from_stream(chunks), 1, 2);
        assert_eq!(&to_bytes(selected, 8).await?[..], b"at");

        let truncated = slice_body(Body::from("x"), 0, 2);
        assert!(to_bytes(truncated, 8).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn delete_uses_owner_write_or_generic_delete_purpose_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let input = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::ClipboardInput,
        )?);
        let input_uri = format!(
            "/v1/artifacts/{}?desktop_id={desktop_id}&desktop_generation={generation}",
            input.artifact.artifact_id
        );
        let input_response = application(
            desktop_id,
            generation,
            [Grant::ClipboardWrite],
            Arc::clone(&input),
        )?
        .oneshot(authorized(Request::delete(input_uri)).body(Body::empty())?)
        .await?;
        assert_eq!(input_response.status(), StatusCode::NO_CONTENT);

        let output = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::ActionTrace,
        )?);
        let output_uri = format!(
            "/v1/artifacts/{}?desktop_id={desktop_id}&desktop_generation={generation}",
            output.artifact.artifact_id
        );
        let output_response = application(
            desktop_id,
            generation,
            [Grant::ArtifactDelete],
            Arc::clone(&output),
        )?
        .oneshot(authorized(Request::delete(output_uri)).body(Body::empty())?)
        .await?;
        assert_eq!(output_response.status(), StatusCode::NO_CONTENT);
        assert_eq!(input.delete_calls.load(Ordering::Relaxed), 1);
        assert_eq!(output.delete_calls.load(Ordering::Relaxed), 1);
        Ok(())
    }

    #[tokio::test]
    async fn purpose_specific_output_delete_requires_both_grants()
    -> Result<(), Box<dyn std::error::Error>> {
        for (purpose, grants, expected_calls) in [
            (
                ArtifactPurpose::ClipboardOutput,
                vec![Grant::ArtifactDelete],
                1,
            ),
            (
                ArtifactPurpose::ClipboardOutput,
                vec![Grant::ClipboardRead],
                0,
            ),
            (ArtifactPurpose::Screenshot, vec![Grant::ArtifactDelete], 1),
            (ArtifactPurpose::Screenshot, vec![Grant::CaptureRead], 0),
        ] {
            let desktop_id = DesktopId::new();
            let generation = DesktopGeneration::new();
            let service = Arc::new(FakeArtifactService::new(desktop_id, generation, purpose)?);
            let uri = format!(
                "/v1/artifacts/{}?desktop_id={desktop_id}&desktop_generation={generation}",
                service.artifact.artifact_id
            );
            let response = application(desktop_id, generation, grants, Arc::clone(&service))?
                .oneshot(authorized(Request::delete(uri)).body(Body::empty())?)
                .await?;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(service.delete_calls.load(Ordering::Relaxed), expected_calls);
        }

        let desktop_id = DesktopId::new();
        let generation = DesktopGeneration::new();
        let service = Arc::new(FakeArtifactService::new(
            desktop_id,
            generation,
            ArtifactPurpose::Screenshot,
        )?);
        let uri = format!(
            "/v1/artifacts/{}?desktop_id={desktop_id}&desktop_generation={generation}",
            service.artifact.artifact_id
        );
        let allowed = application(
            desktop_id,
            generation,
            [Grant::ArtifactDelete, Grant::CaptureRead],
            Arc::clone(&service),
        )?
        .oneshot(authorized(Request::delete(uri)).body(Body::empty())?)
        .await?;
        assert_eq!(allowed.status(), StatusCode::NO_CONTENT);
        assert_eq!(service.delete_calls.load(Ordering::Relaxed), 1);
        Ok(())
    }
}
