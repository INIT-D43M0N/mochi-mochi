// SPDX-License-Identifier: GPL-3.0-or-later
//! Custom Glass Material Protocol (`mochi-glass-v1`) for Mochi Mochi.
//!
//! Provides an explicit, declarative contract between the Mochi Mochi compositor
//! and external desktop environment components (panels, docks, launchers, notifications,
//! control centers) or custom client applications (GTK4, Qt6, Iced, Slint).
//!
//! Clients bind `mochi_glass_manager_v1` and attach a `mochi_glass_surface_v1` to any
//! `wl_surface` to control:
//! - Custom blur radius (0..128 px, or 0 to disable blur)
//! - Translucent glass opacity (0.0..1.0)
//! - Rounded corner clip radius (0..64 px)
//! - Snell optical refraction strength (0.0..0.15)
//! - Frosted micro-roughness noise grain (0.0..0.10)
//! - Specular edge highlight reflection (0.0..1.0)
//! - Visual style preset overrides (Inherited, NormalBlur, FrostedGlass, LiquidGlass)
//! - Offscreen FBO backdrop sampling pipeline interaction (Normal, ExcludeFromBackdrop, Xray)

use std::sync::Mutex;

use mochi_wm::{VisualStyle, VisualStyleConfig};
use smithay::{
    reexports::wayland_server::{
        backend::GlobalId,
        protocol::wl_surface::WlSurface,
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
    },
    wayland::compositor::{add_destruction_hook, with_states, Cacheable},
};
use tracing::info;

use crate::state::MochiState;

pub mod protocol {
    use wayland_server;
    use wayland_server::protocol::*;

    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocols/mochi-glass-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("../../protocols/mochi-glass-v1.xml");
}

pub use protocol::mochi_glass_manager_v1::{self, MochiGlassManagerV1};
pub use protocol::mochi_glass_surface_v1::{self, MochiGlassSurfaceV1};

/// Interaction modes between a surface and the Dual-Kawase offscreen backdrop FBO pyramid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GlassBackdropMode {
    /// Sample underlying content and contribute normally to the offscreen blur pyramid.
    #[default]
    Normal,
    /// Exclude this surface from being captured into lower blur passes (avoids recursive blur halos).
    ExcludeFromBackdrop,
    /// Sample only the root wallpaper and lower layer shells, ignoring intermediate client windows.
    Xray,
}

impl From<protocol::mochi_glass_surface_v1::BackdropMode> for GlassBackdropMode {
    fn from(mode: protocol::mochi_glass_surface_v1::BackdropMode) -> Self {
        match mode {
            protocol::mochi_glass_surface_v1::BackdropMode::Normal => GlassBackdropMode::Normal,
            protocol::mochi_glass_surface_v1::BackdropMode::ExcludeFromBackdrop => {
                GlassBackdropMode::ExcludeFromBackdrop
            }
            protocol::mochi_glass_surface_v1::BackdropMode::Xray => GlassBackdropMode::Xray,
        }
    }
}

/// Double-buffered glass material attributes associated with a `wl_surface`.
#[derive(Debug, Clone, PartialEq)]
pub struct GlassSurfaceAttributes {
    /// Active style preset (or `None` to inherit the compositor's global style).
    pub style: Option<protocol::mochi_glass_surface_v1::Style>,
    /// Effective blur radius in pixels (0 disables blur pass).
    pub blur_radius: Option<u32>,
    /// Glass backdrop surface opacity factor (0.0 to 1.0).
    pub opacity: Option<f32>,
    /// Rounded corner radius in pixels (0 to 64).
    pub corner_radius: Option<u32>,
    /// Snell optical refraction strength (0.0 to 0.15).
    pub refraction_strength: Option<f32>,
    /// Micro-roughness frosted noise grain intensity (0.0 to 0.10).
    pub noise_intensity: Option<f32>,
    /// Specular highlight reflection intensity (0.0 to 1.0).
    pub specular_intensity: Option<f32>,
    /// Interaction with offscreen backdrop sampling pipeline.
    pub backdrop_mode: GlassBackdropMode,
}

impl Default for GlassSurfaceAttributes {
    fn default() -> Self {
        Self {
            style: None,
            blur_radius: None,
            opacity: None,
            corner_radius: None,
            refraction_strength: None,
            noise_intensity: None,
            specular_intensity: None,
            backdrop_mode: GlassBackdropMode::Normal,
        }
    }
}

impl Cacheable for GlassSurfaceAttributes {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        self.clone()
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// Marker stored in `SurfaceData.data_map` tracking whether a `MochiGlassSurfaceV1`
/// is actively associated with the `wl_surface`.
#[derive(Debug, Default)]
pub struct GlassSurfaceMarker {
    pub active: Mutex<Option<MochiGlassSurfaceV1>>,
}

/// User data for a `MochiGlassSurfaceV1` Wayland protocol resource.
#[derive(Debug, Clone)]
pub struct GlassSurfaceUserData {
    pub surface: WlSurface,
}

/// State tracking for the `mochi_glass_manager_v1` global interface.
#[derive(Debug)]
pub struct GlassManagerState {
    _global: Option<GlobalId>,
}

impl GlassManagerState {
    /// Initializes and advertises the `mochi_glass_manager_v1` global.
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<MochiState, MochiGlassManagerV1, _>(1, ());
        info!("Advertised mochi_glass_manager_v1 global");
        Self {
            _global: Some(global),
        }
    }
}

/// Retrieves the committed glass attributes for a given `WlSurface`, if attached and active.
pub fn get_glass_attributes(surface: &WlSurface) -> Option<GlassSurfaceAttributes> {
    with_states(surface, |states| {
        if let Some(marker) = states.data_map.get::<GlassSurfaceMarker>() {
            let guard = marker.active.lock().unwrap();
            if guard.is_some() {
                if states.cached_state.has::<GlassSurfaceAttributes>() {
                    Some(states.cached_state.get::<GlassSurfaceAttributes>().current().clone())
                } else {
                    Some(GlassSurfaceAttributes::default())
                }
            } else {
                None
            }
        } else {
            None
        }
    })
}

/// Resolves the effective `VisualStyleConfig` and shader style scalar for a surface,
/// taking into account the compositor default/morphed style and any client-specified
/// glass attributes.
pub fn resolve_glass_style(
    base: &VisualStyleConfig,
    base_scalar: f32,
    attrs: Option<&GlassSurfaceAttributes>,
) -> (VisualStyleConfig, f32) {
    let mut resolved = base.clone();
    let mut style_scalar = base_scalar;

    if let Some(attrs) = attrs {
        if attrs.style.is_some() {
            resolved.style = VisualStyle::LiquidGlass;
            style_scalar = 2.0;
        }
        if let Some(blur) = attrs.blur_radius {
            resolved.blur_radius = blur;
        }
        if let Some(op) = attrs.opacity {
            resolved.opacity = op;
        }
        if let Some(cr) = attrs.corner_radius {
            resolved.corner_radius = cr;
        }
        if let Some(refr) = attrs.refraction_strength {
            resolved.refraction_strength = refr;
        }
        if let Some(noise) = attrs.noise_intensity {
            resolved.noise_intensity = noise;
        }
        if let Some(spec) = attrs.specular_intensity {
            resolved.specular_intensity = spec;
        }
    }

    (resolved, style_scalar)
}

impl GlobalDispatch<MochiGlassManagerV1, ()> for MochiState {
    fn can_view(client: Client, _global_data: &()) -> bool {
        crate::state::is_privileged_client(&client)
    }

    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<MochiGlassManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<MochiGlassManagerV1, ()> for MochiState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        manager: &MochiGlassManagerV1,
        request: mochi_glass_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            mochi_glass_manager_v1::Request::GetGlassSurface { id, surface } => {
                let already_exists = with_states(&surface, |states| {
                    if let Some(marker) = states.data_map.get::<GlassSurfaceMarker>() {
                        marker.active.lock().unwrap().is_some()
                    } else {
                        false
                    }
                });

                if already_exists {
                    manager.post_error(
                        mochi_glass_manager_v1::Error::GlassSurfaceAlreadyExists,
                        "a glass surface is already associated with this wl_surface",
                    );
                    return;
                }

                let glass_surface = data_init.init(
                    id,
                    GlassSurfaceUserData {
                        surface: surface.clone(),
                    },
                );

                with_states(&surface, |states| {
                    let marker = states
                        .data_map
                        .get_or_insert_threadsafe(GlassSurfaceMarker::default);
                    let mut guard = marker.active.lock().unwrap();
                    *guard = Some(glass_surface.clone());

                    // Ensure CachedState is initialized for pending mutations
                    drop(states.cached_state.get::<GlassSurfaceAttributes>());
                });

                add_destruction_hook(&surface, |_: &mut Self, surf| {
                    with_states(surf, |states| {
                        if let Some(marker) = states.data_map.get::<GlassSurfaceMarker>() {
                            let mut guard = marker.active.lock().unwrap();
                            *guard = None;
                        }
                    });
                });
            }
            mochi_glass_manager_v1::Request::Destroy => {}
        }
    }
}

impl Dispatch<MochiGlassSurfaceV1, GlassSurfaceUserData> for MochiState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _glass_surface: &MochiGlassSurfaceV1,
        request: mochi_glass_surface_v1::Request,
        data: &GlassSurfaceUserData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            mochi_glass_surface_v1::Request::SetStyle { style } => {
                with_states(&data.surface, |states| {
                    let mut guard = states.cached_state.get::<GlassSurfaceAttributes>();
                    let pending = guard.pending();
                    match style {
                        WEnum::Value(protocol::mochi_glass_surface_v1::Style::Inherited) => {
                            pending.style = None;
                        }
                        WEnum::Value(s) => {
                            pending.style = Some(s);
                        }
                        WEnum::Unknown(_) => {}
                    }
                });
            }
            mochi_glass_surface_v1::Request::SetBlurRadius { radius } => {
                with_states(&data.surface, |states| {
                    let mut guard = states.cached_state.get::<GlassSurfaceAttributes>();
                    guard.pending().blur_radius = Some(radius.min(128));
                });
            }
            mochi_glass_surface_v1::Request::SetOpacity { opacity } => {
                with_states(&data.surface, |states| {
                    let mut guard = states.cached_state.get::<GlassSurfaceAttributes>();
                    guard.pending().opacity = Some((opacity as f32).clamp(0.0, 1.0));
                });
            }
            mochi_glass_surface_v1::Request::SetCornerRadius { radius } => {
                with_states(&data.surface, |states| {
                    let mut guard = states.cached_state.get::<GlassSurfaceAttributes>();
                    guard.pending().corner_radius = Some(radius.min(64));
                });
            }
            mochi_glass_surface_v1::Request::SetRefractionStrength { strength } => {
                with_states(&data.surface, |states| {
                    let mut guard = states.cached_state.get::<GlassSurfaceAttributes>();
                    guard.pending().refraction_strength = Some((strength as f32).clamp(0.0, 0.15));
                });
            }
            mochi_glass_surface_v1::Request::SetNoiseIntensity { intensity } => {
                with_states(&data.surface, |states| {
                    let mut guard = states.cached_state.get::<GlassSurfaceAttributes>();
                    guard.pending().noise_intensity = Some((intensity as f32).clamp(0.0, 0.10));
                });
            }
            mochi_glass_surface_v1::Request::SetSpecularIntensity { intensity } => {
                with_states(&data.surface, |states| {
                    let mut guard = states.cached_state.get::<GlassSurfaceAttributes>();
                    guard.pending().specular_intensity = Some((intensity as f32).clamp(0.0, 1.0));
                });
            }
            mochi_glass_surface_v1::Request::SetBackdropMode { mode } => {
                with_states(&data.surface, |states| {
                    let mut guard = states.cached_state.get::<GlassSurfaceAttributes>();
                    if let WEnum::Value(m) = mode {
                        guard.pending().backdrop_mode = GlassBackdropMode::from(m);
                    }
                });
            }
            mochi_glass_surface_v1::Request::Destroy => {
                with_states(&data.surface, |states| {
                    if let Some(marker) = states.data_map.get::<GlassSurfaceMarker>() {
                        let mut active = marker.active.lock().unwrap();
                        *active = None;
                    }
                    if states.cached_state.has::<GlassSurfaceAttributes>() {
                        *states.cached_state.get::<GlassSurfaceAttributes>().pending() =
                            GlassSurfaceAttributes::default();
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glass_surface_attributes_default_and_cacheable() {
        let mut attrs = GlassSurfaceAttributes::default();
        assert_eq!(attrs.style, None);
        assert_eq!(attrs.blur_radius, None);
        assert_eq!(attrs.opacity, None);
        assert_eq!(attrs.corner_radius, None);
        assert_eq!(attrs.backdrop_mode, GlassBackdropMode::Normal);

        attrs.blur_radius = Some(48);
        attrs.opacity = Some(0.75);
        attrs.corner_radius = Some(24);
        attrs.backdrop_mode = GlassBackdropMode::ExcludeFromBackdrop;

        // Verify clone / commit semantics
        let committed = attrs.clone();
        assert_eq!(committed.blur_radius, Some(48));
        assert_eq!(committed.opacity, Some(0.75));
        assert_eq!(committed.corner_radius, Some(24));
        assert_eq!(committed.backdrop_mode, GlassBackdropMode::ExcludeFromBackdrop);
    }

    #[test]
    fn test_resolve_glass_style_fallbacks_and_overrides() {
        let base_cfg = VisualStyleConfig::default(); // FrostedGlass, blur=24, corner=16
        let base_scalar = 1.0f32;

        // Case 1: None attrs -> unmodified base
        let (res1, s1) = resolve_glass_style(&base_cfg, base_scalar, None);
        assert_eq!(res1.blur_radius, 24);
        assert_eq!(res1.corner_radius, 14);
        assert_eq!(s1, 1.0);

        // Case 2: Override to LiquidGlass with custom blur and corner radius
        let mut custom = GlassSurfaceAttributes::default();
        custom.style = Some(protocol::mochi_glass_surface_v1::Style::LiquidGlass);
        custom.blur_radius = Some(40);
        custom.corner_radius = Some(32);
        custom.opacity = Some(0.65);
        custom.refraction_strength = Some(0.08);

        let (res2, s2) = resolve_glass_style(&base_cfg, base_scalar, Some(&custom));
        assert_eq!(res2.style, VisualStyle::LiquidGlass);
        assert_eq!(s2, 2.0);
        assert_eq!(res2.blur_radius, 40);
        assert_eq!(res2.corner_radius, 32);
        assert_eq!(res2.opacity, 0.65);
        assert_eq!(res2.refraction_strength, 0.08);
        // Untouched fields should remain as base_cfg default
        assert_eq!(res2.noise_intensity, base_cfg.noise_intensity);
    }

    #[test]
    fn test_backdrop_mode_conversion() {
        assert_eq!(
            GlassBackdropMode::from(protocol::mochi_glass_surface_v1::BackdropMode::Normal),
            GlassBackdropMode::Normal
        );
        assert_eq!(
            GlassBackdropMode::from(protocol::mochi_glass_surface_v1::BackdropMode::ExcludeFromBackdrop),
            GlassBackdropMode::ExcludeFromBackdrop
        );
        assert_eq!(
            GlassBackdropMode::from(protocol::mochi_glass_surface_v1::BackdropMode::Xray),
            GlassBackdropMode::Xray
        );
    }

    #[test]
    fn test_glass_manager_global_creation() {
        let display: smithay::reexports::wayland_server::Display<MochiState> =
            smithay::reexports::wayland_server::Display::new().unwrap();
        let dh = display.handle();
        let manager_state = GlassManagerState::new(&dh);
        assert!(manager_state._global.is_some());
    }

    #[test]
    fn test_glass_surface_pending_and_commit_cycle() {
        let display: smithay::reexports::wayland_server::Display<MochiState> =
            smithay::reexports::wayland_server::Display::new().unwrap();
        let dh = display.handle();

        // 1. Pending mutations staged by client
        let mut pending = GlassSurfaceAttributes::default();
        pending.style = Some(protocol::mochi_glass_surface_v1::Style::FrostedGlass);
        pending.blur_radius = Some(64);
        pending.opacity = Some(0.85);
        pending.corner_radius = Some(28);
        pending.refraction_strength = Some(0.045);
        pending.noise_intensity = Some(0.025);
        pending.specular_intensity = Some(0.5);
        pending.backdrop_mode = GlassBackdropMode::ExcludeFromBackdrop;

        // 2. Commit cycle (Cacheable::commit -> merge_into)
        let committed = pending.commit(&dh);
        let mut current = GlassSurfaceAttributes::default();
        committed.merge_into(&mut current, &dh);

        assert_eq!(
            current.style,
            Some(protocol::mochi_glass_surface_v1::Style::FrostedGlass)
        );
        assert_eq!(current.blur_radius, Some(64));
        assert_eq!(current.opacity, Some(0.85));
        assert_eq!(current.corner_radius, Some(28));
        assert_eq!(current.refraction_strength, Some(0.045));
        assert_eq!(current.noise_intensity, Some(0.025));
        assert_eq!(current.specular_intensity, Some(0.5));
        assert_eq!(current.backdrop_mode, GlassBackdropMode::ExcludeFromBackdrop);
    }
}
