//! Generation-bound domain objects over the validated HTTP API.

use std::fmt;

use bytes::Bytes;
use http::HeaderValue;
use tokio::io::{AsyncRead, AsyncWrite};
use xenoteer_protocol::{
    AccessibilityQueryLimits, ApplicationArgument, ApplicationId, ApplicationLaunchCommand,
    ArtifactContentType, ArtifactPurpose, ArtifactRef, ClipboardReadRequest, ClipboardReadResult,
    Command, ElementListPage, ElementListRequest, ElementQueryPage, ElementQueryRequest,
    ElementRef, ElementResolveRequest, ElementResolveResult, ElementSnapshotRequest,
    ElementSnapshotResult, ElementWaitRequest, ElementWaitResult, KeyboardChordCommand,
    KeyboardKeyIdentifier, KeyboardPressCommand, OneTimeViewerTicket, PointerClickCommand,
    PointerClickTarget, PointerCurve, PointerDragCommand, PointerDragTarget, PointerLogicalButton,
    PointerMoveCommand, PointerScrollCommand, PointerScrollDirection, ProcessRef,
    ProcessStatusCommand, ProcessTerminateCommand, ScreenshotRequest, ScreenshotResult,
    ViewerOrigin, ViewerTicketRequest, WindowListPage, WindowOrder, WindowPageCursor,
    WindowQueryPage, WindowQueryRequest, WindowRef, WindowResolveRequest, WindowResolveResult,
    WindowSnapshotResult, WindowWaitRequest, WindowWaitResult,
};

use crate::{CommandSubmission, ControlLease, Desktop, SdkError};

/// Physical mouse API backed by one explicit controller lease.
pub struct Mouse<'a> {
    lease: &'a ControlLease,
}

impl fmt::Debug for Mouse<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Mouse")
            .field("lease", &"<redacted>")
            .finish()
    }
}

impl Mouse<'_> {
    /// Moves through interpolated samples. Omitted duration selects server policy.
    pub fn move_to(
        &self,
        target: xenoteer_protocol::Point,
        duration_ms: Option<u32>,
    ) -> Result<CommandSubmission, SdkError> {
        self.lease.submit(Command::PointerMove(PointerMoveCommand {
            target,
            duration_ms,
            curve: PointerCurve::Smooth,
        }))
    }

    /// Performs a complete atomic click, optionally moving smoothly first.
    pub fn click(
        &self,
        target: PointerClickTarget,
        button: PointerLogicalButton,
        count: u8,
        duration_ms: Option<u32>,
    ) -> Result<CommandSubmission, SdkError> {
        self.lease
            .submit(Command::PointerClick(PointerClickCommand {
                target,
                button,
                count,
                duration_ms,
                curve: PointerCurve::Smooth,
                pre_click_dwell_ms: 0,
                press_duration_ms: 0,
                inter_click_interval_ms: 100,
            }))
    }

    /// Performs one atomic press/move/release drag with smooth interpolation.
    pub fn drag(
        &self,
        target: PointerDragTarget,
        button: PointerLogicalButton,
        duration_ms: Option<u32>,
    ) -> Result<CommandSubmission, SdkError> {
        self.lease.submit(Command::PointerDrag(PointerDragCommand {
            target,
            button,
            duration_ms,
            curve: PointerCurve::Smooth,
            press_dwell_ms: 0,
            release_dwell_ms: 0,
        }))
    }

    /// Emits bounded discrete logical scroll notches.
    pub fn scroll(
        &self,
        direction: PointerScrollDirection,
        count: u16,
        interval_ms: u16,
    ) -> Result<CommandSubmission, SdkError> {
        self.lease
            .submit(Command::PointerScroll(PointerScrollCommand {
                direction,
                count,
                interval_ms,
            }))
    }
}

/// Physical keyboard API backed by one explicit controller lease.
pub struct Keyboard<'a> {
    lease: &'a ControlLease,
}

impl fmt::Debug for Keyboard<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Keyboard")
            .field("lease", &"<redacted>")
            .finish()
    }
}

impl Keyboard<'_> {
    /// Presses and releases one named, scalar, or raw key.
    pub fn press(
        &self,
        key: KeyboardKeyIdentifier,
        hold_ms: u16,
    ) -> Result<CommandSubmission, SdkError> {
        self.lease
            .submit(Command::KeyboardPress(KeyboardPressCommand {
                key,
                hold_ms,
            }))
    }

    /// Presses a modifier-first chord and releases it in reverse order.
    pub fn chord(
        &self,
        keys: Vec<KeyboardKeyIdentifier>,
        hold_ms: u16,
    ) -> Result<CommandSubmission, SdkError> {
        self.lease
            .submit(Command::KeyboardChord(KeyboardChordCommand {
                keys,
                hold_ms,
            }))
    }
}

impl ControlLease {
    /// Returns the physical mouse API for this lease.
    #[must_use]
    pub const fn mouse(&self) -> Mouse<'_> {
        Mouse { lease: self }
    }

    /// Returns the physical keyboard API for this lease.
    #[must_use]
    pub const fn keyboard(&self) -> Keyboard<'_> {
        Keyboard { lease: self }
    }
}

macro_rules! desktop_domain {
    ($name:ident) => {
        #[doc = "Cheap immutable generation-bound domain client."]
        #[derive(Clone, Debug)]
        pub struct $name {
            desktop: Desktop,
        }
    };
}

desktop_domain!(Windows);
desktop_domain!(Accessibility);
desktop_domain!(Clipboard);
desktop_domain!(Capture);
desktop_domain!(Artifacts);
desktop_domain!(Viewer);
desktop_domain!(Applications);

/// Immutable identity for one exact observed X11 window birth.
#[derive(Clone, Debug)]
pub struct WindowHandle {
    reference: WindowRef,
}

impl WindowHandle {
    /// Creates a checked immutable handle from a server-issued reference.
    pub fn from_reference(reference: WindowRef) -> Result<Self, SdkError> {
        reference
            .validate()
            .map_err(|_| SdkError::InvalidResponse)?;
        Ok(Self { reference })
    }

    /// Returns the original identity; this handle never silently retargets.
    #[must_use]
    pub const fn reference(&self) -> &WindowRef {
        &self.reference
    }

    /// Checks whether an authoritative observation still names this exact birth.
    pub fn check_current(&self, current: &WindowRef) -> Result<(), SdkError> {
        current.validate().map_err(|_| SdkError::InvalidResponse)?;
        if !same_window_identity(&self.reference, current) {
            return Err(SdkError::StaleReference);
        }
        Ok(())
    }

    /// Explicitly creates a distinct handle after caller-directed relocation.
    pub fn relocate(&self, reference: WindowRef) -> Result<Self, SdkError> {
        let relocated = Self::from_reference(reference)?;
        if !self.reference.shares_desktop_scope(&relocated.reference)
            || self.same_identity(&relocated)
        {
            return Err(SdkError::InvalidRequest);
        }
        Ok(relocated)
    }

    /// Returns whether two handles identify the same immutable XID birth.
    #[must_use]
    pub fn same_identity(&self, other: &Self) -> bool {
        same_window_identity(&self.reference, &other.reference)
    }
}

fn same_window_identity(left: &WindowRef, right: &WindowRef) -> bool {
    left.desktop_id == right.desktop_id
        && left.desktop_generation == right.desktop_generation
        && left.xid == right.xid
        && left.observed_generation == right.observed_generation
        && left.identity_hash == right.identity_hash
}

/// Immutable identity for one exact AT-SPI application/object birth.
#[derive(Clone, Debug)]
pub struct ElementHandle {
    reference: ElementRef,
}

impl ElementHandle {
    /// Creates a checked immutable handle from a server-issued reference.
    pub fn from_reference(reference: ElementRef) -> Result<Self, SdkError> {
        reference
            .validate()
            .map_err(|_| SdkError::InvalidResponse)?;
        Ok(Self { reference })
    }

    /// Returns the original identity; cache revisions never mutate the handle.
    #[must_use]
    pub const fn reference(&self) -> &ElementRef {
        &self.reference
    }

    /// Checks an authoritative observation while permitting revision advances.
    pub fn check_current(&self, current: &ElementRef) -> Result<(), SdkError> {
        current.validate().map_err(|_| SdkError::InvalidResponse)?;
        if !same_element_identity(&self.reference, current) {
            return Err(SdkError::StaleReference);
        }
        Ok(())
    }

    /// Explicitly creates a distinct handle after caller-directed relocation.
    pub fn relocate(&self, reference: ElementRef) -> Result<Self, SdkError> {
        let relocated = Self::from_reference(reference)?;
        if self.reference.desktop_id != relocated.reference.desktop_id
            || self.reference.desktop_generation != relocated.reference.desktop_generation
            || self.same_identity(&relocated)
        {
            return Err(SdkError::InvalidRequest);
        }
        Ok(relocated)
    }

    /// Returns whether two handles identify the same immutable AT-SPI object.
    #[must_use]
    pub fn same_identity(&self, other: &Self) -> bool {
        same_element_identity(&self.reference, &other.reference)
    }
}

fn same_element_identity(left: &ElementRef, right: &ElementRef) -> bool {
    left.desktop_id == right.desktop_id
        && left.desktop_generation == right.desktop_generation
        && left.atspi_generation == right.atspi_generation
        && left.application == right.application
        && left.object_path == right.object_path
        && left.object_identity_hash == right.object_identity_hash
}

impl Desktop {
    /// Returns window discovery and race-free wait operations.
    #[must_use]
    pub fn windows(&self) -> Windows {
        Windows {
            desktop: self.clone(),
        }
    }

    /// Returns accessibility discovery and race-free wait operations.
    #[must_use]
    pub fn accessibility(&self) -> Accessibility {
        Accessibility {
            desktop: self.clone(),
        }
    }

    /// Returns explicit clipboard read operations.
    #[must_use]
    pub fn clipboard(&self) -> Clipboard {
        Clipboard {
            desktop: self.clone(),
        }
    }

    /// Returns screenshot capture operations.
    #[must_use]
    pub fn capture(&self) -> Capture {
        Capture {
            desktop: self.clone(),
        }
    }

    /// Returns immutable private-artifact transfer operations.
    #[must_use]
    pub fn artifacts(&self) -> Artifacts {
        Artifacts {
            desktop: self.clone(),
        }
    }

    /// Returns one-time view-only ticket operations.
    #[must_use]
    pub fn viewer(&self) -> Viewer {
        Viewer {
            desktop: self.clone(),
        }
    }

    /// Returns registered application and exact-process operations.
    #[must_use]
    pub fn applications(&self) -> Applications {
        Applications {
            desktop: self.clone(),
        }
    }
}

fn validate_scope(
    desktop: &Desktop,
    id: xenoteer_protocol::DesktopId,
    generation: xenoteer_protocol::DesktopGeneration,
) -> Result<(), SdkError> {
    if id != desktop.id() || generation != desktop.generation() {
        return Err(SdkError::InvalidRequest);
    }
    Ok(())
}

impl Windows {
    /// Lists one deterministic page from the authoritative window model.
    pub async fn list(
        &self,
        limit: u16,
        order: WindowOrder,
        cursor: Option<&WindowPageCursor>,
    ) -> Result<WindowListPage, SdkError> {
        if limit == 0 || limit > xenoteer_protocol::MAX_WINDOW_PAGE_LIMIT {
            return Err(SdkError::InvalidRequest);
        }
        let order = serde_json::to_string(&order).map_err(|_| SdkError::EncodeRequest)?;
        let order = order
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or(SdkError::EncodeRequest)?;
        let mut path = format!(
            "/v1/desktops/{}/windows?desktop_generation={}&limit={limit}&order={order}",
            self.desktop.id(),
            self.desktop.generation()
        );
        if let Some(cursor) = cursor {
            path.push_str("&cursor=");
            path.push_str(cursor.as_str());
        }
        let response: WindowListPage = self.desktop.transport.get_json(&path).await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        validate_scope(
            &self.desktop,
            response.desktop_id,
            response.desktop_generation,
        )?;
        Ok(response)
    }

    /// Executes a bounded typed selector query.
    pub async fn query(&self, request: &WindowQueryRequest) -> Result<WindowQueryPage, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            request.desktop_id,
            request.desktop_generation,
        )?;
        let path = format!("/v1/desktops/{}/windows/query", self.desktop.id());
        let response: WindowQueryPage = self.desktop.transport.post_json(&path, request).await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        validate_scope(
            &self.desktop,
            response.desktop_id,
            response.desktop_generation,
        )?;
        Ok(response)
    }

    /// Resolves exactly one match unless the request explicitly selects `first`.
    pub async fn resolve(
        &self,
        request: &WindowResolveRequest,
    ) -> Result<WindowResolveResult, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            request.desktop_id,
            request.desktop_generation,
        )?;
        let path = format!("/v1/desktops/{}/windows/resolve", self.desktop.id());
        let response: WindowResolveResult =
            self.desktop.transport.post_json(&path, request).await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        validate_scope(
            &self.desktop,
            response.desktop_id,
            response.desktop_generation,
        )?;
        Ok(response)
    }

    /// Waits through the server's atomic check-register-recheck implementation.
    pub async fn wait(&self, request: &WindowWaitRequest) -> Result<WindowWaitResult, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            request.desktop_id,
            request.desktop_generation,
        )?;
        let path = format!("/v1/desktops/{}/windows/wait", self.desktop.id());
        let response: WindowWaitResult = self.desktop.transport.post_json(&path, request).await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        validate_scope(
            &self.desktop,
            response.desktop_id,
            response.desktop_generation,
        )?;
        Ok(response)
    }

    /// Refreshes an exact server-issued opaque window reference token.
    pub async fn snapshot(
        &self,
        token: &xenoteer_protocol::WindowReferenceToken,
    ) -> Result<WindowSnapshotResult, SdkError> {
        let path = format!(
            "/v1/desktops/{}/windows/{}?desktop_generation={}",
            self.desktop.id(),
            token.as_str(),
            self.desktop.generation()
        );
        let response: WindowSnapshotResult = self.desktop.transport.get_json(&path).await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        validate_scope(
            &self.desktop,
            response.window.snapshot.window.desktop_id,
            response.window.snapshot.window.desktop_generation,
        )?;
        Ok(response)
    }
}

impl Accessibility {
    /// Lists elements under an explicit bounded scope.
    pub async fn list(&self, request: &ElementListRequest) -> Result<ElementListPage, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            request.desktop_id,
            request.desktop_generation,
        )?;
        self.post_validated("list", request).await
    }

    /// Executes a bounded typed element selector query.
    pub async fn query(&self, request: &ElementQueryRequest) -> Result<ElementQueryPage, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            request.desktop_id,
            request.desktop_generation,
        )?;
        self.post_validated("query", request).await
    }

    async fn post_validated<T>(
        &self,
        operation: &str,
        request: &T,
    ) -> Result<ElementListPage, SdkError>
    where
        T: serde::Serialize + ?Sized,
    {
        let path = format!(
            "/v1/desktops/{}/accessibility/elements/{operation}",
            self.desktop.id()
        );
        let response: ElementListPage = self
            .desktop
            .transport
            .post_json_with_limit(&path, request, crate::MAX_ACCESSIBILITY_RESPONSE_BYTES)
            .await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        validate_scope(
            &self.desktop,
            response.desktop_id,
            response.desktop_generation,
        )?;
        Ok(response)
    }

    /// Resolves a selector only when the complete evaluation has exactly one match.
    pub async fn resolve(
        &self,
        request: &ElementResolveRequest,
    ) -> Result<ElementResolveResult, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            request.desktop_id,
            request.desktop_generation,
        )?;
        let path = format!(
            "/v1/desktops/{}/accessibility/elements/resolve",
            self.desktop.id()
        );
        let response: ElementResolveResult = self
            .desktop
            .transport
            .post_json_with_limit(&path, request, crate::MAX_ACCESSIBILITY_RESPONSE_BYTES)
            .await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        validate_scope(
            &self.desktop,
            response.desktop_id,
            response.desktop_generation,
        )?;
        Ok(response)
    }

    /// Refreshes one exact generation-fenced element reference.
    pub async fn snapshot(
        &self,
        request: &ElementSnapshotRequest,
    ) -> Result<ElementSnapshotResult, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            request.desktop_id,
            request.desktop_generation,
        )?;
        let path = format!(
            "/v1/desktops/{}/accessibility/elements/snapshot",
            self.desktop.id()
        );
        let response: ElementSnapshotResult = self
            .desktop
            .transport
            .post_json_with_limit(&path, request, crate::MAX_ACCESSIBILITY_RESPONSE_BYTES)
            .await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        validate_scope(
            &self.desktop,
            response.element.snapshot.element.desktop_id,
            response.element.snapshot.element.desktop_generation,
        )?;
        Ok(response)
    }

    /// Waits through the server's race-free accessibility actor.
    pub async fn wait(&self, request: &ElementWaitRequest) -> Result<ElementWaitResult, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            request.desktop_id,
            request.desktop_generation,
        )?;
        let path = format!(
            "/v1/desktops/{}/accessibility/elements/wait",
            self.desktop.id()
        );
        let response: ElementWaitResult = self
            .desktop
            .transport
            .post_json_with_limit(&path, request, crate::MAX_ACCESSIBILITY_RESPONSE_BYTES)
            .await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        validate_scope(
            &self.desktop,
            response.desktop_id,
            response.desktop_generation,
        )?;
        Ok(response)
    }

    /// Returns protocol defaults for callers constructing bounded requests.
    #[must_use]
    pub fn default_limits() -> AccessibilityQueryLimits {
        AccessibilityQueryLimits::default()
    }
}

impl Clipboard {
    /// Reads a bounded representation without changing selection ownership.
    pub async fn read(
        &self,
        request: &ClipboardReadRequest,
    ) -> Result<ClipboardReadResult, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        let path = format!(
            "/v1/desktops/{}/clipboard/read?desktop_generation={}",
            self.desktop.id(),
            self.desktop.generation()
        );
        let response: ClipboardReadResult =
            self.desktop.transport.post_json(&path, request).await?;
        response
            .validate_for_desktop(self.desktop.id(), self.desktop.generation())
            .map_err(|_| SdkError::InvalidResponse)?;
        Ok(response)
    }
}

impl Capture {
    /// Captures pixels to a private immutable artifact.
    pub async fn screenshot(
        &self,
        request: &ScreenshotRequest,
    ) -> Result<ScreenshotResult, SdkError> {
        request
            .validate_for_desktop(self.desktop.id(), self.desktop.generation())
            .map_err(|_| SdkError::InvalidRequest)?;
        let path = format!(
            "/v1/desktops/{}/screenshots?desktop_generation={}",
            self.desktop.id(),
            self.desktop.generation()
        );
        let response: ScreenshotResult = self.desktop.transport.post_json(&path, request).await?;
        response
            .validate_for_desktop(self.desktop.id(), self.desktop.generation())
            .map_err(|_| SdkError::InvalidResponse)?;
        Ok(response)
    }
}

impl Artifacts {
    /// Uploads one bounded immutable clipboard-input object.
    pub async fn upload_clipboard_input(
        &self,
        content_type: ArtifactContentType,
        body: Bytes,
    ) -> Result<ArtifactRef, SdkError> {
        let artifact = self
            .desktop
            .transport
            .upload_artifact("/v1/artifacts?purpose=clipboard_input", &content_type, body)
            .await?;
        if artifact.purpose != ArtifactPurpose::ClipboardInput {
            return Err(SdkError::InvalidResponse);
        }
        validate_scope(
            &self.desktop,
            artifact.desktop_id,
            artifact.desktop_generation,
        )?;
        Ok(artifact)
    }

    /// Streams one exact-length clipboard-input object from an async reader.
    ///
    /// The reader is never collected into one in-memory buffer. `content_length`
    /// must be the exact non-zero byte length and is bounded before network I/O.
    pub async fn upload_clipboard_input_stream<R>(
        &self,
        content_type: ArtifactContentType,
        content_length: u64,
        reader: R,
    ) -> Result<ArtifactRef, SdkError>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let artifact = self
            .desktop
            .transport
            .upload_artifact_from(
                "/v1/artifacts?purpose=clipboard_input",
                &content_type,
                content_length,
                reader,
            )
            .await?;
        if artifact.purpose != ArtifactPurpose::ClipboardInput {
            return Err(SdkError::InvalidResponse);
        }
        validate_scope(
            &self.desktop,
            artifact.desktop_id,
            artifact.desktop_generation,
        )?;
        Ok(artifact)
    }

    /// Streams and verifies a complete immutable artifact to a caller sink.
    ///
    /// A failing stream may have written a prefix. Callers that need atomic
    /// files should write to a new temporary path and rename only on success.
    pub async fn download_to<W>(
        &self,
        artifact: &ArtifactRef,
        output: &mut W,
    ) -> Result<(), SdkError>
    where
        W: AsyncWrite + Unpin,
    {
        artifact.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            artifact.desktop_id,
            artifact.desktop_generation,
        )?;
        let path = format!(
            "/v1/artifacts/{}?desktop_id={}&desktop_generation={}",
            artifact.artifact_id,
            self.desktop.id(),
            self.desktop.generation()
        );
        self.desktop
            .transport
            .download_artifact_to(&path, artifact, output)
            .await
    }

    /// Deletes one exact purpose-authorized private artifact.
    pub async fn delete(&self, artifact: &ArtifactRef) -> Result<(), SdkError> {
        artifact.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            artifact.desktop_id,
            artifact.desktop_generation,
        )?;
        let path = format!(
            "/v1/artifacts/{}?desktop_id={}&desktop_generation={}",
            artifact.artifact_id,
            self.desktop.id(),
            self.desktop.generation()
        );
        self.desktop.transport.delete_artifact(&path).await
    }
}

impl Viewer {
    /// Issues a one-use view-only browser ticket bound to an explicit Origin.
    pub async fn ticket(
        &self,
        origin: &str,
        request: &ViewerTicketRequest,
    ) -> Result<OneTimeViewerTicket, SdkError> {
        request.validate().map_err(|_| SdkError::InvalidRequest)?;
        validate_scope(
            &self.desktop,
            request.desktop_id,
            request.desktop_generation,
        )?;
        let expected_origin = ViewerOrigin::new(origin).map_err(|_| SdkError::InvalidRequest)?;
        let mut origin = HeaderValue::from_str(origin).map_err(|_| SdkError::InvalidRequest)?;
        origin.set_sensitive(true);
        let path = format!("/v1/desktops/{}/viewer-tickets", self.desktop.id());
        let response: OneTimeViewerTicket = self
            .desktop
            .transport
            .post_json_with_headers(&path, request, &[("origin", origin)])
            .await?;
        response.validate().map_err(|_| SdkError::InvalidResponse)?;
        validate_scope(
            &self.desktop,
            response.desktop_id,
            response.desktop_generation,
        )?;
        if response.origin != expected_origin || response.mode != request.mode {
            return Err(SdkError::InvalidResponse);
        }
        Ok(response)
    }
}

impl Applications {
    /// Launches an image-registered profile without invoking a shell.
    pub fn launch(
        &self,
        application: ApplicationId,
        arguments: Vec<ApplicationArgument>,
    ) -> Result<CommandSubmission, SdkError> {
        self.desktop
            .submit(Command::ApplicationLaunch(ApplicationLaunchCommand {
                application,
                arguments,
            }))
    }

    /// Reads current state for one PID-reuse-safe managed-process reference.
    pub fn status(&self, process: ProcessRef) -> Result<CommandSubmission, SdkError> {
        self.desktop
            .submit(Command::ProcessStatus(ProcessStatusCommand { process }))
    }

    /// Terminates one exact managed process group after an optional grace period.
    pub fn terminate(
        &self,
        process: ProcessRef,
        grace_ms: Option<u32>,
    ) -> Result<CommandSubmission, SdkError> {
        self.desktop
            .submit(Command::ProcessTerminate(ProcessTerminateCommand {
                process,
                grace_ms,
            }))
    }
}
