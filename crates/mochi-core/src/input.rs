// SPDX-License-Identifier: GPL-3.0-or-later
//! Input Event Filter Pipeline & Keybinding Dispatch

use std::process::Command;

use mochi_wm::{hit_test_hud_pill, hit_test_titlebar, HudPillHit, TitlebarButton, TitlebarHit};
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device, DeviceCapability, Event,
        GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent, GestureSwipeUpdateEvent,
        InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent,
        PointerButtonEvent, PointerMotionEvent, ProximityState, TabletToolButtonEvent,
        TabletToolEvent, TabletToolProximityEvent, TabletToolTipEvent, TabletToolTipState,
        TouchEvent,
    },
    desktop::Window,
    input::{
        keyboard::{keysyms, FilterResult, ModifiersState},
        pointer::{
            AxisFrame, ButtonEvent, Focus, GestureHoldBeginEvent as PointerGestureHoldBeginEvent,
            GestureHoldEndEvent as PointerGestureHoldEndEvent,
            GesturePinchBeginEvent as PointerGesturePinchBeginEvent,
            GesturePinchEndEvent as PointerGesturePinchEndEvent,
            GesturePinchUpdateEvent as PointerGesturePinchUpdateEvent,
            GestureSwipeBeginEvent as PointerGestureSwipeBeginEvent,
            GestureSwipeEndEvent as PointerGestureSwipeEndEvent,
            GestureSwipeUpdateEvent as PointerGestureSwipeUpdateEvent,
            GrabStartData as PointerGrabStartData, MotionEvent, RelativeMotionEvent,
        },
        touch::{
            DownEvent as TouchDownData, MotionEvent as TouchMotionData, UpEvent as TouchUpData,
        },
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Rectangle, SERIAL_COUNTER},
    wayland::{
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat,
        pointer_constraints::{with_pointer_constraint, PointerConstraint},
        tablet_manager::{TabletDescriptor, TabletSeatTrait},
    },
};
use tracing::{info, warn};

use crate::{
    config::{KeyAction, KeyBinding},
    grabs::{PointerMoveSurfaceGrab, PointerResizeSurfaceGrab, ResizeEdge},
    state::MochiState,
};

impl MochiState {
    /// Processes any `InputEvent` from a nested (`winit`) or hardware (`libinput`) backend.
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // ext-idle-notify-v1: any user input resets idle timers (screen blanking, auto-lock)
        if !matches!(
            event,
            InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
        ) {
            self.idle_notifier_state.notify_activity(&self.seat);
        }

        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let press_state = event.state();

                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };

                // While locked, make sure keystrokes land on a lock surface (never a window).
                if self.session_locked && keyboard.current_focus().is_none() {
                    if let Some(lock_surface) = self.preferred_lock_surface() {
                        keyboard.set_focus(self, Some(lock_surface), serial);
                    }
                }

                let shortcuts_inhibited = keyboard
                    .current_focus()
                    .and_then(|surface| self.seat.keyboard_shortcuts_inhibitor_for_surface(&surface))
                    .is_some_and(|inhibitor| inhibitor.is_active());

                let action = keyboard.input::<Option<KeyAction>, _>(
                    self,
                    event.key_code(),
                    press_state,
                    serial,
                    time,
                    |state, modifiers, handle| {
                        // Compositor keybindings are disabled while the session is locked
                        // or when the focused surface holds an active shortcut inhibitor.
                        if press_state == KeyState::Pressed
                            && !state.session_locked
                            && !shortcuts_inhibited
                        {
                            let sym = handle.modified_sym();
                            if let Some(act) = evaluate_keybinding(
                                &state.keybindings,
                                state.wm.overview_active,
                                modifiers,
                                sym.raw(),
                            ) {
                                return FilterResult::Intercept(Some(act));
                            }
                        }
                        FilterResult::Forward
                    },
                );

                if let Some(Some(action)) = action {
                    self.execute_key_action(action);
                }
            }

            InputEvent::PointerMotion { event, .. } => {
                let Some(output_geo) = self.wm.bounding_outputs_geometry() else {
                    return;
                };
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let delta = event.delta();

                // zwp_pointer_constraints_v1: inspect constraint on the surface under the pointer
                let under = self
                    .surface_under(self.pointer_location)
                    .map(|(s, loc)| (s, loc.to_f64()));
                let mut pointer_locked = false;
                let mut pointer_confined = false;
                let mut confine_region = None;
                if let Some((surface, surface_loc)) = under.as_ref() {
                    let local = (self.pointer_location - *surface_loc).to_i32_round();
                    with_pointer_constraint(surface, &pointer, |constraint| match constraint {
                        Some(constraint) if constraint.is_active() => {
                            if !constraint.region().map_or(true, |r| r.contains(local)) {
                                return;
                            }
                            match &*constraint {
                                PointerConstraint::Locked(_) => pointer_locked = true,
                                PointerConstraint::Confined(confine) => {
                                    pointer_confined = true;
                                    confine_region = confine.region().cloned();
                                }
                            }
                        }
                        _ => {}
                    });
                }

                // zwp_relative_pointer_v1: raw (accelerated + unaccelerated) motion deltas
                pointer.relative_motion(
                    self,
                    under.clone(),
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: event.delta_unaccel(),
                        utime: Event::time(&event),
                    },
                );

                if pointer_locked {
                    pointer.frame(self);
                    return;
                }

                let min_x = output_geo.loc.x as f64;
                let min_y = output_geo.loc.y as f64;
                let max_x = (output_geo.loc.x + output_geo.size.w - 1).max(0) as f64;
                let max_y = (output_geo.loc.y + output_geo.size.h - 1).max(0) as f64;
                let mut pos = self.pointer_location + delta;
                pos.x = pos.x.clamp(min_x, max_x);
                pos.y = pos.y.clamp(min_y, max_y);

                let new_under = self.surface_under(pos).map(|(s, loc)| (s, loc.to_f64()));

                if pointer_confined {
                    if let Some((surface, surface_loc)) = under.as_ref() {
                        let same_surface =
                            new_under.as_ref().map(|(s, _)| s == surface).unwrap_or(false);
                        let in_region = confine_region
                            .as_ref()
                            .map_or(true, |r| r.contains((pos - *surface_loc).to_i32_round()));
                        if !same_surface || !in_region {
                            pointer.frame(self);
                            return;
                        }
                    }
                }

                self.pointer_location = pos;
                pointer.motion(
                    self,
                    new_under.clone(),
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);

                // Activate a pending constraint once the pointer enters its region
                if let Some((surface, surface_loc)) = new_under {
                    let local = (pos - surface_loc).to_i32_round();
                    with_pointer_constraint(&surface, &pointer, |constraint| match constraint {
                        Some(constraint) if !constraint.is_active() => {
                            if constraint.region().map_or(true, |r| r.contains(local)) {
                                constraint.activate();
                            }
                        }
                        _ => {}
                    });
                }
            }

            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output_geo) = self.wm.primary_output_geometry() else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_location = pos;

                let serial = SERIAL_COUNTER.next_serial();
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let under = self.surface_under(pos).map(|(s, loc)| (s, loc.to_f64()));

                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }

            InputEvent::PointerButton { event, .. } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };

                let serial = SERIAL_COUNTER.next_serial();
                let button = event.button_code();
                let button_state = event.state();

                // While locked, clicks only focus/feed the lock surface; no WM interaction.
                if self.session_locked {
                    if button_state == ButtonState::Pressed {
                        if let Some((surface, _)) = self.surface_under(self.pointer_location) {
                            keyboard.set_focus(self, Some(surface), serial);
                        }
                    }
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state: button_state,
                            serial,
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(self);
                    return;
                }

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    let modifiers = keyboard.modifier_state();
                    let mod_down = modifiers.logo || modifiers.alt;
                    const BTN_LEFT: u32 = 0x110;
                    const BTN_RIGHT: u32 = 0x111;

                    // 0. Check if Left-Click hit the On-Screen Glass HUD Pill
                    if button == BTN_LEFT {
                        if let Some(output_geo) = self.wm.output_geometry_under(self.pointer_location) {
                            let hud_prog = self.wm.hud_spring.current as f32;
                            let ws_count = self.wm.workspace_count();
                            if let Some(hud_hit) =
                                hit_test_hud_pill(output_geo, hud_prog, ws_count, self.pointer_location)
                            {
                                match hud_hit {
                                    HudPillHit::Workspace(ws_idx) => {
                                        if self.wm.switch_workspace(ws_idx) {
                                            self.focus_top_window_in_active_workspace();
                                        }
                                    }
                                    HudPillHit::ToggleLayout => {
                                        self.wm.toggle_active_layout();
                                    }
                                    HudPillHit::SelectStyle(style) => {
                                        self.wm.set_visual_style(style);
                                    }
                                }
                                return;
                            }
                        }
                    }

                    // 1. Check if Left-Click hit any window's Server-Side Glass Titlebar
                    let tb_h = self.wm.visual_style.titlebar_height;
                    if button == BTN_LEFT && tb_h > 0 {
                        let titlebar_target = self
                            .wm
                            .active_space()
                            .elements()
                            .rev()
                            .find_map(|w| {
                                if mochi_wm::is_shoji_shell_window(w).is_some() {
                                    return None;
                                }
                                let loc = self.wm.active_space().element_location(w)?;
                                let width = w.geometry().size.w;
                                let hit =
                                    hit_test_titlebar(loc, width, tb_h, self.pointer_location)?;
                                Some((w.clone(), loc, hit))
                            });

                        if let Some((window, window_loc, hit)) = titlebar_target {
                            self.wm.active_space_mut().raise_element(&window, true);
                            mochi_wm::enforce_shoji_z_order(self.wm.active_space_mut());
                            let wl_surface = mochi_wm::window_wl_surface(&window);
                            keyboard.set_focus(self, wl_surface.clone(), serial);

                            for w in self.wm.active_space().elements() {
                                w.set_activated(w == &window);
                                mochi_wm::send_window_configure(w);
                            }

                            match hit {
                                TitlebarHit::Button(TitlebarButton::Close) => {
                                    mochi_wm::send_window_close(&window);
                                    info!("Closed window via glass titlebar Close button");
                                    return;
                                }
                                TitlebarHit::Button(TitlebarButton::Maximize) => {
                                    self.wm.toggle_maximize(&window);
                                    return;
                                }
                                TitlebarHit::Button(TitlebarButton::TileToggle) => {
                                    self.wm.toggle_active_layout();
                                    return;
                                }
                                TitlebarHit::DragBar => {
                                    let start_data = PointerGrabStartData {
                                        focus: wl_surface.map(|s| (s, window_loc.to_f64())),
                                        button,
                                        location: self.pointer_location,
                                    };
                                    let grab = PointerMoveSurfaceGrab {
                                        start_data,
                                        window,
                                        initial_window_location: window_loc,
                                    };
                                    pointer.set_grab(self, grab, serial, Focus::Clear);
                                    return;
                                }
                            }
                        }
                    }

                    // 2. Check if click hit a window's client surface
                    if let Some((window, window_loc)) = self
                        .wm
                        .active_space()
                        .element_under(self.pointer_location)
                        .map(|(w, l)| (w.clone(), l))
                    {
                        let wl_surface = mochi_wm::window_wl_surface(&window);
                        let is_shoji_shell = mochi_wm::is_shoji_shell_window(&window).is_some();
                        if mochi_wm::is_shoji_wallpaper(&window) {
                            // Never raise, focus, or drag the desktop wallpaper; still forward pointer events below.
                        } else if is_shoji_shell {
                            self.wm.active_space_mut().raise_element(&window, true);
                            mochi_wm::enforce_shoji_z_order(self.wm.active_space_mut());
                            if mochi_wm::shoji_wants_keyboard_focus(&window) {
                                keyboard.set_focus(self, wl_surface.clone(), serial);
                            }
                        } else {
                            // Raise window to top and focus it while keeping shell chrome above
                            self.wm.active_space_mut().raise_element(&window, true);
                            mochi_wm::enforce_shoji_z_order(self.wm.active_space_mut());
                            keyboard.set_focus(self, wl_surface.clone(), serial);

                            for w in self.wm.active_space().elements() {
                                w.set_activated(w == &window);
                                mochi_wm::send_window_configure(w);
                            }
                        }

                        // Clicking a window while Expose / Overview is active focuses it and zooms back in
                        if self.wm.overview_active && button == BTN_LEFT {
                            self.wm.exit_overview();
                            return;
                        }

                        // Mod + Left Mouse Button drags window directly from anywhere
                        if !is_shoji_shell && mod_down && button == BTN_LEFT {
                            let start_data = PointerGrabStartData {
                                focus: wl_surface.map(|s| (s, window_loc.to_f64())),
                                button,
                                location: self.pointer_location,
                            };
                            let grab = PointerMoveSurfaceGrab {
                                start_data,
                                window,
                                initial_window_location: window_loc,
                            };
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                            return;
                        }

                        // Mod + Right Mouse Button resizes window directly from nearest quadrant
                        if !is_shoji_shell && mod_down && button == BTN_RIGHT {
                            let win_size = window.geometry().size;
                            let initial_rect = Rectangle::new(window_loc, win_size);
                            let edges = ResizeEdge::from_quadrant(self.pointer_location, initial_rect);

                            if let Some(toplevel) = window.toplevel() {
                                toplevel.with_pending_state(|state| {
                                    state.states.set(xdg_toplevel::State::Resizing);
                                });
                                toplevel.send_pending_configure();
                            } else if let Some(x11) = window.x11_surface() {
                                let _ = x11.configure(Some(initial_rect));
                            }

                            let start_data = PointerGrabStartData {
                                focus: wl_surface.map(|s| (s, window_loc.to_f64())),
                                button,
                                location: self.pointer_location,
                            };
                            let grab = PointerResizeSurfaceGrab::start(
                                start_data,
                                window,
                                edges,
                                initial_rect,
                            );
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                            return;
                        }
                    } else {
                        self.wm.active_space_mut().elements().for_each(|w| {
                            w.set_activated(false);
                            mochi_wm::send_window_configure(w);
                        });
                        keyboard.set_focus(self, None, serial);
                    }
                }

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }

            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0
                });
                let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0
                });
                let horizontal_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }
                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                if let Some(pointer) = self.seat.get_pointer() {
                    pointer.axis(self, frame);
                    pointer.frame(self);
                }
            }

            // ----------------------------------------------------------------
            // Touchpad Swipe Gestures (3-Finger Workspace Navigation & Forwarding)
            // ----------------------------------------------------------------
            InputEvent::GestureSwipeBegin { event, .. } => {
                if self.session_locked {
                    return;
                }
                let fingers = event.fingers();
                if fingers == 3 || fingers == 4 {
                    self.active_gesture.swipe_active = true;
                    self.active_gesture.swipe_initial_workspace = self.wm.active_workspace_index();
                    self.active_gesture.swipe_accumulated_dx = 0.0;
                    self.wm.trigger_hud();
                } else if let Some(pointer) = self.seat.get_pointer() {
                    let serial = SERIAL_COUNTER.next_serial();
                    pointer.gesture_swipe_begin(
                        self,
                        &PointerGestureSwipeBeginEvent {
                            serial,
                            time: event.time_msec(),
                            fingers,
                        },
                    );
                }
            }

            InputEvent::GestureSwipeUpdate { event, .. } => {
                if self.session_locked {
                    return;
                }
                if self.active_gesture.swipe_active {
                    let dx = event.delta_x();
                    self.active_gesture.swipe_accumulated_dx += dx;
                    let offset = self.active_gesture.swipe_accumulated_dx;
                    self.wm.workspace_slide_x.current = offset;
                    self.wm.workspace_slide_x.set_target(offset);
                    self.wm.trigger_hud();
                } else if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_swipe_update(
                        self,
                        &PointerGestureSwipeUpdateEvent {
                            time: event.time_msec(),
                            delta: event.delta(),
                        },
                    );
                }
            }

            InputEvent::GestureSwipeEnd { event, .. } => {
                if self.session_locked {
                    return;
                }
                if self.active_gesture.swipe_active {
                    self.active_gesture.swipe_active = false;
                    let dx = self.active_gesture.swipe_accumulated_dx;
                    self.active_gesture.swipe_accumulated_dx = 0.0;

                    if event.cancelled() {
                        self.wm.workspace_slide_x.set_target(0.0);
                    } else {
                        const SWIPE_THRESHOLD: f64 = 140.0;
                        let cur_ws = self.wm.active_workspace_index();
                        if dx > SWIPE_THRESHOLD {
                            // Swiped right: reveal previous workspace
                            if cur_ws > 0 {
                                if self.wm.switch_workspace(cur_ws - 1) {
                                    self.focus_top_window_in_active_workspace();
                                }
                            } else {
                                self.wm.workspace_slide_x.set_target(0.0);
                            }
                        } else if dx < -SWIPE_THRESHOLD {
                            // Swiped left: reveal next workspace
                            if cur_ws + 1 < self.wm.workspace_count() {
                                if self.wm.switch_workspace(cur_ws + 1) {
                                    self.focus_top_window_in_active_workspace();
                                }
                            } else {
                                self.wm.workspace_slide_x.set_target(0.0);
                            }
                        } else {
                            self.wm.workspace_slide_x.set_target(0.0);
                        }
                    }
                } else if let Some(pointer) = self.seat.get_pointer() {
                    let serial = SERIAL_COUNTER.next_serial();
                    pointer.gesture_swipe_end(
                        self,
                        &PointerGestureSwipeEndEvent {
                            serial,
                            time: event.time_msec(),
                            cancelled: event.cancelled(),
                        },
                    );
                }
            }

            // ----------------------------------------------------------------
            // Touchpad Pinch Gestures (Expose / Window Overview & Forwarding)
            // ----------------------------------------------------------------
            InputEvent::GesturePinchBegin { event, .. } => {
                if self.session_locked {
                    return;
                }
                let fingers = event.fingers();
                if fingers >= 3 {
                    self.active_gesture.pinch_active = true;
                    self.active_gesture.pinch_initial_scale = 1.0;
                    self.active_gesture.pinch_started_in_overview = self.wm.overview_active;
                    self.wm.begin_overview_pinch();
                } else if let Some(pointer) = self.seat.get_pointer() {
                    let serial = SERIAL_COUNTER.next_serial();
                    pointer.gesture_pinch_begin(
                        self,
                        &PointerGesturePinchBeginEvent {
                            serial,
                            time: event.time_msec(),
                            fingers,
                        },
                    );
                }
            }

            InputEvent::GesturePinchUpdate { event, .. } => {
                if self.session_locked {
                    return;
                }
                if self.active_gesture.pinch_active {
                    let scale = event.scale();
                    let progress = if !self.active_gesture.pinch_started_in_overview {
                        ((1.0 - scale) / 0.40).clamp(0.0, 1.0)
                    } else {
                        (1.0 - (scale - 1.0) / 0.55).clamp(0.0, 1.0)
                    };
                    self.wm.scrub_overview_pinch(progress);
                } else if let Some(pointer) = self.seat.get_pointer() {
                    pointer.gesture_pinch_update(
                        self,
                        &PointerGesturePinchUpdateEvent {
                            time: event.time_msec(),
                            delta: event.delta(),
                            scale: event.scale(),
                            rotation: event.rotation(),
                        },
                    );
                }
            }

            InputEvent::GesturePinchEnd { event, .. } => {
                if self.session_locked {
                    return;
                }
                if self.active_gesture.pinch_active {
                    self.active_gesture.pinch_active = false;
                    let commit = if event.cancelled() {
                        self.active_gesture.pinch_started_in_overview
                    } else {
                        self.wm.overview_progress.current >= 0.45
                    };
                    self.wm.finish_overview_pinch(commit);
                } else if let Some(pointer) = self.seat.get_pointer() {
                    let serial = SERIAL_COUNTER.next_serial();
                    pointer.gesture_pinch_end(
                        self,
                        &PointerGesturePinchEndEvent {
                            serial,
                            time: event.time_msec(),
                            cancelled: event.cancelled(),
                        },
                    );
                }
            }

            // ----------------------------------------------------------------
            // Touchpad Hold Gestures (Client Forwarding)
            // ----------------------------------------------------------------
            InputEvent::GestureHoldBegin { event, .. } => {
                if self.session_locked {
                    return;
                }
                if let Some(pointer) = self.seat.get_pointer() {
                    let serial = SERIAL_COUNTER.next_serial();
                    pointer.gesture_hold_begin(
                        self,
                        &PointerGestureHoldBeginEvent {
                            serial,
                            time: event.time_msec(),
                            fingers: event.fingers(),
                        },
                    );
                }
            }

            InputEvent::GestureHoldEnd { event, .. } => {
                if self.session_locked {
                    return;
                }
                if let Some(pointer) = self.seat.get_pointer() {
                    let serial = SERIAL_COUNTER.next_serial();
                    pointer.gesture_hold_end(
                        self,
                        &PointerGestureHoldEndEvent {
                            serial,
                            time: event.time_msec(),
                            cancelled: event.cancelled(),
                        },
                    );
                }
            }

            // ----------------------------------------------------------------
            // Touchscreen Multi-Touch Events (wl_touch + HUD & Titlebar Taps)
            // ----------------------------------------------------------------
            InputEvent::TouchDown { event, .. } => {
                let Some(output_geo) = self.wm.primary_output_geometry() else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_location = pos;
                let serial = SERIAL_COUNTER.next_serial();

                if self.session_locked {
                    if let Some((surface, _)) = self.surface_under(pos) {
                        if let Some(keyboard) = self.seat.get_keyboard() {
                            keyboard.set_focus(self, Some(surface), serial);
                        }
                    }
                    if let Some(touch) = self.seat.get_touch() {
                        let under = self.surface_under(pos).map(|(s, loc)| (s, loc.to_f64()));
                        touch.down(
                            self,
                            under,
                            &TouchDownData {
                                slot: event.slot(),
                                location: pos,
                                serial,
                                time: event.time_msec(),
                            },
                        );
                    }
                    return;
                }

                // 1. Check if tap hit the On-Screen Glass HUD Pill
                if let Some(output_geo) = self.wm.output_geometry_under(pos) {
                    let hud_prog = self.wm.hud_spring.current as f32;
                    let ws_count = self.wm.workspace_count();
                    if let Some(hud_hit) = hit_test_hud_pill(output_geo, hud_prog, ws_count, pos) {
                        match hud_hit {
                            HudPillHit::Workspace(ws_idx) => {
                                if self.wm.switch_workspace(ws_idx) {
                                    self.focus_top_window_in_active_workspace();
                                }
                            }
                            HudPillHit::ToggleLayout => {
                                self.wm.toggle_active_layout();
                            }
                            HudPillHit::SelectStyle(style) => {
                                self.wm.set_visual_style(style);
                            }
                        }
                        return;
                    }
                }

                // 2. Check if tap hit any window's Server-Side Glass Titlebar buttons
                let tb_h = self.wm.visual_style.titlebar_height;
                if tb_h > 0 {
                    let titlebar_target = self
                        .wm
                        .active_space()
                        .elements()
                        .rev()
                        .find_map(|w| {
                            if mochi_wm::is_shoji_shell_window(w).is_some() {
                                return None;
                            }
                            let loc = self.wm.active_space().element_location(w)?;
                            let width = w.geometry().size.w;
                            let hit = hit_test_titlebar(loc, width, tb_h, pos)?;
                            Some((w.clone(), loc, hit))
                        });

                    if let Some((window, _window_loc, hit)) = titlebar_target {
                        self.wm.active_space_mut().raise_element(&window, true);
                        mochi_wm::enforce_shoji_z_order(self.wm.active_space_mut());
                        let wl_surface = mochi_wm::window_wl_surface(&window);
                        if let Some(keyboard) = self.seat.get_keyboard() {
                            keyboard.set_focus(self, wl_surface.clone(), serial);
                        }
                        for w in self.wm.active_space().elements() {
                            w.set_activated(w == &window);
                            mochi_wm::send_window_configure(w);
                        }

                        match hit {
                            TitlebarHit::Button(TitlebarButton::Close) => {
                                mochi_wm::send_window_close(&window);
                                return;
                            }
                            TitlebarHit::Button(TitlebarButton::Maximize) => {
                                self.wm.toggle_maximize(&window);
                                return;
                            }
                            TitlebarHit::Button(TitlebarButton::TileToggle) => {
                                self.wm.toggle_active_layout();
                                return;
                            }
                            TitlebarHit::DragBar => {
                                // Tap on drag bar focuses and raises window
                            }
                        }
                    }
                }

                // 3. Focus window under touch point
                if let Some((window, _loc)) = self
                    .wm
                    .active_space()
                    .element_under(pos)
                    .map(|(w, l)| (w.clone(), l))
                {
                    if mochi_wm::is_shoji_wallpaper(&window) {
                        // Never raise or focus the desktop wallpaper.
                    } else if mochi_wm::is_shoji_shell_window(&window).is_some() {
                        self.wm.active_space_mut().raise_element(&window, true);
                        mochi_wm::enforce_shoji_z_order(self.wm.active_space_mut());
                        if mochi_wm::shoji_wants_keyboard_focus(&window) {
                            let wl_surface = mochi_wm::window_wl_surface(&window);
                            if let Some(keyboard) = self.seat.get_keyboard() {
                                keyboard.set_focus(self, wl_surface, serial);
                            }
                        }
                    } else {
                        self.wm.active_space_mut().raise_element(&window, true);
                        mochi_wm::enforce_shoji_z_order(self.wm.active_space_mut());
                        let wl_surface = mochi_wm::window_wl_surface(&window);
                        if let Some(keyboard) = self.seat.get_keyboard() {
                            keyboard.set_focus(self, wl_surface, serial);
                        }
                        for w in self.wm.active_space().elements() {
                            w.set_activated(w == &window);
                            mochi_wm::send_window_configure(w);
                        }
                    }
                }

                // 4. Forward touch down to Wayland client
                if let Some(touch) = self.seat.get_touch() {
                    let under = self.surface_under(pos).map(|(s, loc)| (s, loc.to_f64()));
                    touch.down(
                        self,
                        under,
                        &TouchDownData {
                            slot: event.slot(),
                            location: pos,
                            serial,
                            time: event.time_msec(),
                        },
                    );
                }
            }

            InputEvent::TouchMotion { event, .. } => {
                let Some(output_geo) = self.wm.primary_output_geometry() else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_location = pos;

                if let Some(touch) = self.seat.get_touch() {
                    let under = self.surface_under(pos).map(|(s, loc)| (s, loc.to_f64()));
                    touch.motion(
                        self,
                        under,
                        &TouchMotionData {
                            slot: event.slot(),
                            location: pos,
                            time: event.time_msec(),
                        },
                    );
                }
            }

            InputEvent::TouchUp { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                if let Some(touch) = self.seat.get_touch() {
                    touch.up(
                        self,
                        &TouchUpData {
                            slot: event.slot(),
                            serial,
                            time: event.time_msec(),
                        },
                    );
                }
            }

            InputEvent::TouchFrame { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.frame(self);
                }
            }

            InputEvent::TouchCancel { .. } => {
                if let Some(touch) = self.seat.get_touch() {
                    touch.cancel(self);
                }
            }

            InputEvent::DeviceAdded { device } => {
                if device.has_capability(DeviceCapability::TabletTool) {
                    self.seat
                        .tablet_seat()
                        .add_tablet::<Self>(&self.display_handle, &TabletDescriptor::from(&device));
                }
            }

            InputEvent::DeviceRemoved { device } => {
                if device.has_capability(DeviceCapability::TabletTool) {
                    self.seat
                        .tablet_seat()
                        .remove_tablet(&TabletDescriptor::from(&device));
                }
            }

            InputEvent::TabletToolProximity { event, .. } => {
                let Some(output_geo) = self.wm.primary_output_geometry() else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_location = pos;
                let time = event.time_msec();
                let serial = SERIAL_COUNTER.next_serial();
                let dh = self.display_handle.clone();
                let tablet_seat = self.seat.tablet_seat();
                let tablet_desc = TabletDescriptor::from(&event.device());
                let tablet = tablet_seat
                    .get_tablet(&tablet_desc)
                    .unwrap_or_else(|| tablet_seat.add_tablet::<Self>(&dh, &tablet_desc));
                let tool_desc = event.tool();
                let tool = tablet_seat
                    .get_tool(&tool_desc)
                    .unwrap_or_else(|| tablet_seat.add_tool::<Self>(self, &dh, &tool_desc));

                match event.state() {
                    ProximityState::In => {
                        if let Some((surface, loc)) = self.surface_under(pos) {
                            tool.proximity_in(pos, (surface, loc.to_f64()), &tablet, serial, time);
                        }
                    }
                    ProximityState::Out => {
                        tool.proximity_out(time);
                    }
                }
            }

            InputEvent::TabletToolAxis { event, .. } => {
                let Some(output_geo) = self.wm.primary_output_geometry() else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_location = pos;
                let time = event.time_msec();
                let serial = SERIAL_COUNTER.next_serial();
                let dh = self.display_handle.clone();
                let tablet_seat = self.seat.tablet_seat();
                let tablet_desc = TabletDescriptor::from(&event.device());
                let tablet = tablet_seat
                    .get_tablet(&tablet_desc)
                    .unwrap_or_else(|| tablet_seat.add_tablet::<Self>(&dh, &tablet_desc));
                let tool_desc = event.tool();
                let tool = tablet_seat
                    .get_tool(&tool_desc)
                    .unwrap_or_else(|| tablet_seat.add_tool::<Self>(self, &dh, &tool_desc));

                if event.pressure_has_changed() {
                    tool.pressure(event.pressure());
                }
                if event.distance_has_changed() {
                    tool.distance(event.distance());
                }
                if event.tilt_has_changed() {
                    tool.tilt(event.tilt());
                }
                if event.slider_has_changed() {
                    tool.slider_position(event.slider_position());
                }
                if event.rotation_has_changed() {
                    tool.rotation(event.rotation());
                }
                if event.wheel_has_changed() {
                    tool.wheel(event.wheel_delta(), event.wheel_delta_discrete());
                }
                let under = self.surface_under(pos).map(|(s, loc)| (s, loc.to_f64()));
                tool.motion(pos, under, &tablet, serial, time);
            }

            InputEvent::TabletToolTip { event, .. } => {
                let time = event.time_msec();
                let serial = SERIAL_COUNTER.next_serial();
                let dh = self.display_handle.clone();
                let tablet_seat = self.seat.tablet_seat();
                let tool_desc = event.tool();
                let tool = tablet_seat
                    .get_tool(&tool_desc)
                    .unwrap_or_else(|| tablet_seat.add_tool::<Self>(self, &dh, &tool_desc));
                match event.tip_state() {
                    TabletToolTipState::Down => tool.tip_down(serial, time),
                    TabletToolTipState::Up => tool.tip_up(time),
                }
            }

            InputEvent::TabletToolButton { event, .. } => {
                let time = event.time_msec();
                let serial = SERIAL_COUNTER.next_serial();
                let dh = self.display_handle.clone();
                let tablet_seat = self.seat.tablet_seat();
                let tool_desc = event.tool();
                let tool = tablet_seat
                    .get_tool(&tool_desc)
                    .unwrap_or_else(|| tablet_seat.add_tool::<Self>(self, &dh, &tool_desc));
                tool.button(event.button(), event.button_state(), serial, time);
            }

            _ => {}
        }
    }

    /// Returns the currently focused `Window` in the active workspace, if any.
    pub fn focused_window(&self) -> Option<Window> {
        let keyboard = self.seat.get_keyboard()?;
        let focused_surface = keyboard.current_focus()?;
        self.wm
            .active_space()
            .elements()
            .find(|w| mochi_wm::window_wl_surface(w).as_ref() == Some(&focused_surface))
            .cloned()
    }

    fn execute_key_action(&mut self, action: KeyAction) {
        match action {
            KeyAction::Quit => {
                info!("Quit keybinding triggered; shutting down Mochi Mochi");
                self.running = false;
                self.loop_signal.stop();
            }
            KeyAction::SpawnTerminal => {
                self.spawn_default_terminal();
            }
            KeyAction::SpawnCommand(cmd) => {
                self.spawn_client_command(&cmd);
            }
            KeyAction::CloseActiveWindow => {
                if let Some(window) = self.focused_window() {
                    mochi_wm::send_window_close(&window);
                    info!("Sent close request to focused window");
                }
            }
            KeyAction::CycleVisualStyle => {
                self.wm.cycle_visual_style();
            }
            KeyAction::CycleFocus => {
                if let Some(next_win) = self.wm.cycle_focus() {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        let surface = mochi_wm::window_wl_surface(&next_win);
                        let serial = SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, surface, serial);
                    }
                }
            }
            KeyAction::ToggleFullscreen => {
                if let Some(window) = self.focused_window() {
                    self.wm.toggle_fullscreen(&window);
                }
            }
            KeyAction::ToggleLayout => {
                self.wm.toggle_active_layout();
            }
            KeyAction::ToggleOverview => {
                self.wm.toggle_overview();
            }
            KeyAction::ToggleScratchpad => {
                if let Some(win) = self.wm.toggle_scratchpad() {
                    if let Some(keyboard) = self.seat.get_keyboard() {
                        let surface = mochi_wm::window_wl_surface(&win);
                        let serial = SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, surface, serial);
                    }
                } else {
                    self.focus_top_window_in_active_workspace();
                }
            }
            KeyAction::MoveToScratchpad => {
                if let Some(window) = self.focused_window() {
                    if self.wm.move_to_scratchpad(&window) {
                        self.focus_top_window_in_active_workspace();
                    }
                }
            }
            KeyAction::Screenshot => {
                self.queue_screenshot(None);
                self.wm.trigger_hud();
            }
            KeyAction::SwapWithMaster => {
                if let Some(window) = self.focused_window() {
                    self.wm.swap_with_master(&window);
                }
            }
            KeyAction::SwapWindow(direction) => {
                if let Some(window) = self.focused_window() {
                    self.wm.swap_window(&window, direction);
                }
            }
            KeyAction::AdjustMasterRatio(step_pct) => {
                self.wm.adjust_master_ratio(step_pct as f64 / 100.0);
            }
            KeyAction::AdjustMasterCount(delta) => {
                self.wm.adjust_master_count(delta);
            }
            KeyAction::ReloadConfig => {
                self.reload_config_from_disk();
            }
            KeyAction::SnapActiveWindow(zone) => {
                if let Some(window) = self.focused_window() {
                    self.wm.snap_window(&window, zone);
                }
            }
            KeyAction::SwitchWorkspace(idx) => {
                if self.wm.switch_workspace(idx) {
                    self.focus_top_window_in_active_workspace();
                }
            }
            KeyAction::MoveWindowToWorkspace(idx) => {
                if let Some(window) = self.focused_window() {
                    if self.wm.move_window_to_workspace(&window, idx) {
                        self.focus_top_window_in_active_workspace();
                    }
                }
            }
            KeyAction::SwitchVt(vt) => {
                info!(vt, "Requested Linux VT switch");
                self.pending_vt_switch = Some(vt);
            }
            KeyAction::ToggleNightLight => {
                let active = self.night_light_state.toggle();
                info!(active, temp = self.night_light_state.temperature, "Toggled Night Light warmth");
                self.wm.trigger_hud();
            }
            KeyAction::SetNightLightTemperature(kelvin) => {
                self.night_light_state.set_temperature(kelvin);
                info!(kelvin, "Set Night Light temperature");
                self.wm.trigger_hud();
            }
        }
    }

    fn focus_top_window_in_active_workspace(&mut self) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            let next_focus = self
                .wm
                .active_space()
                .elements()
                .last()
                .and_then(mochi_wm::window_wl_surface);
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, next_focus, serial);
        }
    }

    /// Spawns a client command connected to this Mochi Mochi session.
    pub fn spawn_client_command(&self, cmd: &str) {
        info!(command = cmd, socket = ?self.socket_name, "Spawning client inside Mochi Mochi");
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(cmd)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("MOCHI_IPC_SOCKET", &self.ipc_socket_path)
            .env("GDK_BACKEND", "wayland,x11")
            .env("QT_QPA_PLATFORM", "wayland;xcb")
            .env("MOZ_ENABLE_WAYLAND", "1");

        if let Some(display_number) = self.x11_display {
            command.env("DISPLAY", format!(":{}", display_number));
        } else {
            command.env_remove("DISPLAY");
        }

        if let Err(err) = command.spawn() {
            warn!(?err, command = cmd, "Failed to spawn client command");
        }
    }

    pub fn spawn_default_terminal(&self) {
        let candidates: &[(&str, &str)] = &[
            ("foot", "foot"),
            ("alacritty", "alacritty"),
            ("kitty", "kitty"),
            ("weston-terminal", "weston-terminal"),
            ("ptyxis", "ptyxis --standalone"),
            ("kgx", "kgx"),
            ("gtk4-demo", "gtk4-demo"),
        ];
        for (bin, launch_cmd) in candidates {
            if Command::new("which")
                .arg(bin)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                self.spawn_client_command(launch_cmd);
                return;
            }
        }
        warn!("No known Wayland terminal found in PATH");
    }
}

fn evaluate_keybinding(
    bindings: &[KeyBinding],
    overview_active: bool,
    modifiers: &ModifiersState,
    keysym: u32,
) -> Option<KeyAction> {
    if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&keysym) {
        let vt = (keysym - keysyms::KEY_XF86Switch_VT_1 + 1) as i32;
        return Some(KeyAction::SwitchVt(vt));
    }

    // Pressing plain Escape or Return while Expose / Overview is active exits Overview cleanly
    if overview_active
        && !modifiers.logo
        && !modifiers.alt
        && !modifiers.ctrl
        && !modifiers.shift
        && matches!(
            keysym,
            keysyms::KEY_Escape | keysyms::KEY_Return | keysyms::KEY_KP_Enter
        )
    {
        return Some(KeyAction::ToggleOverview);
    }

    bindings
        .iter()
        .find(|b| b.matches(modifiers, keysym))
        .map(|b| b.action.clone())
}
