// SPDX-License-Identifier: GPL-3.0-or-later
//! Central Compositor State (`MochiState`), IPC Server & Live KDL Hot-Reload

use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Write},
    os::unix::{io::OwnedFd, net::UnixListener},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use calloop::{
    generic::Generic,
    timer::{TimeoutAction, Timer},
    Interest, LoopHandle, LoopSignal, Mode, PostAction,
};
use mochi_ipc::{default_socket_path, IpcRequest};
use mochi_wm::{LayoutMode, VisualStyle, WindowManager};
use smithay::{
    backend::drm::DrmNode,
    desktop::{
        layer_map_for_output, utils::{send_frames_surface_tree, OutputPresentationFeedback},
        PopupManager, WindowSurfaceType,
    },
    input::{keyboard::XkbConfig, pointer::CursorImageStatus, Seat, SeatState},
    output::Output,
    reexports::{
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Clock, Logical, Monotonic, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufGlobal, DmabufState},
        fractional_scale::FractionalScaleManagerState,
        alpha_modifier::AlphaModifierState,
        commit_timing::{CommitTimerBarrierStateUserData, CommitTimingManagerState, Timestamp},
        content_type::ContentTypeState,
        cursor_shape::CursorShapeManagerState,
        drm_lease::{DrmLease, DrmLeaseBuilder, DrmLeaseRequest, DrmLeaseState, LeaseRejected},
        fifo::{FifoBarrierCachedState, FifoManagerState},
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        input_method::{InputMethodManagerState, PopupSurface as ImePopupSurface},
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        pointer_gestures::PointerGesturesState,
        presentation::{PresentationState, Refresh},
        relative_pointer::RelativePointerManagerState,
        security_context::{SecurityContext, SecurityContextState},
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        session_lock::{LockSurface, SessionLockManagerState},
        shell::{
            wlr_layer::{Layer as WlrLayer, WlrLayerShellState},
            xdg::{decoration::XdgDecorationState, XdgShellState},
        },
        shm::ShmState,
        single_pixel_buffer::SinglePixelBufferState,
        socket::ListeningSocketSource,
        tablet_manager::TabletManagerState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
        xdg_activation::XdgActivationState,
        xdg_foreign::XdgForeignState,
        xdg_system_bell::XdgSystemBellState,
        xdg_toplevel_icon::XdgToplevelIconManager,
        xdg_toplevel_tag::XdgToplevelTagManager,
        xwayland_keyboard_grab::XWaylandKeyboardGrabState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::{X11Wm, XWayland, XWaylandEvent},
};
use tracing::{error, info, warn};

use crate::config::{KeyBinding, MochiConfig};

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub security_context: Option<SecurityContext>,
}

/// Returns `true` if `client` is an unsandboxed host/DE client (not connected via `wp_security_context_v1`).
pub fn is_privileged_client(client: &smithay::reexports::wayland_server::Client) -> bool {
    client
        .get_data::<ClientState>()
        .is_none_or(|data| data.security_context.is_none())
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Tracks live 1:1 multi-finger touchpad gestures (workspace swipe, overview pinch).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ActiveTouchpadGesture {
    pub swipe_active: bool,
    pub swipe_initial_workspace: usize,
    pub swipe_accumulated_dx: f64,
    pub pinch_active: bool,
    pub pinch_initial_scale: f64,
    pub pinch_started_in_overview: bool,
}

/// State manager for the built-in Night Light / Color Temperature warmth engine.
#[derive(Debug)]
pub struct NightLightState {
    pub enabled: bool,
    pub temperature: u32,
    pub manual_override: Option<bool>,
    pub spring: mochi_wm::Spring1D,
    pub schedule_start: Option<(u8, u8)>,
    pub schedule_end: Option<(u8, u8)>,
    pub external_gamma_active: bool,
    pub external_rgb_multipliers: (f32, f32, f32),
}

impl NightLightState {
    pub fn new(config: &crate::config::NightLightConfig) -> Self {
        let mut spring = mochi_wm::Spring1D::new(0.0, mochi_wm::SpringParams::style_morph());
        let initially_active = config.enabled;
        if initially_active {
            spring.current = 1.0;
            spring.set_target(1.0);
        }
        Self {
            enabled: config.enabled,
            temperature: config.temperature,
            manual_override: None,
            spring,
            schedule_start: config.schedule_start,
            schedule_end: config.schedule_end,
            external_gamma_active: false,
            external_rgb_multipliers: (1.0, 1.0, 1.0),
        }
    }

    pub fn set_external_gamma(&mut self, multipliers: (f32, f32, f32)) {
        self.external_gamma_active = true;
        self.external_rgb_multipliers = multipliers;
        self.spring.set_target(1.0);
    }

    pub fn clear_external_gamma(&mut self) {
        self.external_gamma_active = false;
        self.external_rgb_multipliers = (1.0, 1.0, 1.0);
        let target = if self.is_active_now() { 1.0 } else { 0.0 };
        self.spring.set_target(target);
    }

    pub fn toggle(&mut self) -> bool {
        let current = self.is_active_now();
        let new_state = !current;
        self.manual_override = Some(new_state);
        self.spring.set_target(if new_state { 1.0 } else { 0.0 });
        new_state
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.manual_override = Some(enabled);
        self.spring.set_target(if enabled { 1.0 } else { 0.0 });
    }

    pub fn set_temperature(&mut self, kelvin: u32) {
        self.temperature = kelvin.clamp(1000, 10000);
        if self.is_active_now() {
            self.spring.set_target(1.0);
        }
    }

    pub fn is_active_now(&self) -> bool {
        if self.external_gamma_active {
            return true;
        }
        if let Some(m) = self.manual_override {
            return m;
        }
        if !self.enabled {
            return false;
        }
        if let (Some(start), Some(end)) = (self.schedule_start, self.schedule_end) {
            let now = crate::config::current_local_time_hm();
            crate::config::is_time_in_schedule(start, end, now)
        } else {
            self.enabled
        }
    }

    pub fn tick(&mut self, dt_secs: f64) {
        if self.manual_override.is_none() && !self.external_gamma_active {
            let target = if self.is_active_now() { 1.0 } else { 0.0 };
            if (self.spring.target - target).abs() > 1e-4 {
                self.spring.set_target(target);
            }
        }
        self.spring.advance(dt_secs);
    }

    pub fn current_warmth(&self) -> (f32, f32, f32, f32) {
        let progress = self.spring.current as f32;
        if self.external_gamma_active {
            let (r, g, b) = self.external_rgb_multipliers;
            let warmth_alpha = (1.0 - b).max(0.0) * 0.38 * progress;
            return (r, g, b, warmth_alpha);
        }
        let (tr, tg, tb) = crate::gamma_control::kelvin_to_rgb_multipliers(self.temperature);
        let r = 1.0 - (1.0 - tr) * progress;
        let g = 1.0 - (1.0 - tg) * progress;
        let b = 1.0 - (1.0 - tb) * progress;
        let warmth_alpha = (1.0 - b).max(0.0) * 0.38 * progress;
        (r, g, b, warmth_alpha)
    }
}

/// Core state of the Mochi Mochi Wayland compositor.
pub struct MochiState {
    pub start_time: Instant,
    pub clock: Clock<Monotonic>,
    pub socket_name: OsString,
    pub ipc_socket_path: PathBuf,
    pub config_path: PathBuf,
    pub last_config_mtime: Option<SystemTime>,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Self>,
    pub loop_signal: LoopSignal,
    pub running: bool,

    // Window & Workspace Manager (`mochi-wm`) + Live Keybindings + Autostart
    pub wm: WindowManager,
    pub keybindings: Vec<KeyBinding>,
    pub startup_commands: Vec<String>,
    pub popups: PopupManager,

    // Wayland Protocol Global States
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub xdg_activation_state: XdgActivationState,
    pub xdg_foreign_state: XdgForeignState,
    pub layer_shell_state: WlrLayerShellState,
    pub viewporter_state: ViewporterState,
    pub fractional_scale_state: FractionalScaleManagerState,
    pub presentation_state: PresentationState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,
    pub drm_lease_state: Option<DrmLeaseState>,
    pub active_drm_leases: Vec<DrmLease>,
    pub drm_lease_request_cb:
        Option<Box<dyn FnMut(DrmNode, DrmLeaseRequest) -> Result<DrmLeaseBuilder, LeaseRejected>>>,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,

    // Session Lock (ext-session-lock-v1) & Idle Notification (ext-idle-notify-v1)
    pub session_lock_state: SessionLockManagerState,
    pub session_locked: bool,
    pub lock_surfaces: Vec<(Output, LockSurface)>,
    pub idle_notifier_state: IdleNotifierState<Self>,
    pub idle_inhibiting_surfaces: Vec<WlSurface>,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,

    // Pointer Constraints (zwp_pointer_constraints_v1) & Relative Pointer (zwp_relative_pointer_v1)
    pub pointer_constraints_state: PointerConstraintsState,
    pub relative_pointer_state: RelativePointerManagerState,

    // XWayland Support (smithay::xwayland)
    pub xwayland_shell_state: XWaylandShellState,
    pub xwm: Option<X11Wm>,
    pub x11_display: Option<u32>,

    // Screencopy Protocol (zwlr_screencopy_manager_v1)
    pub screencopy_state: crate::screencopy::ScreencopyState,

    // Pointer Gestures Protocol (zwp_pointer_gestures_v1) & Touchpad Gestures
    pub pointer_gestures_state: PointerGesturesState,
    pub active_gesture: ActiveTouchpadGesture,

    // Gamma Control (zwlr_gamma_control_manager_v1) & Night Light / Warmth Engine
    pub gamma_control_state: crate::gamma_control::GammaControlState,
    pub night_light_state: NightLightState,

    // Custom Glass Material Protocol (mochi_glass_v1)
    pub glass_manager_state: crate::glass::GlassManagerState,

    // DE Shell Protocols (ext-workspace-v1 & wlr-foreign-toplevel-management-v1)
    pub ext_workspace_state: crate::workspaces::ExtWorkspaceManagerState,
    pub foreign_toplevel_state: crate::foreign_toplevel::ForeignToplevelManagerState,

    // Display Management & Explicit Sync (wlr-output-management-v1 & linux-drm-syncobj-v1)
    pub output_management_state: crate::output_management::OutputManagementState,
    pub drm_syncobj_state: Option<smithay::wayland::drm_syncobj::DrmSyncobjState>,

    // Input Method & Virtual Keyboard Popups (zwp_text_input_v3 + zwp_input_method_v2)
    pub ime_popups: Vec<ImePopupSurface>,

    // Primary Input Seat & Pointer State
    pub seat: Seat<Self>,
    pub pointer_location: Point<f64, Logical>,
    pub cursor_status: CursorImageStatus,
    pub pending_vt_switch: Option<i32>,
    pub pending_screenshot_path: Option<PathBuf>,
}

impl MochiState {
    pub fn new(
        event_loop: &mut calloop::EventLoop<'static, Self>,
        display: Display<Self>,
        config: MochiConfig,
    ) -> anyhow::Result<Self> {
        Self::new_with_config_path(
            event_loop,
            display,
            config,
            PathBuf::from("config/mochi.kdl"),
        )
    }

    pub fn new_with_config_path(
        event_loop: &mut calloop::EventLoop<'static, Self>,
        display: Display<Self>,
        config: MochiConfig,
        config_path: PathBuf,
    ) -> anyhow::Result<Self> {
        let start_time = Instant::now();
        let clock = Clock::<Monotonic>::new();
        let display_handle = display.handle();
        let loop_handle = event_loop.handle();
        let loop_signal = event_loop.get_signal();

        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let xdg_activation_state = XdgActivationState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&display_handle);
        let presentation_state =
            PresentationState::new::<Self>(&display_handle, clock.id() as u32);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let dmabuf_state = DmabufState::new();
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&display_handle, |_| true);
        let idle_notifier_state = IdleNotifierState::<Self>::new(&display_handle, loop_handle.clone());
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&display_handle);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);
        Self::init_xwayland(&display_handle, &loop_handle);
        let screencopy_state = crate::screencopy::ScreencopyState::new(&display_handle);
        let pointer_gestures_state = PointerGesturesState::new::<Self>(&display_handle);
        let gamma_control_state = crate::gamma_control::GammaControlState::new(&display_handle);
        let night_light_state = NightLightState::new(&config.night_light);
        let glass_manager_state = crate::glass::GlassManagerState::new(&display_handle);
        let ext_workspace_state = crate::workspaces::ExtWorkspaceManagerState::new(&display_handle);
        let foreign_toplevel_state =
            crate::foreign_toplevel::ForeignToplevelManagerState::new(&display_handle);
        let output_management_state =
            crate::output_management::OutputManagementState::new(&display_handle);
        TextInputManagerState::new::<Self>(&display_handle);
        InputMethodManagerState::new::<Self, _>(&display_handle, |_client| true);
        VirtualKeyboardManagerState::new::<Self, _>(&display_handle, |_client| true);
        AlphaModifierState::new::<Self>(&display_handle);
        SinglePixelBufferState::new::<Self>(&display_handle);
        CursorShapeManagerState::new::<Self>(&display_handle);
        IdleInhibitManagerState::new::<Self>(&display_handle);
        let keyboard_shortcuts_inhibit_state =
            KeyboardShortcutsInhibitState::new::<Self>(&display_handle);
        XWaylandKeyboardGrabState::new::<Self>(&display_handle);
        TabletManagerState::new::<Self>(&display_handle);
        FifoManagerState::new::<Self>(&display_handle);
        CommitTimingManagerState::new::<Self>(&display_handle);
        ContentTypeState::new::<Self>(&display_handle);
        SecurityContextState::new::<Self, _>(&display_handle, is_privileged_client);
        let xdg_foreign_state = XdgForeignState::new::<Self>(&display_handle);
        XdgToplevelIconManager::new::<Self>(&display_handle);
        XdgToplevelTagManager::new::<Self>(&display_handle);
        XdgSystemBellState::new::<Self>(&display_handle);
        let popups = PopupManager::default();

        // Configure primary input seat (Keyboard + Pointer + Touch)
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&display_handle, "seat-0");
        let xkb_config = XkbConfig {
            layout: &config.keyboard.xkb_layout,
            ..XkbConfig::default()
        };
        if let Err(err) = seat.add_keyboard(
            xkb_config,
            config.keyboard.repeat_delay,
            config.keyboard.repeat_rate,
        ) {
            warn!(?err, "Failed to initialize requested XKB layout; falling back to default");
            seat.add_keyboard(
                XkbConfig::default(),
                config.keyboard.repeat_delay,
                config.keyboard.repeat_rate,
            )?;
        }
        seat.add_pointer();
        seat.add_touch();

        // Bind Wayland listening socket for client applications
        let socket_name = Self::init_wayland_listener(display, &loop_handle)?;
        // Bind Unix domain IPC socket for `mochi-msg` CLI and live status queries
        let ipc_socket_path = Self::init_ipc_listener(&loop_handle);
        // Watch `config/mochi.kdl` for live hot-reload on file save
        let last_config_mtime = file_mtime(&config_path);
        Self::init_config_watcher(&loop_handle);

        let mut wm = WindowManager::new_with_layout(
            config.workspace_count,
            config.visual_style,
            config.default_layout,
        );
        wm.output_configs = config.outputs;
        wm.window_rules = config.window_rules;
        let keybindings = config.keybindings;
        let startup_commands = config.startup_commands;

        info!(
            socket = ?socket_name,
            ipc_socket = %ipc_socket_path.display(),
            visual_style = wm.visual_style.style.as_str(),
            layout = wm.active_layout().as_str(),
            workspaces = config.workspace_count,
            binds = keybindings.len(),
            window_rules = wm.window_rules.len(),
            outputs = wm.output_configs.len(),
            startup_commands = startup_commands.len(),
            "Initialized Mochi Mochi core state (xdg-shell, xdg-activation, primary-selection, wp-presentation, layer-shell, session-lock, idle-notify, pointer-constraints, relative-pointer, dmabuf, IPC)"
        );

        Ok(Self {
            start_time,
            clock,
            socket_name,
            ipc_socket_path,
            config_path,
            last_config_mtime,
            display_handle,
            loop_handle,
            loop_signal,
            running: true,
            wm,
            keybindings,
            startup_commands,
            popups,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            xdg_activation_state,
            xdg_foreign_state,
            layer_shell_state,
            viewporter_state,
            fractional_scale_state,
            presentation_state,
            shm_state,
            dmabuf_state,
            dmabuf_global: None,
            drm_lease_state: None,
            active_drm_leases: Vec::new(),
            drm_lease_request_cb: None,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            session_lock_state,
            session_locked: false,
            lock_surfaces: Vec::new(),
            idle_notifier_state,
            idle_inhibiting_surfaces: Vec::new(),
            keyboard_shortcuts_inhibit_state,
            pointer_constraints_state,
            relative_pointer_state,
            xwayland_shell_state,
            xwm: None,
            x11_display: None,
            screencopy_state,
            pointer_gestures_state,
            active_gesture: ActiveTouchpadGesture::default(),
            gamma_control_state,
            night_light_state,
            glass_manager_state,
            ext_workspace_state,
            foreign_toplevel_state,
            output_management_state,
            drm_syncobj_state: None,
            ime_popups: Vec::new(),
            seat,
            pointer_location: Point::from((120.0, 120.0)),
            cursor_status: CursorImageStatus::default_named(),
            pending_vt_switch: None,
            pending_screenshot_path: None,
        })
    }

    fn init_wayland_listener(
        display: Display<Self>,
        loop_handle: &LoopHandle<'static, Self>,
    ) -> anyhow::Result<OsString> {
        let listening_socket = ListeningSocketSource::new_auto()?;
        let socket_name = listening_socket.socket_name().to_os_string();

        loop_handle.insert_source(listening_socket, move |client_stream, _, state| {
            if let Err(err) = state
                .display_handle
                .insert_client(client_stream, Arc::new(ClientState::default()))
            {
                warn!(?err, "Error accepting Wayland client connection");
            }
        })?;

        loop_handle.insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // Safety: We do not drop the Display while wrapped inside Generic.
                match unsafe { display.get_mut().dispatch_clients(state) } {
                    Ok(_) => Ok(PostAction::Continue),
                    Err(err) => {
                        error!(?err, "Fatal error dispatching Wayland clients");
                        state.running = false;
                        state.loop_signal.stop();
                        Err(err)
                    }
                }
            },
        )?;

        Ok(socket_name)
    }

    fn init_ipc_listener(loop_handle: &LoopHandle<'static, Self>) -> PathBuf {
        let socket_path = default_socket_path();
        let _ = std::fs::remove_file(&socket_path);

        match UnixListener::bind(&socket_path) {
            Ok(listener) => {
                if let Err(err) = listener.set_nonblocking(true) {
                    warn!(?err, "Failed to set IPC socket non-blocking");
                    return socket_path;
                }
                let _ = loop_handle.insert_source(
                    Generic::new(listener, Interest::READ, Mode::Level),
                    |_, listener, state| {
                        loop {
                            match listener.accept() {
                                Ok((mut stream, _addr)) => {
                                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                                    let mut reader = BufReader::new(&stream);
                                    let mut line = String::new();
                                    if reader.read_line(&mut line).is_ok() && !line.trim().is_empty()
                                    {
                                        let reply = match IpcRequest::from_wire(&line) {
                                            Ok(req) => state.handle_ipc_request(req),
                                            Err(err) => {
                                                format!("{{\"ok\":false,\"error\":\"{err}\"}}\n")
                                            }
                                        };
                                        let _ = stream.write_all(reply.as_bytes());
                                        let _ = stream.flush();
                                    }
                                }
                                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(err) => {
                                    warn!(?err, "Error accepting IPC client connection");
                                    break;
                                }
                            }
                        }
                        Ok(PostAction::Continue)
                    },
                );
            }
            Err(err) => {
                warn!(
                    path = %socket_path.display(),
                    ?err,
                    "Failed to bind Mochi Mochi IPC socket"
                );
            }
        }

        socket_path
    }

    fn init_config_watcher(loop_handle: &LoopHandle<'static, Self>) {
        let _ = loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(500)),
            |_, _, state| {
                let current_mtime = file_mtime(&state.config_path);
                if current_mtime.is_some() && current_mtime != state.last_config_mtime {
                    state.last_config_mtime = current_mtime;
                    state.reload_config_from_disk();
                }
                TimeoutAction::ToDuration(Duration::from_millis(500))
            },
        );
    }

    fn init_xwayland(
        display_handle: &DisplayHandle,
        loop_handle: &LoopHandle<'static, Self>,
    ) {
        match XWayland::spawn(
            display_handle,
            None,
            std::iter::empty::<(&str, &str)>(),
            true,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
            |_| (),
        ) {
            Ok((xwayland, client)) => {
                info!("Spawned XWayland background process; awaiting readiness");
                let loop_handle_clone = loop_handle.clone();
                let res = loop_handle.insert_source(xwayland, move |event, _, state: &mut MochiState| {
                    match event {
                        XWaylandEvent::Ready {
                            x11_socket,
                            display_number,
                        } => {
                            info!(display_number, "XWayland server ready; launching X11 Window Manager");
                            state.x11_display = Some(display_number);
                            std::env::set_var("DISPLAY", format!(":{}", display_number));
                            crate::session::export_x11_display(display_number);

                            match X11Wm::start_wm(
                                loop_handle_clone.clone(),
                                x11_socket,
                                client.clone(),
                            ) {
                                Ok(wm) => {
                                    state.xwm = Some(wm);
                                    info!(display_number, "Mochi Mochi X11 Window Manager connected and active");
                                }
                                Err(err) => {
                                    warn!(?err, "Failed to start X11 Window Manager for XWayland");
                                }
                            }
                        }
                        XWaylandEvent::Error => {
                            warn!("XWayland server reported an error or crashed during startup");
                        }
                    }
                });
                if let Err(err) = res {
                    warn!(?err, "Failed to register XWayland event source in Calloop event loop");
                }
            }
            Err(err) => {
                info!(?err, "Xwayland binary not available or spawn failed; running without X11 support");
            }
        }
    }

    /// Hot-reloads `config/mochi.kdl` from disk and applies visual style, keybindings, outputs, and window rules live.
    pub fn reload_config_from_disk(&mut self) {
        let new_cfg = MochiConfig::load_from_path(&self.config_path);
        self.wm.apply_live_style_config(new_cfg.visual_style);
        self.wm.output_configs = new_cfg.outputs;
        self.wm.window_rules = new_cfg.window_rules;
        self.keybindings = new_cfg.keybindings;
        self.night_light_state.enabled = new_cfg.night_light.enabled;
        self.night_light_state.temperature = new_cfg.night_light.temperature;
        self.night_light_state.schedule_start = new_cfg.night_light.schedule_start;
        self.night_light_state.schedule_end = new_cfg.night_light.schedule_end;
        if self.night_light_state.manual_override.is_none() && !self.night_light_state.external_gamma_active {
            let target = if self.night_light_state.is_active_now() { 1.0 } else { 0.0 };
            self.night_light_state.spring.set_target(target);
        }
        info!(
            path = %self.config_path.display(),
            style = self.wm.visual_style.style.as_str(),
            binds = self.keybindings.len(),
            window_rules = self.wm.window_rules.len(),
            outputs = self.wm.output_configs.len(),
            night_light = self.night_light_state.enabled,
            temp = self.night_light_state.temperature,
            "Hot-reloaded Mochi Mochi KDL configuration live"
        );
    }

    /// Initializes `linux-drm-syncobj-v1` (Explicit Sync) when running on a DRM device that supports `syncobj_eventfd`.
    pub fn init_drm_syncobj(&mut self, drm_fd: &smithay::backend::drm::DrmDeviceFd) {
        if self.drm_syncobj_state.is_some() {
            return;
        }
        if smithay::wayland::drm_syncobj::supports_syncobj_eventfd(drm_fd) {
            self.drm_syncobj_state = Some(smithay::wayland::drm_syncobj::DrmSyncobjState::new::<
                Self,
            >(&self.display_handle, drm_fd.clone()));
            info!("Advertised wp_linux_drm_syncobj_manager_v1 (Explicit Sync) global");
        }
    }

    /// Advances all window manager animations and night light color temperature springs.
    pub fn tick_frame(&mut self, dt_secs: f64) -> bool {
        let wm_anim = self.wm.tick_animations(dt_secs);
        self.night_light_state.tick(dt_secs);
        self.sync_ext_workspaces();
        self.sync_foreign_toplevels();
        let nl_anim = self.night_light_state.spring.is_animating();
        wm_anim || nl_anim
    }

    /// Sends `wl_surface.frame` callbacks and `wp_presentation_feedback.presented` events
    /// for all visible windows and layer-shell surfaces on `output`.
    pub fn send_frames_and_presentation_feedback(
        &self,
        output: &Output,
        sequence: u64,
        refresh_interval: Duration,
    ) {
        let elapsed = self.start_time.elapsed();
        let mut output_feedback = OutputPresentationFeedback::new(output);

        // While the session is locked, only the lock surface on this output is presented.
        if self.session_locked {
            if let Some(lock_surface) = self.lock_surface_for_output(output) {
                send_frames_surface_tree(
                    lock_surface.wl_surface(),
                    output,
                    elapsed,
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }
            output_feedback.presented::<_, Monotonic>(
                self.clock.now(),
                Refresh::fixed(refresh_interval),
                sequence,
                wp_presentation_feedback::Kind::Vsync,
            );
            return;
        }

        let frame_target = Timestamp::from(self.clock.now());

        for window in self.wm.active_space().elements() {
            window.with_surfaces(|_, states| {
                Self::signal_surface_frame_barriers(states, frame_target);
            });
            window.take_presentation_feedback(
                &mut output_feedback,
                |_, _| Some(output.clone()),
                |_, _| wp_presentation_feedback::Kind::Vsync,
            );
        }

        for layer in layer_map_for_output(output).layers() {
            layer.with_surfaces(|_, states| {
                Self::signal_surface_frame_barriers(states, frame_target);
            });
            layer.take_presentation_feedback(
                &mut output_feedback,
                |_, _| Some(output.clone()),
                |_, _| wp_presentation_feedback::Kind::Vsync,
            );
        }

        // Complete presentation feedback BEFORE sending the next wl_surface.frame
        // callback so toolkits like GTK4/GDK never see a new frame while the
        // previous frame's timings are still in GDK_FRAME_PREPARING.
        output_feedback.presented::<_, Monotonic>(
            self.clock.now(),
            Refresh::fixed(refresh_interval),
            sequence,
            wp_presentation_feedback::Kind::Vsync,
        );

        let frame_throttle = Some(refresh_interval);
        for window in self.wm.active_space().elements() {
            window.send_frame(
                output,
                elapsed,
                frame_throttle,
                |_, _| Some(output.clone()),
            );
        }

        for layer in layer_map_for_output(output).layers() {
            layer.send_frame(
                output,
                elapsed,
                frame_throttle,
                |_, _| Some(output.clone()),
            );
        }
    }

    /// Signals pending `wp_fifo_v1` barriers and `wp_commit_timing_v1` timers on `states`
    /// so frame-paced clients can make forward progress on each compositor presentation tick.
    fn signal_surface_frame_barriers(
        states: &smithay::wayland::compositor::SurfaceData,
        frame_target: Timestamp,
    ) {
        let fifo_barrier = states
            .cached_state
            .get::<FifoBarrierCachedState>()
            .current()
            .barrier
            .take();
        if let Some(barrier) = fifo_barrier {
            barrier.signal();
        }

        if let Some(mut commit_timer_state) = states
            .data_map
            .get::<CommitTimerBarrierStateUserData>()
            .map(|timer| timer.lock().unwrap())
        {
            commit_timer_state.signal_until(frame_target);
        }
    }

    /// Executes an [`IpcRequest`] from `mochi-msg` and returns a JSON response line.
    pub fn handle_ipc_request(&mut self, request: IpcRequest) -> String {
        // While an ext-session-lock is active, refuse requests that could reveal user data
        // (window lists, screenshots) or move focus / spawn clients behind the lockscreen.
        if self.session_locked
            && !matches!(
                request,
                IpcRequest::Status
                    | IpcRequest::ReloadConfig
                    | IpcRequest::SetStyle(_)
                    | IpcRequest::NightLight(_)
                    | IpcRequest::Quit
            )
        {
            return "{\"ok\":false,\"error\":\"session is locked\"}\n".to_string();
        }

        match request {
            IpcRequest::Status => {
                let win_count = self.wm.active_space().elements().count();
                let x11_str = self
                    .x11_display
                    .map(|d| format!(":{}", d))
                    .unwrap_or_default();
                format!(
                    "{{\"ok\":true,\"workspace\":{},\"layout\":\"{}\",\"overview\":{},\"scratchpad_visible\":{},\"visual_style\":\"{}\",\"blur_radius\":{},\"opacity\":{:.2},\"corner_radius\":{},\"windows\":{},\"master_ratio\":{:.2},\"master_count\":{},\"wayland_display\":\"{}\",\"x11_display\":\"{}\",\"gestures\":true,\"touch\":true,\"night_light\":{},\"temperature\":{},\"night_light_active\":{},\"night_light_progress\":{:.2}}}\n",
                    self.wm.active_workspace_number(),
                    self.wm.active_layout().as_str(),
                    self.wm.overview_active,
                    self.wm.scratchpad_visible,
                    self.wm.visual_style.style.as_str(),
                    self.wm.visual_style.blur_radius,
                    self.wm.visual_style.opacity,
                    self.wm.visual_style.corner_radius,
                    win_count,
                    self.wm.active_master_ratio(),
                    self.wm.active_master_count(),
                    self.socket_name.to_string_lossy(),
                    x11_str,
                    self.night_light_state.enabled,
                    self.night_light_state.temperature,
                    self.night_light_state.is_active_now(),
                    self.night_light_state.spring.current
                )
            }
            IpcRequest::SetStyle(name) => {
                let new_style = if name.eq_ignore_ascii_case("cycle") {
                    self.wm.cycle_visual_style()
                } else {
                    let s = VisualStyle::from_name(&name);
                    self.wm.set_visual_style(s);
                    s
                };
                format!(
                    "{{\"ok\":true,\"visual_style\":\"{}\"}}\n",
                    new_style.as_str()
                )
            }
            IpcRequest::SetLayout(mode_str) => {
                let new_mode = if mode_str.eq_ignore_ascii_case("toggle") {
                    self.wm.toggle_active_layout()
                } else {
                    let m = LayoutMode::from_name(&mode_str);
                    self.wm.set_active_layout(m);
                    m
                };
                format!(
                    "{{\"ok\":true,\"workspace\":{},\"layout\":\"{}\"}}\n",
                    self.wm.active_workspace_number(),
                    new_mode.as_str()
                )
            }
            IpcRequest::SwitchWorkspace(ws_1_based) => {
                let idx = ws_1_based.saturating_sub(1);
                let switched = self.wm.switch_workspace(idx);
                format!(
                    "{{\"ok\":true,\"switched\":{},\"workspace\":{},\"layout\":\"{}\"}}\n",
                    switched,
                    self.wm.active_workspace_number(),
                    self.wm.active_layout().as_str()
                )
            }
            IpcRequest::ReloadConfig => {
                self.reload_config_from_disk();
                format!(
                    "{{\"ok\":true,\"action\":\"reloaded\",\"visual_style\":\"{}\",\"blur_radius\":{},\"opacity\":{:.2}}}\n",
                    self.wm.visual_style.style.as_str(),
                    self.wm.visual_style.blur_radius,
                    self.wm.visual_style.opacity
                )
            }
            IpcRequest::Spawn(cmd) => {
                self.spawn_client_command(&cmd);
                format!("{{\"ok\":true,\"spawned\":{:?}}}\n", cmd)
            }
            IpcRequest::ToggleOverview => {
                let active = self.wm.toggle_overview();
                format!("{{\"ok\":true,\"overview\":{}}}\n", active)
            }
            IpcRequest::ToggleScratchpad => {
                if let Some(win) = self.wm.toggle_scratchpad() {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        let surface = mochi_wm::window_wl_surface(&win);
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, surface, serial);
                    }
                }
                format!(
                    "{{\"ok\":true,\"scratchpad_visible\":{}}}\n",
                    self.wm.scratchpad_visible
                )
            }
            IpcRequest::MoveToScratchpad => {
                let stashed = if let Some(win) = self.focused_window() {
                    self.wm.move_to_scratchpad(&win)
                } else {
                    false
                };
                format!("{{\"ok\":true,\"stashed\":{}}}\n", stashed)
            }
            IpcRequest::Screenshot(path_str) => {
                let dest = self.queue_screenshot(Some(PathBuf::from(path_str)));
                format!(
                    "{{\"ok\":true,\"screenshot_queued\":{:?}}}\n",
                    dest.display().to_string()
                )
            }
            IpcRequest::ListWindows => {
                let focused = self.focused_window();
                self.wm.list_windows_json(focused.as_ref())
            }
            IpcRequest::CloseFocusedWindow => {
                let closed = if let Some(win) = self.focused_window() {
                    mochi_wm::send_window_close(&win);
                    true
                } else {
                    false
                };
                format!("{{\"ok\":true,\"closed\":{}}}\n", closed)
            }
            IpcRequest::Minimize => {
                let minimized = if let Some(win) = self.focused_window() {
                    if self.wm.minimize_window(&win) {
                        if let Some(keyboard) = self.seat.get_keyboard() {
                            let next_focus = self
                                .wm
                                .active_space()
                                .elements()
                                .last()
                                .and_then(mochi_wm::window_wl_surface);
                            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                            keyboard.set_focus(self, next_focus, serial);
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                format!(
                    "{{\"ok\":true,\"minimized\":{},\"minimized_count\":{}}}\n",
                    minimized,
                    self.wm.minimized_count()
                )
            }
            IpcRequest::Restore => {
                let restored = if let Some(win) = self.wm.restore_minimized_window(None) {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        let surface = mochi_wm::window_wl_surface(&win);
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, surface, serial);
                    }
                    true
                } else {
                    false
                };
                format!(
                    "{{\"ok\":true,\"restored\":{},\"minimized_count\":{}}}\n",
                    restored,
                    self.wm.minimized_count()
                )
            }
            IpcRequest::FocusWindow(query) => {
                if let Some(win) = self.wm.focus_window_by_query(&query) {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        let surface = mochi_wm::window_wl_surface(&win);
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, surface, serial);
                    }
                    let (app_id, title) = mochi_wm::window_metadata(&win);
                    format!(
                        "{{\"ok\":true,\"focused\":true,\"workspace\":{},\"app_id\":{:?},\"title\":{:?}}}\n",
                        self.wm.active_workspace_number(),
                        app_id.unwrap_or_default(),
                        title.unwrap_or_default()
                    )
                } else {
                    format!("{{\"ok\":false,\"focused\":false,\"query\":{:?}}}\n", query)
                }
            }
            IpcRequest::AdjustMasterRatio(arg) => {
                let norm = arg.trim().to_ascii_lowercase();
                if norm == "grow" {
                    self.wm.adjust_master_ratio(0.05);
                } else if norm == "shrink" {
                    self.wm.adjust_master_ratio(-0.05);
                } else if norm.starts_with('+') || norm.starts_with('-') {
                    if let Ok(delta) = norm.parse::<f64>() {
                        self.wm.adjust_master_ratio(delta);
                    }
                } else if let Ok(val) = norm.parse::<f64>() {
                    self.wm.set_master_ratio(val);
                }
                format!(
                    "{{\"ok\":true,\"workspace\":{},\"master_ratio\":{:.2}}}\n",
                    self.wm.active_workspace_number(),
                    self.wm.active_master_ratio()
                )
            }
            IpcRequest::AdjustMasterCount(arg) => {
                let norm = arg.trim().to_ascii_lowercase();
                if norm == "inc" || norm == "grow" {
                    self.wm.adjust_master_count(1);
                } else if norm == "dec" || norm == "shrink" {
                    self.wm.adjust_master_count(-1);
                } else if norm.starts_with('+') || norm.starts_with('-') {
                    if let Ok(delta) = norm.parse::<i32>() {
                        self.wm.adjust_master_count(delta);
                    }
                } else if let Ok(val) = norm.parse::<usize>() {
                    self.wm.set_master_count(val);
                }
                format!(
                    "{{\"ok\":true,\"workspace\":{},\"master_count\":{}}}\n",
                    self.wm.active_workspace_number(),
                    self.wm.active_master_count()
                )
            }
            IpcRequest::SwapWindow(arg) => {
                let dir = mochi_wm::SwapDirection::from_str(&arg)
                    .unwrap_or(mochi_wm::SwapDirection::Master);
                let swapped = if let Some(win) = self.focused_window() {
                    self.wm.swap_window(&win, dir);
                    true
                } else {
                    false
                };
                format!(
                    "{{\"ok\":true,\"swapped\":{},\"direction\":{:?}}}\n",
                    swapped,
                    dir
                )
            }
            IpcRequest::NightLight(arg) => {
                let sub = arg.trim().to_ascii_lowercase();
                match sub.as_str() {
                    "toggle" => {
                        let active = self.night_light_state.toggle();
                        self.wm.trigger_hud();
                        format!(
                            "{{\"ok\":true,\"night_light\":{},\"temperature\":{},\"active\":{}}}\n",
                            self.night_light_state.enabled,
                            self.night_light_state.temperature,
                            active
                        )
                    }
                    "on" | "enable" | "true" => {
                        self.night_light_state.set_enabled(true);
                        self.wm.trigger_hud();
                        format!(
                            "{{\"ok\":true,\"night_light\":true,\"temperature\":{},\"active\":true}}\n",
                            self.night_light_state.temperature
                        )
                    }
                    "off" | "disable" | "false" => {
                        self.night_light_state.set_enabled(false);
                        self.wm.trigger_hud();
                        format!(
                            "{{\"ok\":true,\"night_light\":false,\"temperature\":{},\"active\":false}}\n",
                            self.night_light_state.temperature
                        )
                    }
                    other => {
                        let val_str = other.strip_prefix("temp").map(str::trim).unwrap_or(other);
                        if let Ok(kelvin) = val_str.parse::<u32>() {
                            self.night_light_state.set_temperature(kelvin);
                            self.wm.trigger_hud();
                            format!(
                                "{{\"ok\":true,\"night_light\":{},\"temperature\":{},\"active\":{}}}\n",
                                self.night_light_state.enabled,
                                self.night_light_state.temperature,
                                self.night_light_state.is_active_now()
                            )
                        } else {
                            let (r, g, b, alpha) = self.night_light_state.current_warmth();
                            format!(
                                "{{\"ok\":true,\"night_light\":{},\"temperature\":{},\"active\":{},\"progress\":{:.2},\"tint_rgb\":[{:.2},{:.2},{:.2}],\"tint_alpha\":{:.2}}}\n",
                                self.night_light_state.enabled,
                                self.night_light_state.temperature,
                                self.night_light_state.is_active_now(),
                                self.night_light_state.spring.current,
                                r, g, b, alpha
                            )
                        }
                    }
                }
            }
            IpcRequest::Quit => {
                self.running = false;
                self.loop_signal.stop();
                "{\"ok\":true,\"action\":\"quit\"}\n".to_string()
            }
        }
    }

    /// Executes all `startup { spawn "..." }` commands configured in `mochi.kdl`.
    pub fn run_startup_commands(&self) {
        for cmd in &self.startup_commands {
            info!(command = %cmd, "Launching configured startup command from mochi.kdl");
            self.spawn_client_command(cmd);
        }
    }

    /// Queues a framebuffer screenshot to be written on the next composited frame.
    pub fn queue_screenshot(&mut self, custom_path: Option<PathBuf>) -> PathBuf {
        let path = custom_path.unwrap_or_else(|| {
            let secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            PathBuf::from(format!("mochi-screenshot-{secs}.ppm"))
        });
        info!(path = %path.display(), "Queued compositor framebuffer screenshot");
        self.pending_screenshot_path = Some(path.clone());
        path
    }

    /// Finds the topmost surface (Overlay/Top Layer-Shell -> Window/Popup -> Bottom/Background Layer-Shell)
    /// under a given logical point and returns both the `WlSurface` and its surface-local origin.
    pub fn surface_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<i32, Logical>)> {
        let space = self.wm.active_space();
        let output = space.outputs().find(|o| {
            space
                .output_geometry(o)
                .map(|geo| geo.to_f64().contains(point))
                .unwrap_or(false)
        });

        // While locked, only the lock surface of the output under the pointer may receive input.
        if self.session_locked {
            let output = output?;
            let output_geo = space.output_geometry(output)?;
            let lock_surface = self.lock_surface_for_output(output)?;
            return Some((lock_surface.wl_surface().clone(), output_geo.loc));
        }

        if let Some(output) = output {
            let output_geo = space.output_geometry(output).unwrap();
            let layers = layer_map_for_output(output);

            // 1. Check Overlay & Top layer-shell surfaces first (e.g., docks, top bars, launchers)
            if let Some(layer) = layers
                .layer_under(WlrLayer::Overlay, point - output_geo.loc.to_f64())
                .or_else(|| layers.layer_under(WlrLayer::Top, point - output_geo.loc.to_f64()))
            {
                let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                if let Some((s, s_loc)) = layer.surface_under(
                    point - output_geo.loc.to_f64() - layer_loc.to_f64(),
                    WindowSurfaceType::ALL,
                ) {
                    return Some((s, s_loc + layer_loc + output_geo.loc));
                }
            }
        }

        // 2. Check standard XDG windows & popups in the active workspace
        if let Some((window, location)) = space.element_under(point) {
            if let Some((surface, surface_offset)) =
                window.surface_under(point - location.to_f64(), WindowSurfaceType::ALL)
            {
                return Some((surface, surface_offset + location));
            }
        }

        // 3. Check Bottom & Background layer-shell surfaces (e.g., desktop widgets, wallpaper canvas)
        if let Some(output) = output {
            let output_geo = space.output_geometry(output).unwrap();
            let layers = layer_map_for_output(output);

            if let Some(layer) = layers
                .layer_under(WlrLayer::Bottom, point - output_geo.loc.to_f64())
                .or_else(|| {
                    layers.layer_under(WlrLayer::Background, point - output_geo.loc.to_f64())
                })
            {
                let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                if let Some((s, s_loc)) = layer.surface_under(
                    point - output_geo.loc.to_f64() - layer_loc.to_f64(),
                    WindowSurfaceType::ALL,
                ) {
                    return Some((s, s_loc + layer_loc + output_geo.loc));
                }
            }
        }

        None
    }

    /// Returns the `ext-session-lock-v1` lock surface assigned to `output`, if any.
    pub fn lock_surface_for_output(&self, output: &Output) -> Option<&LockSurface> {
        self.lock_surfaces
            .iter()
            .find(|(o, s)| o == output && s.alive())
            .map(|(_, s)| s)
    }

    /// Returns the lock surface that should hold keyboard focus while locked
    /// (the one under the pointer, falling back to the first live one).
    pub fn preferred_lock_surface(&self) -> Option<WlSurface> {
        let space = self.wm.active_space();
        let under_pointer = space
            .outputs()
            .find(|o| {
                space
                    .output_geometry(o)
                    .map(|geo| geo.to_f64().contains(self.pointer_location))
                    .unwrap_or(false)
            })
            .and_then(|o| self.lock_surface_for_output(o));
        under_pointer
            .or_else(|| self.lock_surfaces.iter().map(|(_, s)| s).find(|s| s.alive()))
            .map(|s| s.wl_surface().clone())
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Helper to retrieve a file descriptor where needed.
pub type _UnusedFd = OwnedFd;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mochi_state_initialization() {
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
        std::env::set_var("MOCHI_IPC_SOCKET", "/tmp/mochi-mochi-test.sock");
        let mut event_loop: calloop::EventLoop<'static, MochiState> =
            calloop::EventLoop::try_new().expect("Failed to create calloop EventLoop");
        let display: Display<MochiState> =
            Display::new().expect("Failed to create Wayland Display");
        let config = MochiConfig::default();

        let mut state = MochiState::new(&mut event_loop, display, config)
            .expect("Failed to initialize MochiState");

        assert!(state.running);
        assert!(!state.socket_name.is_empty());
        assert_eq!(state.wm.active_workspace_number(), 1);

        let status_json = state.handle_ipc_request(IpcRequest::Status);
        assert!(status_json.contains("\"ok\":true"));
        assert!(status_json.contains("\"layout\":\"floating\""));
        assert!(status_json.contains("\"x11_display\""));
        assert!(status_json.contains("\"master_ratio\":0.55"));
        assert!(status_json.contains("\"master_count\":1"));

        let ratio_json = state.handle_ipc_request(IpcRequest::AdjustMasterRatio("grow".to_string()));
        assert!(ratio_json.contains("\"master_ratio\":0.60"));

        let count_json = state.handle_ipc_request(IpcRequest::AdjustMasterCount("inc".to_string()));
        assert!(count_json.contains("\"master_count\":2"));

        let swap_json = state.handle_ipc_request(IpcRequest::SwapWindow("master".to_string()));
        assert!(swap_json.contains("\"ok\":true"));

        state.x11_display = Some(0);
        let x11_status = state.handle_ipc_request(IpcRequest::Status);
        assert!(x11_status.contains("\"x11_display\":\":0\""));
        state.x11_display = None;

        let layout_json = state.handle_ipc_request(IpcRequest::SetLayout("toggle".to_string()));
        assert!(layout_json.contains("\"layout\":\"tiling\""));

        // ext-session-lock-v1: data-revealing / focus-moving IPC is refused while locked
        assert!(!state.session_locked);
        assert!(state.lock_surfaces.is_empty());
        assert!(state.preferred_lock_surface().is_none());
        state.session_locked = true;
        for req in [
            IpcRequest::ListWindows,
            IpcRequest::Screenshot("/tmp/mochi-locked.ppm".to_string()),
            IpcRequest::Spawn("true".to_string()),
            IpcRequest::FocusWindow("foot".to_string()),
            IpcRequest::SwitchWorkspace(2),
        ] {
            let resp = state.handle_ipc_request(req);
            assert!(resp.contains("session is locked"), "unexpected response: {resp}");
        }
        assert!(state.pending_screenshot_path.is_none());
        assert!(state
            .handle_ipc_request(IpcRequest::Status)
            .contains("\"ok\":true"));
        state.session_locked = false;
        assert!(state
            .handle_ipc_request(IpcRequest::ListWindows)
            .contains("\"ok\":true"));
    }

    #[test]
    fn test_touchpad_gestures_and_touch_state() {
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
        std::env::set_var("MOCHI_IPC_SOCKET", "/tmp/mochi-mochi-touch-test.sock");
        let mut event_loop: calloop::EventLoop<'static, MochiState> =
            calloop::EventLoop::try_new().expect("Failed to create calloop EventLoop");
        let display: Display<MochiState> =
            Display::new().expect("Failed to create Wayland Display");
        let config = MochiConfig::default();

        let mut state = MochiState::new(&mut event_loop, display, config)
            .expect("MochiState initialization failed");

        let output = Output::new(
            "HEADLESS-1".to_string(),
            smithay::output::PhysicalProperties {
                size: (500, 300).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "Mochi Mochi".into(),
                model: "Virtual Display".into(),
            },
        );
        let mode = smithay::output::Mode {
            size: (1920, 1080).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
        output.set_preferred(mode);
        state.wm.map_output(&output, Point::from((0, 0)));

        // 1. Touch handle verification & IPC status capabilities
        assert!(state.seat.get_touch().is_some(), "Seat must provide a touch handle");
        let status_json = state.handle_ipc_request(IpcRequest::Status);
        assert!(status_json.contains("\"gestures\":true"));
        assert!(status_json.contains("\"touch\":true"));

        // 2. Gesture default state
        assert_eq!(state.active_gesture, ActiveTouchpadGesture::default());
        assert!(!state.active_gesture.swipe_active);
        assert!(!state.active_gesture.pinch_active);

        // 3. Workspace slide swipe tracking simulation
        state.active_gesture.swipe_active = true;
        state.active_gesture.swipe_initial_workspace = state.wm.active_workspace_index();
        state.active_gesture.swipe_accumulated_dx = 150.0;
        state.wm.workspace_slide_x.current = 150.0;
        state.wm.workspace_slide_x.set_target(150.0);
        assert_eq!(state.wm.workspace_slide_x.current, 150.0);

        // End swipe right on workspace 0 (cannot go lower than 0, snaps to 0.0)
        let dx = state.active_gesture.swipe_accumulated_dx;
        state.active_gesture.swipe_active = false;
        state.active_gesture.swipe_accumulated_dx = 0.0;
        if dx > 140.0 {
            let cur_ws = state.wm.active_workspace_index();
            if cur_ws > 0 {
                state.wm.switch_workspace(cur_ws - 1);
            } else {
                state.wm.workspace_slide_x.set_target(0.0);
            }
        }
        assert_eq!(state.wm.workspace_slide_x.target, 0.0);
        assert_eq!(state.wm.active_workspace_index(), 0);

        // End swipe left from workspace 0 to workspace 1 (dx < -140.0)
        state.active_gesture.swipe_active = true;
        state.active_gesture.swipe_accumulated_dx = -160.0;
        let dx = state.active_gesture.swipe_accumulated_dx;
        state.active_gesture.swipe_active = false;
        state.active_gesture.swipe_accumulated_dx = 0.0;
        if dx < -140.0 {
            let cur_ws = state.wm.active_workspace_index();
            if cur_ws + 1 < state.wm.workspace_count() {
                state.wm.switch_workspace(cur_ws + 1);
            }
        }
        assert_eq!(state.wm.active_workspace_index(), 1);

        // 4. Pinch gesture expose / overview toggle simulation
        assert!(!state.wm.overview_active);
        state.active_gesture.pinch_active = true;
        // Pinch in: scale < 0.72 triggers overview / HUD
        let pinch_scale = 0.65;
        if pinch_scale < 0.72 && !state.wm.overview_active {
            state.wm.toggle_overview();
        }
        // With no client windows mapped, overview trigger falls back to HUD pill indicator
        assert_eq!(state.wm.hud_spring.target, 1.0);

        // When overview is active, pinch-out (scale > 1.35) exits overview
        state.wm.overview_active = true;
        let pinch_out_scale = 1.45;
        if pinch_out_scale > 1.35 && state.wm.overview_active {
            state.wm.toggle_overview();
        }
        assert!(!state.wm.overview_active);
        state.active_gesture.pinch_active = false;
    }

    #[test]
    fn test_night_light_state_and_ipc() {
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
        std::env::set_var("MOCHI_IPC_SOCKET", "/tmp/mochi-mochi-nl-test.sock");
        let mut event_loop: calloop::EventLoop<'static, MochiState> =
            calloop::EventLoop::try_new().expect("Failed to create calloop EventLoop");
        let display: Display<MochiState> =
            Display::new().expect("Failed to create Wayland Display");
        let config = MochiConfig::default();

        let mut state = MochiState::new(&mut event_loop, display, config)
            .expect("MochiState initialization failed");

        // 1. Initial default state (disabled, 3800K)
        assert!(!state.night_light_state.enabled);
        assert_eq!(state.night_light_state.temperature, 3800);
        assert_eq!(state.night_light_state.spring.current, 0.0);

        let status_json = state.handle_ipc_request(IpcRequest::Status);
        assert!(status_json.contains("\"night_light\":false"));
        assert!(status_json.contains("\"temperature\":3800"));

        // 2. Toggle via IPC
        let toggle_json = state.handle_ipc_request(IpcRequest::NightLight("toggle".to_string()));
        assert!(toggle_json.contains("\"active\":true"));
        assert_eq!(state.night_light_state.spring.target, 1.0);

        // Advance spring animations
        state.tick_frame(0.2);
        assert!(state.night_light_state.spring.current > 0.0);

        // 3. Set temperature via IPC
        let temp_json = state.handle_ipc_request(IpcRequest::NightLight("3200".to_string()));
        assert!(temp_json.contains("\"temperature\":3200"));
        assert_eq!(state.night_light_state.temperature, 3200);

        // 4. Query detailed warmth status via IPC
        let query_json = state.handle_ipc_request(IpcRequest::NightLight("status".to_string()));
        assert!(query_json.contains("\"temperature\":3200"));
        assert!(query_json.contains("\"tint_rgb\""));

        // 5. External gamma ramps override (from gammastep / wlsunset)
        state.night_light_state.set_external_gamma((1.0, 0.75, 0.50));
        assert!(state.night_light_state.external_gamma_active);
        let (r, g, b, alpha) = state.night_light_state.current_warmth();
        assert_eq!(r, 1.0);
        assert_eq!(g, 0.75);
        assert_eq!(b, 0.50);
        assert!(alpha > 0.0);

        state.night_light_state.clear_external_gamma();
        assert!(!state.night_light_state.external_gamma_active);

        // 6. Turn off via IPC
        let off_json = state.handle_ipc_request(IpcRequest::NightLight("off".to_string()));
        assert!(off_json.contains("\"active\":false"));
        assert_eq!(state.night_light_state.spring.target, 0.0);
    }
}
