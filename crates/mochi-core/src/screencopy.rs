// SPDX-License-Identifier: GPL-3.0-or-later
//! `wlr-screencopy-unstable-v1` protocol implementation for Mochi Mochi.
//!
//! Enables external Wayland screen capture tools (e.g. `grim`, `slurp`,
//! `wl-screenrec`, OBS Studio, PipeWire portal `xdg-desktop-portal-wlr`) to
//! capture physical outputs or specified sub-regions via client shared memory (SHM).

use std::{
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            gles::GlesRenderer,
            ExportMem, RendererSuper,
        },
    },
    output::Output,
    reexports::wayland_server::{
        backend::GlobalId,
        protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm},
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    },
    utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Size},
    wayland::shm,
};
use tracing::{info, trace, warn};
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};

use crate::state::MochiState;

/// Status states for a screencopy frame request.
const FRAME_STATUS_READY: u8 = 0;
const FRAME_STATUS_QUEUED: u8 = 1;
const FRAME_STATUS_DONE: u8 = 2;

/// Frame request context associated with a `ZwlrScreencopyFrameV1` resource.
#[derive(Debug, Clone)]
pub struct ScreencopyFrameUserData {
    pub output: Output,
    pub region: Rectangle<i32, Physical>,
    pub overlay_cursor: bool,
    pub status: Arc<AtomicU8>,
}

/// A pending screencopy operation waiting for the next rendered framebuffer.
#[derive(Debug)]
pub struct PendingScreencopy {
    pub frame: ZwlrScreencopyFrameV1,
    pub output: Output,
    pub region: Rectangle<i32, Physical>,
    pub buffer: WlBuffer,
    pub with_damage: bool,
    pub overlay_cursor: bool,
    pub status: Arc<AtomicU8>,
}

/// Central state manager for `zwlr_screencopy_manager_v1`.
#[derive(Debug)]
pub struct ScreencopyState {
    _global: Option<GlobalId>,
    pending: Vec<PendingScreencopy>,
}

impl ScreencopyState {
    /// Initializes and advertises the `zwlr_screencopy_manager_v1` global.
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<MochiState, ZwlrScreencopyManagerV1, _>(3, ());
        info!("Advertised zwlr_screencopy_manager_v1 (version 3) global");
        Self {
            _global: Some(global),
            pending: Vec::new(),
        }
    }

    /// Queues a frame for copy on the next composited frame.
    pub fn queue_copy(
        &mut self,
        frame: ZwlrScreencopyFrameV1,
        data: &ScreencopyFrameUserData,
        buffer: WlBuffer,
        with_damage: bool,
    ) {
        if data
            .status
            .compare_exchange(
                FRAME_STATUS_READY,
                FRAME_STATUS_QUEUED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "zwlr_screencopy_frame_v1 already used for copy",
            );
            return;
        }

        self.pending.push(PendingScreencopy {
            frame,
            output: data.output.clone(),
            region: data.region,
            buffer,
            with_damage,
            overlay_cursor: data.overlay_cursor,
            status: data.status.clone(),
        });
    }

    /// Removes any pending copies associated with a destroyed frame resource.
    pub fn remove_frame(&mut self, frame: &ZwlrScreencopyFrameV1) {
        self.pending.retain(|p| &p.frame != frame);
    }

    /// Checks if there are any pending screencopy requests for the specified output.
    pub fn has_pending_for_output(&self, output: &Output) -> bool {
        self.pending.iter().any(|p| &p.output == output)
    }

    /// Takes all pending screencopy requests for the specified output.
    pub fn take_pending_for_output(&mut self, output: &Output) -> Vec<PendingScreencopy> {
        let mut matching = Vec::new();
        let mut remaining = Vec::new();
        for p in self.pending.drain(..) {
            if &p.output == output {
                matching.push(p);
            } else {
                remaining.push(p);
            }
        }
        self.pending = remaining;
        matching
    }
}

// ============================================================================
// Wayland Protocol Implementations: GlobalDispatch & Dispatch
// ============================================================================

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for MochiState {
    fn can_view(client: Client, _global_data: &()) -> bool {
        crate::state::is_privileged_client(&client)
    }

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for MochiState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                let Some(resolved_output) = resolve_output(state, &output) else {
                    let frame_res = data_init.init(
                        frame,
                        ScreencopyFrameUserData {
                            output: fallback_dummy_output(),
                            region: Rectangle::new(Point::from((0, 0)), Size::from((1, 1))),
                            overlay_cursor: overlay_cursor != 0,
                            status: Arc::new(AtomicU8::new(FRAME_STATUS_DONE)),
                        },
                    );
                    frame_res.failed();
                    return;
                };

                let mode = resolved_output
                    .current_mode()
                    .unwrap_or(smithay::output::Mode {
                        size: (1920, 1080).into(),
                        refresh: 60_000,
                    });
                let region = Rectangle::new(Point::from((0, 0)), mode.size);

                setup_frame(
                    data_init,
                    frame,
                    resolved_output,
                    region,
                    overlay_cursor != 0,
                    manager.version(),
                );
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                let Some(resolved_output) = resolve_output(state, &output) else {
                    let frame_res = data_init.init(
                        frame,
                        ScreencopyFrameUserData {
                            output: fallback_dummy_output(),
                            region: Rectangle::new(Point::from((0, 0)), Size::from((1, 1))),
                            overlay_cursor: overlay_cursor != 0,
                            status: Arc::new(AtomicU8::new(FRAME_STATUS_DONE)),
                        },
                    );
                    frame_res.failed();
                    return;
                };

                let mode = resolved_output
                    .current_mode()
                    .unwrap_or(smithay::output::Mode {
                        size: (1920, 1080).into(),
                        refresh: 60_000,
                    });
                let scale = resolved_output.current_scale().fractional_scale();

                let out_bounds = Rectangle::new(Point::from((0, 0)), mode.size);
                let phys_loc = Point::from(((x as f64 * scale) as i32, (y as f64 * scale) as i32));
                let phys_size = Size::from((
                    (width as f64 * scale).max(1.0) as i32,
                    (height as f64 * scale).max(1.0) as i32,
                ));
                let requested_region = Rectangle::new(phys_loc, phys_size);
                let clamped_region = requested_region
                    .intersection(out_bounds)
                    .unwrap_or(Rectangle::new(Point::from((0, 0)), Size::from((1, 1))));

                setup_frame(
                    data_init,
                    frame,
                    resolved_output,
                    clamped_region,
                    overlay_cursor != 0,
                    manager.version(),
                );
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameUserData> for MochiState {
    fn request(
        state: &mut Self,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &ScreencopyFrameUserData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                state
                    .screencopy_state
                    .queue_copy(frame.clone(), data, buffer, false);
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                state
                    .screencopy_state
                    .queue_copy(frame.clone(), data, buffer, true);
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {
                state.screencopy_state.remove_frame(frame);
            }
            _ => {}
        }
    }
}

/// Helper to configure a newly requested `ZwlrScreencopyFrameV1` and advertise supported formats.
fn setup_frame(
    data_init: &mut DataInit<'_, MochiState>,
    frame: New<ZwlrScreencopyFrameV1>,
    output: Output,
    region: Rectangle<i32, Physical>,
    overlay_cursor: bool,
    version: u32,
) {
    let width = region.size.w.max(1) as u32;
    let height = region.size.h.max(1) as u32;
    let stride = width * 4;

    let user_data = ScreencopyFrameUserData {
        output,
        region,
        overlay_cursor,
        status: Arc::new(AtomicU8::new(FRAME_STATUS_READY)),
    };

    let frame_resource = data_init.init(frame, user_data);

    // Advertise supported SHM formats (ARGB8888, XRGB8888, ABGR8888, XBGR8888)
    frame_resource.buffer(wl_shm::Format::Argb8888, width, height, stride);
    frame_resource.buffer(wl_shm::Format::Xrgb8888, width, height, stride);
    frame_resource.buffer(wl_shm::Format::Abgr8888, width, height, stride);
    frame_resource.buffer(wl_shm::Format::Xbgr8888, width, height, stride);

    if version >= 3 {
        frame_resource.buffer_done();
    }
}

fn fallback_dummy_output() -> Output {
    Output::new(
        "dummy".into(),
        smithay::output::PhysicalProperties {
            size: (1, 1).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: "Mochi".into(),
            model: "Dummy".into(),
        },
    )
}

/// Resolves an `Output` from a `WlOutput` resource or falls back to active space output.
fn resolve_output(state: &MochiState, wl_output: &WlOutput) -> Option<Output> {
    if let Some(out) = Output::from_resource(wl_output) {
        return Some(out);
    }
    state.wm.active_space().outputs().next().cloned()
}

// ============================================================================
// Compositor Frame Download & SHM Buffer Copy
// ============================================================================

/// Fulfills all pending `zwlr_screencopy_frame_v1` requests for the given output using the
/// currently composited OpenGL ES framebuffer.
pub fn process_screencopy_for_output(
    renderer: &mut GlesRenderer,
    framebuffer: &<GlesRenderer as RendererSuper>::Framebuffer<'_>,
    output: &Output,
    output_size: Size<i32, Physical>,
    screencopy_state: &mut ScreencopyState,
    session_locked: bool,
) {
    if !screencopy_state.has_pending_for_output(output) {
        return;
    }

    let pending_requests = screencopy_state.take_pending_for_output(output);
    if pending_requests.is_empty() {
        return;
    }

    // Security: Refuse all external screencopy frame requests while the compositor session is locked
    if session_locked {
        trace!(output = %output.name(), "Refusing screencopy request while session is locked");
        for pending in pending_requests {
            pending.status.store(FRAME_STATUS_DONE, Ordering::SeqCst);
            pending.frame.failed();
        }
        return;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let tv_sec = now.as_secs();
    let tv_sec_hi = (tv_sec >> 32) as u32;
    let tv_sec_lo = (tv_sec & 0xFFFF_FFFF) as u32;
    let tv_nsec = now.subsec_nanos();

    for pending in pending_requests {
        let region = pending.region;
        let w = region.size.w;
        let h = region.size.h;

        if w <= 0 || h <= 0 {
            pending.frame.failed();
            continue;
        }

        // OpenGL ES glReadPixels has (0, 0) at the bottom-left corner of the framebuffer.
        // Convert Wayland top-left region into OpenGL bottom-left region:
        let gl_y = (output_size.h - (region.loc.y + h)).max(0);
        let gl_x = region.loc.x.max(0);
        let gl_region: Rectangle<i32, BufferCoord> = Rectangle::new(
            Point::from((gl_x, gl_y)),
            Size::from((w, h)),
        );

        let mapping_res = renderer.copy_framebuffer(framebuffer, gl_region, Fourcc::Abgr8888);
        let mapping = match mapping_res {
            Ok(m) => m,
            Err(err) => {
                warn!(?err, "Failed to copy OpenGL ES framebuffer for screencopy");
                pending.frame.failed();
                continue;
            }
        };

        let rgba_bytes_res = renderer.map_texture(&mapping);
        let rgba_bytes = match rgba_bytes_res {
            Ok(b) => b,
            Err(err) => {
                warn!(?err, "Failed to map downloaded texture for screencopy");
                pending.frame.failed();
                continue;
            }
        };

        // Write pixels into the client's SHM buffer
        let copy_result = shm::with_buffer_contents_mut(&pending.buffer, |ptr, len, data| {
            let dst_slice = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
            convert_and_copy_pixels(
                rgba_bytes,
                w as usize,
                h as usize,
                dst_slice,
                data.offset as usize,
                data.stride as usize,
                data.format,
            )
        });

        match copy_result {
            Ok(Ok(())) => {
                pending.status.store(FRAME_STATUS_DONE, Ordering::SeqCst);
                if pending.with_damage {
                    pending.frame.damage(0, 0, w as u32, h as u32);
                }
                pending.frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
                pending.frame.ready(tv_sec_hi, tv_sec_lo, tv_nsec);
                trace!(
                    output = %output.name(),
                    width = w,
                    height = h,
                    with_damage = pending.with_damage,
                    "Successfully delivered screencopy frame to client"
                );
            }
            Ok(Err(msg)) => {
                warn!(%msg, "Client screencopy buffer copy error");
                pending.frame.failed();
            }
            Err(err) => {
                warn!(?err, "Failed to access client SHM buffer for screencopy");
                pending.frame.failed();
            }
        }
    }
}

/// Copies and formats raw downloaded OpenGL framebuffer bytes into client SHM memory.
///
/// OpenGL ES ReadPixels returns row 0 at the bottom of the requested region with `Abgr8888`
/// little-endian byte ordering (`[R, G, B, A]`).
/// This function inverts the Y axis so client row 0 is the top, and translates pixel channels
/// to the client's negotiated `wl_shm::Format`.
pub fn convert_and_copy_pixels(
    src_rgba: &[u8],
    w: usize,
    h: usize,
    dst_buf: &mut [u8],
    dst_offset: usize,
    dst_stride: usize,
    format: wl_shm::Format,
) -> Result<(), &'static str> {
    let src_stride = w * 4;
    if dst_offset + dst_stride * h > dst_buf.len() {
        return Err("Destination buffer length smaller than required size");
    }

    for row in 0..h {
        let src_row = h - 1 - row;
        let src_offset = src_row * src_stride;
        let dst_row_offset = dst_offset + row * dst_stride;

        let src_slice = &src_rgba[src_offset..src_offset + src_stride];
        let dst_slice = &mut dst_buf[dst_row_offset..dst_row_offset + dst_stride];

        match format {
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => {
                for (src_px, dst_px) in src_slice
                    .chunks_exact(4)
                    .zip(dst_slice[..src_stride].chunks_exact_mut(4))
                {
                    dst_px[0] = src_px[2]; // B
                    dst_px[1] = src_px[1]; // G
                    dst_px[2] = src_px[0]; // R
                    dst_px[3] = if format == wl_shm::Format::Xrgb8888 {
                        0xFF
                    } else {
                        src_px[3]
                    };
                }
            }
            wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => {
                let copy_len = src_stride.min(dst_stride);
                dst_slice[..copy_len].copy_from_slice(&src_slice[..copy_len]);
                if format == wl_shm::Format::Xbgr8888 {
                    for px in dst_slice[..copy_len].chunks_exact_mut(4) {
                        px[3] = 0xFF;
                    }
                }
            }
            _ => return Err("Unsupported client buffer pixel format"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screencopy_pixel_conversion_and_y_invert() {
        // Create a 2x2 image in OpenGL bottom-up format (Row 0 = bottom, Row 1 = top)
        // Row 0 (Bottom): [Pixel (0, 1), Pixel (1, 1)]
        // Row 1 (Top):    [Pixel (0, 0), Pixel (1, 0)]
        //
        // Pixel colors in OpenGL Abgr8888 [R, G, B, A]:
        // (0, 0) Top-Left:     Red   [255, 0, 0, 200]
        // (1, 0) Top-Right:    Green [0, 255, 0, 210]
        // (0, 1) Bottom-Left:  Blue  [0, 0, 255, 220]
        // (1, 1) Bottom-Right: White [255, 255, 255, 255]
        let mut src_gl_abgr = vec![0u8; 16];
        // Row 0 of OpenGL (Bottom row):
        src_gl_abgr[0..4].copy_from_slice(&[0, 0, 255, 220]);     // Blue
        src_gl_abgr[4..8].copy_from_slice(&[255, 255, 255, 255]); // White
        // Row 1 of OpenGL (Top row):
        src_gl_abgr[8..12].copy_from_slice(&[255, 0, 0, 200]);    // Red
        src_gl_abgr[12..16].copy_from_slice(&[0, 255, 0, 210]);   // Green

        // 1. Convert to client Argb8888 (stride 8 bytes = 2 * 4)
        let mut dst_argb = vec![0u8; 16];
        convert_and_copy_pixels(
            &src_gl_abgr,
            2,
            2,
            &mut dst_argb,
            0,
            8,
            wl_shm::Format::Argb8888,
        )
        .expect("Conversion to ARGB8888 should succeed");

        // Client Row 0 (Top) should have Red then Green in Argb8888 [B, G, R, A]:
        assert_eq!(&dst_argb[0..4], &[0, 0, 255, 200]);   // Red [B=0, G=0, R=255, A=200]
        assert_eq!(&dst_argb[4..8], &[0, 255, 0, 210]);   // Green [B=0, G=255, R=0, A=210]

        // Client Row 1 (Bottom) should have Blue then White in Argb8888 [B, G, R, A]:
        assert_eq!(&dst_argb[8..12], &[255, 0, 0, 220]);  // Blue [B=255, G=0, R=0, A=220]
        assert_eq!(&dst_argb[12..16], &[255, 255, 255, 255]); // White

        // 2. Convert to client Xrgb8888: Alpha channel must be 0xFF
        let mut dst_xrgb = vec![0u8; 16];
        convert_and_copy_pixels(
            &src_gl_abgr,
            2,
            2,
            &mut dst_xrgb,
            0,
            8,
            wl_shm::Format::Xrgb8888,
        )
        .expect("Conversion to XRGB8888 should succeed");
        assert_eq!(dst_xrgb[3], 0xFF);
        assert_eq!(dst_xrgb[7], 0xFF);
        assert_eq!(dst_xrgb[11], 0xFF);
        assert_eq!(dst_xrgb[15], 0xFF);

        // 3. Stride with padding test (stride = 12 bytes, w = 2, so 4 padding bytes per row)
        let mut dst_padded = vec![0xEEu8; 24];
        convert_and_copy_pixels(
            &src_gl_abgr,
            2,
            2,
            &mut dst_padded,
            0,
            12,
            wl_shm::Format::Argb8888,
        )
        .expect("Padded stride copy should succeed");
        assert_eq!(&dst_padded[0..4], &[0, 0, 255, 200]);
        assert_eq!(&dst_padded[4..8], &[0, 255, 0, 210]);
        assert_eq!(&dst_padded[8..12], &[0xEE, 0xEE, 0xEE, 0xEE]); // Padding untouched
        assert_eq!(&dst_padded[12..16], &[255, 0, 0, 220]);
        assert_eq!(&dst_padded[16..20], &[255, 255, 255, 255]);
        assert_eq!(&dst_padded[20..24], &[0xEE, 0xEE, 0xEE, 0xEE]); // Padding untouched
    }

    #[test]
    fn test_screencopy_buffer_bounds_check() {
        let src = vec![0u8; 16];
        let mut short_dst = vec![0u8; 10]; // Requires 16 bytes
        let res = convert_and_copy_pixels(
            &src,
            2,
            2,
            &mut short_dst,
            0,
            8,
            wl_shm::Format::Argb8888,
        );
        assert!(res.is_err());
    }

    #[test]
    fn test_screencopy_status_transitions() {
        let status = Arc::new(AtomicU8::new(FRAME_STATUS_READY));
        assert_eq!(status.load(Ordering::SeqCst), FRAME_STATUS_READY);

        // First transition to queued: succeeds
        assert!(status
            .compare_exchange(
                FRAME_STATUS_READY,
                FRAME_STATUS_QUEUED,
                Ordering::SeqCst,
                Ordering::SeqCst
            )
            .is_ok());
        assert_eq!(status.load(Ordering::SeqCst), FRAME_STATUS_QUEUED);

        // Second transition to queued: must fail (preventing double copy on same frame)
        assert!(status
            .compare_exchange(
                FRAME_STATUS_READY,
                FRAME_STATUS_QUEUED,
                Ordering::SeqCst,
                Ordering::SeqCst
            )
            .is_err());
    }
}

