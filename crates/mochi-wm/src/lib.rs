// SPDX-License-Identifier: GPL-3.0-or-later
//! Mochi Mochi Window Management, Hybrid Tiling, Glass Titlebars, HUD & Physics (`mochi-wm`)
//!
//! Coordinates workspaces, hybrid Floating + Master-Stack auto-tiling layouts,
//! interactive glass titlebars, on-screen HUD state, window snapping, focus cycling,
//! and physical spring animations (`mochi-anim`).

use std::collections::{HashMap, HashSet};

pub use mochi_anim::{BubblePhysicsState, Spring1D, SpringParams, WindowAnimationState};
use smithay::{
    desktop::{layer_map_for_output, Space, Window},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
    xwayland::X11Surface,
};
use tracing::info;

/// Interactive buttons located on the right side of a window's glass titlebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarButton {
    /// Closes the window (`xdg_toplevel.send_close`).
    Close,
    /// Toggles window maximization (`SnapZone::Maximize` / `SnapZone::CenterRestore`).
    Maximize,
    /// Toggles the workspace between Floating and Master-Stack Auto-Tiling.
    TileToggle,
}

/// Result of hit-testing a pointer click against a window's glass titlebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarHit {
    /// Pointer clicked on the draggable glass titlebar body.
    DragBar,
    /// Pointer clicked one of the three circular action buttons.
    Button(TitlebarButton),
}

/// Hit-tests `pointer` against a window's glass titlebar located immediately above `window_loc`.
/// The titlebar spans `[window_loc.x .. window_loc.x + window_width, window_loc.y - titlebar_height .. window_loc.y]`.
pub fn hit_test_titlebar(
    window_loc: Point<i32, Logical>,
    window_width: i32,
    titlebar_height: i32,
    pointer: Point<f64, Logical>,
) -> Option<TitlebarHit> {
    if titlebar_height <= 0 || window_width <= 0 {
        return None;
    }
    let bar_x = window_loc.x as f64;
    let bar_y = (window_loc.y - titlebar_height) as f64;
    let bar_w = window_width as f64;
    let bar_h = titlebar_height as f64;

    let local_x = pointer.x - bar_x;
    let local_y = pointer.y - bar_y;

    if local_x < 0.0 || local_x > bar_w || local_y < 0.0 || local_y > bar_h {
        return None;
    }

    let center_y = bar_h * 0.5;
    let hit_radius = 9.5;

    let close_x = bar_w - 20.0;
    let max_x = bar_w - 42.0;
    let tile_x = bar_w - 64.0;

    if ((local_x - close_x).powi(2) + (local_y - center_y).powi(2)).sqrt() <= hit_radius {
        return Some(TitlebarHit::Button(TitlebarButton::Close));
    }
    if ((local_x - max_x).powi(2) + (local_y - center_y).powi(2)).sqrt() <= hit_radius {
        return Some(TitlebarHit::Button(TitlebarButton::Maximize));
    }
    if ((local_x - tile_x).powi(2) + (local_y - center_y).powi(2)).sqrt() <= hit_radius {
        return Some(TitlebarHit::Button(TitlebarButton::TileToggle));
    }

    Some(TitlebarHit::DragBar)
}

/// Interactive click regions inside the On-Screen Glass HUD Pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudPillHit {
    /// Clicked workspace indicator dot (`0..workspace_count`).
    Workspace(usize),
    /// Clicked the center layout icon to toggle Floating / Master-Stack Tiling.
    ToggleLayout,
    /// Clicked one of the three Visual Style orbs (`NormalBlur`, `FrostedGlass`, `LiquidGlass`).
    SelectStyle(VisualStyle),
}

/// Hit-tests `pointer` against the top-center On-Screen Glass HUD Pill (`340x54` logical px).
pub fn hit_test_hud_pill(
    output_geo: Rectangle<i32, Logical>,
    hud_progress: f32,
    workspace_count: usize,
    pointer: Point<f64, Logical>,
) -> Option<HudPillHit> {
    if hud_progress <= 0.15 {
        return None;
    }
    let pill_w = 340.0;
    let pill_h = 54.0;
    let x = output_geo.loc.x as f64 + (output_geo.size.w as f64 - pill_w) * 0.5;
    let slide_offset = ((1.0 - hud_progress.clamp(0.0, 1.2)) * -36.0).round() as f64;
    let y = output_geo.loc.y as f64 + 14.0 + slide_offset;

    let frag_x = pointer.x - x;
    let frag_y = pointer.y - y;
    // Exclude the 10px outer SDF drop-shadow margin
    if frag_x < 10.0 || frag_x > (pill_w - 10.0) || frag_y < 10.0 || frag_y > (pill_h - 10.0) {
        return None;
    }

    let inner_x = frag_x - 10.0;
    let ws_max = workspace_count.clamp(1, 4);

    if inner_x < 152.0 {
        // Left section: Workspace pills centered at cx = 28.0 + i * 32.0
        let idx = ((inner_x - 12.0) / 32.0).floor().max(0.0) as usize;
        let clamped_idx = idx.min(ws_max.saturating_sub(1));
        Some(HudPillHit::Workspace(clamped_idx))
    } else if inner_x < 224.0 {
        // Center section: Layout mode icon
        Some(HudPillHit::ToggleLayout)
    } else {
        // Right section: Liquid Glass orb
        Some(HudPillHit::SelectStyle(VisualStyle::LiquidGlass))
    }
}

/// The official visual language of Mochi Mochi & Shoji.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualStyle {
    /// Translucent Liquid Glass look with curved-lens refraction and crisp meniscus rim.
    LiquidGlass,
}

impl VisualStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LiquidGlass => "liquid-glass",
        }
    }

    pub fn from_name(_name: &str) -> Self {
        Self::LiquidGlass
    }

    /// Numeric scalar target (`2.0`) for shader rendering.
    pub fn scalar_value(self) -> f64 {
        2.0
    }

    /// Cycle to the active visual style at runtime.
    pub fn next(self) -> Self {
        Self::LiquidGlass
    }

    /// Ambient canvas clear color `[R, G, B, A]` representing the style's base atmosphere.
    pub fn backdrop_clear_color(&self) -> [f32; 4] {
        [0.09, 0.16, 0.26, 1.0]
    }

    /// Active window focus border color `[R, G, B, A]` (pre-multiplied alpha).
    pub fn accent_border_color(&self) -> [f32; 4] {
        [0.38, 0.82, 0.98, 0.95]
    }

    /// Translucent snap preview zone color `[R, G, B, A]` (pre-multiplied alpha).
    pub fn snap_preview_color(&self) -> [f32; 4] {
        [0.15, 0.34, 0.46, 0.40]
    }
}

/// Per-workspace window arrangement mode (Hybrid Floating + Auto-Tiling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// Free-form floating windows with interactive edge/corner snapping.
    Floating,
    /// Dynamic Master-Stack auto-tiling with spring-animated geometry transitions.
    MasterStack,
}

impl LayoutMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Floating => "floating",
            Self::MasterStack => "tiling",
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "tiling" | "master-stack" | "tile" | "auto-tiling" => Self::MasterStack,
            _ => Self::Floating,
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::Floating => Self::MasterStack,
            Self::MasterStack => Self::Floating,
        }
    }
}

/// Computes `count` Master-Stack tiled window geometries within `usable_geo`.
pub fn compute_master_stack_slots(
    count: usize,
    usable_geo: Rectangle<i32, Logical>,
    outer_gap: i32,
    inner_gap: i32,
    master_ratio: f64,
    master_count: usize,
) -> Vec<Rectangle<i32, Logical>> {
    if count == 0 {
        return Vec::new();
    }

    let x0 = usable_geo.loc.x + outer_gap;
    let y0 = usable_geo.loc.y + outer_gap;
    let total_w = (usable_geo.size.w - outer_gap * 2).max(240);
    let total_h = (usable_geo.size.h - outer_gap * 2).max(180);

    let m_count = master_count.max(1);

    // If all windows fit in the master area, or only 1 window is present
    if count <= m_count {
        if count == 1 {
            return vec![Rectangle::new(
                Point::from((x0, y0)),
                Size::from((total_w, total_h)),
            )];
        }
        // Divide total_h among `count` windows vertically
        let c = count as i32;
        let total_gaps = inner_gap * (c - 1);
        let avail_h = (total_h - total_gaps).max(c * 60);
        let item_h = avail_h / c;
        let mut slots = Vec::with_capacity(count);
        for i in 0..c {
            let sy = y0 + i * (item_h + inner_gap);
            let h = if i == c - 1 {
                (y0 + total_h - sy).max(60)
            } else {
                item_h.max(60)
            };
            slots.push(Rectangle::new(
                Point::from((x0, sy)),
                Size::from((total_w, h)),
            ));
        }
        return slots;
    }

    // Both Master and Stack columns are populated
    let clamped_ratio = master_ratio.clamp(0.25, 0.75);
    let avail_w = (total_w - inner_gap).max(200);
    let master_w = ((avail_w as f64) * clamped_ratio).round() as i32;
    let stack_w = (avail_w - master_w).max(100);
    let stack_x = x0 + master_w + inner_gap;

    let mut slots = Vec::with_capacity(count);

    // Master pane windows (indices 0..m_count)
    let n_master = m_count as i32;
    let total_master_gaps = inner_gap * (n_master - 1);
    let avail_master_h = (total_h - total_master_gaps).max(n_master * 60);
    let item_master_h = avail_master_h / n_master;

    for i in 0..n_master {
        let sy = y0 + i * (item_master_h + inner_gap);
        let h = if i == n_master - 1 {
            (y0 + total_h - sy).max(60)
        } else {
            item_master_h.max(60)
        };
        slots.push(Rectangle::new(
            Point::from((x0, sy)),
            Size::from((master_w, h)),
        ));
    }

    // Stack pane windows (indices m_count..count)
    let stack_count = (count - m_count) as i32;
    let total_stack_gaps = inner_gap * (stack_count - 1).max(0);
    let avail_stack_h = (total_h - total_stack_gaps).max(stack_count * 60);
    let item_stack_h = avail_stack_h / stack_count;

    for i in 0..stack_count {
        let sy = y0 + i * (item_stack_h + inner_gap);
        let h = if i == stack_count - 1 {
            (y0 + total_h - sy).max(60)
        } else {
            item_stack_h.max(60)
        };
        slots.push(Rectangle::new(
            Point::from((stack_x, sy)),
            Size::from((stack_w, h)),
        ));
    }

    slots
}

/// Direction for swapping windows within tiling layout or cycling focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapDirection {
    Next,
    Prev,
    Up,
    Down,
    Left,
    Right,
    Master,
}

impl SwapDirection {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "master" => Some(Self::Master),
            "next" => Some(Self::Next),
            "prev" | "previous" => Some(Self::Prev),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

/// Screen snap targets for interactive drag-snapping and keyboard tiling shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapZone {
    LeftHalf,
    RightHalf,
    TopHalf,
    BottomHalf,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Maximize,
    CenterRestore,
}

impl SnapZone {
    /// Computes the target logical rectangle for this snap zone within `usable_geo` with an outer `gap`.
    pub fn target_geometry(
        self,
        usable_geo: Rectangle<i32, Logical>,
        gap: i32,
    ) -> Rectangle<i32, Logical> {
        let x = usable_geo.loc.x + gap;
        let y = usable_geo.loc.y + gap;
        let w = (usable_geo.size.w - gap * 2).max(200);
        let h = (usable_geo.size.h - gap * 2).max(150);
        let half_w = (w - gap) / 2;
        let half_h = (h - gap) / 2;

        match self {
            Self::LeftHalf => Rectangle::new(Point::from((x, y)), Size::from((half_w, h))),
            Self::RightHalf => Rectangle::new(
                Point::from((x + half_w + gap, y)),
                Size::from((half_w, h)),
            ),
            Self::TopHalf => Rectangle::new(Point::from((x, y)), Size::from((w, half_h))),
            Self::BottomHalf => Rectangle::new(
                Point::from((x, y + half_h + gap)),
                Size::from((w, half_h)),
            ),
            Self::TopLeft => Rectangle::new(Point::from((x, y)), Size::from((half_w, half_h))),
            Self::TopRight => Rectangle::new(
                Point::from((x + half_w + gap, y)),
                Size::from((half_w, half_h)),
            ),
            Self::BottomLeft => Rectangle::new(
                Point::from((x, y + half_h + gap)),
                Size::from((half_w, half_h)),
            ),
            Self::BottomRight => Rectangle::new(
                Point::from((x + half_w + gap, y + half_h + gap)),
                Size::from((half_w, half_h)),
            ),
            Self::Maximize => Rectangle::new(Point::from((x, y)), Size::from((w, h))),
            Self::CenterRestore => {
                let cw = (w * 2 / 3).max(360);
                let ch = (h * 2 / 3).max(240);
                let cx = x + (w - cw) / 2;
                let cy = y + (h - ch) / 2;
                Rectangle::new(Point::from((cx, cy)), Size::from((cw, ch)))
            }
        }
    }

    /// Detects if a pointer position near the edges/corners of `output_geo` triggers a snap zone.
    pub fn from_pointer_edge(
        pointer: Point<f64, Logical>,
        output_geo: Rectangle<i32, Logical>,
    ) -> Option<Self> {
        let edge_threshold = 36.0;
        let corner_threshold = 72.0;

        let px = pointer.x - output_geo.loc.x as f64;
        let py = pointer.y - output_geo.loc.y as f64;
        let w = output_geo.size.w as f64;
        let h = output_geo.size.h as f64;

        if px < 0.0 || py < 0.0 || px > w || py > h {
            return None;
        }

        let near_left = px <= edge_threshold;
        let near_right = px >= (w - edge_threshold);
        let near_top = py <= edge_threshold;
        let near_bottom = py >= (h - edge_threshold);

        if (near_left || near_top) && px <= corner_threshold && py <= corner_threshold {
            Some(Self::TopLeft)
        } else if (near_right || near_top) && px >= (w - corner_threshold) && py <= corner_threshold
        {
            Some(Self::TopRight)
        } else if (near_left || near_bottom)
            && px <= corner_threshold
            && py >= (h - corner_threshold)
        {
            Some(Self::BottomLeft)
        } else if (near_right || near_bottom)
            && px >= (w - corner_threshold)
            && py >= (h - corner_threshold)
        {
            Some(Self::BottomRight)
        } else if near_left {
            Some(Self::LeftHalf)
        } else if near_right {
            Some(Self::RightHalf)
        } else if near_top {
            Some(Self::Maximize)
        } else if near_bottom {
            Some(Self::BottomHalf)
        } else {
            None
        }
    }
}

/// Parameters controlling the rendering pipeline for the active [`VisualStyle`].
#[derive(Debug, Clone)]
pub struct VisualStyleConfig {
    pub style: VisualStyle,
    pub blur_radius: u32,
    pub opacity: f32,
    pub corner_radius: u32,
    pub shadow_elevation: u32,
    pub window_gap: i32,
    pub titlebar_height: i32,
    pub refraction_strength: f32,
    pub noise_intensity: f32,
    pub specular_intensity: f32,
}

impl Default for VisualStyleConfig {
    fn default() -> Self {
        Self {
            style: VisualStyle::LiquidGlass,
            blur_radius: 24,
            opacity: 0.82,
            corner_radius: 14,
            shadow_elevation: 16,
            window_gap: 10,
            titlebar_height: 0,
            refraction_strength: 0.038,
            noise_intensity: 0.025,
            specular_intensity: 0.45,
        }
    }
}

/// Declarative multi-monitor placement and scale rule parsed from `output "<name>"` blocks in `mochi.kdl`.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputConfig {
    /// Connector or backend output name (e.g. `"eDP-1"`, `"HDMI-A-1"`, `"DP-1"`, `"mochi-winit-0"`).
    pub name: String,
    /// Optional logical `(x, y)` coordinates in the global compositor coordinate space.
    pub position: Option<Point<i32, Logical>>,
    /// Optional fractional or integer output scale factor (`0.5..=4.0`).
    pub scale: Option<f64>,
}

/// Declarative per-window placement and styling rule parsed from `window-rule` blocks in `mochi.kdl`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowRule {
    /// Case-insensitive substring or exact match against the client's `xdg_toplevel` `app_id`.
    pub app_id_match: Option<String>,
    /// Case-insensitive substring or exact match against the client's `xdg_toplevel` `title`.
    pub title_match: Option<String>,
    /// Optional 0-based workspace index to automatically move the window to on map.
    pub target_workspace: Option<usize>,
    /// Optional override forcing the window to float (`Some(true)`) even in `MasterStack` tiling mode.
    pub floating: Option<bool>,
    /// Optional per-window opacity override (`0.1..=1.0`).
    pub opacity: Option<f32>,
}

impl WindowRule {
    /// Returns `true` if this rule matches the given `app_id` and `title`.
    pub fn matches(&self, app_id: Option<&str>, title: Option<&str>) -> bool {
        if self.app_id_match.is_none() && self.title_match.is_none() {
            return false;
        }
        if let Some(ref pat) = self.app_id_match {
            let pat_low = pat.to_ascii_lowercase();
            let Some(actual) = app_id else {
                return false;
            };
            if !actual.to_ascii_lowercase().contains(&pat_low) {
                return false;
            }
        }
        if let Some(ref pat) = self.title_match {
            let pat_low = pat.to_ascii_lowercase();
            let Some(actual) = title else {
                return false;
            };
            if !actual.to_ascii_lowercase().contains(&pat_low) {
                return false;
            }
        }
        true
    }
}

/// Computes a non-overlapping Expose / Mission-Control grid of `count` slots inside `usable_geo`.
pub fn compute_overview_grid_slots(
    count: usize,
    usable_geo: Rectangle<i32, Logical>,
    gap: i32,
    tb_h: i32,
) -> Vec<Rectangle<i32, Logical>> {
    if count == 0 {
        return Vec::new();
    }
    let cols = (count as f64).sqrt().ceil() as usize;
    let rows = count.div_ceil(cols);

    let outer_pad_x = (usable_geo.size.w / 14).max(36);
    let outer_pad_y = (usable_geo.size.h / 12).max(48);
    let cell_gap = (gap * 2).max(20);

    let avail_w = (usable_geo.size.w - outer_pad_x * 2 - cell_gap * (cols as i32 - 1).max(0)).max(200);
    let avail_h = (usable_geo.size.h - outer_pad_y * 2 - cell_gap * (rows as i32 - 1).max(0)).max(160);

    let cell_w = (avail_w / cols as i32).max(160);
    let cell_h = (avail_h / rows as i32).max(120);

    let mut slots = Vec::with_capacity(count);
    for idx in 0..count {
        let r = idx / cols;
        let c = idx % cols;
        // Center incomplete last row horizontally
        let items_in_row = if r == rows - 1 {
            count - r * cols
        } else {
            cols
        };
        let row_total_w = items_in_row as i32 * cell_w + (items_in_row as i32 - 1).max(0) * cell_gap;
        let row_start_x = usable_geo.loc.x + (usable_geo.size.w - row_total_w) / 2;

        let x = row_start_x + c as i32 * (cell_w + cell_gap);
        let y = usable_geo.loc.y + outer_pad_y + r as i32 * (cell_h + cell_gap) + tb_h.max(0);
        let h = (cell_h - tb_h.max(0)).max(100);

        slots.push(Rectangle::new(Point::from((x, y)), Size::from((cell_w, h))));
    }

    slots
}

/// Manages workspaces, hybrid layouts, glass titlebars, HUD pill, overview grid, scratchpad, and physical spring animations.
pub struct WindowManager {
    /// One `Space<Window>` per workspace index.
    workspaces: Vec<Space<Window>>,
    /// Per-workspace layout mode (`Floating` or `MasterStack`).
    workspace_layouts: Vec<LayoutMode>,
    /// Per-workspace Master pane width ratio (`0.25..=0.75`).
    master_ratios: Vec<f64>,
    /// Per-workspace Master pane window count (default 1, clamped 1..=5).
    master_counts: Vec<usize>,
    /// Currently active workspace index (0-based).
    active_workspace: usize,
    /// Cascading offset counter for newly mapped floating windows.
    cascade_step: i32,
    /// Active visual style configuration.
    pub visual_style: VisualStyleConfig,
    /// Declarative multi-monitor placement & scaling rules from `mochi.kdl`.
    pub output_configs: Vec<OutputConfig>,
    /// Declarative window placement & styling rules from `mochi.kdl`.
    pub window_rules: Vec<WindowRule>,
    /// Per-window opacity overrides applied via `window-rule`.
    pub window_opacity_overrides: HashMap<Window, f32>,
    /// Windows explicitly marked floating (excluded from `MasterStack` tiling).
    pub floating_overrides: HashSet<Window>,
    /// Tracks windows that have already had their initial `window-rule` evaluated.
    applied_rules: HashSet<Window>,
    /// Whether the interactive Expose / Window Overview grid (`Mod+W`) is currently active.
    pub overview_active: bool,
    /// Continuous spring-interpolated Expose / Overview progress (`0.0` = normal, `1.0` = Overview).
    pub overview_progress: Spring1D,
    /// Saved window geometries prior to entering Overview mode so exiting restores them cleanly.
    pre_overview_geometries: HashMap<Window, Rectangle<i32, Logical>>,
    /// Computed Expose / Overview target slots per window for 1:1 continuous pinch scrubbing.
    overview_slots: HashMap<Window, Rectangle<i32, Logical>>,
    /// Per-workspace minimized windows paired with their saved restore geometry.
    pub minimized_windows: HashMap<usize, Vec<(Window, Rectangle<i32, Logical>)>>,
    /// Windows currently playing a spring minimize-to-dock animation before hiding.
    pub minimizing_windows: Vec<(Window, WindowAnimationState)>,
    /// Optional window stashed in the quick-toggle dropdown scratchpad (`Mod+Minus`).
    scratchpad_window: Option<Window>,
    /// Whether the scratchpad window is currently visible on the active workspace.
    pub scratchpad_visible: bool,
    /// Optional translucent snap preview rectangle shown during interactive window drag.
    pub snap_preview: Option<Rectangle<i32, Logical>>,
    /// Last active snap preview rectangle (kept while collapsing back to 0.0).
    pub last_snap_preview: Option<Rectangle<i32, Logical>>,
    /// Spring progress (`0.0..=1.0`) for the Liquid Glass tiling bubble & simultaneous shrink-to-icon.
    pub snap_preview_progress: Spring1D,
    /// Currently dragged window and pointer location during interactive move/tiling.
    pub snap_drag_window: Option<(Window, Point<f64, Logical>)>,
    /// Per-window physical spring animation states (`mochi-anim`).
    pub window_animations: HashMap<Window, WindowAnimationState>,
    /// Horizontal camera slide spring for workspace transitions.
    pub workspace_slide_x: Spring1D,
    /// Continuous spring-interpolated visual style scalar (`0.0..=2.0`).
    pub style_morph: Spring1D,
    /// Visibility spring (`0.0` = hidden above top edge, `1.0` = visible) for the On-Screen Glass HUD Pill.
    pub hud_spring: Spring1D,
    /// Remaining seconds before the On-Screen Glass HUD Pill auto-hides.
    pub hud_timer_secs: f64,
}

impl WindowManager {
    pub fn new(workspace_count: usize, visual_style: VisualStyleConfig) -> Self {
        Self::new_with_layout(workspace_count, visual_style, LayoutMode::Floating)
    }

    pub fn new_with_layout(
        workspace_count: usize,
        visual_style: VisualStyleConfig,
        default_layout: LayoutMode,
    ) -> Self {
        let count = workspace_count.max(1);
        let mut workspaces = Vec::with_capacity(count);
        for _ in 0..count {
            workspaces.push(Space::default());
        }
        let initial_style_scalar = visual_style.style.scalar_value();
        let mut hud_spring = Spring1D::new(0.0, SpringParams::window_snap());
        hud_spring.set_target(1.0);
        Self {
            workspaces,
            workspace_layouts: vec![default_layout; count],
            master_ratios: vec![0.55; count],
            master_counts: vec![1; count],
            active_workspace: 0,
            cascade_step: 0,
            visual_style,
            output_configs: Vec::new(),
            window_rules: Vec::new(),
            window_opacity_overrides: HashMap::new(),
            floating_overrides: HashSet::new(),
            applied_rules: HashSet::new(),
            overview_active: false,
            overview_progress: Spring1D::new(0.0, SpringParams::style_morph()),
            pre_overview_geometries: HashMap::new(),
            overview_slots: HashMap::new(),
            minimized_windows: HashMap::new(),
            minimizing_windows: Vec::new(),
            scratchpad_window: None,
            scratchpad_visible: false,
            snap_preview: None,
            last_snap_preview: None,
            snap_preview_progress: Spring1D::new(0.0, SpringParams::style_morph()),
            snap_drag_window: None,
            window_animations: HashMap::new(),
            workspace_slide_x: Spring1D::new(0.0, SpringParams::workspace_slide()),
            style_morph: Spring1D::new(initial_style_scalar, SpringParams::style_morph()),
            hud_spring,
            hud_timer_secs: 2.2,
        }
    }

    /// Returns the effective opacity for `window`, honoring any matching `window-rule` override.
    /// Normal application windows default to 1.0 (opaque, like Mutter/GNOME/HarmonyOS).
    pub fn effective_window_opacity(&self, window: &Window) -> f32 {
        self.window_opacity_overrides
            .get(window)
            .copied()
            .unwrap_or(1.0)
    }

    /// Evaluates `self.window_rules` once for a newly committed window with `app_id` / `title`.
    pub fn apply_window_rules(
        &mut self,
        window: &Window,
        app_id: Option<&str>,
        title: Option<&str>,
    ) {
        if self.applied_rules.contains(window) || (app_id.is_none() && title.is_none()) {
            return;
        }
        self.applied_rules.insert(window.clone());

        let matching_rules: Vec<WindowRule> = self
            .window_rules
            .iter()
            .filter(|r| r.matches(app_id, title))
            .cloned()
            .collect();

        for rule in matching_rules {
            if let Some(op) = rule.opacity {
                self.window_opacity_overrides
                    .insert(window.clone(), op.clamp(0.1, 1.0));
            }
            if let Some(is_float) = rule.floating {
                if is_float {
                    self.floating_overrides.insert(window.clone());
                } else {
                    self.floating_overrides.remove(window);
                }
                if self.active_layout() == LayoutMode::MasterStack {
                    self.retile_active_workspace();
                }
            }
            if let Some(target_ws) = rule.target_workspace {
                let _ = self.move_window_to_workspace(window, target_ws);
            }
            info!(
                ?app_id,
                ?title,
                ?rule,
                "Applied matching KDL window-rule to Wayland window"
            );
        }
    }

    /// Toggles the spring-animated Expose / Window Overview grid (`Mod+W`) on the active workspace.
    pub fn toggle_overview(&mut self) -> bool {
        if self.overview_active {
            self.exit_overview();
            false
        } else {
            self.enter_overview()
        }
    }

    fn enter_overview(&mut self) -> bool {
        let Some(usable_geo) = self.usable_output_geometry() else {
            return false;
        };
        let windows: Vec<(Window, Point<i32, Logical>, Size<i32, Logical>)> = self
            .active_space()
            .elements()
            .map(|w| {
                let loc = self
                    .active_space()
                    .element_location(w)
                    .unwrap_or_else(|| Point::from((64, 96)));
                let sz = w.geometry().size;
                (w.clone(), loc, sz)
            })
            .collect();

        if windows.is_empty() {
            self.trigger_hud();
            return false;
        }

        self.overview_active = true;
        self.overview_progress.set_target(1.0);
        self.pre_overview_geometries.clear();
        self.overview_slots.clear();

        let gap = self.visual_style.window_gap.max(8);
        let tb_h = self.visual_style.titlebar_height.max(0);
        let slots = compute_overview_grid_slots(windows.len(), usable_geo, gap, tb_h);

        for ((window, current_loc, current_size), slot) in windows.into_iter().zip(slots) {
            self.pre_overview_geometries
                .insert(window.clone(), Rectangle::new(current_loc, current_size));
            self.overview_slots.insert(window.clone(), slot);

            let anim = self
                .window_animations
                .entry(window.clone())
                .or_insert_with(|| WindowAnimationState::new_opening(current_loc, current_size));
            anim.position.set_target(slot.loc.to_f64());
            anim.size_w.snap_to(current_size.w.max(120) as f64);
            anim.size_h.snap_to(current_size.h.max(80) as f64);
            anim.size_w.set_target(slot.size.w as f64);
            anim.size_h.set_target(slot.size.h as f64);
        }

        self.trigger_hud();
        info!(
            workspace = self.active_workspace_number(),
            windows = self.pre_overview_geometries.len(),
            "Entered Expose / Window Overview grid mode"
        );
        true
    }

    /// Exits Expose / Window Overview mode and animates all windows back to their normal layout.
    pub fn exit_overview(&mut self) {
        if !self.overview_active {
            return;
        }
        self.overview_active = false;
        self.overview_progress.set_target(0.0);
        self.overview_slots.clear();

        if self.active_layout() == LayoutMode::MasterStack {
            self.pre_overview_geometries.clear();
            self.retile_active_workspace();
        } else {
            let saved = std::mem::take(&mut self.pre_overview_geometries);
            for (window, rect) in saved {
                if let Some(anim) = self.window_animations.get_mut(&window) {
                    anim.position.set_target(rect.loc.to_f64());
                    anim.size_w.set_target(rect.size.w as f64);
                    anim.size_h.set_target(rect.size.h as f64);
                }
            }
        }

        self.trigger_hud();
        info!(
            workspace = self.active_workspace_number(),
            "Exited Expose / Window Overview grid mode"
        );
    }

    /// Prepares Overview slot targets for 1:1 continuous touchpad pinch scrubbing.
    pub fn begin_overview_pinch(&mut self) -> bool {
        let Some(usable_geo) = self.usable_output_geometry() else {
            return false;
        };
        let windows: Vec<(Window, Point<i32, Logical>, Size<i32, Logical>)> = self
            .active_space()
            .elements()
            .map(|w| {
                let loc = self
                    .active_space()
                    .element_location(w)
                    .unwrap_or_else(|| Point::from((64, 96)));
                let sz = w.geometry().size;
                (w.clone(), loc, sz)
            })
            .collect();
        if windows.is_empty() {
            return false;
        }

        if !self.overview_active && self.pre_overview_geometries.is_empty() {
            for (window, loc, sz) in &windows {
                self.pre_overview_geometries
                    .insert(window.clone(), Rectangle::new(*loc, *sz));
            }
        }

        let gap = self.visual_style.window_gap.max(8);
        let tb_h = self.visual_style.titlebar_height.max(0);
        let slots = compute_overview_grid_slots(windows.len(), usable_geo, gap, tb_h);
        self.overview_slots.clear();
        for ((window, _, _), slot) in windows.into_iter().zip(slots) {
            self.overview_slots.insert(window, slot);
        }
        true
    }

    /// Scrubs all workspace windows and `overview_progress` 1:1 between normal and Overview grid (`0.0..=1.0`).
    pub fn scrub_overview_pinch(&mut self, progress: f64) {
        let t = progress.clamp(0.0, 1.0);
        self.overview_progress.snap_to(t);

        for (window, slot) in &self.overview_slots {
            let Some(pre) = self.pre_overview_geometries.get(window) else {
                continue;
            };
            let lerp = |a: f64, b: f64| a + (b - a) * t;
            let x = lerp(pre.loc.x as f64, slot.loc.x as f64);
            let y = lerp(pre.loc.y as f64, slot.loc.y as f64);
            let w = lerp(pre.size.w.max(120) as f64, slot.size.w as f64);
            let h = lerp(pre.size.h.max(80) as f64, slot.size.h as f64);

            let anim = self
                .window_animations
                .entry(window.clone())
                .or_insert_with(|| WindowAnimationState::new_opening(pre.loc, pre.size));
            anim.position.snap_to(Point::from((x, y)));
            anim.size_w.snap_to(w);
            anim.size_h.snap_to(h);
        }

        let space = &mut self.workspaces[self.active_workspace];
        for (window, _) in &self.overview_slots {
            if let Some(anim) = self.window_animations.get(window) {
                space.map_element(window.clone(), anim.position.current_i32(), false);
            }
        }
    }

    /// Completes a 1:1 Overview pinch gesture by springing to either Overview (`true`) or normal layout (`false`).
    pub fn finish_overview_pinch(&mut self, commit_overview: bool) {
        if self.overview_slots.is_empty() {
            return;
        }
        if commit_overview {
            self.overview_active = true;
            self.overview_progress.set_target(1.0);
            for (window, slot) in &self.overview_slots {
                if let Some(anim) = self.window_animations.get_mut(window) {
                    anim.position.set_target(slot.loc.to_f64());
                    anim.size_w.set_target(slot.size.w as f64);
                    anim.size_h.set_target(slot.size.h as f64);
                }
            }
            self.trigger_hud();
        } else {
            self.overview_active = true;
            self.exit_overview();
        }
    }

    /// Minimizes `window` with a spring scale-down/fade-out flight toward the bottom dock edge.
    pub fn minimize_window(&mut self, window: &Window) -> bool {
        let ws = self.active_workspace;
        let loc = match self.workspaces[ws].element_location(window) {
            Some(l) => l,
            None => return false,
        };
        let win_size = window.geometry().size;
        let current_rect = self
            .window_animations
            .get(window)
            .map(|a| a.current_rect(win_size))
            .unwrap_or_else(|| Rectangle::new(loc, win_size));

        self.workspaces[ws].unmap_elem(window);
        self.window_animations.remove(window);
        self.pre_overview_geometries.remove(window);
        self.overview_slots.remove(window);
        self.minimized_windows
            .entry(ws)
            .or_default()
            .push((window.clone(), current_rect));

        let usable = self
            .usable_output_geometry()
            .unwrap_or_else(|| Rectangle::new(Point::from((0, 0)), Size::from((1280, 800))));
        let dock_target = Point::from((
            (usable.loc.x + usable.size.w / 2 - 40) as f64,
            (usable.loc.y + usable.size.h - 28) as f64,
        ));

        let mut anim = WindowAnimationState::new_opening(current_rect.loc, current_rect.size);
        anim.position.snap_to(current_rect.loc.to_f64());
        anim.size_w.snap_to(current_rect.size.w.max(120) as f64);
        anim.size_h.snap_to(current_rect.size.h.max(80) as f64);
        anim.alpha.snap_to(1.0);
        anim.position.set_target(dock_target);
        anim.size_w.set_target(80.0);
        anim.size_h.set_target(48.0);
        anim.alpha.set_target(0.0);
        self.minimizing_windows.push((window.clone(), anim));

        if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        }
        info!(
            workspace = self.active_workspace_number(),
            minimized_count = self.minimized_count(),
            "Minimized window with spring flight"
        );
        true
    }

    /// Restores the specified (or most recently) minimized window on the active workspace.
    pub fn restore_minimized_window(&mut self, target: Option<&Window>) -> Option<Window> {
        let ws = self.active_workspace;
        let list = self.minimized_windows.get_mut(&ws)?;
        let idx = if let Some(win) = target {
            list.iter().position(|(w, _)| w == win)?
        } else {
            list.len().checked_sub(1)?
        };
        let (window, saved_rect) = list.remove(idx);
        self.minimizing_windows.retain(|(w, _)| w != &window);

        let usable = self
            .usable_output_geometry()
            .unwrap_or_else(|| Rectangle::new(Point::from((0, 0)), Size::from((1280, 800))));
        let dock_origin = Point::from((
            usable.loc.x + usable.size.w / 2 - 40,
            usable.loc.y + usable.size.h - 28,
        ));

        let mut anim = WindowAnimationState::new_opening(dock_origin, Size::from((80, 48)));
        anim.position.snap_to(dock_origin.to_f64());
        anim.size_w.snap_to(80.0);
        anim.size_h.snap_to(48.0);
        anim.alpha.snap_to(0.0);
        anim.position.set_target(saved_rect.loc.to_f64());
        anim.size_w.set_target(saved_rect.size.w.max(120) as f64);
        anim.size_h.set_target(saved_rect.size.h.max(80) as f64);
        anim.alpha.set_target(1.0);
        self.window_animations.insert(window.clone(), anim);

        self.workspaces[ws].map_element(window.clone(), dock_origin, true);
        if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        }
        info!(
            workspace = self.active_workspace_number(),
            minimized_count = self.minimized_count(),
            "Restored minimized window with spring flight"
        );
        Some(window)
    }

    /// Number of currently minimized windows on the active workspace.
    pub fn minimized_count(&self) -> usize {
        self.minimized_windows
            .get(&self.active_workspace)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Stashes `window` into the dropdown scratchpad (`Mod+Shift+Minus`), or restores it to a normal
    /// workspace window if it was already the scratchpad window.
    pub fn move_to_scratchpad(&mut self, window: &Window) -> bool {
        if self.scratchpad_window.as_ref() == Some(window) {
            self.scratchpad_window = None;
            self.scratchpad_visible = false;
            self.floating_overrides.remove(window);
            if self.active_layout() == LayoutMode::MasterStack {
                self.retile_active_workspace();
            }
            self.trigger_hud();
            info!("Restored window from scratchpad to normal workspace management");
            return false;
        }

        self.scratchpad_window = Some(window.clone());
        self.scratchpad_visible = false;
        self.floating_overrides.insert(window.clone());
        self.workspaces[self.active_workspace].unmap_elem(window);
        if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        }
        self.trigger_hud();
        info!("Moved window into hidden dropdown scratchpad");
        true
    }

    /// Toggles showing or hiding the stashed dropdown scratchpad window (`Mod+Minus`) on the active workspace.
    /// Returns `Some(window)` when the scratchpad becomes visible and should receive keyboard focus.
    pub fn toggle_scratchpad(&mut self) -> Option<Window> {
        let scratch_win = self.scratchpad_window.clone()?;
        let is_mapped_here = self
            .active_space()
            .elements()
            .any(|w| w == &scratch_win);

        if self.scratchpad_visible && is_mapped_here {
            self.workspaces[self.active_workspace].unmap_elem(&scratch_win);
            self.scratchpad_visible = false;
            self.trigger_hud();
            info!("Hid dropdown scratchpad window");
            None
        } else {
            // Ensure it is unmapped from any previous workspace first
            for space in &mut self.workspaces {
                space.unmap_elem(&scratch_win);
            }

            let usable = self
                .usable_output_geometry()
                .unwrap_or_else(|| Rectangle::new(Point::from((0, 0)), Size::from((1280, 800))));
            let tb_h = self.visual_style.titlebar_height.max(0);
            let target_w = ((usable.size.w as f64) * 0.68).round() as i32;
            let target_h = ((usable.size.h as f64) * 0.62).round() as i32;
            let target_x = usable.loc.x + (usable.size.w - target_w) / 2;
            let target_y = usable.loc.y + 52 + tb_h;
            let target_loc = Point::from((target_x, target_y));
            let target_size = Size::from((target_w.max(320), target_h.max(220)));

            let mut anim = WindowAnimationState::new_opening(target_loc, target_size);
            // Slide down smoothly from the top of the screen
            anim.position
                .snap_to(Point::from((target_x as f64, (usable.loc.y - 140) as f64)));
            anim.position.set_target(target_loc.to_f64());

            let start_loc = anim.position.current_i32();
            self.window_animations.insert(scratch_win.clone(), anim);
            self.workspaces[self.active_workspace].map_element(
                scratch_win.clone(),
                start_loc,
                true,
            );
            self.workspaces[self.active_workspace].raise_element(&scratch_win, true);

            if let Some(toplevel) = scratch_win.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some(target_size);
                });
                toplevel.send_pending_configure();
            } else if let Some(x11) = scratch_win.x11_surface() {
                let _ = x11.configure(Some(Rectangle::new(start_loc, target_size)));
            }

            self.scratchpad_visible = true;
            self.trigger_hud();
            info!("Revealed dropdown scratchpad window on active workspace");
            Some(scratch_win)
        }
    }

    /// Pops up the On-Screen Glass HUD Pill for 1.8 seconds.
    pub fn trigger_hud(&mut self) {
        self.hud_timer_secs = 1.8;
        self.hud_spring.set_target(1.0);
    }

    /// Total number of workspaces.
    pub fn workspace_count(&self) -> usize {
        self.workspaces.len()
    }

    /// Returns the 0-based index of the currently active workspace.
    pub fn active_workspace_index(&self) -> usize {
        self.active_workspace
    }

    /// Returns the layout mode (`Floating` or `MasterStack`) of the active workspace.
    pub fn active_layout(&self) -> LayoutMode {
        self.workspace_layouts[self.active_workspace]
    }

    /// Sets the layout mode of the active workspace and animates windows into their new positions.
    pub fn set_active_layout(&mut self, mode: LayoutMode) {
        if self.workspace_layouts[self.active_workspace] == mode {
            self.trigger_hud();
            return;
        }
        self.workspace_layouts[self.active_workspace] = mode;
        self.trigger_hud();
        info!(
            workspace = self.active_workspace_number(),
            layout = mode.as_str(),
            "Switched workspace layout mode"
        );
        match mode {
            LayoutMode::MasterStack => self.retile_active_workspace(),
            LayoutMode::Floating => self.restore_floating_workspace(),
        }
    }

    /// Toggles the active workspace between `Floating` and `MasterStack` auto-tiling.
    pub fn toggle_active_layout(&mut self) -> LayoutMode {
        let next = self.active_layout().toggle();
        self.set_active_layout(next);
        next
    }

    /// Returns the Master pane width ratio (`0.25..=0.75`) for the active workspace.
    pub fn active_master_ratio(&self) -> f64 {
        self.master_ratios.get(self.active_workspace).copied().unwrap_or(0.55)
    }

    /// Sets the Master pane width ratio (`0.25..=0.75`) on the active workspace and re-tiles.
    pub fn set_master_ratio(&mut self, ratio: f64) {
        let new_ratio = ratio.clamp(0.25, 0.75);
        if let Some(r) = self.master_ratios.get_mut(self.active_workspace) {
            *r = new_ratio;
        }
        self.trigger_hud();
        info!(
            workspace = self.active_workspace_number(),
            master_ratio = new_ratio,
            "Set Master-Stack ratio"
        );
        if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        }
    }

    /// Adjusts the Master pane width ratio (`0.25..=0.75`) on the active workspace and re-tiles.
    pub fn adjust_master_ratio(&mut self, delta: f64) {
        let current = self.active_master_ratio();
        self.set_master_ratio(current + delta);
    }

    /// Returns the Master window count for the active workspace.
    pub fn active_master_count(&self) -> usize {
        self.master_counts.get(self.active_workspace).copied().unwrap_or(1)
    }

    /// Sets the Master window count on the active workspace (clamped 1..=5) and re-tiles.
    pub fn set_master_count(&mut self, count: usize) {
        let clamped = count.clamp(1, 5);
        if let Some(c) = self.master_counts.get_mut(self.active_workspace) {
            *c = clamped;
        }
        self.trigger_hud();
        info!(
            workspace = self.active_workspace_number(),
            master_count = clamped,
            "Updated Master window count"
        );
        if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        }
    }

    /// Increments or decrements the Master window count on the active workspace and re-tiles.
    pub fn adjust_master_count(&mut self, delta: i32) {
        let current = self.active_master_count() as i32;
        let next = (current + delta).clamp(1, 5) as usize;
        self.set_master_count(next);
    }

    /// Swaps the focused window with the primary Master (first) window in the active workspace.
    pub fn swap_with_master(&mut self, focused: &Window) {
        self.swap_window(focused, SwapDirection::Master);
    }

    /// Directionally swaps the focused window within the active workspace tiling layout.
    pub fn swap_window(&mut self, focused: &Window, direction: SwapDirection) {
        let m_count_raw = self.active_master_count();
        let space = &mut self.workspaces[self.active_workspace];
        let windows: Vec<(Window, Point<i32, Logical>)> = space
            .elements()
            .map(|w| {
                let loc = space.element_location(w).unwrap_or_else(|| Point::from((0, 0)));
                (w.clone(), loc)
            })
            .collect();

        if windows.len() < 2 {
            return;
        }
        let Some(cur_idx) = windows.iter().position(|(w, _)| w == focused) else {
            return;
        };

        let n = windows.len();
        let m_count = m_count_raw.min(n);

        let target_idx = match direction {
            SwapDirection::Master => 0,
            SwapDirection::Next => (cur_idx + 1) % n,
            SwapDirection::Prev => (cur_idx + n - 1) % n,
            SwapDirection::Up => {
                if cur_idx < m_count {
                    if cur_idx > 0 { cur_idx - 1 } else { m_count - 1 }
                } else {
                    if cur_idx > m_count { cur_idx - 1 } else { n - 1 }
                }
            }
            SwapDirection::Down => {
                if cur_idx < m_count {
                    if cur_idx + 1 < m_count { cur_idx + 1 } else { 0 }
                } else {
                    if cur_idx + 1 < n { cur_idx + 1 } else { m_count }
                }
            }
            SwapDirection::Left => {
                if cur_idx >= m_count {
                    0
                } else {
                    m_count.min(n - 1)
                }
            }
            SwapDirection::Right => {
                if cur_idx < m_count && n > m_count {
                    m_count
                } else {
                    0
                }
            }
        };

        if target_idx == cur_idx {
            return;
        }

        let mut reordered = windows;
        reordered.swap(cur_idx, target_idx);
        for (w, _) in &reordered {
            space.unmap_elem(w);
        }
        for (w, loc) in reordered {
            let is_focus = &w == focused;
            space.map_element(w, loc, is_focus);
        }

        if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        }
    }

    /// Computes Master-Stack slots for all windows on the active workspace and animates them via springs.
    pub fn retile_active_workspace(&mut self) {
        if self.active_layout() != LayoutMode::MasterStack {
            return;
        }
        let Some(usable_geo) = self.usable_output_geometry() else {
            return;
        };

        let windows: Vec<(Window, Point<i32, Logical>, Size<i32, Logical>)> = self
            .active_space()
            .elements()
            .filter(|w| !self.floating_overrides.contains(*w) && is_shoji_shell_window(w).is_none())
            .map(|w| {
                let loc = self
                    .active_space()
                    .element_location(w)
                    .unwrap_or_else(|| Point::from((64, 64)));
                let size = w.geometry().size;
                (w.clone(), loc, size)
            })
            .collect();

        if windows.is_empty() {
            return;
        }

        let gap = self.visual_style.window_gap.max(4);
        let tb_h = self.visual_style.titlebar_height.max(0);
        let ratio = self.active_master_ratio();
        let master_count = self.active_master_count();
        let slots = compute_master_stack_slots(
            windows.len(),
            usable_geo,
            gap,
            gap + tb_h / 2,
            ratio,
            master_count,
        );

        for ((window, current_loc, current_size), mut rect) in windows.into_iter().zip(slots) {
            // Reserve top space inside the slot for the glass titlebar
            if tb_h > 0 && rect.size.h > tb_h + 80 {
                rect.loc.y += tb_h;
                rect.size.h -= tb_h;
            }

            let anim = self
                .window_animations
                .entry(window.clone())
                .or_insert_with(|| WindowAnimationState::new_opening(current_loc, current_size));
            anim.position.set_target(rect.loc.to_f64());
            anim.size_w.set_target(rect.size.w as f64);
            anim.size_h.set_target(rect.size.h as f64);

            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Maximized);
                    state.states.set(xdg_toplevel::State::TiledLeft);
                    state.states.set(xdg_toplevel::State::TiledRight);
                    state.states.set(xdg_toplevel::State::TiledTop);
                    state.states.set(xdg_toplevel::State::TiledBottom);
                    state.size = Some(rect.size);
                });
                toplevel.send_pending_configure();
            } else if let Some(x11) = window.x11_surface() {
                let _ = x11.set_maximized(false);
                let _ = x11.configure(Some(rect));
            }
        }
    }

    fn restore_floating_workspace(&mut self) {
        let usable_loc = self
            .usable_output_geometry()
            .map(|g| g.loc)
            .unwrap_or_else(|| Point::from((0, 0)));
        let windows: Vec<(Window, Point<i32, Logical>, Size<i32, Logical>)> = self
            .active_space()
            .elements()
            .map(|w| {
                let loc = self
                    .active_space()
                    .element_location(w)
                    .unwrap_or_else(|| Point::from((usable_loc.x + 80, usable_loc.y + 96)));
                (w.clone(), loc, w.geometry().size)
            })
            .collect();

        for (idx, (window, current_loc, current_size)) in windows.into_iter().enumerate() {
            let target_loc = Point::from((
                usable_loc.x + 80 + (idx as i32 * 42) % 360,
                usable_loc.y + 96 + (idx as i32 * 38) % 260,
            ));
            let target_size = Size::from((760, 480));
            let anim = self
                .window_animations
                .entry(window.clone())
                .or_insert_with(|| WindowAnimationState::new_opening(current_loc, current_size));
            anim.position.set_target(target_loc.to_f64());
            anim.size_w.set_target(target_size.w as f64);
            anim.size_h.set_target(target_size.h as f64);

            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::TiledLeft);
                    state.states.unset(xdg_toplevel::State::TiledRight);
                    state.states.unset(xdg_toplevel::State::TiledTop);
                    state.states.unset(xdg_toplevel::State::TiledBottom);
                    state.size = Some(target_size);
                });
                toplevel.send_pending_configure();
            } else if let Some(x11) = window.x11_surface() {
                let _ = x11.set_maximized(false);
                let _ = x11.configure(Some(Rectangle::new(target_loc, target_size)));
            }
        }
    }

    /// Advances all active window, workspace, HUD, and visual-style springs by `dt_secs`.
    pub fn tick_animations(&mut self, dt_secs: f64) -> bool {
        let mut any_animating = false;

        if self.hud_timer_secs > 0.0 {
            self.hud_timer_secs = (self.hud_timer_secs - dt_secs).max(0.0);
            if self.hud_timer_secs == 0.0 {
                self.hud_spring.set_target(0.0);
            }
        }
        self.hud_spring.advance(dt_secs);
        if self.hud_spring.is_animating() {
            any_animating = true;
        }

        self.workspace_slide_x.advance(dt_secs);
        if self.workspace_slide_x.is_animating() {
            any_animating = true;
        }

        self.style_morph.advance(dt_secs);
        if self.style_morph.is_animating() {
            any_animating = true;
        }

        self.overview_progress.advance(dt_secs);
        if self.overview_progress.is_animating() {
            any_animating = true;
        }

        self.snap_preview_progress.advance(dt_secs);
        if self.snap_preview_progress.is_animating() {
            any_animating = true;
        } else if self.snap_preview.is_none() && self.snap_preview_progress.current <= 0.01 {
            self.last_snap_preview = None;
        }

        self.minimizing_windows.retain_mut(|(_, anim)| {
            anim.advance(dt_secs);
            let keep = anim.alpha.current > 0.02 && anim.is_animating();
            if keep {
                any_animating = true;
            }
            keep
        });

        let mut updates = Vec::new();
        for (win, anim) in &mut self.window_animations {
            anim.advance(dt_secs);
            if anim.is_animating() {
                any_animating = true;
                updates.push((win.clone(), anim.position.current_i32()));
            }
        }

        let space = &mut self.workspaces[self.active_workspace];
        for (win, new_loc) in updates {
            if space.elements().any(|w| w == &win) {
                space.map_element(win, new_loc, false);
            }
        }

        any_animating
    }

    /// Returns a reference to the active workspace's `Space<Window>`.
    pub fn active_space(&self) -> &Space<Window> {
        &self.workspaces[self.active_workspace]
    }

    /// Returns a mutable reference to the active workspace's `Space<Window>`.
    pub fn active_space_mut(&mut self) -> &mut Space<Window> {
        &mut self.workspaces[self.active_workspace]
    }

    /// Iterates immutably over all workspace spaces.
    pub fn all_spaces(&self) -> &[Space<Window>] {
        &self.workspaces
    }

    /// Finds the 0-based workspace index containing `window` (mapped or minimized).
    pub fn find_window_workspace(&self, window: &Window) -> Option<usize> {
        if let Some(idx) = self
            .workspaces
            .iter()
            .position(|space| space.elements().any(|w| w == window))
        {
            return Some(idx);
        }
        self.minimized_windows
            .iter()
            .find(|(_, list)| list.iter().any(|(w, _)| w == window))
            .map(|(&idx, _)| idx)
    }

    /// Iterates mutably over all workspace spaces.
    pub fn all_spaces_mut(&mut self) -> impl Iterator<Item = &mut Space<Window>> {
        self.workspaces.iter_mut()
    }

    /// Current 1-based workspace number for user-facing logs and IPC.
    pub fn active_workspace_number(&self) -> usize {
        self.active_workspace + 1
    }

    /// Switches to the given 0-based workspace index with a horizontal spring slide transition.
    pub fn switch_workspace(&mut self, index: usize) -> bool {
        if index < self.workspaces.len() && index != self.active_workspace {
            let direction = if index > self.active_workspace {
                1.0
            } else {
                -1.0
            };
            let slide_span = self
                .primary_output_geometry()
                .map(|g| g.size.w as f64 * 0.28)
                .unwrap_or(320.0);

            self.workspace_slide_x.current = direction * slide_span;
            self.workspace_slide_x.set_target(0.0);

            let old_idx = self.active_workspace;
            self.active_workspace = index;
            self.snap_preview = None;

            // Keep Shoji shell surfaces (wallpaper, bar, dock, launcher, control)
            // sticky across all workspaces in their exact locations and Z-order.
            let sticky: Vec<(Window, Point<i32, Logical>)> = self.workspaces[old_idx]
                .elements()
                .filter(|w| is_shoji_shell_window(w).is_some())
                .filter_map(|w| {
                    let loc = self.workspaces[old_idx].element_location(w)?;
                    Some((w.clone(), loc))
                })
                .collect();
            for (win, loc) in sticky {
                self.workspaces[old_idx].unmap_elem(&win);
                self.workspaces[self.active_workspace].map_element(win, loc, false);
            }
            enforce_shoji_z_order(self.active_space_mut());

            self.trigger_hud();
            info!(
                workspace = self.active_workspace_number(),
                layout = self.active_layout().as_str(),
                windows = self.active_space().elements().count(),
                "Switched active workspace (spring slide + HUD active)"
            );
            true
        } else {
            self.trigger_hud();
            false
        }
    }

    /// Moves a window from the active workspace to `target_index` (0-based).
    pub fn move_window_to_workspace(&mut self, window: &Window, target_index: usize) -> bool {
        if target_index >= self.workspaces.len() || target_index == self.active_workspace {
            return false;
        }
        let loc = self.workspaces[self.active_workspace]
            .element_location(window)
            .unwrap_or_else(|| Point::from((64, 96)));
        self.workspaces[self.active_workspace].unmap_elem(window);
        self.workspaces[target_index].map_element(window.clone(), loc, true);
        if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        }
        self.trigger_hud();
        info!(
            from_workspace = self.active_workspace + 1,
            to_workspace = target_index + 1,
            "Moved window to workspace"
        );
        true
    }

    /// Cycles focus to the next window in the active workspace and raises it.
    pub fn cycle_focus(&mut self) -> Option<Window> {
        let space = self.active_space_mut();
        let next_window = space.elements().next().cloned()?;
        space.raise_element(&next_window, true);
        for w in space.elements() {
            let is_top = w == &next_window;
            w.set_activated(is_top);
            send_window_configure(w);
        }
        Some(next_window)
    }

    /// Updates a window's position immediately (used during live mouse drag) and syncs its spring.
    pub fn move_window_immediate(&mut self, window: &Window, location: Point<i32, Logical>) {
        if let Some(anim) = self.window_animations.get_mut(window) {
            anim.position.snap_to(location.to_f64());
        }
        self.active_space_mut()
            .map_element(window.clone(), location, true);
    }

    /// Snaps `window` into the specified [`SnapZone`] using a physical spring transition.
    pub fn snap_window(&mut self, window: &Window, zone: SnapZone) {
        let Some(usable_geo) = self.usable_output_geometry() else {
            return;
        };
        let mut rect = zone.target_geometry(usable_geo, self.visual_style.window_gap);
        let tb_h = self.visual_style.titlebar_height.max(0);
        if tb_h > 0 && rect.size.h > tb_h + 80 {
            rect.loc.y += tb_h;
            rect.size.h -= tb_h;
        }

        let current_loc = self
            .active_space()
            .element_location(window)
            .unwrap_or(rect.loc);
        let current_size = window.geometry().size;

        let anim = self
            .window_animations
            .entry(window.clone())
            .or_insert_with(|| WindowAnimationState::new_opening(current_loc, current_size));
        anim.position.set_target(rect.loc.to_f64());
        anim.size_w.snap_to(current_size.w.max(120) as f64);
        anim.size_h.snap_to(current_size.h.max(80) as f64);
        anim.size_w.set_target(rect.size.w as f64);
        anim.size_h.set_target(rect.size.h as f64);

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                if zone == SnapZone::Maximize {
                    state.states.set(xdg_toplevel::State::Maximized);
                } else {
                    state.states.unset(xdg_toplevel::State::Maximized);
                }
                if zone == SnapZone::CenterRestore {
                    state.states.unset(xdg_toplevel::State::TiledLeft);
                    state.states.unset(xdg_toplevel::State::TiledRight);
                    state.states.unset(xdg_toplevel::State::TiledTop);
                    state.states.unset(xdg_toplevel::State::TiledBottom);
                } else {
                    state.states.set(xdg_toplevel::State::TiledLeft);
                    state.states.set(xdg_toplevel::State::TiledRight);
                    state.states.set(xdg_toplevel::State::TiledTop);
                    state.states.set(xdg_toplevel::State::TiledBottom);
                }
                state.size = Some(rect.size);
            });
            toplevel.send_pending_configure();
        } else if let Some(x11) = window.x11_surface() {
            let _ = x11.set_maximized(zone == SnapZone::Maximize);
            let _ = x11.configure(Some(rect));
        }

        self.active_space_mut()
            .map_element(window.clone(), current_loc, true);
        info!(?zone, ?rect, "Spring-snapping window to target geometry");
    }

    /// Toggles Maximize / CenterRestore state on `window`.
    pub fn toggle_maximize(&mut self, window: &Window) {
        if let Some(toplevel) = window.toplevel() {
            let is_max = toplevel
                .with_pending_state(|state| state.states.contains(xdg_toplevel::State::Maximized));
            if is_max {
                self.snap_window(window, SnapZone::CenterRestore);
            } else {
                self.snap_window(window, SnapZone::Maximize);
            }
        } else if let Some(x11) = window.x11_surface() {
            let is_max = x11.is_maximized();
            if is_max {
                self.snap_window(window, SnapZone::CenterRestore);
            } else {
                self.snap_window(window, SnapZone::Maximize);
            }
        }
    }

    /// Toggles fullscreen state on `window` with a spring transition.
    pub fn toggle_fullscreen(&mut self, window: &Window) {
        let Some(output_geo) = self.primary_output_geometry() else {
            return;
        };

        let is_fullscreen = if let Some(toplevel) = window.toplevel() {
            let currently_fs = toplevel.with_pending_state(|state| {
                let currently_fs = state.states.contains(xdg_toplevel::State::Fullscreen);
                if currently_fs {
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                    state.size = None;
                } else {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    state.size = Some(output_geo.size);
                }
                !currently_fs
            });
            toplevel.send_pending_configure();
            !currently_fs
        } else if let Some(x11) = window.x11_surface() {
            let currently_fs = x11.is_fullscreen();
            let _ = x11.set_fullscreen(!currently_fs);
            let target = if !currently_fs { Some(output_geo) } else { None };
            let _ = x11.configure(target);
            !currently_fs
        } else {
            return;
        };

        if is_fullscreen {
            let current_loc = self
                .active_space()
                .element_location(window)
                .unwrap_or(output_geo.loc);
            let current_size = window.geometry().size;
            let anim = self
                .window_animations
                .entry(window.clone())
                .or_insert_with(|| WindowAnimationState::new_opening(current_loc, current_size));
            anim.position.set_target(output_geo.loc.to_f64());
            anim.size_w.set_target(output_geo.size.w as f64);
            anim.size_h.set_target(output_geo.size.h as f64);
        } else if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        } else {
            self.snap_window(window, SnapZone::CenterRestore);
        }
    }

    /// Maps an output onto all workspaces at the given logical position.
    pub fn map_output(&mut self, output: &Output, location: Point<i32, Logical>) {
        for space in &mut self.workspaces {
            space.map_output(output, location);
        }
    }

    /// Unmaps a disconnected output from all workspaces.
    pub fn unmap_output(&mut self, output: &Output) {
        for space in &mut self.workspaces {
            space.unmap_output(output);
        }
    }

    /// Computes initial placement for a new window, starts its entrance spring animation,
    /// and re-tiles the workspace if `LayoutMode::MasterStack` is active.
    pub fn map_new_window(&mut self, window: Window, activate: bool) -> Point<i32, Logical> {
        let usable_loc = self
            .usable_output_geometry()
            .map(|g| g.loc)
            .unwrap_or_else(|| Point::from((0, 0)));
        let tb_h = self.visual_style.titlebar_height.max(0);
        let base_x = usable_loc.x + 64 + (self.cascade_step * 36) % 320;
        let base_y = usable_loc.y + (64 + tb_h) + (self.cascade_step * 36) % 240;
        self.cascade_step = self.cascade_step.wrapping_add(1);

        let target_loc = Point::from((base_x, base_y));
        let anim = WindowAnimationState::new_opening(target_loc, Size::from((640, 420)));
        let start_loc = anim.position.current_i32();
        self.window_animations.insert(window.clone(), anim);

        self.active_space_mut()
            .map_element(window, start_loc, activate);

        if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        }

        target_loc
    }

    /// Removes animation and rule state for a destroyed window and re-tiles if in auto-tiling mode.
    pub fn remove_window_animation(&mut self, window: &Window) {
        self.window_animations.remove(window);
        self.window_opacity_overrides.remove(window);
        self.floating_overrides.remove(window);
        self.applied_rules.remove(window);
        self.pre_overview_geometries.remove(window);
        self.overview_slots.remove(window);
        self.minimizing_windows.retain(|(w, _)| w != window);
        for list in self.minimized_windows.values_mut() {
            list.retain(|(w, _)| w != window);
        }
        if self.scratchpad_window.as_ref() == Some(window) {
            self.scratchpad_window = None;
            self.scratchpad_visible = false;
        }
        if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        }
    }

    /// Sets the active [`VisualStyle`] and triggers a smooth spring morph in the shader pipeline.
    pub fn set_visual_style(&mut self, style: VisualStyle) {
        self.visual_style.style = style;
        self.style_morph
            .set_target(self.visual_style.style.scalar_value());
        self.trigger_hud();
        info!(
            style = self.visual_style.style.as_str(),
            target_scalar = self.visual_style.style.scalar_value(),
            "Set Mochi Mochi visual style (spring morph + HUD active)"
        );
    }

    /// Cycles the active [`VisualStyle`] and triggers a smooth spring morph in the shader pipeline.
    pub fn cycle_visual_style(&mut self) -> VisualStyle {
        let next = self.visual_style.style.next();
        self.set_visual_style(next);
        next
    }

    /// Applies updated visual style parameters from a live KDL config reload.
    pub fn apply_live_style_config(&mut self, new_cfg: VisualStyleConfig) {
        self.visual_style = new_cfg;
        self.style_morph
            .set_target(self.visual_style.style.scalar_value());
        self.trigger_hud();
        if self.active_layout() == LayoutMode::MasterStack {
            self.retile_active_workspace();
        }
        info!(
            style = self.visual_style.style.as_str(),
            blur_radius = self.visual_style.blur_radius,
            opacity = self.visual_style.opacity,
            corner_radius = self.visual_style.corner_radius,
            window_gap = self.visual_style.window_gap,
            "Applied live KDL visual style configuration"
        );
    }

    /// Resolves any matching `output "<name>"` rule from `mochi.kdl` for a newly connected output.
    pub fn configured_output_placement(
        &self,
        output_name: &str,
        fallback_loc: Point<i32, Logical>,
    ) -> (Point<i32, Logical>, Option<f64>) {
        if let Some(cfg) = self
            .output_configs
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(output_name) || c.name == "*")
        {
            let loc = cfg.position.unwrap_or(fallback_loc);
            (loc, cfg.scale)
        } else {
            (fallback_loc, None)
        }
    }

    /// Activates and raises the window owning `surface` (used by `xdg_activation_v1` tokens).
    /// If the window resides on another workspace, switches to that workspace automatically.
    pub fn activate_surface(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<Window> {
        let mut found: Option<(usize, Window)> = None;
        for (ws_idx, space) in self.workspaces.iter().enumerate() {
            if let Some(w) = space
                .elements()
                .find(|w| window_wl_surface(w).as_ref() == Some(surface))
            {
                found = Some((ws_idx, w.clone()));
                break;
            }
        }

        let (ws_idx, window) = found?;
        if ws_idx != self.active_workspace {
            self.switch_workspace(ws_idx);
        }

        let space = &mut self.workspaces[self.active_workspace];
        space.raise_element(&window, true);
        enforce_shoji_z_order(space);
        for w in space.elements() {
            w.set_activated(w == &window);
            send_window_configure(w);
        }

        self.trigger_hud();
        info!(
            workspace = self.active_workspace_number(),
            "Activated window via xdg_activation_v1 token"
        );
        Some(window)
    }

    /// Returns the union bounding rectangle spanning all connected outputs on the active workspace.
    pub fn bounding_outputs_geometry(&self) -> Option<Rectangle<i32, Logical>> {
        let space = self.active_space();
        let mut iter = space.outputs().filter_map(|o| space.output_geometry(o));
        let first = iter.next()?;
        Some(iter.fold(first, |acc, g| acc.merge(g)))
    }

    /// Returns the geometry of the output containing `point` (falling back to the primary output).
    pub fn output_geometry_under(&self, point: Point<f64, Logical>) -> Option<Rectangle<i32, Logical>> {
        let space = self.active_space();
        space
            .outputs()
            .filter_map(|o| space.output_geometry(o))
            .find(|geo| geo.to_f64().contains(point))
            .or_else(|| self.primary_output_geometry())
    }

    /// Returns the full geometry of the primary output on the active workspace.
    pub fn primary_output_geometry(&self) -> Option<Rectangle<i32, Logical>> {
        let space = self.active_space();
        let output = space.outputs().next()?;
        space.output_geometry(output)
    }

    /// Returns the non-exclusive usable geometry of the primary output (respecting `wlr-layer-shell` docks/bars).
    pub fn usable_output_geometry(&self) -> Option<Rectangle<i32, Logical>> {
        let space = self.active_space();
        let output = space.outputs().next()?;
        let output_geo = space.output_geometry(output)?;
        let layer_map = layer_map_for_output(output);
        let mut zone = layer_map.non_exclusive_zone();
        zone.loc += output_geo.loc;

        // Reserve space for Shoji's top menu bar and bottom macOS dock when present.
        let mut top_reserved = 0;
        let mut bottom_reserved = 0;
        for w in space.elements() {
            if let Some(role) = is_shoji_shell_window(w) {
                let h = w.geometry().size.h.max(0);
                if role == "shoji-bar" && h > 0 {
                    top_reserved = top_reserved.max(h + 2);
                } else if role.starts_with("shoji-dock") && h > 0 {
                    bottom_reserved = bottom_reserved.max(h + 18);
                }
            }
        }
        if top_reserved > 0 {
            let min_y = output_geo.loc.y + top_reserved;
            if zone.loc.y < min_y {
                let delta = min_y - zone.loc.y;
                zone.loc.y = min_y;
                zone.size.h = (zone.size.h - delta).max(1);
            }
        }
        if bottom_reserved > 0 {
            let max_bottom = output_geo.loc.y + output_geo.size.h - bottom_reserved;
            let cur_bottom = zone.loc.y + zone.size.h;
            if cur_bottom > max_bottom {
                zone.size.h = (max_bottom - zone.loc.y).max(1);
            }
        }

        if zone.size.w > 0 && zone.size.h > 0 {
            Some(zone)
        } else {
            Some(output_geo)
        }
    }

    /// Searches across all workspaces for a window whose `app_id` or `title` matches `query`
    /// (case-insensitive substring), switches to its workspace if needed, and raises/activates it.
    pub fn focus_window_by_query(&mut self, query: &str) -> Option<Window> {
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return None;
        }

        let mut matched: Option<(usize, Window)> = None;
        for (ws_idx, space) in self.workspaces.iter().enumerate() {
            for win in space.elements().rev() {
                if is_shoji_shell_window(win).is_some() {
                    continue;
                }
                let (app_id, title) = window_metadata(win);
                let app_hit = app_id
                    .as_deref()
                    .map(|s| s.to_ascii_lowercase().contains(&q))
                    .unwrap_or(false);
                let title_hit = title
                    .as_deref()
                    .map(|s| s.to_ascii_lowercase().contains(&q))
                    .unwrap_or(false);
                if app_hit || title_hit {
                    matched = Some((ws_idx, win.clone()));
                    break;
                }
            }
            if matched.is_some() {
                break;
            }
        }

        let (ws_idx, window) = matched?;
        if ws_idx != self.active_workspace {
            self.switch_workspace(ws_idx);
        }

        let space = &mut self.workspaces[self.active_workspace];
        space.raise_element(&window, true);
        enforce_shoji_z_order(space);
        for w in space.elements() {
            w.set_activated(w == &window);
            send_window_configure(w);
        }
        self.trigger_hud();
        Some(window)
    }

    /// Finds a mapped `Window` matching the given `X11Surface` across all workspaces.
    pub fn find_window_by_x11(&self, surface: &X11Surface) -> Option<Window> {
        for space in self.workspaces.iter() {
            if let Some(w) = space
                .elements()
                .find(|w| w.x11_surface() == Some(surface))
            {
                return Some(w.clone());
            }
        }
        None
    }

    /// Serializes all mapped windows across all workspaces as a JSON response line for `mochi-msg windows`.
    pub fn list_windows_json(&self, focused: Option<&Window>) -> String {
        let mut entries = Vec::new();
        let mut seq_id = 1usize;

        for (ws_idx, space) in self.workspaces.iter().enumerate() {
            let ws_layout = self.workspace_layouts[ws_idx];
            for win in space.elements() {
                if is_shoji_shell_window(win).is_some() {
                    continue;
                }
                let (app_id, title) = window_metadata(win);
                let loc = space
                    .element_location(win)
                    .unwrap_or_else(|| Point::from((0, 0)));
                let size = win.geometry().size;
                let is_focused = focused == Some(win) && ws_idx == self.active_workspace;
                let is_floating =
                    ws_layout == LayoutMode::Floating || self.floating_overrides.contains(win);
                let opacity = self.effective_window_opacity(win);

                entries.push(format!(
                    "{{\"id\":{},\"app_id\":{:?},\"title\":{:?},\"workspace\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"focused\":{},\"floating\":{},\"opacity\":{:.2}}}",
                    seq_id,
                    app_id.unwrap_or_default(),
                    title.unwrap_or_default(),
                    ws_idx + 1,
                    loc.x,
                    loc.y,
                    size.w,
                    size.h,
                    is_focused,
                    is_floating,
                    opacity
                ));
                seq_id += 1;
            }
        }

        format!("{{\"ok\":true,\"windows\":[{}]}}\n", entries.join(","))
    }
}

/// Extracts `(app_id, title)` from a Wayland or X11 `Window`.
pub fn window_metadata(window: &Window) -> (Option<String>, Option<String>) {
    if let Some(toplevel) = window.toplevel() {
        with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .map(|data| {
                    let guard = data.lock().unwrap();
                    (guard.app_id.clone(), guard.title.clone())
                })
                .unwrap_or((None, None))
        })
    } else if let Some(x11) = window.x11_surface() {
        (Some(x11.class()), Some(x11.title()))
    } else {
        (None, None)
    }
}

/// Returns `Some(role)` if `window` is a Shoji GTK4 shell surface (`shoji-bar`,
/// `shoji-dock`, `shoji-launcher`, `shoji-control`, `shoji-wallpaper`) so
/// Mochi Mochi can render it undecorated in the shell layer and exclude it from tiling.
pub fn is_shoji_shell_window(window: &Window) -> Option<String> {
    let (app_id, title) = window_metadata(window);
    if app_id.as_deref() == Some("org.moncherry.Shoji") {
        if let Some(t) = title {
            if t.starts_with("shoji-") {
                return Some(t);
            }
        }
    }
    None
}

/// Returns `true` if `window` is the Shoji desktop wallpaper surface (`shoji-wallpaper`).
pub fn is_shoji_wallpaper(window: &Window) -> bool {
    is_shoji_shell_window(window).as_deref() == Some("shoji-wallpaper")
}

/// Returns `true` if a Shoji shell surface should receive keyboard focus when clicked
/// (currently `shoji-launcher` for its search bar; bar/dock/control/wallpaper keep app focus).
pub fn shoji_wants_keyboard_focus(window: &Window) -> bool {
    is_shoji_shell_window(window)
        .is_some_and(|role| role.starts_with("shoji-launcher"))
}

/// Enforces stable Z-order for Shoji shell surfaces inside `space`:
/// 1. `shoji-wallpaper` stays at the very bottom of the stack.
/// 2. Normal application windows stay in the middle, preserving their relative order.
/// 3. `shoji-bar`, `shoji-dock`, `shoji-control`, and `shoji-launcher` float on top.
pub fn enforce_shoji_z_order(space: &mut Space<Window>) {
    let all: Vec<Window> = space.elements().cloned().collect();
    if all.is_empty() {
        return;
    }

    // If a wallpaper exists and is not already at index 0, re-raise non-wallpaper windows
    // in their current relative order so wallpaper sinks to the bottom.
    let wallpaper_at_bottom = is_shoji_wallpaper(&all[0]);
    if !wallpaper_at_bottom && all.iter().any(is_shoji_wallpaper) {
        for w in all.iter().filter(|w| !is_shoji_wallpaper(w)) {
            space.raise_element(w, false);
        }
    }

    // Keep shell chrome (bar, dock, control center, launcher) above normal app windows.
    for w in &all {
        if let Some(role) = is_shoji_shell_window(w) {
            if role != "shoji-wallpaper" {
                space.raise_element(w, false);
            }
        }
    }
}

/// Retrieves the underlying `WlSurface` of a `Window` (both XDG toplevel and X11 surfaces).
pub fn window_wl_surface(window: &Window) -> Option<WlSurface> {
    if let Some(toplevel) = window.toplevel() {
        Some(toplevel.wl_surface().clone())
    } else if let Some(x11) = window.x11_surface() {
        x11.wl_surface()
    } else {
        None
    }
}

/// Dispatches a window close request (`xdg_toplevel.send_close` or `WM_DELETE_WINDOW` for X11).
pub fn send_window_close(window: &Window) {
    if let Some(toplevel) = window.toplevel() {
        toplevel.send_close();
    } else if let Some(x11) = window.x11_surface() {
        let _ = x11.close();
    }
}

/// Sends pending configure to a window (`toplevel.send_pending_configure` or synthetic X11 configure).
pub fn send_window_configure(window: &Window) {
    if let Some(toplevel) = window.toplevel() {
        toplevel.send_pending_configure();
    } else if let Some(x11) = window.x11_surface() {
        let _ = x11.configure(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_visual_style_cycle_and_morph_spring() {
        let mut wm = WindowManager::new(4, VisualStyleConfig::default());
        assert_eq!(wm.visual_style.style, VisualStyle::LiquidGlass);
        assert!((wm.style_morph.current - 2.0).abs() < 1e-5);

        assert_eq!(wm.cycle_visual_style(), VisualStyle::LiquidGlass);
        for _ in 0..90 {
            wm.tick_animations(1.0 / 60.0);
        }
        assert!((wm.style_morph.current - 2.0).abs() < 1e-3);
    }

    #[test]
    fn test_workspace_switching_and_slide_spring() {
        let mut wm = WindowManager::new(4, VisualStyleConfig::default());
        assert_eq!(wm.active_workspace_number(), 1);
        assert!(wm.switch_workspace(2));
        assert_eq!(wm.active_workspace_number(), 3);
        assert!(wm.workspace_slide_x.is_animating());

        for _ in 0..90 {
            wm.tick_animations(1.0 / 60.0);
        }
        assert!(!wm.workspace_slide_x.is_animating());
    }

    #[test]
    fn test_snap_zones_and_edge_detection() {
        let output = Rectangle::new(Point::from((0, 0)), Size::from((1920, 1080)));
        assert_eq!(
            SnapZone::from_pointer_edge(Point::from((5.0, 500.0)), output),
            Some(SnapZone::LeftHalf)
        );
        assert_eq!(
            SnapZone::from_pointer_edge(Point::from((1915.0, 500.0)), output),
            Some(SnapZone::RightHalf)
        );
        assert_eq!(
            SnapZone::from_pointer_edge(Point::from((10.0, 10.0)), output),
            Some(SnapZone::TopLeft)
        );
        assert_eq!(
            SnapZone::from_pointer_edge(Point::from((960.0, 4.0)), output),
            Some(SnapZone::Maximize)
        );
        assert_eq!(
            SnapZone::from_pointer_edge(Point::from((960.0, 540.0)), output),
            None
        );

        let left_rect = SnapZone::LeftHalf.target_geometry(output, 8);
        assert_eq!(left_rect.loc, Point::from((8, 8)));
        assert_eq!(left_rect.size, Size::from((948, 1064)));
    }

    #[test]
    fn test_master_stack_auto_tiling_layout() {
        let usable = Rectangle::new(Point::from((0, 0)), Size::from((1920, 1080)));
        let slots_1 = compute_master_stack_slots(1, usable, 10, 10, 0.55, 1);
        assert_eq!(slots_1.len(), 1);
        assert_eq!(slots_1[0].loc, Point::from((10, 10)));
        assert_eq!(slots_1[0].size, Size::from((1900, 1060)));

        let slots_3 = compute_master_stack_slots(3, usable, 10, 10, 0.50, 1);
        assert_eq!(slots_3.len(), 3);
        assert_eq!(slots_3[0].loc, Point::from((10, 10)));
        assert_eq!(slots_3[0].size.w, 945);
        assert_eq!(slots_3[0].size.h, 1060);
        assert_eq!(slots_3[1].loc, Point::from((965, 10)));
        assert_eq!(slots_3[2].loc.x, 965);
        assert!(slots_3[2].loc.y > slots_3[1].loc.y);

        // 2 Master windows + 2 Stack windows
        let slots_4_m2 = compute_master_stack_slots(4, usable, 10, 10, 0.50, 2);
        assert_eq!(slots_4_m2.len(), 4);
        // Master column (slots 0 and 1) at x = 10
        assert_eq!(slots_4_m2[0].loc.x, 10);
        assert_eq!(slots_4_m2[1].loc.x, 10);
        assert!(slots_4_m2[1].loc.y > slots_4_m2[0].loc.y);
        // Stack column (slots 2 and 3) at x = 965
        assert_eq!(slots_4_m2[2].loc.x, 965);
        assert_eq!(slots_4_m2[3].loc.x, 965);
        assert!(slots_4_m2[3].loc.y > slots_4_m2[2].loc.y);
    }

    #[test]
    fn test_master_count_and_ratio_adjustments() {
        let mut wm = WindowManager::new(4, VisualStyleConfig::default());
        assert_eq!(wm.active_master_count(), 1);
        assert!((wm.active_master_ratio() - 0.55).abs() < 1e-5);

        // Grow master ratio
        wm.adjust_master_ratio(0.05);
        assert!((wm.active_master_ratio() - 0.60).abs() < 1e-5);

        // Shrink master ratio
        wm.adjust_master_ratio(-0.15);
        assert!((wm.active_master_ratio() - 0.45).abs() < 1e-5);

        // Clamping master ratio (0.25..=0.75)
        wm.adjust_master_ratio(1.0);
        assert_eq!(wm.active_master_ratio(), 0.75);
        wm.adjust_master_ratio(-1.0);
        assert_eq!(wm.active_master_ratio(), 0.25);

        // Master count increment & decrement
        wm.adjust_master_count(1);
        assert_eq!(wm.active_master_count(), 2);
        wm.adjust_master_count(2);
        assert_eq!(wm.active_master_count(), 4);

        // Master count clamping (1..=5)
        wm.adjust_master_count(10);
        assert_eq!(wm.active_master_count(), 5);
        wm.adjust_master_count(-10);
        assert_eq!(wm.active_master_count(), 1);

        // Direction parsing
        assert_eq!(SwapDirection::from_str("next"), Some(SwapDirection::Next));
        assert_eq!(SwapDirection::from_str("prev"), Some(SwapDirection::Prev));
        assert_eq!(SwapDirection::from_str("up"), Some(SwapDirection::Up));
        assert_eq!(SwapDirection::from_str("down"), Some(SwapDirection::Down));
        assert_eq!(SwapDirection::from_str("left"), Some(SwapDirection::Left));
        assert_eq!(SwapDirection::from_str("right"), Some(SwapDirection::Right));
        assert_eq!(SwapDirection::from_str("master"), Some(SwapDirection::Master));
    }

    #[test]
    fn test_titlebar_hit_testing() {
        let win_loc = Point::from((100, 130));
        let win_w = 600;
        let tb_h = 30;

        // Close button is at x = 100 + 600 - 20 = 580, y = 100 + 15 = 115
        assert_eq!(
            hit_test_titlebar(win_loc, win_w, tb_h, Point::from((680.0, 115.0))),
            Some(TitlebarHit::Button(TitlebarButton::Close))
        );
        // Maximize button is at x = 100 + 600 - 42 = 658, y = 115
        assert_eq!(
            hit_test_titlebar(win_loc, win_w, tb_h, Point::from((658.0, 115.0))),
            Some(TitlebarHit::Button(TitlebarButton::Maximize))
        );
        // TileToggle button is at x = 100 + 600 - 64 = 636, y = 115
        assert_eq!(
            hit_test_titlebar(win_loc, win_w, tb_h, Point::from((636.0, 115.0))),
            Some(TitlebarHit::Button(TitlebarButton::TileToggle))
        );
        // Drag bar body in the center
        assert_eq!(
            hit_test_titlebar(win_loc, win_w, tb_h, Point::from((300.0, 115.0))),
            Some(TitlebarHit::DragBar)
        );
        // Inside client area (below titlebar)
        assert_eq!(
            hit_test_titlebar(win_loc, win_w, tb_h, Point::from((300.0, 160.0))),
            None
        );
    }

    #[test]
    fn test_window_rule_matching_and_overview_grid() {
        let rule = WindowRule {
            app_id_match: Some("firefox".to_string()),
            title_match: Some("Private".to_string()),
            target_workspace: Some(1),
            floating: Some(true),
            opacity: Some(0.92),
        };

        assert!(rule.matches(Some("org.mozilla.firefox"), Some("Mozilla Firefox (Private Browsing)")));
        assert!(!rule.matches(Some("org.mozilla.firefox"), Some("Normal Tab")));
        assert!(!rule.matches(Some("foot"), Some("Private")));

        let usable = Rectangle::new(Point::from((0, 0)), Size::from((1920, 1080)));
        let grid = compute_overview_grid_slots(4, usable, 12, 30);
        assert_eq!(grid.len(), 4);
        // 2x2 grid: top-left, top-right, bottom-left, bottom-right
        assert!(grid[1].loc.x > grid[0].loc.x);
        assert_eq!(grid[0].loc.y, grid[1].loc.y);
        assert!(grid[2].loc.y > grid[0].loc.y);
    }

    #[test]
    fn test_hud_pill_hit_testing() {
        let output = Rectangle::new(Point::from((0, 0)), Size::from((1920, 1080)));
        // HUD pill (340x54) is centered at x = (1920 - 340)/2 = 790, y = 14 when hud_progress = 1.0
        // Workspace 1 dot at inner_x = 28 -> screen_x = 790 + 10 + 28 = 828, screen_y = 41
        assert_eq!(
            hit_test_hud_pill(output, 1.0, 4, Point::from((828.0, 41.0))),
            Some(HudPillHit::Workspace(0))
        );
        // Workspace 3 dot at inner_x = 28 + 2*32 = 92 -> screen_x = 800 + 92 = 892
        assert_eq!(
            hit_test_hud_pill(output, 1.0, 4, Point::from((892.0, 41.0))),
            Some(HudPillHit::Workspace(2))
        );
        // Center Layout toggle icon at inner_x = 188 -> screen_x = 800 + 188 = 988
        assert_eq!(
            hit_test_hud_pill(output, 1.0, 4, Point::from((988.0, 41.0))),
            Some(HudPillHit::ToggleLayout)
        );
        // Right Style orbs select LiquidGlass
        assert_eq!(
            hit_test_hud_pill(output, 1.0, 4, Point::from((1048.0, 41.0))),
            Some(HudPillHit::SelectStyle(VisualStyle::LiquidGlass))
        );
        assert_eq!(
            hit_test_hud_pill(output, 1.0, 4, Point::from((1074.0, 41.0))),
            Some(HudPillHit::SelectStyle(VisualStyle::LiquidGlass))
        );
        assert_eq!(
            hit_test_hud_pill(output, 1.0, 4, Point::from((1100.0, 41.0))),
            Some(HudPillHit::SelectStyle(VisualStyle::LiquidGlass))
        );
        // Hidden HUD (hud_progress = 0.0) ignores clicks
        assert_eq!(
            hit_test_hud_pill(output, 0.0, 4, Point::from((988.0, 41.0))),
            None
        );
    }

    #[test]
    fn test_overview_progress_pinch_and_minimize_state() {
        let mut wm = WindowManager::new(4, VisualStyleConfig::default());
        assert_eq!(wm.overview_progress.current, 0.0);
        assert_eq!(wm.minimized_count(), 0);

        wm.scrub_overview_pinch(0.65);
        assert!((wm.overview_progress.current - 0.65).abs() < 1e-6);

        wm.overview_progress.set_target(1.0);
        for _ in 0..90 {
            wm.tick_animations(1.0 / 60.0);
        }
        assert!((wm.overview_progress.current - 1.0).abs() < 1e-2);
    }
}
