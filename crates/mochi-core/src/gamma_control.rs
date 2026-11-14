// SPDX-License-Identifier: GPL-3.0-or-later
//! `wlr-gamma-control-unstable-v1` Protocol Implementation for Mochi Mochi.
//!
//! Enables external color temperature / gamma managers (such as `gammastep`,
//! `wlsunset`, `wl-gammactl`) to query output gamma sizes and upload 16-bit
//! gamma curves (R, G, B ramps) over client-provided file descriptors.

use std::{
    fs::File,
    io::Read,
    os::unix::io::OwnedFd,
};

use smithay::{
    output::Output,
    reexports::wayland_server::{
        backend::GlobalId,
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    },
};
use tracing::{info, warn};
use wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};

use crate::state::MochiState;

/// Converts color temperature in Kelvin (1000..10000) to normalized linear RGB multipliers (0.0..1.0).
/// Uses Tanner Helland's Planckian locus approximation algorithm.
pub fn kelvin_to_rgb_multipliers(kelvin: u32) -> (f32, f32, f32) {
    let t = (kelvin.clamp(1000, 40000) as f64) / 100.0;

    // Red
    let r = if t <= 66.0 {
        255.0
    } else {
        329.698727446 * (t - 60.0).powf(-0.1332047592)
    };

    // Green
    let g = if t <= 66.0 {
        99.4708025861 * t.ln() - 161.1195681661
    } else {
        288.1221695283 * (t - 60.0).powf(-0.0755148492)
    };

    // Blue
    let b = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.5177312231 * (t - 10.0).ln() - 305.0447927307
    };

    let r_norm = (r.clamp(0.0, 255.0) / 255.0) as f32;
    let g_norm = (g.clamp(0.0, 255.0) / 255.0) as f32;
    let b_norm = (b.clamp(0.0, 255.0) / 255.0) as f32;

    // Normalize relative to 6500K neutral daylight reference (r=1.0, g=0.997, b=0.981)
    let r_ref = 1.0f32;
    let g_ref = 0.997f32;
    let b_ref = 0.981f32;

    (
        (r_norm / r_ref).clamp(0.0, 1.0),
        (g_norm / g_ref).clamp(0.0, 1.0),
        (b_norm / b_ref).clamp(0.0, 1.0),
    )
}

/// Parses 3 * ramp_size * 2 bytes of 16-bit native-endian gamma ramps from an owned fd.
/// Returns (R, G, B) multipliers normalized to 0.0..1.0.
pub fn parse_gamma_fd(fd: OwnedFd, ramp_size: usize) -> std::io::Result<(f32, f32, f32)> {
    let mut file = File::from(fd);
    let total_bytes = ramp_size * 3 * 2;
    let mut buf = vec![0u8; total_bytes];
    file.read_exact(&mut buf)?;

    let read_ramp_max = |start_idx: usize| -> f32 {
        let mut max_val = 0u16;
        for i in 0..ramp_size {
            let offset = (start_idx + i) * 2;
            let val = u16::from_ne_bytes([buf[offset], buf[offset + 1]]);
            if val > max_val {
                max_val = val;
            }
        }
        (max_val as f32) / 65535.0
    };

    let r = read_ramp_max(0);
    let g = read_ramp_max(ramp_size);
    let b = read_ramp_max(ramp_size * 2);

    Ok((r.clamp(0.01, 1.0), g.clamp(0.01, 1.0), b.clamp(0.01, 1.0)))
}

/// State tracking for `wlr-gamma-control-unstable-v1`.
#[derive(Debug)]
pub struct GammaControlState {
    _global: Option<GlobalId>,
}

impl GammaControlState {
    /// Initializes and advertises the `zwlr_gamma_control_manager_v1` global.
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<MochiState, ZwlrGammaControlManagerV1, _>(1, ());
        info!("Advertised zwlr_gamma_control_manager_v1 global");
        Self {
            _global: Some(global),
        }
    }
}

/// Per-output user data for a gamma control resource.
#[derive(Debug, Clone)]
pub struct GammaControlUserData {
    pub output: Option<Output>,
}

impl GlobalDispatch<ZwlrGammaControlManagerV1, ()> for MochiState {
    fn can_view(client: Client, _global_data: &()) -> bool {
        crate::state::is_privileged_client(&client)
    }

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for MochiState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _manager: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } => {
                let resolved = Output::from_resource(&output)
                    .or_else(|| state.wm.active_space().outputs().next().cloned());
                let control = data_init.init(
                    id,
                    GammaControlUserData {
                        output: resolved.clone(),
                    },
                );
                if resolved.is_some() {
                    // Standard 256-element gamma ramp size
                    control.gamma_size(256);
                } else {
                    control.failed();
                }
            }
            zwlr_gamma_control_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrGammaControlV1, GammaControlUserData> for MochiState {
    fn request(
        state: &mut Self,
        _client: &Client,
        control: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        _data: &GammaControlUserData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                match parse_gamma_fd(fd, 256) {
                    Ok((r, g, b)) => {
                        info!(r, g, b, "Received external gamma ramps from wayland client");
                        state.night_light_state.set_external_gamma((r, g, b));
                    }
                    Err(err) => {
                        warn!(?err, "Failed to read gamma table fd from client");
                        control.post_error(
                            zwlr_gamma_control_v1::Error::InvalidGamma,
                            "Invalid gamma table length or unreadable fd",
                        );
                    }
                }
            }
            zwlr_gamma_control_v1::Request::Destroy => {
                info!("Client destroyed zwlr_gamma_control_v1; restoring default gamma");
                state.night_light_state.clear_external_gamma();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kelvin_to_rgb_color_temperatures() {
        // Daylight 6500K is neutral (1.0, 1.0, 1.0)
        let (r65, g65, b65) = kelvin_to_rgb_multipliers(6500);
        assert!((r65 - 1.0).abs() < 0.05);
        assert!((g65 - 1.0).abs() < 0.05);
        assert!((b65 - 1.0).abs() < 0.05);

        // Warm 3500K reduces blue light significantly
        let (r35, g35, b35) = kelvin_to_rgb_multipliers(3500);
        assert_eq!(r35, 1.0);
        assert!(g35 < 0.85 && g35 > 0.65);
        assert!(b35 < 0.65 && b35 > 0.45);

        // Candlelight 2000K has heavy blue reduction
        let (r20, g20, b20) = kelvin_to_rgb_multipliers(2000);
        assert_eq!(r20, 1.0);
        assert!(g20 < g35);
        assert!(b20 < b35);
    }
}
