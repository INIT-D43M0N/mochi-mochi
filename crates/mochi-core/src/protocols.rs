// SPDX-License-Identifier: GPL-3.0-or-later
//! Wayland Protocol Implementations & Delegates for `MochiState`

use std::{os::unix::io::AsFd, sync::Arc};

use mochi_wm::SnapZone;
use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        drm::DrmNode,
        renderer::utils::on_commit_buffer_handler,
    },
    delegate_alpha_modifier, delegate_commit_timing, delegate_compositor, delegate_content_type,
    delegate_cursor_shape, delegate_data_device, delegate_dmabuf, delegate_drm_lease,
    delegate_drm_syncobj,
    delegate_fifo, delegate_fractional_scale, delegate_idle_inhibit, delegate_idle_notify,
    delegate_input_method_manager, delegate_keyboard_shortcuts_inhibit, delegate_layer_shell,
    delegate_output, delegate_pointer_constraints, delegate_pointer_gestures,
    delegate_presentation, delegate_primary_selection, delegate_relative_pointer, delegate_seat,
    delegate_security_context, delegate_session_lock, delegate_shm, delegate_single_pixel_buffer,
    delegate_tablet_manager, delegate_text_input_manager, delegate_viewporter,
    delegate_virtual_keyboard_manager, delegate_xdg_activation, delegate_xdg_decoration,
    delegate_xdg_foreign, delegate_xdg_shell, delegate_xdg_system_bell,
    delegate_xdg_toplevel_icon, delegate_xdg_toplevel_tag, delegate_xwayland_keyboard_grab,
    delegate_xwayland_shell,
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output, LayerSurface,
        PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window,
    },
    input::{
        pointer::{
            CursorImageStatus, Focus, GrabStartData as PointerGrabStartData, MotionEvent,
            PointerHandle,
        },
        Seat, SeatHandler, SeatState,
    },
    output::Output,
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
            shell::server::xdg_toplevel::{self, XdgToplevel},
        },
        wayland_server::{
            protocol::{wl_buffer, wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
            Client, Resource,
        },
    },
    utils::{Logical, Point, Rectangle, Serial, Size, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            add_destruction_hook, get_parent, is_sync_subsurface, with_states,
            CompositorClientState, CompositorHandler, CompositorState,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        drm_lease::{
            DrmLease, DrmLeaseBuilder, DrmLeaseHandler, DrmLeaseRequest, DrmLeaseState,
            LeaseRejected,
        },
        drm_syncobj::{DrmSyncobjHandler, DrmSyncobjState},
        fractional_scale::{self, FractionalScaleHandler},
        idle_inhibit::IdleInhibitHandler,
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
        input_method::{InputMethodHandler, PopupSurface as ImePopupSurface},
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState,
            KeyboardShortcutsInhibitor,
        },
        output::OutputHandler,
        pointer_constraints::{with_pointer_constraint, PointerConstraintsHandler},
        security_context::{
            SecurityContext, SecurityContextHandler, SecurityContextListenerSource,
        },
        session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker},
        selection::{
            data_device::{
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
                ServerDndGrabHandler,
            },
            primary_selection::{
                set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
            },
            SelectionHandler,
        },
        shell::{
            wlr_layer::{
                Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::{
                decoration::XdgDecorationHandler, PopupSurface, PositionerState, ToplevelSurface,
                XdgShellHandler, XdgShellState, XdgToplevelSurfaceData,
            },
        },
        shm::{ShmHandler, ShmState},
        tablet_manager::TabletSeatHandler,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        xdg_foreign::{XdgForeignHandler, XdgForeignState},
        xdg_system_bell::XdgSystemBellHandler,
        xdg_toplevel_icon::XdgToplevelIconHandler,
        xdg_toplevel_tag::XdgToplevelTagHandler,
        xwayland_keyboard_grab::XWaylandKeyboardGrabHandler,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge, X11Window, XwmId},
        X11Surface, X11Wm, XwmHandler,
    },
};
use tracing::{debug, info, warn};

use crate::{
    grabs::{PointerMoveSurfaceGrab, PointerResizeSurfaceGrab},
    state::{ClientState, MochiState},
};

// ============================================================================
// 1. wl_compositor & wl_buffer
// ============================================================================

impl BufferHandler for MochiState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl CompositorHandler for MochiState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<ClientState>() {
            &state.compositor_state
        } else if let Some(xwl) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            &xwl.compositor_state
        } else {
            panic!("Unknown ClientData type on Wayland client");
        }
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        add_destruction_hook(surface, |state: &mut Self, surface| {
            if let CursorImageStatus::Surface(ref cursor_surface) = state.cursor_status {
                if cursor_surface == surface {
                    state.cursor_status = CursorImageStatus::default_named();
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            let window = self
                .wm
                .active_space()
                .elements()
                .find(|w| mochi_wm::window_wl_surface(w).as_ref() == Some(&root))
                .cloned();
            if let Some(window) = window {
                window.on_commit();
                let (app_id, title) = mochi_wm::window_metadata(&window);
                self.wm
                    .apply_window_rules(&window, app_id.as_deref(), title.as_deref());
                if let Some(role) = mochi_wm::is_shoji_shell_window(&window) {
                    let out_geo = self
                        .wm
                        .active_space()
                        .outputs()
                        .next()
                        .and_then(|o| self.wm.active_space().output_geometry(o))
                        .unwrap_or_else(|| {
                            smithay::utils::Rectangle::new(
                                smithay::utils::Point::from((0, 0)),
                                smithay::utils::Size::from((1280, 800)),
                            )
                        });
                    let win_size = window.geometry().size;
                    let target = match role.as_str() {
                        "shoji-wallpaper" | "shoji-bar" => {
                            smithay::utils::Point::from((out_geo.loc.x, out_geo.loc.y))
                        }
                        r if r.starts_with("shoji-dock") => smithay::utils::Point::from((
                            out_geo.loc.x + (out_geo.size.w - win_size.w) / 2,
                            out_geo.loc.y + out_geo.size.h - win_size.h - 12,
                        )),
                        "shoji-launcher" => smithay::utils::Point::from((
                            out_geo.loc.x + (out_geo.size.w - win_size.w) / 2,
                            out_geo.loc.y + 84,
                        )),
                        "shoji-launcher-cat-0"
                        | "shoji-launcher-cat-1"
                        | "shoji-launcher-cat-2"
                        | "shoji-launcher-cat-3"
                        | "shoji-launcher-cat-4"
                        | "shoji-launcher-cat-5" => {
                            let start_x = out_geo.loc.x + (out_geo.size.w - 604) / 2;
                            let dx = match role.as_str() {
                                "shoji-launcher-cat-0" => 0,
                                "shoji-launcher-cat-1" => 86,
                                "shoji-launcher-cat-2" => 194,
                                "shoji-launcher-cat-3" => 290,
                                "shoji-launcher-cat-4" => 432,
                                _ => 518,
                            };
                            smithay::utils::Point::from((start_x + dx, out_geo.loc.y + 146))
                        }
                        "shoji-launcher-recent" => smithay::utils::Point::from((
                            out_geo.loc.x + (out_geo.size.w - win_size.w) / 2,
                            out_geo.loc.y + 186,
                        )),
                        "shoji-launcher-apps" => smithay::utils::Point::from((
                            out_geo.loc.x + (out_geo.size.w - win_size.w) / 2,
                            out_geo.loc.y + 296,
                        )),
                        r if r.starts_with("shoji-launcher-catapps") => {
                            smithay::utils::Point::from((
                                out_geo.loc.x + (out_geo.size.w - win_size.w) / 2,
                                out_geo.loc.y + 186,
                            ))
                        }
                        r if r.starts_with("shoji-control-") => {
                            let (dx, dy) = match r {
                                "shoji-control-conn" => (0, 0),
                                "shoji-control-media" => (200, 0),
                                "shoji-control-sliders" => (0, 148),
                                "shoji-control-actions" => (0, 256),
                                _ => (0, 0),
                            };
                            let cc_w = 388;
                            let cc_h = 324;
                            let mut anchor_cx = out_geo.loc.x + out_geo.size.w - cc_w / 2 - 18;
                            let mut anchor_top = out_geo.loc.y + out_geo.size.h - 66;
                            for w in self.wm.active_space().elements() {
                                if mochi_wm::is_shoji_shell_window(w).as_deref()
                                    == Some("shoji-dock-status")
                                {
                                    if let Some(loc) = self.wm.active_space().element_location(w) {
                                        let sz = w.geometry().size;
                                        anchor_cx = loc.x + sz.w / 2;
                                        anchor_top = loc.y;
                                    }
                                    break;
                                }
                            }
                            let base_x = (anchor_cx - cc_w / 2)
                                .clamp(out_geo.loc.x + 12, out_geo.loc.x + out_geo.size.w - cc_w - 12);
                            let base_y = (anchor_top - cc_h - 12).max(out_geo.loc.y + 12);
                            smithay::utils::Point::from((base_x + dx, base_y + dy))
                        }
                        "shoji-control" => smithay::utils::Point::from((
                            out_geo.loc.x + out_geo.size.w - win_size.w - 18,
                            out_geo.loc.y + out_geo.size.h - win_size.h - 96,
                        )),
                        _ => self
                            .wm
                            .active_space()
                            .element_location(&window)
                            .unwrap_or_default(),
                    };
                    if role == "shoji-wallpaper" && win_size != out_geo.size {
                        if let Some(toplevel) = window.toplevel() {
                            toplevel.with_pending_state(|s| {
                                s.size = Some(out_geo.size);
                            });
                            toplevel.send_pending_configure();
                        }
                    }
                    if role.starts_with("shoji-launcher") {
                        let cur_loc = self.wm.active_space().element_location(&window);
                        let target_f = target.to_f64();
                        let tag: u32 = role
                            .strip_prefix("shoji-launcher-catapps-")
                            .and_then(|s| s.parse::<u32>().ok())
                            .map(|n| 100 + n)
                            .unwrap_or(1);
                        let existing = self
                            .wm
                            .window_animations
                            .get(&window)
                            .map(|a| (a.position.x.target, a.position.y.target, a.anim_tag));
                        if existing != Some((target_f.x, target_f.y, tag)) {
                            if let Some(anim) = self.wm.window_animations.get_mut(&window) {
                                if cur_loc.is_some_and(|loc| (loc.x - target.x).abs() < 40) {
                                    anim.position.set_target(target_f);
                                    anim.size_w.snap_to(win_size.w as f64);
                                    anim.size_h.snap_to(win_size.h as f64);
                                    anim.anim_tag = tag;
                                    anim.bubble = mochi_wm::BubblePhysicsState::new_forming();
                                } else {
                                    let mut new_anim = mochi_wm::WindowAnimationState::new_opening(
                                        target, win_size,
                                    );
                                    new_anim.anim_tag = tag;
                                    *anim = new_anim;
                                    let start_loc = anim.position.current_i32();
                                    self.wm
                                        .active_space_mut()
                                        .map_element(window.clone(), start_loc, false);
                                }
                            } else {
                                let mut anim =
                                    mochi_wm::WindowAnimationState::new_opening(target, win_size);
                                anim.anim_tag = tag;
                                let start_loc = anim.position.current_i32();
                                self.wm.window_animations.insert(window.clone(), anim);
                                self.wm
                                    .active_space_mut()
                                    .map_element(window.clone(), start_loc, false);
                            }
                        }
                    } else {
                        self.wm.window_animations.remove(&window);
                        if self.wm.active_space().element_location(&window) != Some(target) {
                            self.wm
                                .active_space_mut()
                                .map_element(window.clone(), target, false);
                        }
                    }
                    if role.starts_with("shoji-dock") {
                        const DOCK_ROLES: [&str; 5] = [
                            "shoji-dock-launcher",
                            "shoji-dock-pinned",
                            "shoji-dock-running",
                            "shoji-dock-status",
                            "shoji-dock-places",
                        ];
                        let mut dock_wins: Vec<(
                            smithay::desktop::Window,
                            smithay::utils::Size<i32, smithay::utils::Logical>,
                        )> = Vec::new();
                        for dr in DOCK_ROLES {
                            for w in self.wm.active_space().elements() {
                                if mochi_wm::is_shoji_shell_window(w).as_deref() == Some(dr) {
                                    let sz = w.geometry().size;
                                    if sz.w > 0 && sz.h > 0 {
                                        dock_wins.push((w.clone(), sz));
                                    }
                                    break;
                                }
                            }
                        }
                        if !dock_wins.is_empty() {
                            let gap = 12;
                            let total_w: i32 = dock_wins.iter().map(|(_, s)| s.w).sum::<i32>()
                                + gap * (dock_wins.len() as i32 - 1);
                            let mut cur_x = out_geo.loc.x + (out_geo.size.w - total_w) / 2;
                            for (dw, dsz) in dock_wins {
                                let pos = smithay::utils::Point::from((
                                    cur_x,
                                    out_geo.loc.y + out_geo.size.h - dsz.h - 12,
                                ));
                                if self.wm.active_space().element_location(&dw) != Some(pos) {
                                    self.wm.active_space_mut().map_element(dw, pos, false);
                                }
                                cur_x += dsz.w + gap;
                            }
                        }
                    }
                    if role != "shoji-wallpaper" {
                        self.wm.active_space_mut().raise_element(&window, false);
                    }
                    mochi_wm::enforce_shoji_z_order(self.wm.active_space_mut());
                }
            }
        }

        // Refresh wlr-layer-shell layout if a layer surface committed
        let mut layer_committed = false;
        for output in self.wm.active_space().outputs() {
            let mut map = layer_map_for_output(output);
            if let Some(layer) = map.layer_for_surface(surface, smithay::desktop::WindowSurfaceType::TOPLEVEL).cloned() {
                map.arrange();
                layer_committed = true;
                let initial_configure_sent = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<LayerSurfaceData>()
                        .map(|data| data.lock().unwrap().initial_configure_sent)
                        .unwrap_or(true)
                });
                if !initial_configure_sent {
                    layer.layer_surface().send_configure();
                }
            }
        }
        if layer_committed {
            self.wm.retile_active_workspace();
        }

        self.popups.commit(surface);
        ensure_initial_configure(surface, &self.wm, &mut self.popups);
    }
}

delegate_compositor!(MochiState);

// ============================================================================
// 2. wl_shm & linux-dmabuf-v1
// ============================================================================

impl ShmHandler for MochiState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_shm!(MochiState);

impl DmabufHandler for MochiState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let _ = notifier.successful::<Self>();
    }
}

delegate_dmabuf!(MochiState);

impl DrmSyncobjHandler for MochiState {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.drm_syncobj_state.as_mut()
    }
}
delegate_drm_syncobj!(MochiState);

// ============================================================================
// 3. wp_viewporter & wp_fractional_scale_v1
// ============================================================================

delegate_viewporter!(MochiState);

impl FractionalScaleHandler for MochiState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self
            .wm
            .active_space()
            .outputs()
            .next()
            .map(|o| o.current_scale().fractional_scale())
            .unwrap_or(1.0);
        with_states(&surface, |states| {
            fractional_scale::with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(scale);
            });
        });
    }
}

delegate_fractional_scale!(MochiState);
delegate_presentation!(MochiState);

// ============================================================================
// 4. wl_seat & wl_data_device & zwp_primary_selection_v1 (Clipboard / Middle-Click Paste / DnD)
// ============================================================================

impl SeatHandler for MochiState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}

delegate_seat!(MochiState);

impl SelectionHandler for MochiState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for MochiState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for MochiState {}
impl ServerDndGrabHandler for MochiState {}

delegate_data_device!(MochiState);

impl PrimarySelectionHandler for MochiState {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

delegate_primary_selection!(MochiState);

// ============================================================================
// 5. wl_output
// ============================================================================

impl OutputHandler for MochiState {}
delegate_output!(MochiState);

// ============================================================================
// 6. xdg_shell, xdg_decoration, & xdg_activation_v1
// ============================================================================

impl XdgActivationHandler for MochiState {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if !self.session_locked && token_data.timestamp.elapsed().as_secs() < 15 {
            if let Some(window) = self.wm.activate_surface(&surface) {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    let wl_surf = mochi_wm::window_wl_surface(&window);
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(self, wl_surf, serial);
                }
                info!("Activated window via xdg_activation_v1 token");
            }
        }
        self.xdg_activation_state.remove_token(&token);
    }
}

delegate_xdg_activation!(MochiState);

impl XdgDecorationHandler for MochiState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ClientSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_pending_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ClientSide);
        });
        toplevel.send_pending_configure();
    }
}

delegate_xdg_decoration!(MochiState);

impl XdgShellHandler for MochiState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());
        let pos = self.wm.map_new_window(window.clone(), true);

        let wl_surface = surface.wl_surface().clone();
        if self.session_locked {
            info!(?pos, "Mapped new XDG toplevel behind active session lock (focus withheld)");
            return;
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(wl_surface), serial);
        }

        info!(?pos, "Mapped new XDG toplevel window into active workspace");
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            warn!(?err, "Failed to track new XDG popup");
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let seat = Seat::from_resource(&seat).unwrap();
        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();
            let window = self
                .wm
                .active_space()
                .elements()
                .find(|w| w.toplevel().map(|t| t.wl_surface() == wl_surface).unwrap_or(false))
                .unwrap()
                .clone();
            let initial_window_location = self.wm.active_space().element_location(&window).unwrap();

            let grab = PointerMoveSurfaceGrab {
                start_data,
                window,
                initial_window_location,
            };
            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let seat = Seat::from_resource(&seat).unwrap();
        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();
            let window = self
                .wm
                .active_space()
                .elements()
                .find(|w| w.toplevel().map(|t| t.wl_surface() == wl_surface).unwrap_or(false))
                .unwrap()
                .clone();
            let initial_window_location = self.wm.active_space().element_location(&window).unwrap();
            let initial_window_size = window.geometry().size;

            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });
            surface.send_pending_configure();

            let grab = PointerResizeSurfaceGrab::start(
                start_data,
                window,
                edges.into(),
                Rectangle::new(initial_window_location, initial_window_size),
            );
            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        if self.session_locked {
            return;
        }
        let seat: Seat<MochiState> = Seat::from_resource(&seat).unwrap();
        let kind = PopupKind::Xdg(surface);
        let root = find_popup_root_surface(&kind).ok();

        if let Some(root) = root {
            let ret = self.popups.grab_popup(root, kind, &seat, serial);
            if let Ok(mut grab) = ret {
                if let Some(keyboard) = seat.get_keyboard() {
                    if keyboard.is_grabbed()
                        && !(keyboard.has_grab(serial)
                            || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
                    {
                        grab.ungrab(PopupUngrabStrategy::All);
                        return;
                    }
                    keyboard.set_focus(self, grab.current_grab(), serial);
                    keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
                }
                if let Some(pointer) = seat.get_pointer() {
                    if pointer.is_grabbed()
                        && !(pointer.has_grab(serial)
                            || pointer
                                .has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
                    {
                        grab.ungrab(PopupUngrabStrategy::All);
                        return;
                    }
                    pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
                }
            }
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        let window = self
            .wm
            .active_space()
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == surface.wl_surface()).unwrap_or(false))
            .cloned();
        if let Some(window) = window {
            self.wm.snap_window(&window, SnapZone::Maximize);
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        let window = self
            .wm
            .active_space()
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == surface.wl_surface()).unwrap_or(false))
            .cloned();
        if let Some(window) = window {
            self.wm.snap_window(&window, SnapZone::CenterRestore);
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<WlOutput>,
    ) {
        let window = self
            .wm
            .active_space()
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == surface.wl_surface()).unwrap_or(false))
            .cloned();
        if let Some(window) = window {
            self.wm.toggle_fullscreen(&window);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let window = self
            .wm
            .active_space()
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == surface.wl_surface()).unwrap_or(false))
            .cloned();
        if let Some(window) = window {
            self.wm.toggle_fullscreen(&window);
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        let window = self
            .wm
            .active_space()
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == surface.wl_surface()).unwrap_or(false))
            .cloned();
        if let Some(window) = window {
            if self.wm.minimize_window(&window) && !self.session_locked {
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
            }
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        let mut removed = Vec::new();
        for space in self.wm.all_spaces_mut() {
            let window = space
                .elements()
                .find(|w| w.toplevel().map(|t| t.wl_surface() == wl_surface).unwrap_or(false))
                .cloned();
            if let Some(window) = window {
                space.unmap_elem(&window);
                removed.push(window);
                debug!("Unmapped destroyed XDG toplevel window");
            }
        }
        for win in removed {
            self.wm.remove_window_animation(&win);
        }
    }
}

delegate_xdg_shell!(MochiState);

// ============================================================================
// 7. wlr_layer_shell_v1 (Panels, Docks, Launchers, Wallpapers)
// ============================================================================

impl WlrLayerShellHandler for MochiState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let target_output = output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.wm.active_space().outputs().next().cloned());

        let Some(target_output) = target_output else {
            warn!(%namespace, "No output available to map new wlr_layer_shell surface");
            return;
        };

        let mut map = layer_map_for_output(&target_output);
        let layer_surface = LayerSurface::new(surface, namespace.clone());
        if let Err(err) = map.map_layer(&layer_surface) {
            warn!(?err, %namespace, "Failed to map wlr_layer_shell surface");
        } else {
            info!(%namespace, "Mapped wlr_layer_shell surface onto output");
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let mut unmapped = false;
        for output in self.wm.active_space().outputs() {
            let mut map = layer_map_for_output(output);
            let found = map
                .layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = found {
                map.unmap_layer(&layer);
                unmapped = true;
                debug!("Unmapped destroyed wlr_layer_shell surface");
            }
        }
        if unmapped {
            self.wm.retile_active_workspace();
        }
    }
}

delegate_layer_shell!(MochiState);

// ============================================================================
// 8. ext_session_lock_v1 & ext_idle_notify_v1 (Lockscreens & Power Management)
// ============================================================================

impl SessionLockHandler for MochiState {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // From this point on, the renderer hides every window & layer surface and input is
        // routed exclusively to lock surfaces, so it is safe to confirm the lock immediately.
        self.session_locked = true;
        self.lock_surfaces.clear();

        let serial = SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, None, serial);
        }
        if let Some(pointer) = self.seat.get_pointer() {
            let location = self.pointer_location;
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location,
                    serial,
                    time: self.start_time.elapsed().as_millis() as u32,
                },
            );
            pointer.frame(self);
        }

        confirmation.lock();
        info!("Session locked via ext-session-lock-v1");
    }

    fn unlock(&mut self) {
        self.session_locked = false;
        self.lock_surfaces.clear();

        // Restore keyboard focus to the topmost window of the active workspace.
        let top = self
            .wm
            .active_space()
            .elements()
            .last()
            .and_then(mochi_wm::window_wl_surface);
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, top, SERIAL_COUNTER.next_serial());
        }
        info!("Session unlocked via ext-session-lock-v1");
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = Output::from_resource(&output) else {
            warn!("ext-session-lock surface requested for unknown output");
            return;
        };

        if let Some(geo) = self.wm.active_space().output_geometry(&output) {
            surface.with_pending_state(|states| {
                states.size = Some((geo.size.w.max(1) as u32, geo.size.h.max(1) as u32).into());
            });
        }
        surface.send_configure();

        let wl_surface = surface.wl_surface().clone();
        self.lock_surfaces.retain(|(o, s)| o != &output && s.alive());
        self.lock_surfaces.push((output, surface));

        let needs_focus = self
            .seat
            .get_keyboard()
            .map(|k| k.current_focus().is_none())
            .unwrap_or(false);
        if needs_focus {
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(wl_surface), SERIAL_COUNTER.next_serial());
            }
        }
        debug!("Mapped ext-session-lock surface onto output");
    }
}

delegate_session_lock!(MochiState);

impl IdleNotifierHandler for MochiState {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

delegate_idle_notify!(MochiState);

impl IdleInhibitHandler for MochiState {
    fn inhibit(&mut self, surface: WlSurface) {
        if !self.idle_inhibiting_surfaces.contains(&surface) {
            self.idle_inhibiting_surfaces.push(surface);
        }
        self.idle_notifier_state.set_is_inhibited(true);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibiting_surfaces.retain(|s| s != &surface);
        self.idle_notifier_state
            .set_is_inhibited(!self.idle_inhibiting_surfaces.is_empty());
    }
}

delegate_idle_inhibit!(MochiState);

impl TabletSeatHandler for MochiState {}

delegate_tablet_manager!(MochiState);
delegate_cursor_shape!(MochiState);
delegate_single_pixel_buffer!(MochiState);
delegate_alpha_modifier!(MochiState);
delegate_fifo!(MochiState);
delegate_commit_timing!(MochiState);
delegate_content_type!(MochiState);

impl SecurityContextHandler for MochiState {
    fn context_created(&mut self, source: SecurityContextListenerSource, context: SecurityContext) {
        if let Err(err) = self.loop_handle.insert_source(source, move |client_stream, _, state| {
            let client_state = Arc::new(crate::state::ClientState {
                compositor_state: Default::default(),
                security_context: Some(context.clone()),
            });
            if let Err(err) = state.display_handle.insert_client(client_stream, client_state) {
                warn!(?err, "Failed to insert sandboxed security-context client");
            }
        }) {
            warn!(?err, "Failed to register security-context listener source");
        }
    }
}

delegate_security_context!(MochiState);

impl XdgForeignHandler for MochiState {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.xdg_foreign_state
    }
}

delegate_xdg_foreign!(MochiState);

impl XdgToplevelIconHandler for MochiState {}

delegate_xdg_toplevel_icon!(MochiState);

impl XdgToplevelTagHandler for MochiState {
    fn set_tag(&mut self, toplevel: XdgToplevel, tag: String) {
        debug!(toplevel = ?toplevel.id(), %tag, "Updated xdg_toplevel_tag");
    }

    fn set_description(&mut self, toplevel: XdgToplevel, description: String) {
        debug!(toplevel = ?toplevel.id(), %description, "Updated xdg_toplevel_tag description");
    }
}

delegate_xdg_toplevel_tag!(MochiState);

impl XdgSystemBellHandler for MochiState {
    fn ring(&mut self, surface: Option<WlSurface>) {
        debug!(surface = ?surface.map(|s| s.id()), "XDG system bell rung");
    }
}

delegate_xdg_system_bell!(MochiState);

impl DrmLeaseHandler for MochiState {
    fn drm_lease_state(&mut self, _node: DrmNode) -> &mut DrmLeaseState {
        self.drm_lease_state
            .as_mut()
            .expect("drm_lease_state initialized before lease request")
    }

    fn lease_request(
        &mut self,
        node: DrmNode,
        request: DrmLeaseRequest,
    ) -> Result<DrmLeaseBuilder, LeaseRejected> {
        if let Some(cb) = self.drm_lease_request_cb.as_mut() {
            cb(node, request)
        } else {
            Err(LeaseRejected::default())
        }
    }

    fn new_active_lease(&mut self, _node: DrmNode, lease: DrmLease) {
        self.active_drm_leases.push(lease);
    }

    fn lease_destroyed(&mut self, _node: DrmNode, lease_id: u32) {
        self.active_drm_leases.retain(|lease| lease.id() != lease_id);
    }
}

delegate_drm_lease!(MochiState);

impl KeyboardShortcutsInhibitHandler for MochiState {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }

    fn inhibitor_destroyed(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.inactivate();
    }
}

delegate_keyboard_shortcuts_inhibit!(MochiState);

// ============================================================================
// 9. zwp_pointer_constraints_v1 & zwp_relative_pointer_v1 (Games, Blender, Remote Desktop)
// ============================================================================

impl PointerConstraintsHandler for MochiState {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Activate immediately if the pointer is already over the constraining surface.
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let active = with_pointer_constraint(surface, pointer, |constraint| {
            constraint.map(|c| c.is_active()).unwrap_or(false)
        });
        if !active {
            return;
        }
        // Warp the compositor cursor to the hinted position when the lock is released.
        if let Some((_, origin)) = self
            .surface_under(self.pointer_location)
            .filter(|(s, _)| s == surface)
        {
            let target = origin.to_f64() + location;
            self.pointer_location = target;
            pointer.set_location(target);
        }
    }
}

delegate_pointer_constraints!(MochiState);
delegate_relative_pointer!(MochiState);
delegate_pointer_gestures!(MochiState);

// ============================================================================
// 10. Input Methods & Virtual Keyboards (zwp_text_input_v3 + zwp_input_method_v2 + virtual_keyboard_v1)
// ============================================================================
impl InputMethodHandler for MochiState {
    fn new_popup(&mut self, surface: ImePopupSurface) {
        if let Err(err) = self.popups.track_popup(PopupKind::InputMethod(surface.clone())) {
            tracing::warn!(?err, "Failed to track IME popup surface");
        }
        self.ime_popups.push(surface);
    }

    fn dismiss_popup(&mut self, surface: ImePopupSurface) {
        self.ime_popups.retain(|p| p != &surface);
    }

    fn popup_repositioned(&mut self, _surface: ImePopupSurface) {}

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, Logical> {
        let space = self.wm.active_space();
        if let Some(win) = space
            .elements()
            .find(|w| mochi_wm::window_wl_surface(w).as_ref() == Some(parent))
        {
            let loc = space
                .element_location(win)
                .unwrap_or_else(|| Point::from((0, 0)));
            return Rectangle::new(loc, win.geometry().size);
        }
        for output in space.outputs() {
            let map = layer_map_for_output(output);
            for layer in map.layers() {
                if layer.wl_surface() == parent {
                    if let Some(geo) = map.layer_geometry(layer) {
                        return geo;
                    }
                }
            }
        }
        Rectangle::default()
    }
}

delegate_text_input_manager!(MochiState);
delegate_input_method_manager!(MochiState);
delegate_virtual_keyboard_manager!(MochiState);

impl MochiState {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .wm
            .active_space()
            .elements()
            .find(|w| mochi_wm::window_wl_surface(w).as_ref() == Some(&root))
        else {
            return;
        };

        let Some(output_geo) = self.wm.primary_output_geometry() else {
            return;
        };
        let Some(window_geo) = self.wm.active_space().element_geometry(window) else {
            return;
        };

        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

fn check_grab(
    seat: &Seat<MochiState>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<MochiState>> {
    let pointer = seat.get_pointer()?;
    if !pointer.has_grab(serial) {
        return None;
    }
    let start_data = pointer.grab_start_data()?;
    let (focus, _) = start_data.focus.as_ref()?;
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }
    Some(start_data)
}

fn ensure_initial_configure(
    surface: &WlSurface,
    wm: &mochi_wm::WindowManager,
    popups: &mut smithay::desktop::PopupManager,
) {
    if let Some(window) = wm
        .active_space()
        .elements()
        .find(|w| mochi_wm::window_wl_surface(w).as_ref() == Some(surface))
    {
        if let Some(toplevel) = window.toplevel() {
            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            });
            if !initial_configure_sent {
                toplevel.send_configure();
            }
        }
    }

    if let Some(PopupKind::Xdg(ref popup)) = popups.find_popup(surface) {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgPopupSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        if !initial_configure_sent {
            let _ = popup.send_configure();
        }
    }
}

// ============================================================================
// 10. XWayland Support (smithay::xwayland)
// ============================================================================

impl XWaylandShellHandler for MochiState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm_id: XwmId, wl_surface: WlSurface, surface: X11Surface) {
        debug!(
            wl_surface = ?wl_surface.id().protocol_id(),
            window = surface.window_id(),
            title = %surface.title(),
            class = %surface.class(),
            "Associated X11 surface with Wayland surface"
        );
        if !self.session_locked {
            if let Some(top) = self.wm.active_space().elements().last() {
                if top.x11_surface() == Some(&surface) {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        let serial = SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, Some(wl_surface), serial);
                    }
                }
            }
        }
    }
}

delegate_xwayland_shell!(MochiState);

impl XWaylandKeyboardGrabHandler for MochiState {
    fn keyboard_focus_for_xsurface(&self, surface: &WlSurface) -> Option<Self::KeyboardFocus> {
        Some(surface.clone())
    }
}

delegate_xwayland_keyboard_grab!(MochiState);

impl XwmHandler for MochiState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("invalid or missing X11Wm state")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        debug!(window = window.window_id(), title = %window.title(), class = %window.class(), "New X11 window created");
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        debug!(window = window.window_id(), title = %window.title(), class = %window.class(), "New X11 override-redirect window created");
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Err(err) = window.set_mapped(true) {
            warn!(?err, "Failed to set X11 window mapped");
            return;
        }

        let win = Window::new_x11_window(window.clone());
        let pos = self.wm.map_new_window(win.clone(), true);

        let (app_id, title) = mochi_wm::window_metadata(&win);
        self.wm
            .apply_window_rules(&win, app_id.as_deref(), title.as_deref());

        if !self.session_locked {
            if let Some(keyboard) = self.seat.get_keyboard() {
                if let Some(wl_surface) = window.wl_surface() {
                    let serial = SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(self, Some(wl_surface), serial);
                }
            }
        }

        info!(?pos, title = %window.title(), class = %window.class(), "Mapped new X11 window into active workspace");
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let win = Window::new_x11_window(window.clone());
        let loc = window.geometry().loc;
        self.wm.active_space_mut().map_element(win, loc, true);
        debug!(?loc, title = %window.title(), "Mapped X11 override-redirect window");
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let mut removed = Vec::new();
        for space in self.wm.all_spaces_mut() {
            let found = space
                .elements()
                .find(|w| w.x11_surface().map(|s| s == &window).unwrap_or(false))
                .cloned();
            if let Some(w) = found {
                space.unmap_elem(&w);
                removed.push(w);
                debug!(title = %window.title(), "Unmapped X11 window");
            }
        }
        for win in removed {
            self.wm.remove_window_animation(&win);
        }
    }

    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        self.unmapped_window(xwm, window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let existing = self.wm.find_window_by_x11(&window);

        let current_geo = window.geometry();
        let new_loc = Point::from((
            x.unwrap_or(current_geo.loc.x),
            y.unwrap_or(current_geo.loc.y),
        ));
        let new_size = Size::from((
            w.unwrap_or(current_geo.size.w.max(1) as u32) as i32,
            h.unwrap_or(current_geo.size.h.max(1) as u32) as i32,
        ));
        let target_rect = Rectangle::new(new_loc, new_size);

        if let Some(win) = existing {
            if self.wm.active_layout() == mochi_wm::LayoutMode::MasterStack {
                if let Some(tiled_geo) = self.wm.active_space().element_geometry(&win) {
                    let _ = window.configure(Some(tiled_geo));
                    return;
                }
            }
            let _ = window.configure(Some(target_rect));
            self.wm.active_space_mut().map_element(win, target_rect.loc, false);
        } else {
            let _ = window.configure(Some(target_rect));
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<X11Window>,
    ) {
        if let Some(win) = self.wm.find_window_by_x11(&window) {
            if self.wm.active_layout() != mochi_wm::LayoutMode::MasterStack {
                self.wm.active_space_mut().map_element(win, geometry.loc, false);
            }
        }
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        button: u32,
        resize_edge: ResizeEdge,
    ) {
        if let Some(win) = self.wm.find_window_by_x11(&window) {
            let wl_surf = window.wl_surface();
            let win_loc = self.wm.active_space().element_location(&win).unwrap_or_default();
            let start_data = smithay::input::pointer::GrabStartData {
                focus: wl_surf.map(|s| (s, win_loc.to_f64())),
                button,
                location: self.pointer_location,
            };
            let edges = match resize_edge {
                ResizeEdge::Top => crate::grabs::ResizeEdge::TOP,
                ResizeEdge::Bottom => crate::grabs::ResizeEdge::BOTTOM,
                ResizeEdge::Left => crate::grabs::ResizeEdge::LEFT,
                ResizeEdge::Right => crate::grabs::ResizeEdge::RIGHT,
                ResizeEdge::TopLeft => crate::grabs::ResizeEdge::TOP_LEFT,
                ResizeEdge::TopRight => crate::grabs::ResizeEdge::TOP_RIGHT,
                ResizeEdge::BottomLeft => crate::grabs::ResizeEdge::BOTTOM_LEFT,
                ResizeEdge::BottomRight => crate::grabs::ResizeEdge::BOTTOM_RIGHT,
            };
            let initial_rect = Rectangle::new(win_loc, win.geometry().size);
            let grab = crate::grabs::PointerResizeSurfaceGrab::start(
                start_data,
                win,
                edges,
                initial_rect,
            );
            if let Some(pointer) = self.seat.get_pointer() {
                pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), smithay::input::pointer::Focus::Clear);
            }
        }
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, button: u32) {
        if let Some(win) = self.wm.find_window_by_x11(&window) {
            let wl_surf = window.wl_surface();
            let win_loc = self.wm.active_space().element_location(&win).unwrap_or_default();
            let start_data = smithay::input::pointer::GrabStartData {
                focus: wl_surf.map(|s| (s, win_loc.to_f64())),
                button,
                location: self.pointer_location,
            };
            let grab = crate::grabs::PointerMoveSurfaceGrab {
                start_data,
                window: win,
                initial_window_location: win_loc,
            };
            if let Some(pointer) = self.seat.get_pointer() {
                pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), smithay::input::pointer::Focus::Clear);
            }
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, _property: smithay::xwayland::xwm::WmWindowProperty) {
        if let Some(win) = self.wm.find_window_by_x11(&window) {
            let class = window.class();
            let title = window.title();
            self.wm.apply_window_rules(&win, Some(&class), Some(&title));
        }
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(win) = self.wm.find_window_by_x11(&window) {
            self.wm.snap_window(&win, mochi_wm::SnapZone::Maximize);
        }
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(win) = self.wm.find_window_by_x11(&window) {
            self.wm.snap_window(&win, mochi_wm::SnapZone::CenterRestore);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(win) = self.wm.find_window_by_x11(&window) {
            self.wm.toggle_fullscreen(&win);
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(win) = self.wm.find_window_by_x11(&window) {
            self.wm.toggle_fullscreen(&win);
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
        self.x11_display = None;
        warn!("XWayland Window Manager disconnected");
    }
}

pub type _UnusedAsFd<'a> = &'a dyn AsFd;
