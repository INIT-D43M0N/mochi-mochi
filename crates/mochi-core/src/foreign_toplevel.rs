// SPDX-License-Identifier: GPL-3.0-or-later
//! `wlr-foreign-toplevel-management-unstable-v1` Protocol Implementation for Mochi Mochi.
//!
//! Advertises all open windows (across workspaces, including minimized windows)
//! with live title, app_id, output association, and state flags (`Activated`,
//! `Maximized`, `Minimized`, `Fullscreen`), and executes external dock/taskbar
//! actions (`activate`, `set_minimized`, `unset_minimized`, `set_maximized`,
//! `unset_maximized`, `set_fullscreen`, `unset_fullscreen`, `close`).

use std::collections::HashMap;

use mochi_wm::SnapZone;
use smithay::{
    desktop::Window,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::GlobalId, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
            Resource,
        },
    },
    utils::SERIAL_COUNTER,
};
use tracing::info;
use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use crate::state::MochiState;

/// Snapshot of a toplevel window's observable protocol state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToplevelSnapshot {
    pub title: String,
    pub app_id: String,
    pub maximized: bool,
    pub minimized: bool,
    pub activated: bool,
    pub fullscreen: bool,
}

impl ToplevelSnapshot {
    fn encode_state(&self) -> Vec<u8> {
        let mut states: Vec<u32> = Vec::with_capacity(4);
        if self.maximized {
            states.push(zwlr_foreign_toplevel_handle_v1::State::Maximized as u32);
        }
        if self.minimized {
            states.push(zwlr_foreign_toplevel_handle_v1::State::Minimized as u32);
        }
        if self.activated {
            states.push(zwlr_foreign_toplevel_handle_v1::State::Activated as u32);
        }
        if self.fullscreen {
            states.push(zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32);
        }
        states.into_iter().flat_map(|s| s.to_ne_bytes()).collect()
    }
}

/// User data stored on each `ZwlrForeignToplevelHandleV1`.
#[derive(Debug, Clone)]
pub struct ForeignToplevelHandleData {
    pub window: Window,
}

/// Per-bound-client state for `ZwlrForeignToplevelManagerV1`.
#[derive(Debug)]
struct BoundToplevelManager {
    manager: ZwlrForeignToplevelManagerV1,
    client: Client,
    handles: HashMap<Window, ZwlrForeignToplevelHandleV1>,
}

/// Server-side state for `wlr-foreign-toplevel-management-unstable-v1`.
#[derive(Debug)]
pub struct ForeignToplevelManagerState {
    _global: GlobalId,
    managers: Vec<BoundToplevelManager>,
    snapshots: HashMap<Window, ToplevelSnapshot>,
}

impl ForeignToplevelManagerState {
    /// Initializes and advertises the `zwlr_foreign_toplevel_manager_v1` global (version 3).
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<MochiState, ZwlrForeignToplevelManagerV1, _>(3, ());
        info!("Advertised zwlr_foreign_toplevel_manager_v1 global");
        Self {
            _global: global,
            managers: Vec::new(),
            snapshots: HashMap::new(),
        }
    }
}

fn build_snapshot(state: &MochiState, window: &Window, minimized: bool) -> ToplevelSnapshot {
    let (app_id, title) = mochi_wm::window_metadata(window);
    let activated = !minimized && state.focused_window().as_ref() == Some(window);

    let (maximized, fullscreen) = if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|st| {
            (
                st.states.contains(xdg_toplevel::State::Maximized),
                st.states.contains(xdg_toplevel::State::Fullscreen),
            )
        })
    } else if let Some(x11) = window.x11_surface() {
        (x11.is_maximized(), x11.is_fullscreen())
    } else {
        (false, false)
    };

    ToplevelSnapshot {
        title: title.unwrap_or_default(),
        app_id: app_id.unwrap_or_default(),
        maximized,
        minimized,
        activated,
        fullscreen,
    }
}

fn collect_all_windows(state: &MochiState) -> Vec<(Window, ToplevelSnapshot)> {
    let mut entries = Vec::new();
    for space in state.wm.all_spaces() {
        for win in space.elements() {
            if mochi_wm::is_shoji_shell_window(win).is_some() {
                continue;
            }
            entries.push((win.clone(), build_snapshot(state, win, false)));
        }
    }
    for list in state.wm.minimized_windows.values() {
        for (win, _) in list {
            entries.push((win.clone(), build_snapshot(state, win, true)));
        }
    }
    entries
}

fn emit_initial_handle(
    state: &MochiState,
    dh: &DisplayHandle,
    bound: &mut BoundToplevelManager,
    window: &Window,
    snap: &ToplevelSnapshot,
) {
    let Ok(handle) = bound
        .client
        .create_resource::<ZwlrForeignToplevelHandleV1, ForeignToplevelHandleData, MochiState>(
            dh,
            bound.manager.version(),
            ForeignToplevelHandleData {
                window: window.clone(),
            },
        )
    else {
        return;
    };

    bound.manager.toplevel(&handle);
    handle.title(snap.title.clone());
    handle.app_id(snap.app_id.clone());
    handle.state(snap.encode_state());
    for output in state.wm.active_space().outputs() {
        for wl_output in output.client_outputs(&bound.client) {
            handle.output_enter(&wl_output);
        }
    }
    handle.done();
    bound.handles.insert(window.clone(), handle);
}

impl MochiState {
    /// Diffs open/minimized windows against tracked handles and broadcasts any changes
    /// to bound `zwlr_foreign_toplevel_manager_v1` clients.
    pub fn sync_foreign_toplevels(&mut self) {
        if self.foreign_toplevel_state.managers.is_empty() {
            return;
        }

        self.foreign_toplevel_state
            .managers
            .retain(|m| m.manager.is_alive());
        if self.foreign_toplevel_state.managers.is_empty() {
            self.foreign_toplevel_state.snapshots.clear();
            return;
        }

        let current = collect_all_windows(self);
        let current_map: HashMap<Window, ToplevelSnapshot> = current.into_iter().collect();

        // 1. Notify closed windows
        let closed_windows: Vec<Window> = self
            .foreign_toplevel_state
            .snapshots
            .keys()
            .filter(|w| !current_map.contains_key(*w))
            .cloned()
            .collect();

        for win in &closed_windows {
            self.foreign_toplevel_state.snapshots.remove(win);
            for bound in &mut self.foreign_toplevel_state.managers {
                if let Some(handle) = bound.handles.remove(win) {
                    if handle.is_alive() {
                        handle.closed();
                    }
                }
            }
        }

        // 2. Create handles for new windows & emit updates for changed windows
        let dh = self.display_handle.clone();
        let mut new_windows = Vec::new();
        let mut changed_windows = Vec::new();

        for (win, snap) in &current_map {
            match self.foreign_toplevel_state.snapshots.get(win) {
                None => new_windows.push((win.clone(), snap.clone())),
                Some(prev) if prev != snap => {
                    changed_windows.push((win.clone(), prev.clone(), snap.clone()))
                }
                _ => {}
            }
        }

        // Update snapshot cache first, then emit using immutable `&self`
        for (win, snap) in &new_windows {
            self.foreign_toplevel_state
                .snapshots
                .insert(win.clone(), snap.clone());
        }
        for (win, _, snap) in &changed_windows {
            self.foreign_toplevel_state
                .snapshots
                .insert(win.clone(), snap.clone());
        }

        // Temporarily move managers out to pass `&self` cleanly
        let mut managers = std::mem::take(&mut self.foreign_toplevel_state.managers);
        for bound in &mut managers {
            for (win, snap) in &new_windows {
                emit_initial_handle(self, &dh, bound, win, snap);
            }
            for (win, prev, snap) in &changed_windows {
                if let Some(handle) = bound.handles.get(win) {
                    if !handle.is_alive() {
                        continue;
                    }
                    if prev.title != snap.title {
                        handle.title(snap.title.clone());
                    }
                    if prev.app_id != snap.app_id {
                        handle.app_id(snap.app_id.clone());
                    }
                    if prev.maximized != snap.maximized
                        || prev.minimized != snap.minimized
                        || prev.activated != snap.activated
                        || prev.fullscreen != snap.fullscreen
                    {
                        handle.state(snap.encode_state());
                    }
                    handle.done();
                }
            }
        }
        self.foreign_toplevel_state.managers = managers;
    }
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for MochiState {
    fn bind(
        state: &mut Self,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let current = collect_all_windows(state);

        let mut bound = BoundToplevelManager {
            manager,
            client: client.clone(),
            handles: HashMap::with_capacity(current.len()),
        };

        for (win, snap) in &current {
            state
                .foreign_toplevel_state
                .snapshots
                .entry(win.clone())
                .or_insert_with(|| snap.clone());
            emit_initial_handle(state, handle, &mut bound, win, snap);
        }

        state.foreign_toplevel_state.managers.push(bound);
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for MochiState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            manager.finished();
            state
                .foreign_toplevel_state
                .managers
                .retain(|m| &m.manager != manager);
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ForeignToplevelHandleData> for MochiState {
    fn request(
        state: &mut Self,
        _client: &Client,
        handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &ForeignToplevelHandleData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let win = &data.window;
        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                state.wm.snap_window(win, SnapZone::Maximize);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                state.wm.snap_window(win, SnapZone::CenterRestore);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => {
                if state.wm.minimize_window(win) && !state.session_locked {
                    if let Some(keyboard) = state.seat.get_keyboard() {
                        let next_focus = state
                            .wm
                            .active_space()
                            .elements()
                            .last()
                            .and_then(mochi_wm::window_wl_surface);
                        keyboard.set_focus(state, next_focus, SERIAL_COUNTER.next_serial());
                    }
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {
                if let Some(ws_idx) = state.wm.find_window_workspace(win) {
                    if ws_idx + 1 != state.wm.active_workspace_number() {
                        state.wm.switch_workspace(ws_idx);
                    }
                }
                if let Some(restored) = state.wm.restore_minimized_window(Some(win)) {
                    if !state.session_locked {
                        if let Some(keyboard) = state.seat.get_keyboard() {
                            let surf = mochi_wm::window_wl_surface(&restored);
                            keyboard.set_focus(state, surf, SERIAL_COUNTER.next_serial());
                        }
                    }
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
                if let Some(ws_idx) = state.wm.find_window_workspace(win) {
                    if ws_idx + 1 != state.wm.active_workspace_number() {
                        state.wm.switch_workspace(ws_idx);
                    }
                }
                let _ = state.wm.restore_minimized_window(Some(win));
                if state.wm.active_space().elements().any(|w| w == win) {
                    state.wm.active_space_mut().raise_element(win, true);
                    mochi_wm::enforce_shoji_z_order(state.wm.active_space_mut());
                    if !state.session_locked {
                        if let Some(keyboard) = state.seat.get_keyboard() {
                            let surf = mochi_wm::window_wl_surface(win);
                            keyboard.set_focus(state, surf, SERIAL_COUNTER.next_serial());
                        }
                    }
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                mochi_wm::send_window_close(win);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { .. }
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.wm.toggle_fullscreen(win);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => {}
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {
                for bound in &mut state.foreign_toplevel_state.managers {
                    bound.handles.retain(|_, h| h != handle);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toplevel_snapshot_state_encoding() {
        let snap = ToplevelSnapshot {
            title: "Terminal".to_string(),
            app_id: "org.gnome.Ptyxis".to_string(),
            maximized: true,
            minimized: false,
            activated: true,
            fullscreen: false,
        };
        let encoded = snap.encode_state();
        let vals: Vec<u32> = encoded
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(
            vals,
            vec![
                zwlr_foreign_toplevel_handle_v1::State::Maximized as u32,
                zwlr_foreign_toplevel_handle_v1::State::Activated as u32,
            ]
        );
    }

    #[test]
    fn test_foreign_toplevel_manager_global_creation() {
        let display: smithay::reexports::wayland_server::Display<MochiState> =
            smithay::reexports::wayland_server::Display::new().unwrap();
        let dh = display.handle();
        let state = ForeignToplevelManagerState::new(&dh);
        assert!(state.managers.is_empty());
        assert!(state.snapshots.is_empty());
    }
}

