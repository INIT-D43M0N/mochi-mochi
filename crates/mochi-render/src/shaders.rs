// SPDX-License-Identifier: GPL-3.0-or-later
//! GLSL Fragment Shaders for Mochi Mochi's Three Official Visual Languages:
//! 1. `normal-blur` (0.0)
//! 2. `frosted-glass` (1.0)
//! 3. `liquid-glass` (2.0)
//!
//! Includes:
//! - `WALLPAPER_FRAG_SRC`: Procedural atmospheric multi-harmonic wallpaper
//! - `WINDOW_BACKDROP_TEX_FRAG_SRC`: Real offscreen FBO 13-tap Dual-Kawase blur,
//!   Frosted saturation bleed + grain, and Liquid Glass Snell UV refraction + RGB dispersion
//! - `WINDOW_MATERIAL_FRAG_SRC`: Analytic fallback & Snap-Preview glass card shader
//! - `CLIENT_ROUNDED_CLIP_FRAG_SRC`: Subpixel SDF rounded corner clipping for Wayland client surfaces
//! - `TITLEBAR_FRAG_SRC`: Interactive translucent glass titlebar with hover-reactive Close/Max/Tile buttons
//! - `HUD_PILL_FRAG_SRC`: Auto-hiding On-Screen Glass HUD Pill (Workspaces + Layout Icon + Visual Style Orbs)

/// Procedural Atmospheric Wallpaper Shader (`WALLPAPER_FRAG_SRC`).
pub const WALLPAPER_FRAG_SRC: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;

uniform float u_style;
uniform float u_time;
uniform vec2 u_pointer;
uniform float u_overview;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

float hash12(vec2 p) {
    vec3 p3  = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

vec3 sample_atmosphere(vec2 uv, float style, float t) {
    vec2 p = uv * 2.0 - 1.0;
    p.x *= size.x / max(size.y, 1.0);

    float wave1 = sin(p.x * 2.1 + t * 0.35 + cos(p.y * 1.8 - t * 0.25));
    float wave2 = cos(p.y * 2.4 - t * 0.28 + sin(p.x * 2.7 + t * 0.20));
    float orb1 = exp(-length(p - vec2(-0.45 + 0.15 * sin(t * 0.4), -0.25)) * 1.8);
    float orb2 = exp(-length(p - vec2(0.50 + 0.12 * cos(t * 0.3), 0.30)) * 1.6);

    float w0 = clamp(1.0 - style, 0.0, 1.0);
    float w1 = clamp(1.0 - abs(style - 1.0), 0.0, 1.0);
    float w2 = clamp(style - 1.0, 0.0, 1.0);

    vec3 base = w0 * vec3(0.08, 0.10, 0.15)
              + w1 * vec3(0.10, 0.14, 0.22)
              + w2 * vec3(0.05, 0.11, 0.20);
    vec3 accent_a = w0 * vec3(0.20, 0.32, 0.58)
                  + w1 * vec3(0.28, 0.52, 0.84)
                  + w2 * vec3(0.12, 0.68, 0.88);
    vec3 accent_b = w0 * vec3(0.32, 0.22, 0.52)
                  + w1 * vec3(0.56, 0.34, 0.78)
                  + w2 * vec3(0.24, 0.42, 0.92);

    vec3 col = base;
    col += accent_a * (0.35 + 0.35 * wave1) * (0.45 + 0.75 * orb1);
    col += accent_b * (0.35 + 0.35 * wave2) * (0.45 + 0.75 * orb2);

    float vig = clamp(1.0 - 0.22 * dot(p, p), 0.65, 1.0);
    col *= vig;

    return col;
}

void main() {
    float ov = clamp(u_overview, 0.0, 1.0);
    vec2 zoomed_uv = mix(v_coords, (v_coords - 0.5) * 0.92 + 0.5, ov);
    vec3 col = sample_atmosphere(zoomed_uv, u_style, u_time);
    col *= mix(1.0, 0.72, ov);

    vec2 frag_px = v_coords * size;

    float grain = (hash12(frag_px) - 0.5) * (1.5 / 255.0);
    col += vec3(grain);

    gl_FragColor = vec4(col * alpha, alpha);
}
"#;

/// Subpixel SDF Rounded Corner Clipping Shader for Wayland Client Surfaces (`CLIENT_ROUNDED_CLIP_FRAG_SRC`).
pub const CLIENT_ROUNDED_CLIP_FRAG_SRC: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform vec2 u_surface_size;
uniform vec2 u_surface_offset;
uniform vec2 u_window_size;
uniform float u_corner_radius;
uniform float u_has_titlebar;

// Continuous G2-smooth squircle SDF (superellipse L^p norm, p = 2.55)
float lpNorm(vec2 v, float p) {
    vec2 a = max(v, 0.0);
    float m = max(a.x, a.y);
    if (m <= 0.0001) return 0.0;
    vec2 n = a / m;
    return m * pow(pow(n.x, p) + pow(n.y, p), 1.0 / p);
}

float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + vec2(r);
    return min(max(q.x, q.y), 0.0) + lpNorm(q, 2.55) - r;
}

void main() {
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif

    vec2 win_px = u_surface_offset + v_coords * u_surface_size;
    vec2 half_win = max(u_window_size * 0.5, vec2(1.0));
    vec2 p = win_px - half_win;
    float r = min(u_corner_radius, min(half_win.x, half_win.y));

    // When a top glass titlebar is attached (u_has_titlebar > 0.5), keep top corners square
    // so the client surface joins the bottom of the titlebar seamlessly, and round the bottom corners.
    float eff_r = (u_has_titlebar > 0.5 && p.y < 0.0) ? 0.0 : r;
    float d = sdRoundedBox(p, half_win, eff_r);
    float clip_mask = 1.0 - smoothstep(-1.15, 0.75, d);

    gl_FragColor = color * clip_mask;
}
"#;

/// Interactive Translucent Glass Titlebar Shader (`TITLEBAR_FRAG_SRC`).
pub const TITLEBAR_FRAG_SRC: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;

uniform float u_style;
uniform float u_corner_radius;
uniform float u_focused;
uniform vec2 u_bar_origin;
uniform vec2 u_pointer;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

float lpNormTop(vec2 v, float p) {
    vec2 a = max(v, 0.0);
    float m = max(a.x, a.y);
    if (m <= 0.0001) return 0.0;
    vec2 n = a / m;
    return m * pow(pow(n.x, p) + pow(n.y, p), 1.0 / p);
}

float sdRoundedBoxTop(vec2 p, vec2 b, float r) {
    float eff_r = (p.y < 0.0) ? r : 0.0;
    vec2 q = abs(p) - b + vec2(eff_r);
    return min(max(q.x, q.y), 0.0) + lpNormTop(q, 2.55) - eff_r;
}

void main() {
    vec2 frag_px = v_coords * size;
    vec2 half_sz = size * 0.5;
    vec2 p = frag_px - half_sz;
    float r = min(u_corner_radius, min(half_sz.x, size.y));

    float d = sdRoundedBoxTop(p, half_sz, r);
    if (d > 0.5) {
        gl_FragColor = vec4(0.0);
        return;
    }
    float edge_aa = 1.0 - smoothstep(-0.75, 0.5, d);

    float w0 = clamp(1.0 - u_style, 0.0, 1.0);
    float w1 = clamp(1.0 - abs(u_style - 1.0), 0.0, 1.0);
    float w2 = clamp(u_style - 1.0, 0.0, 1.0);

    vec3 base_col = w0 * vec3(0.15, 0.17, 0.24)
                  + w1 * vec3(0.24, 0.30, 0.42)
                  + w2 * vec3(0.12, 0.28, 0.42);
    base_col += vec3(0.05, 0.07, 0.10) * u_focused;

    // Top specular highlight & 1px bottom separator line
    float top_sheen = exp(-frag_px.y / 5.0) * (0.14 + 0.12 * u_focused);
    base_col += vec3(top_sheen);
    if (frag_px.y > size.y - 1.4) {
        base_col = mix(base_col, vec3(0.65, 0.78, 0.96), 0.28 + 0.22 * u_focused);
    }

    // Center drag pill handle (38px x 4px)
    vec2 pill_d = abs(frag_px - vec2(half_sz.x, half_sz.y)) - vec2(19.0, 1.5);
    float d_pill = min(max(pill_d.x, pill_d.y), 0.0) + length(max(pill_d, 0.0)) - 1.5;
    if (d_pill < 0.5) {
        float pill_aa = 1.0 - smoothstep(-0.6, 0.5, d_pill);
        vec3 pill_col = mix(vec3(0.55, 0.62, 0.75), vec3(0.78, 0.90, 1.00), u_focused);
        base_col = mix(base_col, pill_col, pill_aa * (0.45 + 0.35 * u_focused));
    }

    // Three interactive circular traffic-light buttons on the right:
    // 1. Tile/Float (x = w - 64), 2. Maximize (x = w - 42), 3. Close (x = w - 20)
    vec2 local_ptr = u_pointer - u_bar_origin;
    float cy = half_sz.y;

    vec2 c_tile = vec2(size.x - 64.0, cy);
    vec2 c_max  = vec2(size.x - 42.0, cy);
    vec2 c_close = vec2(size.x - 20.0, cy);

    float hov_tile = step(length(local_ptr - c_tile), 9.5);
    float hov_max  = step(length(local_ptr - c_max), 9.5);
    float hov_close = step(length(local_ptr - c_close), 9.5);

    float r_tile = 6.2 + 1.1 * hov_tile;
    float r_max  = 6.2 + 1.1 * hov_max;
    float r_close = 6.2 + 1.1 * hov_close;

    float d_tile_btn = length(frag_px - c_tile) - r_tile;
    if (d_tile_btn < 0.6) {
        float aa = 1.0 - smoothstep(-0.6, 0.6, d_tile_btn);
        vec3 col = mix(vec3(0.22, 0.78, 0.74), vec3(0.45, 0.98, 0.92), hov_tile);
        base_col = mix(base_col, col, aa * (0.80 + 0.20 * hov_tile));
    }

    float d_max_btn = length(frag_px - c_max) - r_max;
    if (d_max_btn < 0.6) {
        float aa = 1.0 - smoothstep(-0.6, 0.6, d_max_btn);
        vec3 col = mix(vec3(0.92, 0.72, 0.24), vec3(1.00, 0.88, 0.42), hov_max);
        base_col = mix(base_col, col, aa * (0.80 + 0.20 * hov_max));
    }

    float d_close_btn = length(frag_px - c_close) - r_close;
    if (d_close_btn < 0.6) {
        float aa = 1.0 - smoothstep(-0.6, 0.6, d_close_btn);
        vec3 col = mix(vec3(0.92, 0.34, 0.38), vec3(1.00, 0.50, 0.54), hov_close);
        base_col = mix(base_col, col, aa * (0.85 + 0.15 * hov_close));
    }

    float out_a = (0.78 + 0.14 * u_focused) * edge_aa * alpha;
    gl_FragColor = vec4(base_col * out_a, out_a);
}
"#;

/// Auto-Hiding On-Screen Glass HUD Pill Shader (`HUD_PILL_FRAG_SRC`).
/// Displays Workspace Dots/Pills (Left), Floating/Tiling Layout Icon (Center),
/// and Visual Style Indicator Orbs (Right).
pub const HUD_PILL_FRAG_SRC: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;

uniform float u_style;
uniform float u_active_workspace;
uniform float u_workspace_count;
uniform float u_is_tiling;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// Continuous G2-smooth squircle SDF (superellipse L^p norm, p = 2.55)
float lpNorm(vec2 v, float p) {
    vec2 a = max(v, 0.0);
    float m = max(a.x, a.y);
    if (m <= 0.0001) return 0.0;
    vec2 n = a / m;
    return m * pow(pow(n.x, p) + pow(n.y, p), 1.0 / p);
}

float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + vec2(r);
    return min(max(q.x, q.y), 0.0) + lpNorm(q, 2.55) - r;
}

void main() {
    vec2 frag_px = v_coords * size;
    vec2 center = size * 0.5;
    vec2 p = frag_px - center;
    // Outer 10px margin reserved for soft drop shadow
    vec2 half_pill = (size - vec2(20.0, 20.0)) * 0.5;
    float r = half_pill.y;

    float d = sdRoundedBox(p, half_pill, r);

    // Outer SDF drop shadow
    if (d > 0.5) {
        float d_sh = sdRoundedBox(p - vec2(0.0, 3.0), half_pill, r);
        float sh_a = exp(-max(d_sh, 0.0) / 5.5) * 0.48 * alpha;
        gl_FragColor = vec4(vec3(0.02, 0.03, 0.06) * sh_a, sh_a);
        return;
    }

    float edge_aa = 1.0 - smoothstep(-0.75, 0.5, d);
    float w0 = clamp(1.0 - u_style, 0.0, 1.0);
    float w1 = clamp(1.0 - abs(u_style - 1.0), 0.0, 1.0);
    float w2 = clamp(u_style - 1.0, 0.0, 1.0);

    vec3 bg = w0 * vec3(0.12, 0.14, 0.20)
            + w1 * vec3(0.18, 0.23, 0.34)
            + w2 * vec3(0.09, 0.22, 0.35);

    vec3 accent = w0 * vec3(0.48, 0.62, 0.95)
                + w1 * vec3(0.72, 0.84, 1.00)
                + w2 * vec3(0.32, 0.88, 1.00);

    // 1.5px glowing capsule border rim
    float rim = 1.0 - smoothstep(0.0, 1.8, -d);
    vec3 col = mix(bg, accent, rim * 0.72);

    // Local coordinates inside the pill interior
    vec2 inner_px = frag_px - vec2(10.0, 10.0);
    float cy = half_pill.y;

    // ------------------------------------------------------------------------
    // Left Section (x = 18..155): Workspace Pills (1..4)
    // ------------------------------------------------------------------------
    for (int i = 0; i < 4; i++) {
        float fi = float(i) + 1.0;
        if (fi <= u_workspace_count) {
            float is_act = 1.0 - clamp(abs(fi - u_active_workspace), 0.0, 1.0);
            float cx = 28.0 + float(i) * 32.0;
            vec2 half_dot = vec2(mix(5.0, 11.0, is_act), 5.0);
            float d_dot = sdRoundedBox(inner_px - vec2(cx, cy), half_dot, 5.0);
            if (d_dot < 0.6) {
                float aa = 1.0 - smoothstep(-0.6, 0.6, d_dot);
                vec3 dot_col = mix(vec3(0.42, 0.48, 0.60), accent, is_act);
                col = mix(col, dot_col, aa * mix(0.65, 1.0, is_act));
            }
        }
    }

    // Vertical divider 1 at x = 152
    if (abs(inner_px.x - 152.0) < 0.7 && abs(inner_px.y - cy) < half_pill.y * 0.55) {
        col = mix(col, vec3(0.65, 0.75, 0.90), 0.32);
    }

    // ------------------------------------------------------------------------
    // Center Section (x = 165..215): Layout Mode Icon (Floating vs Master-Stack Tiling)
    // ------------------------------------------------------------------------
    vec2 layout_center = vec2(188.0, cy);
    if (u_is_tiling > 0.5) {
        // Master-Stack Tiling icon: Left Master pane + 2 Right Stack panes
        float d_m = sdRoundedBox(inner_px - (layout_center + vec2(-6.5, 0.0)), vec2(5.0, 8.5), 1.8);
        float d_s1 = sdRoundedBox(inner_px - (layout_center + vec2(5.5, -4.5)), vec2(5.0, 3.5), 1.5);
        float d_s2 = sdRoundedBox(inner_px - (layout_center + vec2(5.5,  4.5)), vec2(5.0, 3.5), 1.5);
        float d_tile_icon = min(d_m, min(d_s1, d_s2));
        if (d_tile_icon < 0.6) {
            float aa = 1.0 - smoothstep(-0.6, 0.6, d_tile_icon);
            col = mix(col, vec3(0.40, 0.95, 0.86), aa * 0.95);
        }
    } else {
        // Floating icon: Two overlapping floating window cards
        float d_f1 = sdRoundedBox(inner_px - (layout_center + vec2(-3.5, -2.5)), vec2(7.0, 5.5), 2.0);
        float d_f2 = sdRoundedBox(inner_px - (layout_center + vec2( 3.5,  2.5)), vec2(7.0, 5.5), 2.0);
        if (d_f2 < 0.6) {
            float aa = 1.0 - smoothstep(-0.6, 0.6, d_f2);
            col = mix(col, accent, aa * 0.95);
        } else if (d_f1 < 0.6) {
            float aa = 1.0 - smoothstep(-0.6, 0.6, d_f1);
            col = mix(col, vec3(0.55, 0.66, 0.85), aa * 0.75);
        }
    }

    // Vertical divider 2 at x = 224
    if (abs(inner_px.x - 224.0) < 0.7 && abs(inner_px.y - cy) < half_pill.y * 0.55) {
        col = mix(col, vec3(0.65, 0.75, 0.90), 0.32);
    }

    // ------------------------------------------------------------------------
    // Right Section (x = 236..320): 3 Visual Style Orbs (Normal, Frosted, Liquid)
    // ------------------------------------------------------------------------
    for (int s = 0; s < 3; s++) {
        float fs = float(s);
        float active_w = clamp(1.0 - abs(u_style - fs), 0.0, 1.0);
        vec2 orb_c = vec2(248.0 + fs * 26.0, cy);
        float d_orb = length(inner_px - orb_c) - mix(4.8, 7.2, active_w);
        vec3 orb_col = (s == 0) ? vec3(0.45, 0.60, 0.92)
                     : (s == 1) ? vec3(0.78, 0.88, 1.00)
                                : vec3(0.25, 0.90, 0.98);
        if (d_orb < 0.6) {
            float aa = 1.0 - smoothstep(-0.6, 0.6, d_orb);
            col = mix(col, orb_col, aa * mix(0.45, 1.0, active_w));
        }
    }

    float out_a = 0.92 * edge_aa * alpha;
    gl_FragColor = vec4(col * out_a, out_a);
}
"#;

/// Real Offscreen FBO Texture Backdrop Shader (`WINDOW_BACKDROP_TEX_FRAG_SRC`).
pub const WINDOW_BACKDROP_TEX_FRAG_SRC: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float u_style;
uniform vec2 u_element_size;
uniform vec2 u_window_size;
uniform float u_padding;
uniform float u_corner_radius;
uniform float u_shadow_elevation;
uniform float u_blur_radius;
uniform float u_opacity;
uniform float u_refraction_strength;
uniform float u_noise_intensity;
uniform float u_specular_intensity;
uniform float u_focused;
uniform float u_is_preview;
uniform vec2 u_output_size;
uniform vec2 u_element_origin;
uniform vec2 u_pointer;
uniform float u_time;
uniform float u_sample_y_offset;

float hash12(vec2 p) {
    vec3 p3  = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

// Continuous G2-smooth squircle SDF (superellipse L^p norm, p = 2.55)
float lpNorm(vec2 v, float p) {
    vec2 a = max(v, 0.0);
    float m = max(a.x, a.y);
    if (m <= 0.0001) return 0.0;
    vec2 n = a / m;
    return m * pow(pow(n.x, p) + pow(n.y, p), 1.0 / p);
}

float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + vec2(r);
    return min(max(q.x, q.y), 0.0) + lpNorm(q, 2.55) - r;
}

vec2 gradRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + vec2(r);
    vec2 s = sign(p);
    if (q.x > 0.0 && q.y > 0.0) {
        return s * normalize(q);
    } else if (q.x > q.y) {
        return vec2(s.x, 0.0);
    } else {
        return vec2(0.0, s.y);
    }
}

vec3 sample_fbo_blur(vec2 screen_uv, float radius_px) {
    vec2 texel = vec2(max(radius_px, 1.0)) / max(u_output_size, vec2(1.0));
    vec2 r_inner = texel * 0.42;
    vec2 r_outer = texel * 0.95;

    vec3 acc = texture2D(tex, clamp(screen_uv, 0.001, 0.999)).rgb * 0.20;

    acc += texture2D(tex, clamp(screen_uv + vec2( r_inner.x,  r_inner.y), 0.001, 0.999)).rgb * 0.11;
    acc += texture2D(tex, clamp(screen_uv + vec2(-r_inner.x,  r_inner.y), 0.001, 0.999)).rgb * 0.11;
    acc += texture2D(tex, clamp(screen_uv + vec2( r_inner.x, -r_inner.y), 0.001, 0.999)).rgb * 0.11;
    acc += texture2D(tex, clamp(screen_uv + vec2(-r_inner.x, -r_inner.y), 0.001, 0.999)).rgb * 0.11;

    acc += texture2D(tex, clamp(screen_uv + vec2( r_outer.x, 0.0), 0.001, 0.999)).rgb * 0.045;
    acc += texture2D(tex, clamp(screen_uv + vec2(-r_outer.x, 0.0), 0.001, 0.999)).rgb * 0.045;
    acc += texture2D(tex, clamp(screen_uv + vec2(0.0,  r_outer.y), 0.001, 0.999)).rgb * 0.045;
    acc += texture2D(tex, clamp(screen_uv + vec2(0.0, -r_outer.y), 0.001, 0.999)).rgb * 0.045;
    vec2 diag = r_outer * 0.7071;
    acc += texture2D(tex, clamp(screen_uv + vec2( diag.x,  diag.y), 0.001, 0.999)).rgb * 0.045;
    acc += texture2D(tex, clamp(screen_uv + vec2(-diag.x,  diag.y), 0.001, 0.999)).rgb * 0.045;
    acc += texture2D(tex, clamp(screen_uv + vec2( diag.x, -diag.y), 0.001, 0.999)).rgb * 0.045;
    acc += texture2D(tex, clamp(screen_uv + vec2(-diag.x, -diag.y), 0.001, 0.999)).rgb * 0.045;

    return acc;
}

void main() {
    vec2 frag_px = v_coords * u_element_size;
    vec2 center = u_element_size * 0.5;
    vec2 p = frag_px - center;
    vec2 half_win = u_window_size * 0.5;
    float r = min(u_corner_radius, min(half_win.x, half_win.y));

    float d = sdRoundedBox(p, half_win, r);

    float w0 = clamp(1.0 - u_style, 0.0, 1.0);
    float w1 = clamp(1.0 - abs(u_style - 1.0), 0.0, 1.0);
    float w2 = clamp(u_style - 1.0, 0.0, 1.0);

    vec2 shadow_offset = vec2(0.0, max(u_shadow_elevation * 0.32, 3.0));
    float d_shadow = sdRoundedBox(p - shadow_offset, half_win, r);
    float sigma = max(u_shadow_elevation * 1.15, 6.0);

    float penumbra = exp(-max(d_shadow, 0.0) / sigma) * 0.52;
    float umbra = exp(-max(d, 0.0) / (sigma * 0.35)) * 0.28;
    float shadow_alpha = clamp((penumbra + umbra) * (0.75 + 0.25 * u_focused), 0.0, 0.85);

    vec3 glow_col = vec3(0.02, 0.03, 0.05);
    float aura = 0.0;
    if (u_focused > 0.5) {
        float aura_falloff = exp(-max(d, 0.0) / 5.5);
        vec3 target_aura_col = w0 * vec3(0.42, 0.56, 0.88)
                             + w1 * vec3(0.68, 0.80, 0.98)
                             + w2 * vec3(0.32, 0.86, 1.00);
        float aura_strength = w0 * 0.38 + w1 * 0.45 + w2 * 0.55;
        glow_col = mix(glow_col, target_aura_col, aura_falloff * 0.65);
        aura = aura_falloff * aura_strength;
    }

    float out_a = clamp(shadow_alpha + aura, 0.0, 0.92) * alpha;
    vec4 shadow_rgba = vec4(glow_col * out_a, out_a);

    if (d > 1.35) {
        gl_FragColor = shadow_rgba;
        return;
    }

    float edge_aa = 1.0 - smoothstep(-1.25, 1.25, d);
    vec2 screen_px = u_element_origin + frag_px;
    vec2 sample_px = vec2(screen_px.x, clamp(screen_px.y + u_sample_y_offset, 0.0, max(u_output_size.y - 1.0, 1.0)));
    vec2 screen_uv = sample_px / max(u_output_size, vec2(1.0));
    vec2 n_xy = gradRoundedBox(p, half_win, r);

    // Smooth inner-edge mask prevents bright rim colors from aliasing at d = 0.0
    float inner_aa = smoothstep(-0.25, 1.15, -d);

    // LIQUID GLASS: curved-lens refraction + clear center + crisp meniscus rim.
    float bevel_width = clamp(min(half_win.x, half_win.y) * 0.42, 18.0, 38.0);
    float bevel_factor = 1.0 - smoothstep(0.0, bevel_width, -d);
    float meniscus = pow(bevel_factor, 1.85);
    float refr_amt = clamp(u_refraction_strength * 0.45, 0.022, 0.040);
    vec2 refract_offset = n_xy * meniscus * refr_amt;
    float liquid_blur = max(u_blur_radius * 0.18, 2.0);
    vec3 bg_r = sample_fbo_blur(screen_uv - refract_offset * 1.02, liquid_blur);
    vec3 bg_g = sample_fbo_blur(screen_uv - refract_offset * 1.00, liquid_blur);
    vec3 bg_b = sample_fbo_blur(screen_uv - refract_offset * 0.98, liquid_blur);
    vec3 rgb_liquid_raw = vec3(bg_r.r, bg_g.g, bg_b.b);
    float luma_l = dot(rgb_liquid_raw, vec3(0.299, 0.587, 0.114));
    vec3 glass_rgb = mix(vec3(luma_l), rgb_liquid_raw, 1.16);

    float border_outer = (1.0 - smoothstep(0.4, 1.7, -d)) * inner_aa;
    glass_rgb = mix(glass_rgb, vec3(1.0), border_outer * 0.30);

    float normal_alpha = clamp(u_opacity, 0.0, 1.0);
    float eff_alpha = max(normal_alpha, 0.86);
    float glass_alpha = mix(eff_alpha, 0.56, u_is_preview) * alpha;
    vec4 glass_rgba = vec4(glass_rgb * glass_alpha, glass_alpha);
    gl_FragColor = mix(shadow_rgba, glass_rgba, edge_aa);
}
"#;

/// Analytic Per-Window & Snap-Preview Material Shader (`WINDOW_MATERIAL_FRAG_SRC`).
pub const WINDOW_MATERIAL_FRAG_SRC: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;

uniform float u_style;
uniform vec2 u_window_size;
uniform float u_padding;
uniform float u_corner_radius;
uniform float u_shadow_elevation;
uniform float u_opacity;
uniform float u_focused;
uniform float u_is_preview;
uniform vec2 u_output_size;
uniform vec2 u_element_origin;
uniform vec2 u_pointer;
uniform float u_time;
uniform float u_sample_y_offset;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

float hash12(vec2 p) {
    vec3 p3  = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

// Continuous G2-smooth squircle SDF (superellipse L^p norm, p = 2.55)
float lpNorm(vec2 v, float p) {
    vec2 a = max(v, 0.0);
    float m = max(a.x, a.y);
    if (m <= 0.0001) return 0.0;
    vec2 n = a / m;
    return m * pow(pow(n.x, p) + pow(n.y, p), 1.0 / p);
}

float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + vec2(r);
    return min(max(q.x, q.y), 0.0) + lpNorm(q, 2.55) - r;
}

vec2 gradRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + vec2(r);
    vec2 s = sign(p);
    if (q.x > 0.0 && q.y > 0.0) {
        return s * normalize(q);
    } else if (q.x > q.y) {
        return vec2(s.x, 0.0);
    } else {
        return vec2(0.0, s.y);
    }
}

vec3 sample_blurred_backdrop(vec2 screen_uv, float style, float t, float blur_norm) {
    vec2 p = screen_uv * 2.0 - 1.0;
    p.x *= u_output_size.x / max(u_output_size.y, 1.0);

    float freq_atten = mix(1.0, 0.55, clamp(blur_norm, 0.0, 1.0));
    float wave1 = sin(p.x * 2.1 * freq_atten + t * 0.35 + cos(p.y * 1.8 * freq_atten - t * 0.25));
    float wave2 = cos(p.y * 2.4 * freq_atten - t * 0.28 + sin(p.x * 2.7 * freq_atten + t * 0.20));
    float orb1 = exp(-length(p - vec2(-0.45 + 0.15 * sin(t * 0.4), -0.25)) * (1.8 * freq_atten));
    float orb2 = exp(-length(p - vec2(0.50 + 0.12 * cos(t * 0.3), 0.30)) * (1.6 * freq_atten));

    float w0 = clamp(1.0 - style, 0.0, 1.0);
    float w1 = clamp(1.0 - abs(style - 1.0), 0.0, 1.0);
    float w2 = clamp(style - 1.0, 0.0, 1.0);

    vec3 base = w0 * vec3(0.10, 0.12, 0.18)
              + w1 * vec3(0.13, 0.18, 0.28)
              + w2 * vec3(0.07, 0.15, 0.26);
    vec3 accent_a = w0 * vec3(0.22, 0.35, 0.62)
                  + w1 * vec3(0.34, 0.58, 0.90)
                  + w2 * vec3(0.16, 0.74, 0.94);
    vec3 accent_b = w0 * vec3(0.35, 0.24, 0.55)
                  + w1 * vec3(0.62, 0.38, 0.84)
                  + w2 * vec3(0.28, 0.48, 0.96);

    vec3 col = base;
    col += accent_a * (0.38 + 0.35 * wave1) * (0.50 + 0.70 * orb1);
    col += accent_b * (0.38 + 0.35 * wave2) * (0.50 + 0.70 * orb2);
    return col;
}

void main() {
    vec2 frag_px = v_coords * size;
    vec2 center = size * 0.5;
    vec2 p = frag_px - center;
    vec2 half_win = u_window_size * 0.5;
    float r = min(u_corner_radius, min(half_win.x, half_win.y));

    float d = sdRoundedBox(p, half_win, r);

    float w0 = clamp(1.0 - u_style, 0.0, 1.0);
    float w1 = clamp(1.0 - abs(u_style - 1.0), 0.0, 1.0);
    float w2 = clamp(u_style - 1.0, 0.0, 1.0);

    if (d > 0.5) {
        vec2 shadow_offset = vec2(0.0, max(u_shadow_elevation * 0.32, 3.0));
        float d_shadow = sdRoundedBox(p - shadow_offset, half_win, r);
        float sigma = max(u_shadow_elevation * 1.15, 6.0);

        float penumbra = exp(-max(d_shadow, 0.0) / sigma) * 0.52;
        float umbra = exp(-max(d, 0.0) / (sigma * 0.35)) * 0.28;
        float shadow_alpha = clamp((penumbra + umbra) * (0.75 + 0.25 * u_focused), 0.0, 0.85);

        vec3 glow_col = vec3(0.02, 0.03, 0.05);
        float aura = 0.0;
        if (u_focused > 0.5) {
            float aura_falloff = exp(-max(d, 0.0) / 5.5);
            vec3 target_aura_col = w0 * vec3(0.42, 0.56, 0.88)
                                 + w1 * vec3(0.68, 0.80, 0.98)
                                 + w2 * vec3(0.32, 0.86, 1.00);
            float aura_strength = w0 * 0.38 + w1 * 0.45 + w2 * 0.55;
            glow_col = mix(glow_col, target_aura_col, aura_falloff * 0.65);
            aura = aura_falloff * aura_strength;
        }

        float out_a = clamp(shadow_alpha + aura, 0.0, 0.92) * alpha;
        gl_FragColor = vec4(glow_col * out_a, out_a);
        return;
    }

    float edge_aa = 1.0 - smoothstep(-0.75, 0.5, d);
    vec2 screen_px = u_element_origin + frag_px;
    vec2 sample_px = vec2(screen_px.x, clamp(screen_px.y + u_sample_y_offset, 0.0, max(u_output_size.y - 1.0, 1.0)));
    vec2 screen_uv = sample_px / max(u_output_size, vec2(1.0));
    vec2 n_xy = gradRoundedBox(p, half_win, r);

    vec2 dyn_light = normalize(vec2(-0.55, -0.83));

    // 1. BLUR (0.0): pure blur only
    vec3 rgb_normal = sample_blurred_backdrop(screen_uv, u_style, u_time, 0.65);

    // 2. FROSTED GLASS (1.0): strong blur + saturate(1.5) + milky white 25% + grain + 1px white border, NO shine/distortion.
    vec3 bg_frosted = sample_blurred_backdrop(screen_uv, u_style, u_time, 0.92);
    float luma_f = dot(bg_frosted, vec3(0.299, 0.587, 0.114));
    vec3 saturated_f = mix(vec3(luma_f), bg_frosted, 1.50);
    vec3 rgb_frosted = mix(saturated_f, vec3(1.0, 1.0, 1.0), 0.25);
    float frost_grain = (hash12(screen_px * 0.85) - 0.5) * 0.032;
    rgb_frosted += vec3(frost_grain);
    float border_1px = 1.0 - smoothstep(0.0, 1.15, -d);
    rgb_frosted = mix(rgb_frosted, vec3(1.0), border_1px * 0.34);

    // 3. LIQUID GLASS (2.0): clean curved-lens refraction + moving white specular + 3D glass rim.
    float bevel_width = clamp(min(half_win.x, half_win.y) * 0.42, 18.0, 38.0);
    float bevel_factor = 1.0 - smoothstep(0.0, bevel_width, -d);
    float meniscus = pow(bevel_factor, 1.85);
    vec2 refract_offset = n_xy * meniscus * 0.032 * (0.35 + 0.65 * w2);
    vec3 bg_r = sample_blurred_backdrop(screen_uv - refract_offset * 1.03, u_style, u_time, 0.22);
    vec3 bg_g = sample_blurred_backdrop(screen_uv - refract_offset * 1.00, u_style, u_time, 0.22);
    vec3 bg_b = sample_blurred_backdrop(screen_uv - refract_offset * 0.97, u_style, u_time, 0.22);
    vec3 rgb_liquid_raw = vec3(bg_r.r, bg_g.g, bg_b.b);
    float luma_l = dot(rgb_liquid_raw, vec3(0.299, 0.587, 0.114));
    vec3 rgb_liquid = mix(vec3(luma_l), rgb_liquid_raw, 1.24);

    float moving_spec = pow(clamp(dot(n_xy, dyn_light), 0.0, 1.0), 4.0);
    float opposite_spec = pow(clamp(dot(n_xy, -dyn_light), 0.0, 1.0), 6.0) * 0.42;
    float fresnel = pow(bevel_factor, 2.2) * 0.14;
    rgb_liquid += vec3(1.0) * (fresnel + moving_spec * bevel_factor * 0.24);

    float inner_rim = smoothstep(0.0, 5.5, -d) * (1.0 - smoothstep(1.2, 9.0, -d));
    rgb_liquid = mix(rgb_liquid, vec3(1.0), inner_rim * (0.14 + 0.36 * (moving_spec + opposite_spec)));

    float border_outer = 1.0 - smoothstep(0.0, 1.4, -d);
    vec3 glass_edge_col = mix(vec3(0.82, 0.85, 0.90), vec3(1.0), clamp(moving_spec * 1.4 + opposite_spec, 0.0, 1.0));
    rgb_liquid = mix(rgb_liquid, glass_edge_col, border_outer * (0.52 + 0.42 * clamp(moving_spec + opposite_spec, 0.0, 1.0)));

    vec3 glass_rgb = w0 * rgb_normal + w1 * rgb_frosted + w2 * rgb_liquid;
    float preview_alpha = w0 * 0.42 + w1 * 0.50 + w2 * 0.56;
    float eff_alpha = w0 * 0.80 + w1 * u_opacity + w2 * max(u_opacity, 0.86);
    float glass_alpha = mix(eff_alpha, preview_alpha, u_is_preview) * edge_aa;

    gl_FragColor = vec4(glass_rgb * glass_alpha * alpha, glass_alpha * alpha);
}
"#;

/// Dual-Kawase 5-Tap Downsample Pass Shader (`KAWASE_DOWNSAMPLE_FRAG_SRC`).
///
/// Used in `OffscreenBackdropFbo` to progressively downsample the full-resolution
/// backdrop FBO (`1/1 -> 1/2 -> 1/4`) with bilinear hardware filtering and zero texel aliasing.
pub const KAWASE_DOWNSAMPLE_FRAG_SRC: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform vec2 u_half_texel;
uniform float u_offset;

void main() {
    vec2 d = u_half_texel * max(u_offset, 0.5);
    vec2 uv = clamp(v_coords, 0.001, 0.999);

    vec4 sum = texture2D(tex, uv) * 4.0;
    sum += texture2D(tex, clamp(uv + vec2(-d.x, -d.y), 0.001, 0.999));
    sum += texture2D(tex, clamp(uv + vec2( d.x, -d.y), 0.001, 0.999));
    sum += texture2D(tex, clamp(uv + vec2(-d.x,  d.y), 0.001, 0.999));
    sum += texture2D(tex, clamp(uv + vec2( d.x,  d.y), 0.001, 0.999));

    gl_FragColor = (sum * 0.125) * alpha;
}
"#;

/// Dual-Kawase 8-Tap Tent Upsample Pass Shader (`KAWASE_UPSAMPLE_FRAG_SRC`).
///
/// Used in `OffscreenBackdropFbo` to reconstruct a buttery-smooth, alias-free
/// blurred backdrop pyramid (`1/4 -> 1/2`) for large blur radii (`48..96px`).
pub const KAWASE_UPSAMPLE_FRAG_SRC: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform vec2 u_half_texel;
uniform float u_offset;

void main() {
    vec2 d = u_half_texel * max(u_offset, 0.5);
    vec2 uv = clamp(v_coords, 0.001, 0.999);

    vec4 sum = vec4(0.0);
    sum += texture2D(tex, clamp(uv + vec2(-d.x * 2.0, 0.0), 0.001, 0.999));
    sum += texture2D(tex, clamp(uv + vec2( d.x * 2.0, 0.0), 0.001, 0.999));
    sum += texture2D(tex, clamp(uv + vec2(0.0, -d.y * 2.0), 0.001, 0.999));
    sum += texture2D(tex, clamp(uv + vec2(0.0,  d.y * 2.0), 0.001, 0.999));

    sum += texture2D(tex, clamp(uv + vec2(-d.x,  d.y), 0.001, 0.999)) * 2.0;
    sum += texture2D(tex, clamp(uv + vec2( d.x,  d.y), 0.001, 0.999)) * 2.0;
    sum += texture2D(tex, clamp(uv + vec2(-d.x, -d.y), 0.001, 0.999)) * 2.0;
    sum += texture2D(tex, clamp(uv + vec2( d.x, -d.y), 0.001, 0.999)) * 2.0;

    gl_FragColor = (sum * (1.0 / 12.0)) * alpha;
}
"#;

/// Full-Screen Warmth & Color Temperature Shader (`NIGHT_LIGHT_FRAG_SRC`).
///
/// Blends an eye-soothing, non-linear warm tone (amber/candlelight) driven by
/// color temperature (1000K-10000K) or external gamma control tables (gammastep/wlsunset),
/// interpolated smoothly via physical spring physics (`Spring1D`).
pub const NIGHT_LIGHT_FRAG_SRC: &str = r#"

//_DEFINES_

precision mediump float;

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform vec3 u_tint_color;
uniform float u_strength; // 0.0 to 1.0 (spring animated)

void main() {
    // Non-linear warmth tone mapping with premultiplied alpha
    float a = clamp(alpha * u_strength, 0.0, 0.65);
    gl_FragColor = vec4(u_tint_color * a, a);
}
"#;
