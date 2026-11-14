// SPDX-License-Identifier: GPL-3.0-or-later
//! `wlr-output-management-unstable-v1` Protocol Implementation for Mochi Mochi.
//!
//! Exposes connected physical outputs, available display modes, scaling,
//! positioning, transforms, and VRR/Adaptive Sync state to external display
//! managers (`kanshi`, `wlr-randr`, `nwg-displays`, or future DE Settings),
//! and applies atomic configuration changes.

use std::collections::{HashMap, HashSet};

use smithay::{
    output::{Mode, Output, Scale},
    reexports::wayland_server::{
        backend::GlobalId,
        protocol::wl_output,
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
    },
    utils::{Logical, Point, Transform},
};
use tracing::info;
use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

use crate::state::MochiState;

/// User data on a `ZwlrOutputHeadV1` resource.
#[derive(Debug, Clone)]
pub struct OutputHeadData {
    pub output: Output,
}

/// User data on a `ZwlrOutputModeV1` resource.
#[derive(Debug, Clone)]
pub struct OutputModeData {
    pub output: Output,
    pub mode: Mode,
}

/// Pending per-head configuration staged inside `ZwlrOutputConfigurationV1`.
#[derive(Debug, Clone, Default)]
pub struct StagedHeadConfig {
    pub mode: Option<Mode>,
    pub position: Option<Point<i32, Logical>>,
    pub transform: Option<Transform>,
    pub scale: Option<f64>,
    pub adaptive_sync: Option<bool>,
}

/// User data on a `ZwlrOutputConfigurationV1` resource.
#[derive(Debug, Clone)]
pub struct OutputConfigurationData {
    pub serial: u32,
    pub enabled: HashMap<Output, StagedHeadConfig>,
    pub disabled: HashSet<Output>,
    pub finalized: bool,
}

/// User data on a `ZwlrOutputConfigurationHeadV1` resource.
#[derive(Debug, Clone)]
pub struct OutputConfigurationHeadData {
    pub config: ZwlrOutputConfigurationV1,
    pub output: Output,
}

#[derive(Debug)]
struct BoundOutputManager {
    manager: ZwlrOutputManagerV1,
    client: Client,
    heads: HashMap<String, (Output, ZwlrOutputHeadV1, Vec<ZwlrOutputModeV1>)>,
}

/// Server-side state for `wlr-output-management-unstable-v1`.
#[derive(Debug)]
pub struct OutputManagementState {
    _global: GlobalId,
    pub serial: u32,
    managers: Vec<BoundOutputManager>,
    pub adaptive_sync_outputs: HashMap<String, bool>,
}

impl OutputManagementState {
    /// Initializes and advertises the `zwlr_output_manager_v1` global (version 4).
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<MochiState, ZwlrOutputManagerV1, _>(4, ());
        info!("Advertised zwlr_output_manager_v1 global");
        Self {
            _global: global,
            serial: 1,
            managers: Vec::new(),
            adaptive_sync_outputs: HashMap::new(),
        }
    }

    pub fn is_adaptive_sync_enabled(&self, output_name: &str) -> bool {
        self.adaptive_sync_outputs
            .get(output_name)
            .copied()
            .unwrap_or(false)
    }
}

fn transform_to_wl(t: Transform) -> wl_output::Transform {
    match t {
        Transform::Normal => wl_output::Transform::Normal,
        Transform::_90 => wl_output::Transform::_90,
        Transform::_180 => wl_output::Transform::_180,
        Transform::_270 => wl_output::Transform::_270,
        Transform::Flipped => wl_output::Transform::Flipped,
        Transform::Flipped90 => wl_output::Transform::Flipped90,
        Transform::Flipped180 => wl_output::Transform::Flipped180,
        Transform::Flipped270 => wl_output::Transform::Flipped270,
    }
}

fn wl_to_transform(t: wl_output::Transform) -> Transform {
    match t {
        wl_output::Transform::Normal => Transform::Normal,
        wl_output::Transform::_90 => Transform::_90,
        wl_output::Transform::_180 => Transform::_180,
        wl_output::Transform::_270 => Transform::_270,
        wl_output::Transform::Flipped => Transform::Flipped,
        wl_output::Transform::Flipped90 => Transform::Flipped90,
        wl_output::Transform::Flipped180 => Transform::Flipped180,
        wl_output::Transform::Flipped270 => Transform::Flipped270,
        _ => Transform::Normal,
    }
}

fn emit_output_head(
    state: &MochiState,
    dh: &DisplayHandle,
    bound: &mut BoundOutputManager,
    output: &Output,
) {
    let Ok(head) = bound
        .client
        .create_resource::<ZwlrOutputHeadV1, OutputHeadData, MochiState>(
            dh,
            bound.manager.version(),
            OutputHeadData {
                output: output.clone(),
            },
        )
    else {
        return;
    };

    bound.manager.head(&head);

    let props = output.physical_properties();
    head.name(output.name());
    head.description(output.description());
    if props.size.w > 0 && props.size.h > 0 {
        head.physical_size(props.size.w, props.size.h);
    }
    if head.version() >= 2 {
        head.make(props.make);
        head.model(props.model);
        head.serial_number(output.name());
    }

    let current_mode = output.current_mode();
    let preferred_mode = output.preferred_mode();
    let mut mode_handles = Vec::new();
    let mut current_mode_handle: Option<ZwlrOutputModeV1> = None;

    for mode in output.modes() {
        let Ok(mode_res) = bound
            .client
            .create_resource::<ZwlrOutputModeV1, OutputModeData, MochiState>(
                dh,
                bound.manager.version().min(3),
                OutputModeData {
                    output: output.clone(),
                    mode,
                },
            )
        else {
            continue;
        };
        head.mode(&mode_res);
        mode_res.size(mode.size.w, mode.size.h);
        mode_res.refresh(mode.refresh);
        if preferred_mode == Some(mode) {
            mode_res.preferred();
        }
        if current_mode == Some(mode) {
            current_mode_handle = Some(mode_res.clone());
        }
        mode_handles.push(mode_res);
    }

    head.enabled(1);
    if let Some(ref cur) = current_mode_handle {
        head.current_mode(cur);
    }
    let pos = output.current_location();
    head.position(pos.x, pos.y);
    head.transform(transform_to_wl(output.current_transform()));
    head.scale(output.current_scale().fractional_scale());

    if head.version() >= 4 {
        let vrr = if state
            .output_management_state
            .is_adaptive_sync_enabled(&output.name())
        {
            zwlr_output_head_v1::AdaptiveSyncState::Enabled
        } else {
            zwlr_output_head_v1::AdaptiveSyncState::Disabled
        };
        head.adaptive_sync(vrr);
    }

    bound
        .heads
        .insert(output.name(), (output.clone(), head, mode_handles));
}

impl MochiState {
    /// Broadcasts updated output heads and increments `output_management_state.serial`.
    pub fn sync_output_management(&mut self) {
        self.output_management_state.serial =
            self.output_management_state.serial.wrapping_add(1).max(1);
        let serial = self.output_management_state.serial;

        self.output_management_state
            .managers
            .retain(|m| m.manager.is_alive());
        if self.output_management_state.managers.is_empty() {
            return;
        }

        let outputs: Vec<Output> = self.wm.active_space().outputs().cloned().collect();
        let dh = self.display_handle.clone();
        let mut managers = std::mem::take(&mut self.output_management_state.managers);

        for bound in &mut managers {
            // Mark removed outputs finished
            let existing_names: Vec<String> = bound.heads.keys().cloned().collect();
            for name in existing_names {
                if !outputs.iter().any(|o| o.name() == name) {
                    if let Some((_, head, modes)) = bound.heads.remove(&name) {
                        for m in modes {
                            m.finished();
                        }
                        head.finished();
                    }
                }
            }

            // Update existing or add new outputs
            for out in &outputs {
                if let Some((_, head, modes)) = bound.heads.get(&out.name()) {
                    if !head.is_alive() {
                        continue;
                    }
                    let pos = out.current_location();
                    head.enabled(1);
                    head.position(pos.x, pos.y);
                    head.transform(transform_to_wl(out.current_transform()));
                    head.scale(out.current_scale().fractional_scale());
                    if let Some(cur) = out.current_mode() {
                        if let Some(mode_res) = modes.iter().find(|m| {
                            m.data::<OutputModeData>()
                                .map(|d| d.mode == cur)
                                .unwrap_or(false)
                        }) {
                            head.current_mode(mode_res);
                        }
                    }
                    if head.version() >= 4 {
                        let vrr = if self
                            .output_management_state
                            .is_adaptive_sync_enabled(&out.name())
                        {
                            zwlr_output_head_v1::AdaptiveSyncState::Enabled
                        } else {
                            zwlr_output_head_v1::AdaptiveSyncState::Disabled
                        };
                        head.adaptive_sync(vrr);
                    }
                } else {
                    emit_output_head(self, &dh, bound, out);
                }
            }
            bound.manager.done(serial);
        }

        self.output_management_state.managers = managers;
    }
}

impl GlobalDispatch<ZwlrOutputManagerV1, ()> for MochiState {
    fn can_view(client: Client, _global_data: &()) -> bool {
        crate::state::is_privileged_client(&client)
    }

    fn bind(
        state: &mut Self,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let serial = state.output_management_state.serial;
        let outputs: Vec<Output> = state.wm.active_space().outputs().cloned().collect();

        let mut bound = BoundOutputManager {
            manager,
            client: client.clone(),
            heads: HashMap::with_capacity(outputs.len()),
        };

        for out in &outputs {
            emit_output_head(state, handle, &mut bound, out);
        }

        bound.manager.done(serial);
        state.output_management_state.managers.push(bound);
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for MochiState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    OutputConfigStateLock {
                        inner: std::sync::Mutex::new(OutputConfigurationData {
                            serial,
                            enabled: HashMap::new(),
                            disabled: HashSet::new(),
                            finalized: false,
                        }),
                    },
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                manager.finished();
                state
                    .output_management_state
                    .managers
                    .retain(|m| &m.manager != manager);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputHeadV1, OutputHeadData> for MochiState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _head: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _data: &OutputHeadData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputModeV1, OutputModeData> for MochiState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _mode: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _data: &OutputModeData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

/// Interior-mutable wrapper stored on `ZwlrOutputConfigurationV1`.
#[derive(Debug)]
pub struct OutputConfigStateLock {
    pub inner: std::sync::Mutex<OutputConfigurationData>,
}

impl Dispatch<ZwlrOutputConfigurationV1, OutputConfigStateLock> for MochiState {
    fn request(
        state: &mut Self,
        _client: &Client,
        config: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &OutputConfigStateLock,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let Some(head_data) = head.data::<OutputHeadData>() else {
                    return;
                };
                let output = head_data.output.clone();
                let mut guard = data.inner.lock().unwrap();
                if guard.enabled.contains_key(&output) || guard.disabled.contains(&output) {
                    config.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                        "head already configured in this configuration",
                    );
                    return;
                }
                guard.enabled.insert(output.clone(), StagedHeadConfig::default());
                data_init.init(
                    id,
                    OutputConfigurationHeadData {
                        config: config.clone(),
                        output,
                    },
                );
            }
            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                let Some(head_data) = head.data::<OutputHeadData>() else {
                    return;
                };
                let output = head_data.output.clone();
                let mut guard = data.inner.lock().unwrap();
                if guard.enabled.contains_key(&output) || guard.disabled.contains(&output) {
                    config.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                        "head already configured in this configuration",
                    );
                    return;
                }
                guard.disabled.insert(output);
            }
            zwlr_output_configuration_v1::Request::Test => {
                let mut guard = data.inner.lock().unwrap();
                if guard.finalized {
                    config.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration already applied or tested",
                    );
                    return;
                }
                guard.finalized = true;
                if guard.serial != state.output_management_state.serial {
                    config.cancelled();
                    return;
                }
                let valid = !guard.enabled.is_empty()
                    && guard.enabled.values().all(|cfg| {
                        cfg.scale.map(|s| s > 0.0).unwrap_or(true)
                            && cfg
                                .mode
                                .map(|m| m.size.w > 0 && m.size.h > 0)
                                .unwrap_or(true)
                    });
                if valid {
                    config.succeeded();
                } else {
                    config.failed();
                }
            }
            zwlr_output_configuration_v1::Request::Apply => {
                let (serial, enabled) = {
                    let mut guard = data.inner.lock().unwrap();
                    if guard.finalized {
                        config.post_error(
                            zwlr_output_configuration_v1::Error::AlreadyUsed,
                            "configuration already applied or tested",
                        );
                        return;
                    }
                    guard.finalized = true;
                    (guard.serial, guard.enabled.clone())
                };

                if serial != state.output_management_state.serial {
                    config.cancelled();
                    return;
                }
                if enabled.is_empty() {
                    config.failed();
                    return;
                }

                for (output, cfg) in &enabled {
                    if let Some(m) = cfg.mode {
                        if m.size.w <= 0 || m.size.h <= 0 {
                            config.failed();
                            return;
                        }
                    }
                    if let Some(s) = cfg.scale {
                        if s <= 0.0 {
                            config.failed();
                            return;
                        }
                    }

                    let scale = cfg.scale.map(Scale::Fractional);
                    output.change_current_state(cfg.mode, cfg.transform, scale, cfg.position);
                    if let Some(pos) = cfg.position {
                        for space in state.wm.all_spaces_mut() {
                            if space.outputs().any(|o| o == output) {
                                space.map_output(output, pos);
                            }
                        }
                    }
                    if let Some(vrr) = cfg.adaptive_sync {
                        state
                            .output_management_state
                            .adaptive_sync_outputs
                            .insert(output.name(), vrr);
                    }
                }

                config.succeeded();
                state.sync_output_management();
            }
            zwlr_output_configuration_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadData> for MochiState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        head_cfg: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &OutputConfigurationHeadData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(cfg_lock) = data.config.data::<OutputConfigStateLock>() else {
            return;
        };
        let mut guard = cfg_lock.inner.lock().unwrap();
        let Some(staged) = guard.enabled.get_mut(&data.output) else {
            return;
        };

        match request {
            zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
                if staged.mode.is_some() {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "mode already set",
                    );
                    return;
                }
                if let Some(mode_data) = mode.data::<OutputModeData>() {
                    staged.mode = Some(mode_data.mode);
                }
            }
            zwlr_output_configuration_head_v1::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                if staged.mode.is_some() {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "mode already set",
                    );
                    return;
                }
                if width <= 0 || height <= 0 {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidCustomMode,
                        "custom mode width/height must be positive",
                    );
                    return;
                }
                staged.mode = Some(Mode {
                    size: (width, height).into(),
                    refresh: if refresh > 0 { refresh } else { 60_000 },
                });
            }
            zwlr_output_configuration_head_v1::Request::SetPosition { x, y } => {
                if staged.position.is_some() {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "position already set",
                    );
                    return;
                }
                staged.position = Some(Point::from((x, y)));
            }
            zwlr_output_configuration_head_v1::Request::SetTransform { transform } => {
                if staged.transform.is_some() {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "transform already set",
                    );
                    return;
                }
                if let WEnum::Value(t) = transform {
                    staged.transform = Some(wl_to_transform(t));
                }
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                if staged.scale.is_some() {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "scale already set",
                    );
                    return;
                }
                if scale <= 0.0 {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidScale,
                        "scale must be positive",
                    );
                    return;
                }
                staged.scale = Some(scale);
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { state } => {
                if staged.adaptive_sync.is_some() {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "adaptive sync already set",
                    );
                    return;
                }
                if let WEnum::Value(v) = state {
                    staged.adaptive_sync =
                        Some(v == zwlr_output_head_v1::AdaptiveSyncState::Enabled);
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
    fn test_output_management_global_and_transform_mapping() {
        let display: smithay::reexports::wayland_server::Display<MochiState> =
            smithay::reexports::wayland_server::Display::new().unwrap();
        let dh = display.handle();
        let state = OutputManagementState::new(&dh);
        assert_eq!(state.serial, 1);
        assert!(!state.is_adaptive_sync_enabled("DP-1"));

        assert_eq!(
            wl_to_transform(transform_to_wl(Transform::_90)),
            Transform::_90
        );
        assert_eq!(
            wl_to_transform(transform_to_wl(Transform::Flipped180)),
            Transform::Flipped180
        );
    }
}

