//! Production core GetImage backend owned by the capture actor thread.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, MapState, Window};
use xenoteer_protocol::{
    CursorCaptureEvidence, Rect, ScreenshotRequest, ScreenshotTarget, WindowRef,
};

use super::actor::{
    CaptureActorFailureKind, CaptureBackend, CaptureBackendError, CapturedFrame,
    RawCaptureLimitation, RawCaptureRevalidationError, RawCaptureTransport,
    RawWindowCaptureGeometry, Revalidator,
};
use super::cursor::{CursorSnapshot, compose_cursor};
use super::geometry::{resolve_root_area, resolve_window_area};
use super::{decode_image_bgra8, get_image_bgra8};
use crate::{ExtensionName, Result, X11Error, connect};

pub(super) struct X11CaptureBackend {
    connection: x11rb::rust_connection::RustConnection,
    info: crate::XConnectionInfo,
    xfixes_cursor: bool,
    mit_shm: bool,
}

impl X11CaptureBackend {
    pub fn open(display: &str) -> Result<Self> {
        let opened = connect(display)?;
        let xfixes_cursor = opened
            .info
            .extensions
            .get(ExtensionName::XFixes)
            .is_some_and(|extension| extension.present);
        if xfixes_cursor {
            opened
                .connection
                .xfixes_query_version(5, 0)
                .map_err(|error| X11Error::Connection(error.to_string()))?
                .reply()
                .map_err(|error| X11Error::Reply(error.to_string()))?;
        }
        let mit_shm = if opened
            .info
            .extensions
            .get(ExtensionName::MitShm)
            .is_some_and(|extension| extension.present)
        {
            let version = opened
                .connection
                .shm_query_version()
                .map_err(|error| X11Error::Connection(error.to_string()))?
                .reply();
            match version {
                Ok(version) => {
                    version.major_version > 1
                        || (version.major_version == 1 && version.minor_version >= 2)
                }
                Err(ReplyError::X11Error(_)) => false,
                Err(ReplyError::ConnectionError(error)) => {
                    return Err(X11Error::Connection(error.to_string()));
                }
            }
        } else {
            false
        };
        Ok(Self {
            connection: opened.connection,
            info: opened.info,
            xfixes_cursor,
            mit_shm,
        })
    }

    fn capture_root(
        &mut self,
        request: &ScreenshotRequest,
    ) -> std::result::Result<CapturedFrame, CaptureBackendError> {
        let area = resolve_root_area(
            self.info.root,
            self.info.width_px,
            self.info.height_px,
            request.region,
        )
        .map_err(CaptureBackendError::Operation)?;
        self.capture_area(request, area, RawCaptureLimitation::RootVisibleFramebuffer)
    }

    fn capture_window(
        &mut self,
        request: &ScreenshotRequest,
        revalidate: Revalidator,
        reference: &WindowRef,
    ) -> std::result::Result<CapturedFrame, CaptureBackendError> {
        revalidate(reference).map_err(|error| {
            CaptureBackendError::Operation(match error {
                RawCaptureRevalidationError::StaleReference => {
                    CaptureActorFailureKind::StaleReference
                }
                RawCaptureRevalidationError::TargetVanished => {
                    CaptureActorFailureKind::TargetVanished
                }
                RawCaptureRevalidationError::Unavailable => {
                    CaptureActorFailureKind::BackendUnavailable
                }
            })
        })?;
        let geometry = self.fresh_window_geometry(reference.xid)?;
        let area = resolve_window_area(
            &request.target,
            request.region,
            self.info.width_px,
            self.info.height_px,
            geometry,
        )
        .map_err(CaptureBackendError::Operation)?;
        let limitation = match request.target {
            ScreenshotTarget::WindowVisible { .. } => {
                RawCaptureLimitation::WindowVisibleIncludesOccluders
            }
            ScreenshotTarget::WindowDrawable { .. } => {
                RawCaptureLimitation::WindowDrawableObscuredUndefined
            }
            ScreenshotTarget::Root => {
                return Err(CaptureBackendError::Operation(
                    CaptureActorFailureKind::InvalidTarget,
                ));
            }
        };
        self.capture_area(request, area, limitation)
    }

    fn fresh_window_geometry(
        &self,
        window: Window,
    ) -> std::result::Result<RawWindowCaptureGeometry, CaptureBackendError> {
        let attributes = self
            .connection
            .get_window_attributes(window)
            .map_err(|_| CaptureBackendError::Unavailable)?
            .reply()
            .map_err(map_window_reply)?;
        let client = self
            .connection
            .get_geometry(window)
            .map_err(|_| CaptureBackendError::Unavailable)?
            .reply()
            .map_err(map_window_reply)?;
        let translated = self
            .connection
            .translate_coordinates(window, self.info.root, 0, 0)
            .map_err(|_| CaptureBackendError::Unavailable)?
            .reply()
            .map_err(map_window_reply)?;
        let client_root = Rect::new(
            i32::from(translated.dst_x),
            i32::from(translated.dst_y),
            u32::from(client.width),
            u32::from(client.height),
        )
        .map_err(|_| CaptureBackendError::Operation(CaptureActorFailureKind::RegionOutOfBounds))?;
        let tree = self
            .connection
            .query_tree(window)
            .map_err(|_| CaptureBackendError::Unavailable)?
            .reply()
            .map_err(map_window_reply)?;
        let frame_root = if tree.parent == self.info.root {
            Some(client_root)
        } else {
            Some(self.frame_geometry(tree.parent)?)
        };
        Ok(RawWindowCaptureGeometry {
            root: self.info.root,
            window,
            client_root,
            frame_root,
            viewable: attributes.map_state == MapState::VIEWABLE,
        })
    }

    fn frame_geometry(&self, frame: Window) -> std::result::Result<Rect, CaptureBackendError> {
        let geometry = self
            .connection
            .get_geometry(frame)
            .map_err(|_| CaptureBackendError::Unavailable)?
            .reply()
            .map_err(map_window_reply)?;
        let translated = self
            .connection
            .translate_coordinates(frame, self.info.root, 0, 0)
            .map_err(|_| CaptureBackendError::Unavailable)?
            .reply()
            .map_err(map_window_reply)?;
        let border = u32::from(geometry.border_width);
        let x = i32::from(translated.dst_x)
            .checked_sub(i32::from(geometry.border_width))
            .ok_or(CaptureBackendError::Operation(
                CaptureActorFailureKind::RegionOutOfBounds,
            ))?;
        let y = i32::from(translated.dst_y)
            .checked_sub(i32::from(geometry.border_width))
            .ok_or(CaptureBackendError::Operation(
                CaptureActorFailureKind::RegionOutOfBounds,
            ))?;
        Rect::new(
            x,
            y,
            u32::from(geometry.width)
                .checked_add(border.saturating_mul(2))
                .ok_or(CaptureBackendError::Operation(
                    CaptureActorFailureKind::RegionOutOfBounds,
                ))?,
            u32::from(geometry.height)
                .checked_add(border.saturating_mul(2))
                .ok_or(CaptureBackendError::Operation(
                    CaptureActorFailureKind::RegionOutOfBounds,
                ))?,
        )
        .map_err(|_| CaptureBackendError::Operation(CaptureActorFailureKind::RegionOutOfBounds))
    }

    fn capture_area(
        &mut self,
        request: &ScreenshotRequest,
        area: super::geometry::ResolvedCaptureArea,
        limitation: RawCaptureLimitation,
    ) -> std::result::Result<CapturedFrame, CaptureBackendError> {
        let cursor_before = request
            .include_cursor
            .then(|| self.cursor_snapshot())
            .transpose()?;
        let shm_capture = self.mit_shm.then(|| {
            get_image_bgra8_shm(
                &self.connection,
                &self.info,
                area.drawable,
                area.drawable_x,
                area.drawable_y,
                area.width,
                area.height,
            )
        });
        let capture = match shm_capture {
            Some(Ok(image)) => Ok((image, RawCaptureTransport::MitShm, false)),
            Some(Err(_)) => {
                self.mit_shm = false;
                get_image_bgra8(
                    &self.connection,
                    &self.info,
                    area.drawable,
                    area.drawable_x,
                    area.drawable_y,
                    area.width,
                    area.height,
                )
                .map(|image| (image, RawCaptureTransport::CoreGetImage, true))
            }
            None => get_image_bgra8(
                &self.connection,
                &self.info,
                area.drawable,
                area.drawable_x,
                area.drawable_y,
                area.width,
                area.height,
            )
            .map(|image| (image, RawCaptureTransport::CoreGetImage, false)),
        };
        let (mut bgra, transport, shm_fallback) = capture.map_err(|error| match error {
            X11Error::Connection(_) => CaptureBackendError::Unavailable,
            _ if matches!(request.target, ScreenshotTarget::Root) => {
                CaptureBackendError::Unavailable
            }
            _ => CaptureBackendError::Operation(CaptureActorFailureKind::TargetVanished),
        })?;
        let cursor = if let Some(before) = cursor_before {
            let after = self.cursor_snapshot()?;
            compose_cursor(&mut bgra, area.root_region, &before, &after).map_err(|_| {
                CaptureBackendError::Operation(CaptureActorFailureKind::CursorUnavailable)
            })?
        } else {
            CursorCaptureEvidence {
                requested: false,
                composited: false,
                serial_before: None,
                serial_after: None,
                moved_during_capture: false,
            }
        };
        Ok(CapturedFrame {
            target: request.target.clone(),
            source_region: area.root_region,
            width: u32::from(area.width),
            height: u32::from(area.height),
            bgra,
            cursor,
            limitation,
            transport,
            shm_fallback,
        })
    }

    fn cursor_snapshot(&self) -> std::result::Result<CursorSnapshot, CaptureBackendError> {
        if !self.xfixes_cursor {
            return Err(CaptureBackendError::Operation(
                CaptureActorFailureKind::CursorUnavailable,
            ));
        }
        let reply = self
            .connection
            .xfixes_get_cursor_image()
            .map_err(|_| CaptureBackendError::Unavailable)?
            .reply()
            .map_err(|error| match error {
                ReplyError::ConnectionError(_) => CaptureBackendError::Unavailable,
                ReplyError::X11Error(_) => {
                    CaptureBackendError::Operation(CaptureActorFailureKind::CursorUnavailable)
                }
            })?;
        let snapshot = CursorSnapshot {
            x: reply.x,
            y: reply.y,
            width: reply.width,
            height: reply.height,
            xhot: reply.xhot,
            yhot: reply.yhot,
            serial: reply.cursor_serial,
            premultiplied_argb: reply.cursor_image,
        };
        snapshot.validate().map_err(|_| {
            CaptureBackendError::Operation(CaptureActorFailureKind::CursorUnavailable)
        })?;
        Ok(snapshot)
    }
}

fn get_image_bgra8_shm(
    connection: &x11rb::rust_connection::RustConnection,
    info: &crate::XConnectionInfo,
    drawable: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> Result<Vec<u8>> {
    let pixels = super::image::validate_hard_capture_dimensions(
        u32::from(width),
        u32::from(height),
        "MIT-SHM capture",
    )?;
    let capacity = pixels
        .checked_mul(4)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| X11Error::Pixel("MIT-SHM segment size overflow".to_owned()))?;
    let segment = connection
        .generate_id()
        .map_err(|error| X11Error::Connection(error.to_string()))?;
    let created = connection
        .shm_create_segment(segment, capacity, false)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))?;
    let mut file = File::from(created.shm_fd);
    let capture = match connection.shm_get_image(
        drawable,
        x,
        y,
        width,
        height,
        u32::MAX,
        ImageFormat::Z_PIXMAP.into(),
        segment,
        0,
    ) {
        Ok(cookie) => cookie
            .reply()
            .map_err(|error| X11Error::Reply(error.to_string())),
        Err(error) => Err(X11Error::Connection(error.to_string())),
    };
    let body = capture.and_then(|reply| {
        if reply.size == 0 || reply.size > capacity {
            return Err(X11Error::Pixel(
                "MIT-SHM reply size is outside the allocated segment".to_owned(),
            ));
        }
        let body_len = usize::try_from(reply.size)
            .map_err(|_| X11Error::Pixel("MIT-SHM reply size overflow".to_owned()))?;
        let mut data = vec![0_u8; body_len];
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.read_exact(&mut data))
            .map_err(|error| X11Error::Pixel(format!("could not read MIT-SHM segment: {error}")))?;
        Ok((reply, data))
    });
    let detach = connection
        .shm_detach(segment)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .check()
        .map_err(|error| X11Error::Reply(error.to_string()));
    detach?;
    let (reply, data) = body?;
    decode_image_bgra8(
        connection,
        info,
        reply.depth,
        reply.visual,
        width,
        height,
        data,
    )
}

impl CaptureBackend for X11CaptureBackend {
    fn capture(
        &mut self,
        request: &ScreenshotRequest,
        revalidate: Option<Revalidator>,
    ) -> std::result::Result<CapturedFrame, CaptureBackendError> {
        match &request.target {
            ScreenshotTarget::Root => {
                if revalidate.is_some() {
                    return Err(CaptureBackendError::Operation(
                        CaptureActorFailureKind::InvalidTarget,
                    ));
                }
                self.capture_root(request)
            }
            ScreenshotTarget::WindowVisible { window, .. }
            | ScreenshotTarget::WindowDrawable { window } => {
                let revalidate = revalidate.ok_or(CaptureBackendError::Operation(
                    CaptureActorFailureKind::StaleReference,
                ))?;
                self.capture_window(request, revalidate, window)
            }
        }
    }

    fn shutdown(&mut self) {
        let _ignored = self.connection.flush();
    }
}

fn map_window_reply(error: ReplyError) -> CaptureBackendError {
    match error {
        ReplyError::ConnectionError(_) => CaptureBackendError::Unavailable,
        ReplyError::X11Error(_) => {
            CaptureBackendError::Operation(CaptureActorFailureKind::TargetVanished)
        }
    }
}

#[cfg(test)]
mod live_tests {
    use std::time::{Duration, Instant};

    use tokio_util::sync::CancellationToken;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
    use xenoteer_protocol::{
        DesktopGeneration, DesktopId, Rect, ScreenshotFormat, ScreenshotRequest, ScreenshotTarget,
        WindowCaptureSpace, WindowIdentityHash, WindowRef,
    };

    use crate::capture::{
        CaptureActorExit, CaptureActorFailureKind, RawCaptureLimitation, spawn_capture_actor,
    };
    use crate::{ExtensionName, connect};

    use super::{CaptureBackend, RawCaptureTransport, X11CaptureBackend};

    fn display() -> String {
        std::env::var("XENOTEER_TEST_DISPLAY").unwrap_or_else(|_| ":99".to_owned())
    }

    fn window_reference(window: u32) -> Result<WindowRef, Box<dyn std::error::Error>> {
        Ok(WindowRef {
            desktop_id: DesktopId::new(),
            desktop_generation: DesktopGeneration::new(),
            xid: window,
            observed_generation: 1,
            identity_hash: WindowIdentityHash::new("a".repeat(64))?,
        })
    }

    fn request(
        target: ScreenshotTarget,
        region: Option<Rect>,
        include_cursor: bool,
    ) -> ScreenshotRequest {
        ScreenshotRequest {
            target,
            region,
            format: ScreenshotFormat::RawBgra,
            include_cursor,
            scale: None,
            max_bytes: None,
        }
    }

    #[test]
    #[ignore = "requires an explicitly provisioned authenticated Xvfb display"]
    fn captures_root_through_distinct_actor_connection()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let display = display();
        let (handle, join) = spawn_capture_actor(&display)?;
        let result = handle
            .try_capture_root(
                ScreenshotRequest {
                    target: ScreenshotTarget::Root,
                    region: Some(xenoteer_protocol::Rect::new(0, 0, 2, 2)?),
                    format: ScreenshotFormat::RawBgra,
                    include_cursor: false,
                    scale: None,
                    max_bytes: None,
                },
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )?
            .recv_timeout(Duration::from_secs(2))??;
        assert_eq!(result.bytes.len(), 16);
        assert_eq!(join.join(), CaptureActorExit::Stopped);
        Ok(())
    }

    #[test]
    #[ignore = "requires an isolated authenticated Xvfb display with MIT-SHM 1.2"]
    fn mit_shm_and_core_get_image_are_byte_identical()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let display = display();
        let producer = connect(&display)?;
        assert!(
            producer
                .info
                .extensions
                .require(ExtensionName::MitShm)?
                .present
        );
        producer
            .connection
            .clear_area(false, producer.info.root, 0, 0, 64, 64)?
            .check()?;
        producer.connection.get_input_focus()?.reply()?;
        let request = request(
            ScreenshotTarget::Root,
            Some(Rect::new(0, 0, 64, 64)?),
            false,
        );

        let mut shm = X11CaptureBackend::open(&display)?;
        assert!(shm.mit_shm);
        let shm_frame = shm
            .capture(&request, None)
            .map_err(|_| std::io::Error::other("MIT-SHM capture failed"))?;
        assert_eq!(shm_frame.transport, RawCaptureTransport::MitShm);
        assert!(!shm_frame.shm_fallback);

        let mut core = X11CaptureBackend::open(&display)?;
        core.mit_shm = false;
        let core_frame = core
            .capture(&request, None)
            .map_err(|_| std::io::Error::other("core GetImage capture failed"))?;
        assert_eq!(core_frame.transport, RawCaptureTransport::CoreGetImage);
        assert!(!core_frame.shm_fallback);
        assert_eq!(shm_frame.source_region, core_frame.source_region);
        assert_eq!(shm_frame.bgra, core_frame.bgra);
        shm.shutdown();
        core.shutdown();
        Ok(())
    }

    #[test]
    #[ignore = "requires an isolated authenticated bare Xvfb display"]
    fn window_crop_occlusion_drawable_and_unmapped_semantics_are_truthful()
    -> Result<(), Box<dyn std::error::Error>> {
        let display = display();
        let producer = connect(&display)?;
        let screen = &producer.connection.setup().roots[producer.info.screen_index];
        let base = producer.connection.generate_id()?;
        producer
            .connection
            .create_window(
                screen.root_depth,
                base,
                screen.root,
                32,
                32,
                16,
                16,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &CreateWindowAux::new().background_pixel(screen.white_pixel),
            )?
            .check()?;
        let occluder = producer.connection.generate_id()?;
        producer
            .connection
            .create_window(
                screen.root_depth,
                occluder,
                screen.root,
                32,
                32,
                8,
                16,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &CreateWindowAux::new().background_pixel(screen.black_pixel),
            )?
            .check()?;
        producer.connection.map_window(base)?.check()?;
        producer.connection.map_window(occluder)?.check()?;
        producer.connection.get_input_focus()?.reply()?;

        let reference = window_reference(base)?;
        let crop = Rect::new(4, 3, 8, 6)?;
        let (handle, join) = spawn_capture_actor(&display)?;
        let visible = handle
            .try_capture_window(
                request(
                    ScreenshotTarget::WindowVisible {
                        window: reference.clone(),
                        coordinate_space: WindowCaptureSpace::Client,
                    },
                    Some(crop),
                    false,
                ),
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
                move |candidate| {
                    assert_eq!(candidate.xid, base);
                    Ok(())
                },
            )?
            .recv_timeout(Duration::from_secs(2))??;
        assert_eq!(visible.source_region, Rect::new(36, 35, 8, 6)?);
        assert_eq!(
            visible.limitation,
            RawCaptureLimitation::WindowVisibleIncludesOccluders
        );
        assert_eq!(&visible.bytes.expose_secret()[0..4], &[0, 0, 0, 255]);
        assert_eq!(&visible.bytes.expose_secret()[28..32], &[255; 4]);

        let drawable = handle
            .try_capture_window(
                request(
                    ScreenshotTarget::WindowDrawable {
                        window: reference.clone(),
                    },
                    Some(crop),
                    false,
                ),
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
                |_| Ok(()),
            )?
            .recv_timeout(Duration::from_secs(2))??;
        assert_eq!(drawable.bytes.len(), 8 * 6 * 4);
        assert_eq!(
            drawable.limitation,
            RawCaptureLimitation::WindowDrawableObscuredUndefined
        );

        producer.connection.unmap_window(base)?.check()?;
        producer.connection.get_input_focus()?.reply()?;
        let unmapped = handle
            .try_capture_window(
                request(
                    ScreenshotTarget::WindowDrawable { window: reference },
                    None,
                    false,
                ),
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
                |_| Ok(()),
            )?
            .recv_timeout(Duration::from_secs(2))?;
        let failure = match unmapped {
            Ok(_) => {
                return Err(std::io::Error::other("unmapped window capture succeeded").into());
            }
            Err(failure) => failure,
        };
        assert_eq!(failure.kind, CaptureActorFailureKind::WindowNotViewable);
        assert_eq!(join.join(), CaptureActorExit::Stopped);
        Ok(())
    }

    #[test]
    #[ignore = "requires an authenticated Xvfb display with XFIXES"]
    fn root_cursor_capture_records_xfixes_snapshot_and_composition()
    -> Result<(), Box<dyn std::error::Error>> {
        let display = display();
        let producer = connect(&display)?;
        assert!(
            producer
                .info
                .extensions
                .require(ExtensionName::XFixes)?
                .present
        );
        producer
            .connection
            .warp_pointer(x11rb::NONE, producer.info.root, 0, 0, 0, 0, 24, 24)?
            .check()?;
        producer.connection.get_input_focus()?.reply()?;

        let (handle, join) = spawn_capture_actor(&display)?;
        let result = handle
            .try_capture_root(
                request(ScreenshotTarget::Root, Some(Rect::new(0, 0, 64, 64)?), true),
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )?
            .recv_timeout(Duration::from_secs(2))??;
        assert!(result.cursor.requested);
        assert!(result.cursor.composited);
        assert!(result.cursor.serial_before.is_some());
        assert!(result.cursor.serial_after.is_some());
        assert_eq!(join.join(), CaptureActorExit::Stopped);
        Ok(())
    }
}
