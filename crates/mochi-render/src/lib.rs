// SPDX-License-Identifier: GPL-3.0-or-later
//! Mochi Mochi GPU Shader Pipeline & Visual Style Engine (`mochi-render`)
//!
//! Compiles and manages OpenGL ES fragment shaders (`GlesPixelProgram` and `GlesTexProgram`) for:
//! - Procedural Ambient Wallpaper (`WALLPAPER_FRAG_SRC`)
//! - Live Offscreen FBO Multi-Tap Background Blur, Frosted Saturation Bleed & Liquid Glass
//!   Snell UV Refraction + Chromatic Dispersion (`WINDOW_BACKDROP_TEX_FRAG_SRC`)
//! - Analytic SDF Drop Shadows & Snap-Preview Glass Cards (`WINDOW_MATERIAL_FRAG_SRC`)

pub mod shaders;

use mochi_scene::BackdropEffectNode;
use mochi_wm::{VisualStyle, VisualStyleConfig};
use smithay::{
    backend::renderer::{
        element::{
            surface::{WaylandSurfaceRenderElement, WaylandSurfaceTexture},
            texture::TextureRenderElement,
            Element, Id, Kind, RenderElement, UnderlyingStorage,
        },
        gles::{
            element::{PixelShaderElement, TextureShaderElement},
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture,
            Uniform, UniformName, UniformType,
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
        Frame, Renderer, Texture,
    },
    utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};
use tracing::info;

use crate::shaders::{
    CLIENT_ROUNDED_CLIP_FRAG_SRC, HUD_PILL_FRAG_SRC, KAWASE_DOWNSAMPLE_FRAG_SRC,
    KAWASE_UPSAMPLE_FRAG_SRC, NIGHT_LIGHT_FRAG_SRC, TITLEBAR_FRAG_SRC, WALLPAPER_FRAG_SRC,
    WINDOW_BACKDROP_TEX_FRAG_SRC, WINDOW_MATERIAL_FRAG_SRC,
};

/// Custom render element that wraps a client `WaylandSurfaceRenderElement<GlesRenderer>`
/// and executes `CLIENT_ROUNDED_CLIP_FRAG_SRC` so client surfaces have smooth SDF-clipped
/// rounded corners matching the window's glass backdrop and optional top glass titlebar.
#[derive(Debug)]
pub struct RoundedSurfaceRenderElement {
    inner: WaylandSurfaceRenderElement<GlesRenderer>,
    program: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
}

impl Element for RoundedSurfaceRenderElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn src(&self) -> Rectangle<f64, BufferCoord> {
        self.inner.src()
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // Return empty opaque regions so rounded corners alpha-blend cleanly over the glass backdrop
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for RoundedSurfaceRenderElement {
    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }

    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        match self.inner.texture() {
            WaylandSurfaceTexture::Texture(ref tex) => frame.render_texture_from_to(
                tex,
                src,
                dst,
                damage,
                opaque_regions,
                self.inner.transform(),
                self.inner.alpha(),
                Some(&self.program),
                &self.uniforms,
            ),
            WaylandSurfaceTexture::SolidColor(color) => {
                Frame::draw_solid(frame, dst, damage, *color * self.inner.alpha())
            }
        }
    }
}

/// Compiled GPU shader programs for Mochi Mochi's visual styles and glass UI.
#[derive(Debug, Clone)]
pub struct MochiShaderPipeline {
    pub wallpaper_program: GlesPixelProgram,
    pub window_material_program: GlesPixelProgram,
    pub window_backdrop_tex_program: GlesTexProgram,
    pub kawase_downsample_program: GlesTexProgram,
    pub kawase_upsample_program: GlesTexProgram,
    pub client_clip_program: GlesTexProgram,
    pub titlebar_program: GlesPixelProgram,
    pub hud_pill_program: GlesPixelProgram,
    pub night_light_program: GlesPixelProgram,
}

impl MochiShaderPipeline {
    /// Compiles all Mochi Mochi GLSL fragment shaders on the provided `GlesRenderer`.
    pub fn compile(renderer: &mut GlesRenderer) -> anyhow::Result<Self> {
        let wallpaper_uniforms = [
            UniformName::new("u_style", UniformType::_1f),
            UniformName::new("u_time", UniformType::_1f),
            UniformName::new("u_pointer", UniformType::_2f),
            UniformName::new("u_overview", UniformType::_1f),
        ];
        let wallpaper_program = renderer
            .compile_custom_pixel_shader(WALLPAPER_FRAG_SRC, &wallpaper_uniforms)
            .map_err(|err| anyhow::anyhow!("Failed to compile wallpaper shader: {err:?}"))?;

        let material_uniforms = [
            UniformName::new("u_style", UniformType::_1f),
            UniformName::new("u_window_size", UniformType::_2f),
            UniformName::new("u_padding", UniformType::_1f),
            UniformName::new("u_corner_radius", UniformType::_1f),
            UniformName::new("u_shadow_elevation", UniformType::_1f),
            UniformName::new("u_opacity", UniformType::_1f),
            UniformName::new("u_focused", UniformType::_1f),
            UniformName::new("u_is_preview", UniformType::_1f),
            UniformName::new("u_output_size", UniformType::_2f),
            UniformName::new("u_element_origin", UniformType::_2f),
            UniformName::new("u_pointer", UniformType::_2f),
            UniformName::new("u_time", UniformType::_1f),
            UniformName::new("u_sample_y_offset", UniformType::_1f),
        ];
        let window_material_program = renderer
            .compile_custom_pixel_shader(WINDOW_MATERIAL_FRAG_SRC, &material_uniforms)
            .map_err(|err| anyhow::anyhow!("Failed to compile window material shader: {err:?}"))?;

        let backdrop_tex_uniforms = [
            UniformName::new("u_style", UniformType::_1f),
            UniformName::new("u_element_size", UniformType::_2f),
            UniformName::new("u_window_size", UniformType::_2f),
            UniformName::new("u_padding", UniformType::_1f),
            UniformName::new("u_corner_radius", UniformType::_1f),
            UniformName::new("u_shadow_elevation", UniformType::_1f),
            UniformName::new("u_blur_radius", UniformType::_1f),
            UniformName::new("u_opacity", UniformType::_1f),
            UniformName::new("u_refraction_strength", UniformType::_1f),
            UniformName::new("u_noise_intensity", UniformType::_1f),
            UniformName::new("u_specular_intensity", UniformType::_1f),
            UniformName::new("u_focused", UniformType::_1f),
            UniformName::new("u_is_preview", UniformType::_1f),
            UniformName::new("u_output_size", UniformType::_2f),
            UniformName::new("u_element_origin", UniformType::_2f),
            UniformName::new("u_pointer", UniformType::_2f),
            UniformName::new("u_time", UniformType::_1f),
            UniformName::new("u_sample_y_offset", UniformType::_1f),
        ];
        let window_backdrop_tex_program = renderer
            .compile_custom_texture_shader(WINDOW_BACKDROP_TEX_FRAG_SRC, &backdrop_tex_uniforms)
            .map_err(|err| {
                anyhow::anyhow!("Failed to compile offscreen FBO backdrop texture shader: {err:?}")
            })?;

        let clip_uniforms = [
            UniformName::new("u_surface_size", UniformType::_2f),
            UniformName::new("u_surface_offset", UniformType::_2f),
            UniformName::new("u_window_size", UniformType::_2f),
            UniformName::new("u_corner_radius", UniformType::_1f),
            UniformName::new("u_has_titlebar", UniformType::_1f),
        ];
        let client_clip_program = renderer
            .compile_custom_texture_shader(CLIENT_ROUNDED_CLIP_FRAG_SRC, &clip_uniforms)
            .map_err(|err| {
                anyhow::anyhow!("Failed to compile client rounded clip shader: {err:?}")
            })?;

        let titlebar_uniforms = [
            UniformName::new("u_style", UniformType::_1f),
            UniformName::new("u_corner_radius", UniformType::_1f),
            UniformName::new("u_focused", UniformType::_1f),
            UniformName::new("u_bar_origin", UniformType::_2f),
            UniformName::new("u_pointer", UniformType::_2f),
        ];
        let titlebar_program = renderer
            .compile_custom_pixel_shader(TITLEBAR_FRAG_SRC, &titlebar_uniforms)
            .map_err(|err| anyhow::anyhow!("Failed to compile glass titlebar shader: {err:?}"))?;

        let hud_uniforms = [
            UniformName::new("u_style", UniformType::_1f),
            UniformName::new("u_active_workspace", UniformType::_1f),
            UniformName::new("u_workspace_count", UniformType::_1f),
            UniformName::new("u_is_tiling", UniformType::_1f),
        ];
        let hud_pill_program = renderer
            .compile_custom_pixel_shader(HUD_PILL_FRAG_SRC, &hud_uniforms)
            .map_err(|err| anyhow::anyhow!("Failed to compile glass HUD pill shader: {err:?}"))?;

        let night_light_uniforms = [
            UniformName::new("u_tint_color", UniformType::_3f),
            UniformName::new("u_strength", UniformType::_1f),
        ];
        let night_light_program = renderer
            .compile_custom_pixel_shader(NIGHT_LIGHT_FRAG_SRC, &night_light_uniforms)
            .map_err(|err| anyhow::anyhow!("Failed to compile night light shader: {err:?}"))?;

        let kawase_uniforms = [
            UniformName::new("u_half_texel", UniformType::_2f),
            UniformName::new("u_offset", UniformType::_1f),
        ];
        let kawase_downsample_program = renderer
            .compile_custom_texture_shader(KAWASE_DOWNSAMPLE_FRAG_SRC, &kawase_uniforms)
            .map_err(|err| {
                anyhow::anyhow!("Failed to compile Dual-Kawase downsample shader: {err:?}")
            })?;
        let kawase_upsample_program = renderer
            .compile_custom_texture_shader(KAWASE_UPSAMPLE_FRAG_SRC, &kawase_uniforms)
            .map_err(|err| {
                anyhow::anyhow!("Failed to compile Dual-Kawase upsample shader: {err:?}")
            })?;

        info!(
            "Compiled Mochi Mochi GLSL shader pipeline (Dual-Kawase Pyramid FBO Blur + Frosted Glass + Liquid Glass + Titlebar + HUD)"
        );

        Ok(Self {
            wallpaper_program,
            window_material_program,
            window_backdrop_tex_program,
            kawase_downsample_program,
            kawase_upsample_program,
            client_clip_program,
            titlebar_program,
            hud_pill_program,
            night_light_program,
        })
    }

    /// Creates a `TextureShaderElement` for one Dual-Kawase downsample or upsample pass
    /// in the offscreen ping-pong FBO blur pyramid (`1/1 -> 1/2 -> 1/4 -> 1/2`).
    pub fn kawase_pass_element(
        &self,
        renderer: &GlesRenderer,
        src_texture: GlesTexture,
        dst_size: Size<i32, Logical>,
        offset: f32,
        is_upsample: bool,
    ) -> TextureShaderElement {
        let src_size = src_texture.size();
        let src_logical_rect = Rectangle::from_size(Size::<f64, Logical>::from((
            src_size.w as f64,
            src_size.h as f64,
        )));
        let tex_elem = TextureRenderElement::from_static_texture(
            Id::new(),
            renderer.context_id(),
            Point::from((0.0, 0.0)),
            src_texture,
            1,
            Transform::Normal,
            Some(1.0),
            Some(src_logical_rect),
            Some(dst_size),
            None,
            Kind::Unspecified,
        );

        let half_texel = [
            0.5 / (src_size.w.max(1) as f32),
            0.5 / (src_size.h.max(1) as f32),
        ];
        let uniforms = vec![
            Uniform::new("u_half_texel", half_texel),
            Uniform::new("u_offset", offset),
        ];
        let program = if is_upsample {
            self.kawase_upsample_program.clone()
        } else {
            self.kawase_downsample_program.clone()
        };

        TextureShaderElement::new(tex_elem, program, uniforms)
    }

    /// Wraps a client `WaylandSurfaceRenderElement<GlesRenderer>` in a `RoundedSurfaceRenderElement`
    /// so its corners are SDF-clipped in GLSL to match the window's `corner_radius`.
    pub fn clip_client_surface(
        &self,
        elem: WaylandSurfaceRenderElement<GlesRenderer>,
        window_origin_phys: Point<i32, Physical>,
        scale: Scale<f64>,
        window_size: Size<i32, Logical>,
        corner_radius: f32,
        has_titlebar: bool,
    ) -> RoundedSurfaceRenderElement {
        let elem_geo = elem.geometry(scale);
        let offset_phys = elem_geo.loc - window_origin_phys;
        let offset_log = offset_phys.to_f64().to_logical(scale);
        let size_log = elem.view().dst;

        let uniforms = vec![
            Uniform::new("u_surface_size", [size_log.w as f32, size_log.h as f32]),
            Uniform::new(
                "u_surface_offset",
                [offset_log.x as f32, offset_log.y as f32],
            ),
            Uniform::new(
                "u_window_size",
                [window_size.w as f32, window_size.h as f32],
            ),
            Uniform::new("u_corner_radius", corner_radius),
            Uniform::new("u_has_titlebar", if has_titlebar { 1.0f32 } else { 0.0f32 }),
        ];

        RoundedSurfaceRenderElement {
            inner: elem,
            program: self.client_clip_program.clone(),
            uniforms,
        }
    }

    /// Creates a `PixelShaderElement` for a window's translucent glass titlebar (`30px` height)
    /// with hover-reactive SDF Close, Maximize, and Tile/Float buttons and a drag pill.
    pub fn window_titlebar_element(
        &self,
        bar_rect: Rectangle<i32, Logical>,
        style_scalar: f32,
        corner_radius: f32,
        is_focused: bool,
        alpha: f32,
        pointer: Point<f64, Logical>,
    ) -> PixelShaderElement {
        let uniforms = vec![
            Uniform::new("u_style", style_scalar),
            Uniform::new("u_corner_radius", corner_radius),
            Uniform::new("u_focused", if is_focused { 1.0f32 } else { 0.0f32 }),
            Uniform::new(
                "u_bar_origin",
                [bar_rect.loc.x as f32, bar_rect.loc.y as f32],
            ),
            Uniform::new("u_pointer", [pointer.x as f32, pointer.y as f32]),
        ];

        PixelShaderElement::new(
            self.titlebar_program.clone(),
            bar_rect,
            None,
            alpha.clamp(0.0, 1.0),
            uniforms,
            Kind::Unspecified,
        )
    }

    /// Creates a full-output night light warmth color temperature `PixelShaderElement`.
    pub fn night_light_element(
        &self,
        output_geo: Rectangle<i32, Logical>,
        tint_color: [f32; 3],
        strength: f32,
        alpha: f32,
    ) -> PixelShaderElement {
        let uniforms = vec![
            Uniform::new("u_tint_color", tint_color),
            Uniform::new("u_strength", strength),
        ];

        PixelShaderElement::new(
            self.night_light_program.clone(),
            output_geo,
            None,
            alpha,
            uniforms,
            Kind::Unspecified,
        )
    }

    /// Creates an auto-hiding On-Screen Glass HUD Pill at the top-center of the output
    /// showing Workspace pills, the Floating/Tiling layout icon, and Visual Style orbs.
    pub fn hud_pill_element(
        &self,
        output_geo: Rectangle<i32, Logical>,
        hud_progress: f32,
        style_scalar: f32,
        active_workspace: usize,
        workspace_count: usize,
        is_tiling: bool,
    ) -> PixelShaderElement {
        let pill_w = 340;
        let pill_h = 54;
        let x = output_geo.loc.x + (output_geo.size.w - pill_w) / 2;
        // Spring-animated vertical slide-in from the top edge
        let slide_offset = ((1.0 - hud_progress.clamp(0.0, 1.2)) * -36.0).round() as i32;
        let y = output_geo.loc.y + 14 + slide_offset;
        let rect = Rectangle::new(Point::from((x, y)), Size::from((pill_w, pill_h)));

        let uniforms = vec![
            Uniform::new("u_style", style_scalar),
            Uniform::new("u_active_workspace", (active_workspace + 1) as f32),
            Uniform::new("u_workspace_count", workspace_count.clamp(1, 8) as f32),
            Uniform::new("u_is_tiling", if is_tiling { 1.0f32 } else { 0.0f32 }),
        ];

        PixelShaderElement::new(
            self.hud_pill_program.clone(),
            rect,
            None,
            hud_progress.clamp(0.0, 1.0),
            uniforms,
            Kind::Unspecified,
        )
    }

    /// Creates a full-output procedural wallpaper `PixelShaderElement`.
    pub fn wallpaper_element(
        &self,
        output_rect: Rectangle<i32, Logical>,
        style_scalar: f32,
        elapsed_secs: f32,
        pointer: Point<f64, Logical>,
        overview_progress: f32,
    ) -> PixelShaderElement {
        let uniforms = vec![
            Uniform::new("u_style", style_scalar),
            Uniform::new("u_time", elapsed_secs),
            Uniform::new("u_pointer", [pointer.x as f32, pointer.y as f32]),
            Uniform::new("u_overview", overview_progress.clamp(0.0, 1.0)),
        ];

        PixelShaderElement::new(
            self.wallpaper_program.clone(),
            output_rect,
            Some(vec![output_rect]),
            1.0,
            uniforms,
            Kind::Unspecified,
        )
    }

    /// Creates a `TextureShaderElement` that samples the live offscreen FBO `backdrop_texture`
    /// (containing background windows + wallpaper behind this window) and applies 13-tap
    /// Dual-Kawase blur (`normal-blur`), saturation bleed + grain (`frosted-glass`), or
    /// Snell's-law 3D SDF lens bevel refraction + RGB chromatic dispersion (`liquid-glass`).
    #[allow(clippy::too_many_arguments)]
    pub fn window_backdrop_texture_element(
        &self,
        renderer: &GlesRenderer,
        backdrop_texture: GlesTexture,
        scale: Scale<f64>,
        node: &BackdropEffectNode,
        style_cfg: &VisualStyleConfig,
        style_scalar: f32,
        element_alpha: f32,
        output_size: Size<i32, Logical>,
        pointer: Point<f64, Logical>,
        elapsed_secs: f32,
    ) -> TextureShaderElement {
        let tex_size = backdrop_texture.size();
        let tex_logical_rect = Rectangle::from_size(Size::<f64, Logical>::from((
            tex_size.w as f64,
            tex_size.h as f64,
        )));
        let phys_loc = node.expanded_rect.loc.to_f64().to_physical(scale);

        let tex_elem = TextureRenderElement::from_static_texture(
            Id::new(),
            renderer.context_id(),
            phys_loc,
            backdrop_texture,
            1,
            Transform::Normal,
            Some(element_alpha.clamp(0.0, 1.0)),
            Some(tex_logical_rect),
            Some(node.expanded_rect.size),
            None,
            Kind::Unspecified,
        );

        let uniforms = vec![
            Uniform::new("u_style", style_scalar),
            Uniform::new(
                "u_element_size",
                [
                    node.expanded_rect.size.w as f32,
                    node.expanded_rect.size.h as f32,
                ],
            ),
            Uniform::new(
                "u_window_size",
                [
                    node.content_rect.size.w as f32,
                    node.content_rect.size.h as f32,
                ],
            ),
            Uniform::new("u_padding", node.shadow_margin as f32),
            Uniform::new("u_corner_radius", node.corner_radius as f32),
            Uniform::new("u_shadow_elevation", style_cfg.shadow_elevation as f32),
            Uniform::new("u_blur_radius", style_cfg.blur_radius as f32),
            Uniform::new("u_opacity", style_cfg.opacity),
            Uniform::new("u_refraction_strength", style_cfg.refraction_strength),
            Uniform::new("u_noise_intensity", style_cfg.noise_intensity),
            Uniform::new("u_specular_intensity", style_cfg.specular_intensity),
            Uniform::new("u_focused", if node.is_focused { 1.0f32 } else { 0.0f32 }),
            Uniform::new(
                "u_is_preview",
                if node.is_glass_card { 1.0f32 } else { 0.0f32 },
            ),
            Uniform::new(
                "u_output_size",
                [output_size.w as f32, output_size.h as f32],
            ),
            Uniform::new(
                "u_element_origin",
                [
                    node.expanded_rect.loc.x as f32,
                    node.expanded_rect.loc.y as f32,
                ],
            ),
            Uniform::new("u_pointer", [pointer.x as f32, pointer.y as f32]),
            Uniform::new("u_time", elapsed_secs),
            Uniform::new("u_sample_y_offset", node.sample_y_offset as f32),
        ];

        TextureShaderElement::new(tex_elem, self.window_backdrop_tex_program.clone(), uniforms)
    }

    /// Creates a `PixelShaderElement` for an interactive Edge-Snap Preview glass card or analytic fallback.
    pub fn window_material_element(
        &self,
        node: &BackdropEffectNode,
        style_cfg: &VisualStyleConfig,
        style_scalar: f32,
        element_alpha: f32,
        output_size: Size<i32, Logical>,
        pointer: Point<f64, Logical>,
        elapsed_secs: f32,
    ) -> PixelShaderElement {
        let uniforms = vec![
            Uniform::new("u_style", style_scalar),
            Uniform::new(
                "u_window_size",
                [
                    node.content_rect.size.w as f32,
                    node.content_rect.size.h as f32,
                ],
            ),
            Uniform::new("u_padding", node.shadow_margin as f32),
            Uniform::new("u_corner_radius", node.corner_radius as f32),
            Uniform::new("u_shadow_elevation", style_cfg.shadow_elevation as f32),
            Uniform::new("u_opacity", style_cfg.opacity),
            Uniform::new("u_focused", if node.is_focused { 1.0f32 } else { 0.0f32 }),
            Uniform::new(
                "u_is_preview",
                if node.is_glass_card { 1.0f32 } else { 0.0f32 },
            ),
            Uniform::new(
                "u_output_size",
                [output_size.w as f32, output_size.h as f32],
            ),
            Uniform::new(
                "u_element_origin",
                [
                    node.expanded_rect.loc.x as f32,
                    node.expanded_rect.loc.y as f32,
                ],
            ),
            Uniform::new("u_pointer", [pointer.x as f32, pointer.y as f32]),
            Uniform::new("u_time", elapsed_secs),
            Uniform::new("u_sample_y_offset", node.sample_y_offset as f32),
        ];

        PixelShaderElement::new(
            self.window_material_program.clone(),
            node.expanded_rect,
            None,
            element_alpha.clamp(0.0, 1.0),
            uniforms,
            Kind::Unspecified,
        )
    }
}

#[inline]
pub fn style_to_float(style: VisualStyle) -> f32 {
    style.scalar_value() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_style_to_float_mapping() {
        assert_eq!(style_to_float(VisualStyle::LiquidGlass), 2.0);
        assert!(!WALLPAPER_FRAG_SRC.contains("#version"));
        assert!(!WINDOW_MATERIAL_FRAG_SRC.contains("#version"));
        assert!(WINDOW_BACKDROP_TEX_FRAG_SRC.contains("//_DEFINES_"));
        assert!(KAWASE_DOWNSAMPLE_FRAG_SRC.contains("//_DEFINES_"));
        assert!(KAWASE_UPSAMPLE_FRAG_SRC.contains("//_DEFINES_"));
    }
}
