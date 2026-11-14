// SPDX-License-Identifier: GPL-3.0-or-later
//! `ext-workspace-v1` Protocol Implementation for Mochi Mochi.
//!
//! Advertises live compositor workspaces (`1..=N`), their active state,
//! and output associations to external desktop environment components (bars,
//! docks, workspace switchers) while supporting atomic `activate` + `commit`.

use smithay::reexports::wayland_server::{
    backend::GlobalId, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use tracing::info;
use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

use crate::state::MochiState;

/// Per-client instance of `ext_workspace_manager_v1` with its group and workspace handles.
#[derive(Debug)]
struct BoundWorkspaceManager {
    manager: ExtWorkspaceManagerV1,
    _group: ExtWorkspaceGroupHandleV1,
    workspaces: Vec<ExtWorkspaceHandleV1>,
    pending_activate: Option<usize>,
}

/// User data stored on `ExtWorkspaceHandleV1` resources.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceHandleData {
    pub index: usize,
}

/// Server-side state for `ext-workspace-v1`.
#[derive(Debug)]
pub struct ExtWorkspaceManagerState {
    _global: GlobalId,
    clients: Vec<BoundWorkspaceManager>,
    last_active_workspace: usize,
}

impl ExtWorkspaceManagerState {
    /// Initializes and advertises the `ext_workspace_manager_v1` global.
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<MochiState, ExtWorkspaceManagerV1, _>(1, ());
        info!("Advertised ext_workspace_manager_v1 global");
        Self {
            _global: global,
            clients: Vec::new(),
            last_active_workspace: usize::MAX,
        }
    }
}

impl MochiState {
    /// Synchronizes `ext-workspace-v1` clients whenever the active workspace changes.
    pub fn sync_ext_workspaces(&mut self) {
        let active_ws = self.wm.active_workspace_number().saturating_sub(1);
        if self.ext_workspace_state.last_active_workspace == active_ws {
            return;
        }
        self.ext_workspace_state.last_active_workspace = active_ws;

        self.ext_workspace_state
            .clients
            .retain(|c| c.manager.is_alive());

        for client_mgr in &mut self.ext_workspace_state.clients {
            for (idx, ws_handle) in client_mgr.workspaces.iter().enumerate() {
                if !ws_handle.is_alive() {
                    continue;
                }
                let st = if idx == active_ws {
                    ext_workspace_handle_v1::State::Active
                } else {
                    ext_workspace_handle_v1::State::empty()
                };
                ws_handle.state(st);
            }
            client_mgr.manager.done();
        }
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for MochiState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let ws_count = state.wm.workspace_count();
        let active_ws = state.wm.active_workspace_number().saturating_sub(1);
        state.ext_workspace_state.last_active_workspace = active_ws;

        let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, (), MochiState>(
            _handle,
            manager.version(),
            (),
        ) else {
            return;
        };

        manager.workspace_group(&group);
        group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());

        for output in state.wm.active_space().outputs() {
            for wl_output in output.client_outputs(client) {
                group.output_enter(&wl_output);
            }
        }

        let mut ws_handles = Vec::with_capacity(ws_count);
        for idx in 0..ws_count {
            let Ok(ws_handle) = client
                .create_resource::<ExtWorkspaceHandleV1, WorkspaceHandleData, MochiState>(
                    _handle,
                    manager.version(),
                    WorkspaceHandleData { index: idx },
                )
            else {
                continue;
            };

            manager.workspace(&ws_handle);
            ws_handle.id(format!("ws-{}", idx + 1));
            ws_handle.name(format!("{}", idx + 1));
            let coords: Vec<u8> = [idx as u32, 0u32]
                .into_iter()
                .flat_map(|v| v.to_ne_bytes())
                .collect();
            ws_handle.coordinates(coords);
            ws_handle.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);
            let st = if idx == active_ws {
                ext_workspace_handle_v1::State::Active
            } else {
                ext_workspace_handle_v1::State::empty()
            };
            ws_handle.state(st);
            group.workspace_enter(&ws_handle);
            ws_handles.push(ws_handle);
        }

        manager.done();

        state.ext_workspace_state.clients.push(BoundWorkspaceManager {
            manager,
            _group: group,
            workspaces: ws_handles,
            pending_activate: None,
        });
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for MochiState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                let target_ws = state
                    .ext_workspace_state
                    .clients
                    .iter_mut()
                    .find(|c| &c.manager == manager)
                    .and_then(|c| c.pending_activate.take());

                if let Some(idx) = target_ws {
                    if state.wm.switch_workspace(idx) && !state.session_locked {
                        if let Some(keyboard) = state.seat.get_keyboard() {
                            let next_focus = state
                                .wm
                                .active_space()
                                .elements()
                                .last()
                                .and_then(mochi_wm::window_wl_surface);
                            keyboard.set_focus(
                                state,
                                next_focus,
                                smithay::utils::SERIAL_COUNTER.next_serial(),
                            );
                        }
                    }
                    state.sync_ext_workspaces();
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
                state
                    .ext_workspace_state
                    .clients
                    .retain(|c| &c.manager != manager);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for MochiState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _group: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_group_handle_v1::Request::CreateWorkspace { .. } => {}
            ext_workspace_group_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, WorkspaceHandleData> for MochiState {
    fn request(
        state: &mut Self,
        _client: &Client,
        handle: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &WorkspaceHandleData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                for client_mgr in &mut state.ext_workspace_state.clients {
                    if client_mgr.workspaces.iter().any(|w| w == handle) {
                        client_mgr.pending_activate = Some(data.index);
                    }
                }
            }
            ext_workspace_handle_v1::Request::Deactivate
            | ext_workspace_handle_v1::Request::Assign { .. }
            | ext_workspace_handle_v1::Request::Remove
            | ext_workspace_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ext_workspace_manager_global_creation() {
        let display: smithay::reexports::wayland_server::Display<MochiState> =
            smithay::reexports::wayland_server::Display::new().unwrap();
        let dh = display.handle();
        let state = ExtWorkspaceManagerState::new(&dh);
        assert!(state.clients.is_empty());
        assert_eq!(state.last_active_workspace, usize::MAX);
    }
}
