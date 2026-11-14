// SPDX-License-Identifier: GPL-3.0-or-later
//! Bare-Metal Hardware Backend (`udev` + DRM/KMS + GBM + EGL + `libinput`)
//!
//! Drives physical GPU scanout engines (CRTCs/Planes) directly via Linux DRM/KMS
//! Atomic Modesetting and GBM swapchains (`GbmBufferedSurface`), with hardware
//! input via `libinput` and device hotplug discovery via `udev`.

use std::{
    cell::RefCell,
    collections::HashMap,
    os::unix::io::{AsRawFd, OwnedFd},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use calloop::{timer::{TimeoutAction, Timer}, EventLoop};
use mochi_core::MochiState;
use mochi_render::MochiShaderPipeline;
use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, GbmBufferedSurface, VrrSupport},
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            damage::OutputDamageTracker, gles::GlesRenderer, utils::CommitCounter, Bind, ImportDma,
        },
        session::{AsErrno, Session},
        udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent},
    },
    output::{Mode as WlMode, Output, PhysicalProperties, Subpixel},
    reexports::{
        drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags},
        input,
        rustix::{
            fs::{open, Mode as FileMode, OFlags},
            io::Errno,
        },
        wayland_protocols::wp::content_type::v1::server::wp_content_type_v1::Type as ContentType,
    },
    utils::{DeviceFd, Transform},
    wayland::{
        compositor,
        content_type::ContentTypeSurfaceCachedState,
        drm_lease::{DrmLeaseBuilder, DrmLeaseState, LeaseRejected},
    },
};
use tracing::{error, info, warn};

use crate::{assemble_scene_elements, capture_framebuffer_to_ppm, OffscreenBackdropFbo, OverlayIds};

/// Preferred DRM/GBM scanout pixel formats in priority order.
const SUPPORTED_COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
    Fourcc::Xbgr8888,
    Fourcc::Xrgb8888,
];

/// Error type for [`TtySession`].
#[derive(Debug)]
pub struct TtySessionError(pub Errno);

impl AsErrno for TtySessionError {
    fn as_errno(&self) -> Option<i32> {
        Some(self.0.raw_os_error())
    }
}

/// Direct Linux TTY / `systemd-logind` (`uaccess`) session provider implementing [`Session`].
#[derive(Debug)]
pub struct TtySession {
    seat_name: String,
    active: bool,
}

impl TtySession {
    pub fn new() -> Self {
        let seat_name = std::env::var("XDG_SEAT").unwrap_or_else(|_| "seat0".to_string());
        Self {
            seat_name,
            active: true,
        }
    }
}

impl Default for TtySession {
    fn default() -> Self {
        Self::new()
    }
}

impl Session for TtySession {
    type Error = TtySessionError;

    fn open(&mut self, path: &Path, flags: OFlags) -> Result<OwnedFd, Self::Error> {
        open(path, flags | OFlags::CLOEXEC, FileMode::empty()).map_err(TtySessionError)
    }

    fn close(&mut self, _fd: OwnedFd) -> Result<(), Self::Error> {
        // OwnedFd closes automatically when dropped.
        Ok(())
    }

    fn change_vt(&mut self, vt: i32) -> Result<(), Self::Error> {
        extern "C" {
            fn ioctl(fd: i32, request: u64, ...) -> i32;
        }
        const VT_ACTIVATE: u64 = 0x5606;
        for tty_path in ["/dev/tty0", "/dev/tty"] {
            if let Ok(fd) = open(
                Path::new(tty_path),
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY,
                FileMode::empty(),
            ) {
                let res = unsafe { ioctl(fd.as_raw_fd(), VT_ACTIVATE, vt) };
                if res == 0 {
                    info!(vt, tty = tty_path, "Switched active Linux virtual terminal");
                    return Ok(());
                }
            }
        }
        warn!(vt, "Could not switch VT via /dev/tty0 or /dev/tty");
        Err(TtySessionError(Errno::PERM))
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn seat(&self) -> String {
        self.seat_name.clone()
    }
}

/// Per-CRTC hardware scanout surface state.
struct CrtcSurfaceState {
    connector: connector::Handle,
    output: Output,
    gbm_surface: GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, ()>,
    damage_tracker: OutputDamageTracker,
    backdrop_fbo: OffscreenBackdropFbo,
    frame_pending: bool,
    vrr_supported: bool,
}

/// Shared hardware rendering state across DRM VBlank and timer callbacks.
struct UdevHardwareContext {
    _drm_device: DrmDevice,
    _gbm_device: GbmDevice<DrmDeviceFd>,
    renderer: GlesRenderer,
    shaders: MochiShaderPipeline,
    overlay_ids: OverlayIds,
    frame_commit: CommitCounter,
    last_frame_instant: Instant,
    surfaces: HashMap<crtc::Handle, CrtcSurfaceState>,
}

impl UdevHardwareContext {
    /// Renders a single CRTC output onto its GBM swapchain and queues a DRM page-flip/atomic commit.
    fn render_crtc(&mut self, crtc: crtc::Handle, state: &mut MochiState) {
        let Some(surface_state) = self.surfaces.get_mut(&crtc) else {
            return;
        };
        if surface_state.frame_pending {
            return;
        }

        if surface_state.vrr_supported {
            let want_vrr = state.wm.active_space().elements().any(|w| {
                mochi_wm::window_wl_surface(w).is_some_and(|surf| {
                    compositor::with_states(&surf, |states| {
                        let mut ct = states.cached_state.get::<ContentTypeSurfaceCachedState>();
                        matches!(
                            ct.current().content_type(),
                            ContentType::Game | ContentType::Video
                        )
                    })
                })
            });
            if want_vrr != surface_state.gbm_surface.vrr_enabled() {
                if let Err(err) = surface_state.gbm_surface.use_vrr(want_vrr) {
                    warn!(?crtc, want_vrr, ?err, "Failed to update DRM VRR state");
                }
            }
        }

        let (mut dmabuf, age) = match surface_state.gbm_surface.next_buffer() {
            Ok(res) => res,
            Err(err) => {
                warn!(?crtc, ?err, "Failed to acquire next GBM scanout buffer");
                return;
            }
        };

        self.frame_commit.increment();
        let fbo_tex = surface_state.backdrop_fbo.render_backdrop_pass(
            &mut self.renderer,
            state,
            &surface_state.output,
            &self.shaders,
        );
        let clear_color = state.wm.visual_style.style.backdrop_clear_color();
        let scene_elements = assemble_scene_elements(
            &mut self.renderer,
            state,
            &surface_state.output,
            &self.shaders,
            fbo_tex.as_ref(),
            &self.overlay_ids,
            self.frame_commit,
        );

        let render_result = {
            let mut target = match self.renderer.bind(&mut dmabuf) {
                Ok(t) => t,
                Err(err) => {
                    warn!(?crtc, ?err, "Failed to bind DMABUF to GlesRenderer");
                    return;
                }
            };

            let res = surface_state.damage_tracker.render_output(
                &mut self.renderer,
                &mut target,
                age as usize,
                &scene_elements,
                clear_color,
            );

            if let Some(shot_path) = state.pending_screenshot_path.take() {
                if let Some(mode) = surface_state.output.current_mode() {
                    if let Err(err) = capture_framebuffer_to_ppm(
                        &mut self.renderer,
                        &target,
                        mode.size,
                        &shot_path,
                    ) {
                        warn!(?err, path = %shot_path.display(), "Failed to capture DRM screenshot");
                    }
                }
            }

            if let Some(mode) = surface_state.output.current_mode() {
                mochi_core::screencopy::process_screencopy_for_output(
                    &mut self.renderer,
                    &target,
                    &surface_state.output,
                    mode.size,
                    &mut state.screencopy_state,
                    state.session_locked,
                );
            }

            res
        };

        match render_result {
            Ok(render_output_result) => {
                let damage = render_output_result.damage.cloned();
                match surface_state.gbm_surface.queue_buffer(None, damage, ()) {
                    Ok(()) => {
                        surface_state.frame_pending = true;
                    }
                    Err(err) => {
                        warn!(?crtc, ?err, "Failed to queue GBM buffer for DRM scanout");
                    }
                }
            }
            Err(err) => {
                warn!(?crtc, ?err, "Error rendering Mochi Mochi scene onto GBM surface");
            }
        }
    }

    /// Sends Wayland `wl_surface.frame` callbacks, `wp_presentation` feedback, and refreshes space state after a scanout pass.
    fn finish_frame_callbacks(&self, crtc: crtc::Handle, state: &mut MochiState) {
        let Some(surface_state) = self.surfaces.get(&crtc) else {
            return;
        };
        let output = &surface_state.output;
        state.send_frames_and_presentation_feedback(
            output,
            0,
            Duration::from_nanos(16_666_667),
        );

        state.wm.active_space_mut().refresh();
        state.popups.cleanup();
        let _ = state.display_handle.flush_clients();
    }
}

/// Discovers the primary GPU node for the given seat.
fn find_primary_gpu_node(seat_name: &str) -> anyhow::Result<PathBuf> {
    if let Ok(Some(primary)) = primary_gpu(seat_name) {
        return Ok(primary);
    }
    let mut gpus = all_gpus(seat_name)
        .map_err(|e| anyhow::anyhow!("Failed to enumerate GPUs via udev on {seat_name}: {e}"))?;
    gpus.pop().ok_or_else(|| {
        anyhow::anyhow!(
            "No DRM GPU device nodes (/dev/dri/card*) found via udev for seat '{seat_name}'"
        )
    })
}

/// Returns `true` if the DRM connector advertises the `non-desktop` property (e.g. a VR headset).
fn is_non_desktop_connector(drm_device: &DrmDevice, conn: connector::Handle) -> bool {
    let Ok(props) = drm_device.get_properties(conn) else {
        return false;
    };
    for (&prop_h, &val) in props.iter() {
        if let Ok(info) = drm_device.get_property(prop_h) {
            if info.name().to_string_lossy() == "non-desktop" {
                return val != 0;
            }
        }
    }
    false
}

/// Advertises any connected `non-desktop` (VR HMD) connectors to `DrmLeaseState`.
fn advertise_leasable_connectors(
    drm_device: &DrmDevice,
    surfaces: &HashMap<crtc::Handle, CrtcSurfaceState>,
    state: &mut MochiState,
) {
    let Some(lease_state) = state.drm_lease_state.as_mut() else {
        return;
    };
    let Ok(resources) = drm_device.resource_handles() else {
        return;
    };
    for &conn_h in resources.connectors() {
        if surfaces.values().any(|s| s.connector == conn_h) {
            continue;
        }
        let Ok(info) = drm_device.get_connector(conn_h, false) else {
            continue;
        };
        if info.state() == connector::State::Connected && is_non_desktop_connector(drm_device, conn_h) {
            let name = format!("{:?}-{}", info.interface(), info.interface_id());
            let desc = format!("VR / Non-Desktop Display ({name})");
            lease_state.add_connector::<MochiState>(conn_h, name, desc);
        }
    }
}

/// Initializes a single connected desktop DRM connector into a `CrtcSurfaceState` and maps its `Output`.
fn try_initialize_connector_surface(
    drm_device: &mut DrmDevice,
    gbm_device: &GbmDevice<DrmDeviceFd>,
    render_formats: &[smithay::backend::allocator::Format],
    resources: &smithay::reexports::drm::control::ResourceHandles,
    conn_handle: connector::Handle,
    occupied_crtcs: &HashMap<crtc::Handle, CrtcSurfaceState>,
    x_offset: i32,
    state: &mut MochiState,
) -> Option<(crtc::Handle, CrtcSurfaceState, i32)> {
    let conn_info = drm_device.get_connector(conn_handle, true).ok()?;
    if conn_info.state() != connector::State::Connected
        || is_non_desktop_connector(drm_device, conn_handle)
    {
        return None;
    }
    let &drm_mode = conn_info
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| conn_info.modes().first())?;

    // Find an unclaimed CRTC compatible with one of this connector's encoders
    let selected_crtc = conn_info
        .encoders()
        .iter()
        .filter_map(|&enc_h| drm_device.get_encoder(enc_h).ok())
        .flat_map(|enc| resources.filter_crtcs(enc.possible_crtcs()))
        .find(|crtc_h| !occupied_crtcs.contains_key(crtc_h));

    let Some(crtc_handle) = selected_crtc else {
        warn!(
            connector = ?conn_info.interface(),
            "No free compatible CRTC found for connected display"
        );
        return None;
    };

    let connector_name = format!(
        "{:?}-{}",
        conn_info.interface(),
        conn_info.interface_id()
    );

    let drm_surface = match drm_device.create_surface(crtc_handle, drm_mode, &[conn_handle]) {
        Ok(s) => s,
        Err(err) => {
            warn!(
                connector = %connector_name,
                ?crtc_handle,
                ?err,
                "Failed to create DRM surface for connector"
            );
            return None;
        }
    };

    let gbm_allocator = GbmAllocator::new(
        gbm_device.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );

    let gbm_surface = match GbmBufferedSurface::new(
        drm_surface,
        gbm_allocator,
        SUPPORTED_COLOR_FORMATS,
        render_formats.iter().copied(),
    ) {
        Ok(s) => s,
        Err(err) => {
            warn!(
                connector = %connector_name,
                ?err,
                "Failed to initialize GbmBufferedSurface swapchain"
            );
            return None;
        }
    };

    let (phys_w, phys_h) = conn_info.size().unwrap_or((0, 0));
    let wl_mode = WlMode {
        size: (drm_mode.size().0 as i32, drm_mode.size().1 as i32).into(),
        refresh: (drm_mode.vrefresh() * 1000) as i32,
    };

    let (out_loc, out_scale) = state
        .wm
        .configured_output_placement(&connector_name, (x_offset, 0).into());

    let output = Output::new(
        connector_name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: Subpixel::Unknown,
            make: "Mochi Mochi DRM".into(),
            model: connector_name.clone(),
        },
    );
    let _global = output.create_global::<MochiState>(&state.display_handle);
    output.change_current_state(
        Some(wl_mode),
        Some(Transform::Normal),
        out_scale.map(smithay::output::Scale::Fractional),
        Some(out_loc),
    );
    output.set_preferred(wl_mode);

    state.wm.map_output(&output, out_loc);
    let next_x_offset = out_loc.x + wl_mode.size.w;

    let vrr_supported = matches!(
        gbm_surface.vrr_supported(conn_handle),
        Ok(VrrSupport::Supported)
    );
    let damage_tracker = OutputDamageTracker::from_output(&output);
    info!(
        connector = %connector_name,
        ?crtc_handle,
        resolution = ?wl_mode.size,
        refresh_mhz = wl_mode.refresh,
        vrr_supported,
        "Initialized bare-metal DRM/KMS output surface"
    );

    Some((
        crtc_handle,
        CrtcSurfaceState {
            connector: conn_handle,
            output,
            gbm_surface,
            damage_tracker,
            backdrop_fbo: OffscreenBackdropFbo::new(),
            frame_pending: false,
            vrr_supported,
        },
        next_x_offset,
    ))
}

/// Scans connected DRM connectors, assigns compatible CRTCs, and initializes `GbmBufferedSurface`s.
fn scan_and_initialize_outputs(
    drm_device: &mut DrmDevice,
    gbm_device: &GbmDevice<DrmDeviceFd>,
    renderer: &GlesRenderer,
    state: &mut MochiState,
) -> anyhow::Result<HashMap<crtc::Handle, CrtcSurfaceState>> {
    let resources = drm_device
        .resource_handles()
        .map_err(|e| anyhow::anyhow!("Failed to query DRM resource handles: {e}"))?;

    let render_formats: Vec<_> = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect();

    let mut surfaces = HashMap::new();
    let mut current_x_offset = 0i32;

    for &conn_handle in resources.connectors() {
        if let Some((crtc_handle, surface_state, next_x)) = try_initialize_connector_surface(
            drm_device,
            gbm_device,
            &render_formats,
            &resources,
            conn_handle,
            &surfaces,
            current_x_offset,
            state,
        ) {
            current_x_offset = next_x;
            surfaces.insert(crtc_handle, surface_state);
        }
    }

    if surfaces.is_empty() {
        anyhow::bail!("No connected DRM/KMS display outputs could be initialized");
    }

    Ok(surfaces)
}

/// Initializes and runs the bare-metal `udev` / DRM / KMS / GBM / `libinput` backend.
pub fn run_udev(
    event_loop: &mut EventLoop<'static, MochiState>,
    state: &mut MochiState,
    initial_command: Option<&str>,
) -> anyhow::Result<()> {
    let mut session = Rc::new(RefCell::new(TtySession::new()));
    let seat_name = session.seat();

    let gpu_path = find_primary_gpu_node(&seat_name)?;
    info!(
        seat = %seat_name,
        gpu = %gpu_path.display(),
        "Opening primary DRM/KMS GPU node for bare-metal session"
    );

    let gpu_fd = session
        .open(
            &gpu_path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to open DRM device {} (errno {:?}). Ensure you are running from an active Linux TTY on {seat_name}: {:?}",
                gpu_path.display(),
                e.as_errno(),
                e
            )
        })?;

    let drm_fd = DrmDeviceFd::new(DeviceFd::from(gpu_fd));
    state.init_drm_syncobj(&drm_fd);
    let (mut drm_device, drm_notifier) = DrmDevice::new(drm_fd.clone(), true)
        .map_err(|e| anyhow::anyhow!("Failed to initialize DrmDevice on {}: {e}", gpu_path.display()))?;
    let gbm_device = GbmDevice::new(drm_fd)
        .map_err(|e| anyhow::anyhow!("Failed to initialize GbmDevice on {}: {e}", gpu_path.display()))?;

    let egl_display = unsafe { EGLDisplay::new(gbm_device.clone()) }
        .map_err(|e| anyhow::anyhow!("Failed to initialize EGLDisplay on GBM device: {e}"))?;
    let egl_context = EGLContext::new(&egl_display)
        .map_err(|e| anyhow::anyhow!("Failed to create EGLContext: {e}"))?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context) }
        .map_err(|e| anyhow::anyhow!("Failed to create GlesRenderer: {e}"))?;

    // Compile Mochi Mochi GLSL shader pipeline (Normal Blur, Frosted Glass, Liquid Glass)
    let shaders = MochiShaderPipeline::compile(&mut renderer)?;

    // Advertise hardware DMABUF formats to Wayland clients
    let dmabuf_formats = renderer.dmabuf_formats();
    let dmabuf_global = state
        .dmabuf_state
        .create_global::<MochiState>(&state.display_handle, dmabuf_formats);
    state.dmabuf_global = Some(dmabuf_global);

    // Scan connectors/CRTCs and create GBM buffered scanout swapchains
    let surfaces = scan_and_initialize_outputs(&mut drm_device, &gbm_device, &renderer, state)?;
    let initial_crtcs: Vec<crtc::Handle> = surfaces.keys().copied().collect();

    let hw_ctx = Rc::new(RefCell::new(UdevHardwareContext {
        _drm_device: drm_device,
        _gbm_device: gbm_device,
        renderer,
        shaders,
        overlay_ids: OverlayIds::new(),
        frame_commit: CommitCounter::default(),
        last_frame_instant: Instant::now(),
        surfaces,
    }));

    // Register wp_drm_lease_v1 for VR headsets / non-desktop DRM connectors
    if let Ok(drm_node) = DrmNode::from_file(hw_ctx.borrow()._drm_device.device_fd()) {
        if let Ok(lease_state) = DrmLeaseState::new::<MochiState>(&state.display_handle, &drm_node) {
            state.drm_lease_state = Some(lease_state);
            {
                let ctx = hw_ctx.borrow();
                advertise_leasable_connectors(&ctx._drm_device, &ctx.surfaces, state);
            }
            let hw_for_lease = hw_ctx.clone();
            state.drm_lease_request_cb = Some(Box::new(move |_node, request| {
                let ctx = hw_for_lease.borrow();
                let drm_device = &ctx._drm_device;
                let mut builder = DrmLeaseBuilder::new(drm_device);
                let res_handles = drm_device
                    .resource_handles()
                    .map_err(|_| LeaseRejected::default())?;
                for conn in request.connectors {
                    let conn_info = drm_device
                        .get_connector(conn, false)
                        .map_err(|_| LeaseRejected::default())?;
                    let mut leased_crtc = None;
                    for &enc_handle in conn_info.encoders() {
                        if let Ok(enc) = drm_device.get_encoder(enc_handle) {
                            for crtc in res_handles.filter_crtcs(enc.possible_crtcs()) {
                                if !ctx.surfaces.contains_key(&crtc) {
                                    leased_crtc = Some(crtc);
                                    break;
                                }
                            }
                        }
                        if leased_crtc.is_some() {
                            break;
                        }
                    }
                    let crtc = leased_crtc.ok_or_else(LeaseRejected::default)?;
                    builder.add_connector(conn);
                    builder.add_crtc(crtc);
                }
                Ok(builder)
            }));
        }
    }

    // Initialize libinput hardware input backend via udev
    let mut libinput_context = input::Libinput::new_with_udev::<
        LibinputSessionInterface<Rc<RefCell<TtySession>>>,
    >(session.clone().into());
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|()| anyhow::anyhow!("Failed to assign udev seat '{seat_name}' to libinput"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context);

    let session_for_input = session.clone();
    event_loop
        .handle()
        .insert_source(libinput_backend, move |event, _, state| {
            state.process_input_event(event);
            if let Some(vt) = state.pending_vt_switch.take() {
                let mut sess = session_for_input.clone();
                let _ = sess.change_vt(vt);
            }
        })
        .map_err(|e| anyhow::anyhow!("Failed to register libinput event source: {e:?}"))?;

    // Monitor udev DRM hotplug events
    if let Ok(udev_backend) = UdevBackend::new(&seat_name) {
        let hw_for_hotplug = hw_ctx.clone();
        let _ = event_loop
            .handle()
            .insert_source(udev_backend, move |event, _, state| match event {
                UdevEvent::Added { device_id, path } => {
                    info!(device_id, path = %path.display(), "Udev DRM device added");
                }
                UdevEvent::Changed { device_id } => {
                    info!(device_id, "Udev DRM device connector state changed");
                    let mut newly_added_crtcs = Vec::new();
                    let mut layout_changed = false;
                    {
                        let mut ctx = hw_for_hotplug.borrow_mut();
                        let disconnected: Vec<crtc::Handle> = ctx
                            .surfaces
                            .iter()
                            .filter_map(|(&crtc, surf)| {
                                let still_connected = ctx
                                    ._drm_device
                                    .get_connector(surf.connector, false)
                                    .is_ok_and(|c| c.state() == connector::State::Connected);
                                (!still_connected).then_some(crtc)
                            })
                            .collect();
                        for crtc in disconnected {
                            if let Some(removed) = ctx.surfaces.remove(&crtc) {
                                info!(output = %removed.output.name(), "Unmapping disconnected DRM output");
                                state.wm.unmap_output(&removed.output);
                                layout_changed = true;
                            }
                        }

                        let render_formats: Vec<_> = ctx
                            .renderer
                            .egl_context()
                            .dmabuf_render_formats()
                            .iter()
                            .copied()
                            .collect();
                        let mut x_offset = state
                            .wm
                            .bounding_outputs_geometry()
                            .map(|g| g.loc.x + g.size.w)
                            .unwrap_or(0);

                        let UdevHardwareContext {
                            ref mut _drm_device,
                            ref _gbm_device,
                            ref mut surfaces,
                            ..
                        } = *ctx;

                        if let Ok(resources) = _drm_device.resource_handles() {
                            for &conn_h in resources.connectors() {
                                if surfaces.values().any(|s| s.connector == conn_h) {
                                    continue;
                                }
                                if let Some((crtc_h, surf_state, next_x)) =
                                    try_initialize_connector_surface(
                                        _drm_device,
                                        _gbm_device,
                                        &render_formats,
                                        &resources,
                                        conn_h,
                                        surfaces,
                                        x_offset,
                                        state,
                                    )
                                {
                                    x_offset = next_x;
                                    surfaces.insert(crtc_h, surf_state);
                                    newly_added_crtcs.push(crtc_h);
                                    layout_changed = true;
                                }
                            }
                            advertise_leasable_connectors(_drm_device, surfaces, state);
                        }
                    }

                    for crtc in newly_added_crtcs {
                        hw_for_hotplug.borrow_mut().render_crtc(crtc, state);
                    }
                    if layout_changed {
                        state.wm.retile_active_workspace();
                        state.sync_output_management();
                    }
                }
                UdevEvent::Removed { device_id } => {
                    info!(device_id, "Udev DRM device removed");
                }
            });
    }

    // Register DRM VBlank page-flip notifier
    let hw_for_vblank = hw_ctx.clone();
    event_loop
        .handle()
        .insert_source(drm_notifier, move |event, _metadata, state| match event {
            DrmEvent::VBlank(crtc) => {
                let mut ctx = hw_for_vblank.borrow_mut();
                if let Some(surf) = ctx.surfaces.get_mut(&crtc) {
                    surf.frame_pending = false;
                    if let Err(err) = surf.gbm_surface.frame_submitted() {
                        warn!(?crtc, ?err, "Error marking GBM frame submitted on VBlank");
                    }
                }

                let now = Instant::now();
                let dt_secs = now.duration_since(ctx.last_frame_instant).as_secs_f64();
                ctx.last_frame_instant = now;
                state.tick_frame(dt_secs);

                ctx.render_crtc(crtc, state);
                ctx.finish_frame_callbacks(crtc, state);
            }
            DrmEvent::Error(err) => {
                error!(?err, "DRM hardware event error");
            }
        })
        .map_err(|e| anyhow::anyhow!("Failed to register DRM VBlank notifier: {e:?}"))?;

    // Kick off initial scanout frame on all active CRTCs
    {
        let mut ctx = hw_ctx.borrow_mut();
        for crtc in &initial_crtcs {
            ctx.render_crtc(*crtc, state);
            ctx.finish_frame_callbacks(*crtc, state);
        }
    }

    // Periodic 60Hz fallback heartbeat in case a CRTC page-flip stalls or wakes from idle
    let hw_for_timer = hw_ctx.clone();
    event_loop
        .handle()
        .insert_source(
            Timer::from_duration(Duration::from_millis(16)),
            move |_, _, state| {
                let mut ctx = hw_for_timer.borrow_mut();
                let crtcs: Vec<crtc::Handle> = ctx.surfaces.keys().copied().collect();
                for crtc in crtcs {
                    let is_pending = ctx
                        .surfaces
                        .get(&crtc)
                        .map(|s| s.frame_pending)
                        .unwrap_or(false);
                    if !is_pending {
                        let now = Instant::now();
                        let dt_secs = now.duration_since(ctx.last_frame_instant).as_secs_f64();
                        ctx.last_frame_instant = now;
                        state.tick_frame(dt_secs);
                        ctx.render_crtc(crtc, state);
                        ctx.finish_frame_callbacks(crtc, state);
                    }
                }
                TimeoutAction::ToDuration(Duration::from_millis(16))
            },
        )
        .map_err(|e| anyhow::anyhow!("Failed to insert frame heartbeat timer: {e:?}"))?;

    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
    // Export env to systemd / D-Bus and start `mochi-mochi-session.target`
    mochi_core::session::start_systemd_session(state);
    info!(
        wayland_display = ?state.socket_name,
        gpu = %gpu_path.display(),
        outputs = initial_crtcs.len(),
        style = state.wm.visual_style.style.as_str(),
        "Mochi Mochi bare-metal DRM/KMS backend is running"
    );

    state.run_startup_commands();

    if let Some(cmd) = initial_command {
        state.spawn_client_command(cmd);
    }

    while state.running {
        event_loop.dispatch(Duration::from_millis(16), state)?;
    }

    mochi_core::session::stop_systemd_session();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tty_session_defaults() {
        let session = TtySession::new();
        assert!(session.is_active());
        assert!(!session.seat().is_empty());
    }
}

