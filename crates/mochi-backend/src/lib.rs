// SPDX-License-Identifier: GPL-3.0-or-later
//! Hardware (`udev`/DRM/KMS/GBM/`libinput`) & Nested (`winit`) Display Backends (`mochi-backend`)
//!
//! Both backends share the exact same front-to-back scene graph builder
//! (`assemble_scene_elements`), offscreen FBO background blur pass (`OffscreenBackdropFbo`),
//! OpenGL ES shader pipeline (`MochiShaderPipeline`), physical spring animation engine
//! (`mochi-anim`), and damage tracker (`OutputDamageTracker`).

pub mod udev;
pub mod winit;

use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use mochi_core::glass::{get_glass_attributes, resolve_glass_style, GlassBackdropMode};
use mochi_core::MochiState;
use mochi_render::{MochiShaderPipeline, RoundedSurfaceRenderElement};
use mochi_scene::BackdropEffectNode;
use mochi_wm::LayoutMode;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                solid::SolidColorRenderElement,
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                AsRenderElements, Id, Kind,
            },
            gles::{
                element::{PixelShaderElement, TextureShaderElement},
                GlesRenderer, GlesTexture,
            },
            utils::CommitCounter,
            Bind, ExportMem, Offscreen, RendererSuper,
        },
    },
    desktop::{layer_map_for_output, LayerSurface},
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    render_elements,
    utils::{Buffer as BufferCoord, IsAlive, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::{compositor::with_states, shell::wlr_layer::Layer as WlrLayer},
};
use tracing::{info, warn};

render_elements! {
    pub MochiSceneElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    RoundedSurface=RoundedSurfaceRenderElement,
    Shader=PixelShaderElement,
    TexShader=TextureShaderElement,
    Solid=SolidColorRenderElement,
}

/// Persistent element IDs for compositor UI overlays (cursor & focused window frame).
pub struct OverlayIds {
    pub cursor_primary: Id,
    pub cursor_tip: Id,
    pub focus_top: Id,
    pub focus_bottom: Id,
    pub focus_left: Id,
    pub focus_right: Id,
}

impl Default for OverlayIds {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayIds {
    pub fn new() -> Self {
        Self {
            cursor_primary: Id::new(),
            cursor_tip: Id::new(),
            focus_top: Id::new(),
            focus_bottom: Id::new(),
            focus_left: Id::new(),
            focus_right: Id::new(),
        }
    }
}

/// Output of the 2-level Dual-Kawase ping-pong FBO blur pyramid (`1/1 -> 1/2 -> 1/4 -> 1/2`).
///
/// Provides pre-filtered, alias-free blurred backdrop textures for both the topmost window
/// (which blurs all background windows + lower layer-shell + wallpaper) and all lower
/// translucent windows in the stack (which blur lower layer-shell + wallpaper).
#[derive(Clone)]
pub struct BackdropBlurPyramid {
    pub top_window_backdrop: GlesTexture,
    pub base_backdrop: GlesTexture,
}

impl BackdropBlurPyramid {
    /// Returns the Dual-Kawase pyramid blurred backdrop texture for a window at
    /// `idx_from_top` (`0` = topmost window, `1..` = lower translucent windows in the stack).
    pub fn texture_for_window(&self, idx_from_top: usize) -> &GlesTexture {
        if idx_from_top == 0 {
            &self.top_window_backdrop
        } else {
            &self.base_backdrop
        }
    }
}

/// Offscreen Framebuffer Object (FBO) texture manager with a 2-level Dual-Kawase
/// downsample/upsample ping-pong pyramid (`1/1 -> 1/2 -> 1/4 -> 1/2`) for real multi-pass
/// background blur and refraction over background windows and wallpaper.
#[derive(Default)]
pub struct OffscreenBackdropFbo {
    full_res_tex: Option<GlesTexture>,
    half_ping_tex: Option<GlesTexture>,
    quarter_tex: Option<GlesTexture>,
    base_upsampled_tex: Option<GlesTexture>,
    top_upsampled_tex: Option<GlesTexture>,
    size: Size<i32, Logical>,
    full_tracker: Option<OutputDamageTracker>,
    half_tracker: Option<OutputDamageTracker>,
    quarter_tracker: Option<OutputDamageTracker>,
}

impl OffscreenBackdropFbo {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_pyramid_allocated(
        &mut self,
        renderer: &mut GlesRenderer,
        out_size: Size<i32, Logical>,
    ) -> bool {
        if self.full_res_tex.is_some() && self.size == out_size {
            return true;
        }

        let half_w = (out_size.w / 2).max(1);
        let half_h = (out_size.h / 2).max(1);
        let quarter_w = (out_size.w / 4).max(1);
        let quarter_h = (out_size.h / 4).max(1);

        let full_buf: Size<i32, BufferCoord> = Size::from((out_size.w, out_size.h));
        let half_buf: Size<i32, BufferCoord> = Size::from((half_w, half_h));
        let quarter_buf: Size<i32, BufferCoord> = Size::from((quarter_w, quarter_h));

        let Ok(full_tex) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, full_buf) else {
            warn!("Failed to allocate full-res backdrop FBO texture");
            return false;
        };
        let Ok(half_ping) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, half_buf) else {
            warn!("Failed to allocate half-res ping FBO texture");
            return false;
        };
        let Ok(quarter) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, quarter_buf) else {
            warn!("Failed to allocate quarter-res FBO texture");
            return false;
        };
        let Ok(base_up) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, half_buf) else {
            warn!("Failed to allocate base upsampled FBO texture");
            return false;
        };
        let Ok(top_up) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, half_buf) else {
            warn!("Failed to allocate top upsampled FBO texture");
            return false;
        };

        self.full_res_tex = Some(full_tex);
        self.half_ping_tex = Some(half_ping);
        self.quarter_tex = Some(quarter);
        self.base_upsampled_tex = Some(base_up);
        self.top_upsampled_tex = Some(top_up);
        self.size = out_size;

        self.full_tracker = Some(OutputDamageTracker::new(
            Size::from((out_size.w, out_size.h)),
            1.0,
            Transform::Flipped180,
        ));
        self.half_tracker = Some(OutputDamageTracker::new(
            Size::from((half_w, half_h)),
            1.0,
            Transform::Flipped180,
        ));
        self.quarter_tracker = Some(OutputDamageTracker::new(
            Size::from((quarter_w, quarter_h)),
            1.0,
            Transform::Flipped180,
        ));

        true
    }

    /// Executes the 2-level Dual-Kawase downsample + tent upsample pyramid:
    /// `src_full (1/1) -> half_ping (1/2) -> quarter (1/4) -> dst_upsampled (1/2)`.
    fn run_kawase_pyramid_into(
        renderer: &mut GlesRenderer,
        shaders: &MochiShaderPipeline,
        src_full: &GlesTexture,
        half_ping: &mut GlesTexture,
        quarter: &mut GlesTexture,
        dst_upsampled: &mut GlesTexture,
        half_tracker: &mut OutputDamageTracker,
        quarter_tracker: &mut OutputDamageTracker,
        out_size: Size<i32, Logical>,
        blur_radius: f32,
        clear_color: [f32; 4],
    ) {
        let half_size = Size::from(((out_size.w / 2).max(1), (out_size.h / 2).max(1)));
        let quarter_size = Size::from(((out_size.w / 4).max(1), (out_size.h / 4).max(1)));
        let offset = (blur_radius / 16.0).clamp(0.75, 4.5);

        // Pass 1: Downsample 1/1 -> 1/2
        let down1 = [MochiSceneElement::TexShader(shaders.kawase_pass_element(
            renderer,
            src_full.clone(),
            half_size,
            offset,
            false,
        ))];
        if let Ok(mut target) = renderer.bind(half_ping) {
            let _ = half_tracker.render_output(renderer, &mut target, 0, &down1, clear_color);
        }

        // Pass 2: Downsample 1/2 -> 1/4
        let down2 = [MochiSceneElement::TexShader(shaders.kawase_pass_element(
            renderer,
            half_ping.clone(),
            quarter_size,
            offset * 1.25,
            false,
        ))];
        if let Ok(mut target) = renderer.bind(quarter) {
            let _ = quarter_tracker.render_output(renderer, &mut target, 0, &down2, clear_color);
        }

        // Pass 3: Tent Upsample 1/4 -> 1/2
        let up1 = [MochiSceneElement::TexShader(shaders.kawase_pass_element(
            renderer,
            quarter.clone(),
            half_size,
            offset * 1.15,
            true,
        ))];
        if let Ok(mut target) = renderer.bind(dst_upsampled) {
            let _ = half_tracker.render_output(renderer, &mut target, 0, &up1, clear_color);
        }
    }

    /// Renders the multi-pass Dual-Kawase FBO blur pyramid (`1/1 -> 1/2 -> 1/4 -> 1/2`)
    /// so every translucent window in the stack blurs and refracts live surfaces behind it
    /// with zero texel aliasing even at large blur radii (`48..96px`).
    pub fn render_backdrop_pass(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &MochiState,
        output: &Output,
        shaders: &MochiShaderPipeline,
    ) -> Option<BackdropBlurPyramid> {
        // Never sample user windows into GPU textures while the session is locked.
        if state.session_locked {
            return None;
        }
        let output_geo = state
            .wm
            .active_space()
            .output_geometry(output)
            .unwrap_or_else(|| Rectangle::new(Point::from((0, 0)), Size::from((1280, 800))));
        let out_size = output_geo.size;
        if out_size.w <= 0 || out_size.h <= 0 {
            return None;
        }

        if !self.ensure_pyramid_allocated(renderer, out_size) {
            return None;
        }

        let scale = Scale::from(output.current_scale().fractional_scale());
        let elapsed_secs = state.start_time.elapsed().as_secs_f32();
        let style_cfg = &state.wm.visual_style;
        let style_scalar = state.wm.style_morph.current as f32;
        let slide_x = state.wm.workspace_slide_x.current.round() as i32;
        let clear_color = style_cfg.style.backdrop_clear_color();
        let blur_radius = style_cfg.blur_radius as f32;

        // --------------------------------------------------------------------
        // Stage 1: Base Backdrop (Lower Layer-Shell + Wallpaper) -> base_upsampled_tex
        // --------------------------------------------------------------------
        let mut base_elements: Vec<MochiSceneElement> = Vec::with_capacity(8);
        let layer_map = layer_map_for_output(output);
        for layer in layer_map
            .layers()
            .rev()
            .filter(|s| matches!(s.layer(), WlrLayer::Background | WlrLayer::Bottom))
        {
            let surf = layer.wl_surface();
            if let Some(attrs) = get_glass_attributes(surf) {
                if attrs.backdrop_mode == GlassBackdropMode::ExcludeFromBackdrop {
                    continue;
                }
            }
            if let Some(geo) = layer_map.layer_geometry(layer) {
                let phys_loc = geo.loc.to_physical_precise_round(scale);
                let surf_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    AsRenderElements::<GlesRenderer>::render_elements(
                        layer, renderer, phys_loc, scale, 1.0,
                    );
                base_elements.extend(surf_elems.into_iter().map(MochiSceneElement::Surface));
            }
        }
        for window in state.wm.active_space().elements().rev() {
            if mochi_wm::is_shoji_shell_window(window).as_deref() == Some("shoji-wallpaper") {
                let loc = state
                    .wm
                    .active_space()
                    .element_location(window)
                    .unwrap_or_default()
                    .to_physical_precise_round(scale);
                let surf_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    AsRenderElements::<GlesRenderer>::render_elements(
                        window, renderer, loc, scale, 1.0,
                    );
                base_elements.extend(surf_elems.into_iter().map(MochiSceneElement::Surface));
            }
        }
        base_elements.push(MochiSceneElement::Shader(shaders.wallpaper_element(
            output_geo,
            style_scalar,
            elapsed_secs,
            state.pointer_location,
            state.wm.overview_progress.current as f32,
        )));

        {
            let full_tex = self.full_res_tex.as_mut()?;
            let full_tracker = self.full_tracker.as_mut()?;
            if let Ok(mut fbo_target) = renderer.bind(full_tex) {
                let _ = full_tracker.render_output(
                    renderer,
                    &mut fbo_target,
                    0,
                    &base_elements,
                    clear_color,
                );
            }
        }

        Self::run_kawase_pyramid_into(
            renderer,
            shaders,
            self.full_res_tex.as_ref()?,
            self.half_ping_tex.as_mut()?,
            self.quarter_tex.as_mut()?,
            self.base_upsampled_tex.as_mut()?,
            self.half_tracker.as_mut()?,
            self.quarter_tracker.as_mut()?,
            out_size,
            blur_radius,
            clear_color,
        );

        let space = state.wm.active_space();
        if space.elements().count() <= 1 {
            let base_tex = self.base_upsampled_tex.clone()?;
            return Some(BackdropBlurPyramid {
                top_window_backdrop: base_tex.clone(),
                base_backdrop: base_tex,
            });
        }

        // --------------------------------------------------------------------
        // Stage 2: Background Windows (sampling base_upsampled_tex) + Base Scene -> top_upsampled_tex
        // --------------------------------------------------------------------
        let base_blur_tex = self.base_upsampled_tex.clone()?;
        let mut bg_elements: Vec<MochiSceneElement> = Vec::with_capacity(24);
        let tb_h = style_cfg.titlebar_height.max(0);
        let corner_r = style_cfg.corner_radius as f32;

        for window in space.elements().rev().skip(1) {
            if mochi_wm::is_shoji_shell_window(window).is_some() {
                continue;
            }
            let Some(base_loc) = space.element_location(window) else {
                continue;
            };
            let glass_attrs = mochi_wm::window_wl_surface(window)
                .as_ref()
                .and_then(get_glass_attributes);
            if let Some(ref attrs) = glass_attrs {
                if attrs.backdrop_mode == GlassBackdropMode::ExcludeFromBackdrop {
                    continue;
                }
            }

            let (win_style, win_scalar) =
                resolve_glass_style(style_cfg, style_scalar, glass_attrs.as_ref());
            let win_corner_r = glass_attrs
                .as_ref()
                .and_then(|a| a.corner_radius)
                .map(|r| r as f32)
                .unwrap_or(corner_r);

            let win_geo = window.geometry();
            let (anim_rect, win_alpha) = state
                .wm
                .window_animations
                .get(window)
                .map(|a| (a.current_rect(win_geo.size), a.alpha.current as f32))
                .unwrap_or_else(|| (Rectangle::new(base_loc, win_geo.size), 1.0));

            let loc = Point::from((base_loc.x + slide_x, base_loc.y));
            let full_rect = if tb_h > 0 {
                let bar_rect = Rectangle::new(
                    Point::from((loc.x, loc.y - tb_h)),
                    Size::from((anim_rect.size.w, tb_h)),
                );
                bg_elements.push(MochiSceneElement::Shader(shaders.window_titlebar_element(
                    bar_rect,
                    win_scalar,
                    win_corner_r,
                    false,
                    win_alpha,
                    state.pointer_location,
                )));
                Rectangle::new(
                    Point::from((loc.x, loc.y - tb_h)),
                    Size::from((anim_rect.size.w, anim_rect.size.h + tb_h)),
                )
            } else {
                Rectangle::new(loc, anim_rect.size)
            };

            let render_loc = (loc - win_geo.loc).to_physical_precise_round(scale);
            let client_origin_phys = loc.to_physical_precise_round(scale);

            let window_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                AsRenderElements::<GlesRenderer>::render_elements(
                    window,
                    renderer,
                    render_loc,
                    scale,
                    (state.wm.effective_window_opacity(window) * win_alpha).clamp(0.0, 1.0),
                );
            bg_elements.extend(window_elems.into_iter().map(|elem| {
                MochiSceneElement::RoundedSurface(shaders.clip_client_surface(
                    elem,
                    client_origin_phys,
                    scale,
                    anim_rect.size,
                    win_corner_r,
                    tb_h > 0,
                ))
            }));

            let mut node =
                BackdropEffectNode::from_window_rect(full_rect, &win_style, false, false);
            if let Some(ref attrs) = glass_attrs {
                if let Some(cr) = attrs.corner_radius {
                    node.corner_radius = cr as i32;
                }
                if let Some(br) = attrs.blur_radius {
                    node.kernel_radius = br as i32;
                }
            }

            let skip_blur = glass_attrs.as_ref().and_then(|a| a.blur_radius) == Some(0);
            if !skip_blur {
                bg_elements.push(MochiSceneElement::TexShader(
                    shaders.window_backdrop_texture_element(
                        renderer,
                        base_blur_tex.clone(),
                        scale,
                        &node,
                        &win_style,
                        win_scalar,
                        win_alpha,
                        out_size,
                        state.pointer_location,
                        elapsed_secs,
                    ),
                ));
            }
        }

        bg_elements.extend(base_elements);

        {
            let full_tex = self.full_res_tex.as_mut()?;
            let full_tracker = self.full_tracker.as_mut()?;
            if let Ok(mut fbo_target) = renderer.bind(full_tex) {
                if let Err(err) =
                    full_tracker.render_output(renderer, &mut fbo_target, 0, &bg_elements, clear_color)
                {
                    warn!(?err, "Error rendering offscreen backdrop FBO pass");
                }
            }
        }

        Self::run_kawase_pyramid_into(
            renderer,
            shaders,
            self.full_res_tex.as_ref()?,
            self.half_ping_tex.as_mut()?,
            self.quarter_tex.as_mut()?,
            self.top_upsampled_tex.as_mut()?,
            self.half_tracker.as_mut()?,
            self.quarter_tracker.as_mut()?,
            out_size,
            blur_radius,
            clear_color,
        );

        Some(BackdropBlurPyramid {
            top_window_backdrop: self.top_upsampled_tex.clone()?,
            base_backdrop: base_blur_tex,
        })
    }
}

/// Assembles the full Mochi Mochi scene graph in front-to-back Z-order for any output
/// (shared across both nested `winit` and bare-metal `udev`/DRM/KMS backends).
pub fn assemble_scene_elements(
    renderer: &mut GlesRenderer,
    state: &MochiState,
    output: &Output,
    shaders: &MochiShaderPipeline,
    backdrop_pyramid: Option<&BackdropBlurPyramid>,
    ids: &OverlayIds,
    commit: CommitCounter,
) -> Vec<MochiSceneElement> {
    let mut elements = Vec::with_capacity(32);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let elapsed_secs = state.start_time.elapsed().as_secs_f32();
    let style_cfg = &state.wm.visual_style;
    let style_scalar = state.wm.style_morph.current as f32;
    let slide_x = state.wm.workspace_slide_x.current.round() as i32;
    let accent_color = style_cfg.style.accent_border_color();

    let output_geo = state
        .wm
        .active_space()
        .output_geometry(output)
        .unwrap_or_else(|| Rectangle::new(Point::from((0, 0)), Size::from((1280, 800))));
    let output_size = output_geo.size;

    // ========================================================================
    // 1. Pointer Cursor (Client Surface or Software Pointer)
    // ========================================================================
    match state.cursor_status {
        CursorImageStatus::Surface(ref surface) if surface.alive() => {
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map(|data| data.lock().unwrap().hotspot)
                    .unwrap_or_else(|| Point::from((0, 0)))
            });
            let cursor_pos: Point<i32, Physical> =
                (state.pointer_location - hotspot.to_f64()).to_physical_precise_round(scale);

            let surface_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    renderer,
                    surface,
                    cursor_pos,
                    scale,
                    1.0,
                    Kind::Cursor,
                );
            elements.extend(surface_elems.into_iter().map(MochiSceneElement::Surface));
        }
        CursorImageStatus::Hidden => {}
        _ => {
            let pos: Point<i32, Physical> = state.pointer_location.to_physical_precise_round(scale);
            let tip_rect = Rectangle::new(pos, Size::from((4, 4)));
            let body_rect = Rectangle::new(pos + Point::from((2, 2)), Size::from((10, 10)));
            elements.push(MochiSceneElement::Solid(SolidColorRenderElement::new(
                ids.cursor_tip.clone(),
                tip_rect,
                commit,
                accent_color,
                Kind::Cursor,
            )));
            elements.push(MochiSceneElement::Solid(SolidColorRenderElement::new(
                ids.cursor_primary.clone(),
                body_rect,
                commit,
                [0.95, 0.96, 0.99, 0.95],
                Kind::Cursor,
            )));
        }
    }

    // ========================================================================
    // 1a. ext-session-lock-v1: while locked, render ONLY the cursor, this output's
    // lock surface, and the procedural wallpaper. No windows, layer surfaces, HUD,
    // or blurred backdrops of user content may leak onto the screen.
    // ========================================================================
    if state.session_locked {
        if let Some(lock_surface) = state.lock_surface_for_output(output) {
            let lock_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    renderer,
                    lock_surface.wl_surface(),
                    output_geo.loc.to_physical_precise_round(scale),
                    scale,
                    1.0,
                    Kind::Unspecified,
                );
            elements.extend(lock_elems.into_iter().map(MochiSceneElement::Surface));
        }

        let (r, g, b, warmth_alpha) = state.night_light_state.current_warmth();
        let nl_strength = state.night_light_state.spring.current as f32;
        if nl_strength > 0.005 && warmth_alpha > 0.005 {
            let tint = [r.min(1.0), g.min(1.0) * 0.92, b.min(1.0) * 0.45];
            elements.push(MochiSceneElement::Shader(shaders.night_light_element(
                output_geo,
                tint,
                nl_strength,
                warmth_alpha,
            )));
        }

        elements.push(MochiSceneElement::Shader(shaders.wallpaper_element(
            output_geo,
            style_scalar,
            elapsed_secs,
            state.pointer_location,
            0.0,
        )));
        return elements;
    }

    // ========================================================================
    // 1b. Auto-Hiding On-Screen Glass HUD Pill (hidden when Shoji shell is active)
    // ========================================================================
    let has_shoji_shell = state
        .wm
        .active_space()
        .elements()
        .any(|w| mochi_wm::is_shoji_shell_window(w).is_some());
    let hud_progress = state.wm.hud_spring.current as f32;
    if !has_shoji_shell && hud_progress > 0.01 {
        let is_tiling = state.wm.active_layout() == LayoutMode::MasterStack;
        elements.push(MochiSceneElement::Shader(shaders.hud_pill_element(
            output_geo,
            hud_progress,
            style_scalar,
            state.wm.active_workspace_index(),
            state.wm.workspace_count(),
            is_tiling,
        )));
    }

    // ========================================================================
    // 1c. Night Light & Color Temperature Warmth Overlay (Shader-Level Warmth)
    // ========================================================================
    let (r, g, b, warmth_alpha) = state.night_light_state.current_warmth();
    let nl_strength = state.night_light_state.spring.current as f32;
    if nl_strength > 0.005 && warmth_alpha > 0.005 {
        let tint = [r.min(1.0), g.min(1.0) * 0.92, b.min(1.0) * 0.45];
        elements.push(MochiSceneElement::Shader(shaders.night_light_element(
            output_geo,
            tint,
            nl_strength,
            warmth_alpha,
        )));
    }

    let layer_map = layer_map_for_output(output);
    let (lower_layers, upper_layers): (Vec<&LayerSurface>, Vec<&LayerSurface>) = layer_map
        .layers()
        .rev()
        .partition(|s| matches!(s.layer(), WlrLayer::Background | WlrLayer::Bottom));

    // ========================================================================
    // 2c. Input Method (IME) Candidate Popups (above windows)
    // ========================================================================
    for popup in &state.ime_popups {
        if !popup.alive() {
            continue;
        }
        let parent_loc = popup
            .get_parent()
            .and_then(|p| {
                state
                    .wm
                    .active_space()
                    .elements()
                    .find(|w| mochi_wm::window_wl_surface(w).as_ref() == Some(&p.surface))
                    .and_then(|w| state.wm.active_space().element_location(w))
            })
            .unwrap_or_else(|| Point::from((0, 0)));
        let loc = (parent_loc + popup.location()).to_physical_precise_round(scale);
        let elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                renderer,
                popup.wl_surface(),
                loc,
                scale,
                1.0,
                Kind::Unspecified,
            );
        elements.extend(elems.into_iter().map(MochiSceneElement::Surface));
    }

    // ========================================================================
    // 3. Upper Layer-Shell Surfaces (Overlay & Top: Docks, Top Bars, Launchers)
    // ========================================================================
    for layer in upper_layers {
        if let Some(geo) = layer_map.layer_geometry(layer) {
            let phys_loc = geo.loc.to_physical_precise_round(scale);
            let surf_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                AsRenderElements::<GlesRenderer>::render_elements(
                    layer, renderer, phys_loc, scale, 1.0,
                );
            elements.extend(surf_elems.into_iter().map(MochiSceneElement::Surface));

            let glass_attrs = get_glass_attributes(layer.wl_surface());
            let (layer_style, layer_scalar) =
                resolve_glass_style(style_cfg, style_scalar, glass_attrs.as_ref());
            let mut layer_node =
                BackdropEffectNode::from_window_rect(geo, &layer_style, false, true);
            if let Some(ref attrs) = glass_attrs {
                if let Some(cr) = attrs.corner_radius {
                    layer_node.corner_radius = cr as i32;
                }
                if let Some(br) = attrs.blur_radius {
                    layer_node.kernel_radius = br as i32;
                }
            }

            let skip_blur = glass_attrs.as_ref().and_then(|a| a.blur_radius) == Some(0);
            if !skip_blur {
                if let Some(pyramid) = backdrop_pyramid {
                    let tex = if glass_attrs.as_ref().map(|a| a.backdrop_mode)
                        == Some(GlassBackdropMode::Xray)
                    {
                        pyramid.base_backdrop.clone()
                    } else {
                        pyramid.top_window_backdrop.clone()
                    };
                    elements.push(MochiSceneElement::TexShader(
                        shaders.window_backdrop_texture_element(
                            renderer,
                            tex,
                            scale,
                            &layer_node,
                            &layer_style,
                            layer_scalar,
                            1.0,
                            output_size,
                            state.pointer_location,
                            elapsed_secs,
                        ),
                    ));
                } else {
                    elements.push(MochiSceneElement::Shader(shaders.window_material_element(
                        &layer_node,
                        &layer_style,
                        layer_scalar,
                        1.0,
                        output_size,
                        state.pointer_location,
                        elapsed_secs,
                    )));
                }
            }
        }
    }

    // ========================================================================
    // 4. Workspace Windows (Top-to-Bottom Z-Order) + Glass Titlebar + Rounded Clip + FBO Blur/Refraction
    // ========================================================================
    let focused_win = state.focused_window();
    let space = state.wm.active_space();
    let base_tb_h = style_cfg.titlebar_height.max(0);
    let corner_r = style_cfg.corner_radius as f32;
    let has_maximized_top_window = space.elements().any(|w| {
        if mochi_wm::is_shoji_shell_window(w).is_some() {
            return false;
        }
        let Some(loc) = space.element_location(w) else {
            return false;
        };
        let geo = w.geometry();
        geo.size.w >= output_size.w - 40 && geo.size.h >= output_size.h / 2 && loc.y <= 64
    });

    for (idx, window) in space.elements().rev().enumerate() {
        let Some(base_loc) = space.element_location(window) else {
            continue;
        };
        let shoji_role = mochi_wm::is_shoji_shell_window(window);
        let is_shoji_wallpaper = shoji_role.as_deref() == Some("shoji-wallpaper");
        let is_shoji_bar = shoji_role.as_deref() == Some("shoji-bar");
        let is_shoji_shell = shoji_role.is_some();
        let tb_h = if is_shoji_shell { 0 } else { base_tb_h };
        let glass_attrs = mochi_wm::window_wl_surface(window)
            .as_ref()
            .and_then(get_glass_attributes);
        let (win_style, win_scalar) =
            resolve_glass_style(style_cfg, style_scalar, glass_attrs.as_ref());
        let win_corner_r = if is_shoji_wallpaper || is_shoji_bar {
            0.0
        } else {
            glass_attrs
                .as_ref()
                .and_then(|a| a.corner_radius)
                .map(|r| r as f32)
                .unwrap_or(corner_r)
        };

        let win_geo = window.geometry();
        let mut anim_rect = state
            .wm
            .window_animations
            .get(window)
            .map(|a| a.current_rect(win_geo.size))
            .unwrap_or_else(|| Rectangle::new(base_loc, win_geo.size));
        if shoji_role
            .as_deref()
            .is_some_and(|r| r.starts_with("shoji-launcher-cat-"))
        {
            anim_rect.size.h = anim_rect.size.h.min(26);
        }
        let win_alpha = state
            .wm
            .window_animations
            .get(window)
            .map(|a| a.alpha.current as f32)
            .unwrap_or(1.0);

        let loc = Point::from((
            base_loc.x + if is_shoji_shell { 0 } else { slide_x },
            base_loc.y,
        ));
        let is_focused = !is_shoji_shell && focused_win.as_ref() == Some(window);

        let surface_clip_size = if !is_shoji_shell {
            win_geo.size
        } else {
            anim_rect.size
        };
        let full_rect = if tb_h > 0 {
            Rectangle::new(
                Point::from((loc.x, loc.y - tb_h)),
                Size::from((surface_clip_size.w, surface_clip_size.h + tb_h)),
            )
        } else {
            Rectangle::new(loc, surface_clip_size)
        };

        // 4a. Modern CSD/Liquid Glass windows rely on the Liquid Glass 3D rim & focus aura
        // (no hard 2px tiling-WM border frame).

        // 4b. Server-side interactive translucent glass titlebar (with Close/Max/Tile buttons)
        if tb_h > 0 {
            let bar_rect = Rectangle::new(
                Point::from((loc.x, loc.y - tb_h)),
                Size::from((anim_rect.size.w, tb_h)),
            );
            elements.push(MochiSceneElement::Shader(shaders.window_titlebar_element(
                bar_rect,
                win_scalar,
                win_corner_r,
                is_focused,
                win_alpha,
                state.pointer_location,
            )));
        }

        // 4c. Client window & popup surfaces with SDF Rounded Corner Clipping (100% opaque by default)
        let render_loc = (loc - win_geo.loc).to_physical_precise_round(scale);
        let client_origin_phys = loc.to_physical_precise_round(scale);
        let window_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            AsRenderElements::<GlesRenderer>::render_elements(
                window,
                renderer,
                render_loc,
                scale,
                (state.wm.effective_window_opacity(window) * win_alpha).clamp(0.0, 1.0),
            );
        elements.extend(window_elems.into_iter().map(|elem| {
            MochiSceneElement::RoundedSurface(shaders.clip_client_surface(
                elem,
                client_origin_phys,
                scale,
                surface_clip_size,
                win_corner_r,
                tb_h > 0,
            ))
        }));

        // 4d. Per-window BackdropEffectNode covering full_rect (titlebar + client surface):
        // Every translucent window in the stack (`idx == 0` topmost, `idx >= 1` lower windows)
        // samples its corresponding level of the 2-level Dual-Kawase FBO blur pyramid!
        let mut node = BackdropEffectNode::from_window_rect(full_rect, &win_style, is_focused, false);
        if let Some(ref attrs) = glass_attrs {
            if let Some(cr) = attrs.corner_radius {
                node.corner_radius = cr as i32;
            }
            if let Some(br) = attrs.blur_radius {
                node.kernel_radius = br as i32;
            }
        }
        if is_shoji_bar {
            node.corner_radius = 0;
            if has_maximized_top_window {
                node.sample_y_offset = win_geo.size.h + 10;
            }
        }

        let is_opaque_app_window = !is_shoji_shell && glass_attrs.is_none();
        let mut node_style = win_style.clone();
        if is_opaque_app_window {
            // Normal app windows are opaque (Mutter/GNOME/HarmonyOS): drop shadow only, no glass interior.
            node_style.opacity = 1.0;
        }

        let skip_blur =
            is_shoji_wallpaper || glass_attrs.as_ref().and_then(|a| a.blur_radius) == Some(0);
        if !skip_blur {
            if let Some(pyramid) = backdrop_pyramid {
                let fbo_tex = if glass_attrs.as_ref().map(|a| a.backdrop_mode)
                    == Some(GlassBackdropMode::Xray)
                {
                    pyramid.base_backdrop.clone()
                } else if is_shoji_shell {
                    pyramid.top_window_backdrop.clone()
                } else {
                    pyramid.texture_for_window(idx).clone()
                };
                elements.push(MochiSceneElement::TexShader(
                    shaders.window_backdrop_texture_element(
                        renderer,
                        fbo_tex,
                        scale,
                        &node,
                        &node_style,
                        win_scalar,
                        win_alpha,
                        output_size,
                        state.pointer_location,
                        elapsed_secs,
                    ),
                ));
            } else {
                let material_elem = shaders.window_material_element(
                    &node,
                    &node_style,
                    win_scalar,
                    win_alpha,
                    output_size,
                    state.pointer_location,
                    elapsed_secs,
                );
                elements.push(MochiSceneElement::Shader(material_elem));
            }
        }
    }

    // 4e. Interactive Edge-Snap Preview Card (rendered BEHIND workspace windows, like Mutter/GNOME)
    if let Some(preview_rect) = state.wm.snap_preview {
        let preview_node = BackdropEffectNode::from_window_rect(preview_rect, style_cfg, false, true);
        if let Some(pyramid) = backdrop_pyramid {
            elements.push(MochiSceneElement::TexShader(
                shaders.window_backdrop_texture_element(
                    renderer,
                    pyramid.top_window_backdrop.clone(),
                    scale,
                    &preview_node,
                    style_cfg,
                    style_scalar,
                    1.0,
                    output_size,
                    state.pointer_location,
                    elapsed_secs,
                ),
            ));
        } else {
            elements.push(MochiSceneElement::Shader(shaders.window_material_element(
                &preview_node,
                style_cfg,
                style_scalar,
                1.0,
                output_size,
                state.pointer_location,
                elapsed_secs,
            )));
        }
    }

    // 4f. Minimizing windows playing spring flight to the bottom dock edge
    for (window, anim) in &state.wm.minimizing_windows {
        let win_geo = window.geometry();
        let anim_rect = anim.current_rect(win_geo.size);
        let loc = Point::from((anim_rect.loc.x + slide_x, anim_rect.loc.y));
        let render_loc = (loc - win_geo.loc).to_physical_precise_round(scale);
        let client_origin_phys = loc.to_physical_precise_round(scale);
        let min_alpha = (anim.alpha.current as f32).clamp(0.0, 1.0);
        if min_alpha > 0.01 {
            let window_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                AsRenderElements::<GlesRenderer>::render_elements(
                    window, renderer, render_loc, scale, min_alpha,
                );
            elements.extend(window_elems.into_iter().map(|elem| {
                MochiSceneElement::RoundedSurface(shaders.clip_client_surface(
                    elem,
                    client_origin_phys,
                    scale,
                    anim_rect.size,
                    corner_r,
                    false,
                ))
            }));
        }
    }

    // ========================================================================
    // 5. Lower Layer-Shell Surfaces (Bottom & Background)
    // ========================================================================
    for layer in lower_layers {
        if let Some(geo) = layer_map.layer_geometry(layer) {
            let phys_loc = geo.loc.to_physical_precise_round(scale);
            let surf_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                AsRenderElements::<GlesRenderer>::render_elements(
                    layer, renderer, phys_loc, scale, 1.0,
                );
            elements.extend(surf_elems.into_iter().map(MochiSceneElement::Surface));
        }
    }

    // ========================================================================
    // 6. Full-Output Procedural Atmospheric Wallpaper Shader
    // ========================================================================
    let wallpaper_elem = shaders.wallpaper_element(
        output_geo,
        style_scalar,
        elapsed_secs,
        state.pointer_location,
        state.wm.overview_progress.current as f32,
    );
    elements.push(MochiSceneElement::Shader(wallpaper_elem));

    elements
}

/// Captures the currently bound OpenGL ES framebuffer (`Abgr8888`) directly to a binary `.ppm` image file.
pub fn capture_framebuffer_to_ppm(
    renderer: &mut GlesRenderer,
    framebuffer: &<GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    path: &Path,
) -> anyhow::Result<()> {
    if size.w <= 0 || size.h <= 0 {
        anyhow::bail!("Invalid framebuffer size for screenshot: {size:?}");
    }
    let region: Rectangle<i32, BufferCoord> =
        Rectangle::from_size(Size::from((size.w, size.h)));
    let mapping = renderer
        .copy_framebuffer(framebuffer, region, Fourcc::Abgr8888)
        .map_err(|e| anyhow::anyhow!("Failed to copy OpenGL ES framebuffer: {e:?}"))?;
    let rgba_bytes = renderer
        .map_texture(&mapping)
        .map_err(|e| anyhow::anyhow!("Failed to map downloaded framebuffer texture: {e:?}"))?;

    write_ppm_file(path, size.w as u32, size.h as u32, rgba_bytes)?;
    info!(
        path = %path.display(),
        width = size.w,
        height = size.h,
        "Saved Mochi Mochi compositor screenshot to disk"
    );
    Ok(())
}

/// Writes an `Abgr8888` (`[R, G, B, A]` in little-endian memory) byte slice to a binary `P6` `.ppm` image file.
pub fn write_ppm_file(
    path: &Path,
    width: u32,
    height: u32,
    rgba_pixels: &[u8],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    write!(writer, "P6\n{} {}\n255\n", width, height)?;

    let expected_pixels = (width as usize) * (height as usize);
    let mut rgb_row = Vec::with_capacity((width as usize) * 3);

    for y in 0..(height as usize) {
        rgb_row.clear();
        let row_start = y * (width as usize) * 4;
        for x in 0..(width as usize) {
            let idx = row_start + x * 4;
            if idx + 2 < rgba_pixels.len() && (y * (width as usize) + x) < expected_pixels {
                rgb_row.push(rgba_pixels[idx]);
                rgb_row.push(rgba_pixels[idx + 1]);
                rgb_row.push(rgba_pixels[idx + 2]);
            } else {
                rgb_row.extend_from_slice(&[0, 0, 0]);
            }
        }
        writer.write_all(&rgb_row)?;
    }
    writer.flush()
}

fn push_border_frame(
    elements: &mut Vec<MochiSceneElement>,
    rect: Rectangle<i32, Logical>,
    thickness: i32,
    color: [f32; 4],
    commit: CommitCounter,
    (id_top, id_bottom, id_left, id_right): (&Id, &Id, &Id, &Id),
) {
    let x = rect.loc.x - thickness;
    let y = rect.loc.y - thickness;
    let w = rect.size.w + thickness * 2;
    let h = rect.size.h + thickness * 2;

    let top = Rectangle::new(Point::from((x, y)), Size::from((w, thickness)));
    let bottom = Rectangle::new(
        Point::from((x, y + h - thickness)),
        Size::from((w, thickness)),
    );
    let left = Rectangle::new(
        Point::from((x, y + thickness)),
        Size::from((thickness, (h - thickness * 2).max(1))),
    );
    let right = Rectangle::new(
        Point::from((x + w - thickness, y + thickness)),
        Size::from((thickness, (h - thickness * 2).max(1))),
    );

    for (id, r) in [
        (id_top, top),
        (id_bottom, bottom),
        (id_left, left),
        (id_right, right),
    ] {
        elements.push(MochiSceneElement::Solid(SolidColorRenderElement::new(
            id.clone(),
            r,
            commit,
            color,
            Kind::Unspecified,
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_ppm_file_encoding() {
        let tmp = std::env::temp_dir().join("mochi-test-screenshot.ppm");
        let rgba: [u8; 16] = [
            255, 0, 0, 255, // Red
            0, 255, 0, 255, // Green
            0, 0, 255, 255, // Blue
            255, 255, 255, 255, // White
        ];
        write_ppm_file(&tmp, 2, 2, &rgba).expect("Failed to write test PPM");
        let data = std::fs::read(&tmp).expect("Failed to read test PPM");
        assert!(data.starts_with(b"P6\n2 2\n255\n"));
        let pixel_payload = &data[b"P6\n2 2\n255\n".len()..];
        assert_eq!(
            pixel_payload,
            &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
