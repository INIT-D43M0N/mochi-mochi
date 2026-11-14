// SPDX-License-Identifier: GPL-3.0-or-later
//! Interactive Window Move (with Edge-Snap Preview) & Resize Pointer Grabs

use mochi_wm::SnapZone;
use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
        GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
        GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
        GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
        RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Serial, Size},
};

use crate::state::MochiState;

// ============================================================================
// Interactive Window Move Grab (with Live Edge-Snap Preview)
// ============================================================================

pub struct PointerMoveSurfaceGrab {
    pub start_data: PointerGrabStartData<MochiState>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
}

impl PointerGrab<MochiState> for PointerMoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);

        let delta = event.location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + delta;
        data.wm
            .move_window_immediate(&self.window, new_location.to_i32_round());

        // Update live edge-snap preview zone + Liquid Glass bubble & shrink-to-icon springs
        data.wm.snap_drag_window = Some((self.window.clone(), event.location));
        if let Some(usable_geo) = data.wm.usable_output_geometry() {
            let gap = data.wm.visual_style.window_gap;
            let next_preview = SnapZone::from_pointer_edge(event.location, usable_geo)
                .map(|zone| zone.target_geometry(usable_geo, gap));
            if let Some(rect) = next_preview {
                data.wm.last_snap_preview = Some(rect);
                data.wm.snap_preview_progress.set_target(1.0);
            } else {
                data.wm.snap_preview_progress.set_target(0.0);
            }
            data.wm.snap_preview = next_preview;
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if !handle.current_pressed().contains(&self.start_data.button) {
            let release_loc = handle.current_location();
            data.wm.snap_preview = None;
            data.wm.last_snap_preview = None;
            data.wm.snap_drag_window = None;
            data.wm.snap_preview_progress.snap_to(0.0);

            if let Some(usable_geo) = data.wm.usable_output_geometry() {
                if let Some(zone) = SnapZone::from_pointer_edge(release_loc, usable_geo) {
                    data.wm.snap_window(&self.window, zone);
                }
            }

            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut MochiState, handle: &mut PointerInnerHandle<'_, MochiState>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<MochiState> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut MochiState) {
        data.wm.snap_preview = None;
    }
}

// ============================================================================
// Interactive Window Resize Grab
// ============================================================================

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const TOP          = 0b0001;
        const BOTTOM       = 0b0010;
        const LEFT         = 0b0100;
        const RIGHT        = 0b1000;
        const TOP_LEFT     = Self::TOP.bits() | Self::LEFT.bits();
        const BOTTOM_LEFT  = Self::BOTTOM.bits() | Self::LEFT.bits();
        const TOP_RIGHT    = Self::TOP.bits() | Self::RIGHT.bits();
        const BOTTOM_RIGHT = Self::BOTTOM.bits() | Self::RIGHT.bits();
    }
}

impl ResizeEdge {
    /// Determines the nearest corner quadrant when initiating `Mod + Right Click` resize inside a window.
    pub fn from_quadrant(pointer: Point<f64, Logical>, window_rect: Rectangle<i32, Logical>) -> Self {
        let center_x = window_rect.loc.x as f64 + (window_rect.size.w as f64) * 0.5;
        let center_y = window_rect.loc.y as f64 + (window_rect.size.h as f64) * 0.5;

        match (pointer.x < center_x, pointer.y < center_y) {
            (true, true) => Self::TOP_LEFT,
            (false, true) => Self::TOP_RIGHT,
            (true, false) => Self::BOTTOM_LEFT,
            (false, false) => Self::BOTTOM_RIGHT,
        }
    }
}

impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits(x as u32).unwrap_or(Self::BOTTOM_RIGHT)
    }
}

pub struct PointerResizeSurfaceGrab {
    start_data: PointerGrabStartData<MochiState>,
    window: Window,
    edges: ResizeEdge,
    initial_rect: Rectangle<i32, Logical>,
    last_window_size: Size<i32, Logical>,
}

impl PointerResizeSurfaceGrab {
    pub fn start(
        start_data: PointerGrabStartData<MochiState>,
        window: Window,
        edges: ResizeEdge,
        initial_rect: Rectangle<i32, Logical>,
    ) -> Self {
        Self {
            start_data,
            window,
            edges,
            initial_rect,
            last_window_size: initial_rect.size,
        }
    }
}

impl PointerGrab<MochiState> for PointerResizeSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);

        let delta = event.location - self.start_data.location;
        let mut new_w = self.initial_rect.size.w;
        let mut new_h = self.initial_rect.size.h;
        let mut new_x = self.initial_rect.loc.x;
        let mut new_y = self.initial_rect.loc.y;

        if self.edges.intersects(ResizeEdge::LEFT) {
            new_w = (self.initial_rect.size.w as f64 - delta.x).max(160.0) as i32;
            new_x = self.initial_rect.loc.x + (self.initial_rect.size.w - new_w);
        } else if self.edges.intersects(ResizeEdge::RIGHT) {
            new_w = (self.initial_rect.size.w as f64 + delta.x).max(160.0) as i32;
        }

        if self.edges.intersects(ResizeEdge::TOP) {
            new_h = (self.initial_rect.size.h as f64 - delta.y).max(100.0) as i32;
            new_y = self.initial_rect.loc.y + (self.initial_rect.size.h - new_h);
        } else if self.edges.intersects(ResizeEdge::BOTTOM) {
            new_h = (self.initial_rect.size.h as f64 + delta.y).max(100.0) as i32;
        }

        self.last_window_size = (new_w, new_h).into();

        if new_x != self.initial_rect.loc.x || new_y != self.initial_rect.loc.y {
            data.wm
                .active_space_mut()
                .map_element(self.window.clone(), Point::from((new_x, new_y)), true);
        }

        if let Some(xdg) = self.window.toplevel() {
            xdg.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
                state.size = Some(self.last_window_size);
            });
            xdg.send_pending_configure();
        } else if let Some(x11) = self.window.x11_surface() {
            let loc = data
                .wm
                .active_space()
                .element_location(&self.window)
                .unwrap_or(self.initial_rect.loc);
            let _ = x11.configure(Some(Rectangle::new(loc, self.last_window_size)));
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if !handle.current_pressed().contains(&self.start_data.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);

            if let Some(xdg) = self.window.toplevel() {
                xdg.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Resizing);
                    state.size = Some(self.last_window_size);
                });
                xdg.send_pending_configure();
            } else if let Some(x11) = self.window.x11_surface() {
                let loc = data
                    .wm
                    .active_space()
                    .element_location(&self.window)
                    .unwrap_or(self.initial_rect.loc);
                let _ = x11.configure(Some(Rectangle::new(loc, self.last_window_size)));
            }
        }
    }

    fn axis(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut MochiState, handle: &mut PointerInnerHandle<'_, MochiState>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut MochiState,
        handle: &mut PointerInnerHandle<'_, MochiState>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<MochiState> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut MochiState) {}
}

pub type _UnusedSerial = Serial;
