// SPDX-License-Identifier: GPL-3.0-or-later
//! Mochi Mochi Retained Effect Scene Graph & Regional Damage Expansion (`mochi-scene`)
//!
//! Computes expanded bounding boxes for analytic SDF drop shadows, rounded glass
//! frames, and regional blur/refraction damage dilation.

use mochi_wm::{VisualStyle, VisualStyleConfig};
use smithay::utils::{Logical, Point, Rectangle, Size};

/// Computed geometry for a window or layer surface's backdrop material & shadow pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackdropEffectNode {
    /// Logical rectangle of the client window content.
    pub content_rect: Rectangle<i32, Logical>,
    /// Outer margin (in logical pixels) reserved for the analytic SDF drop shadow & bevel glow.
    pub shadow_margin: i32,
    /// Total expanded logical rectangle (`content_rect` inflated by `shadow_margin` on all 4 sides).
    pub expanded_rect: Rectangle<i32, Logical>,
    /// Effective sampling radius (in logical pixels) for background blur/refraction damage dilation.
    pub kernel_radius: i32,
    /// Corner radius in logical pixels.
    pub corner_radius: i32,
    /// Whether this node represents the currently focused active window.
    pub is_focused: bool,
    /// Whether this node is a full-surface translucent glass card (e.g., Snap Preview or Layer Dock).
    pub is_glass_card: bool,
    /// Vertical backdrop sampling offset in logical pixels (used by `shoji-bar` to sample a maximized window below it).
    pub sample_y_offset: i32,
}

impl BackdropEffectNode {
    /// Constructs a `BackdropEffectNode` for a window or glass preview rectangle using `style_cfg`.
    pub fn from_window_rect(
        content_rect: Rectangle<i32, Logical>,
        style_cfg: &VisualStyleConfig,
        is_focused: bool,
        is_glass_card: bool,
    ) -> Self {
        let elevation = style_cfg.shadow_elevation as i32;
        let shadow_margin = (elevation * 2 + 12).max(16);
        let kernel_radius = match style_cfg.style {
            VisualStyle::LiquidGlass => (style_cfg.blur_radius as i32 * 2).max(32),
        };

        let expanded_rect = Rectangle::new(
            Point::from((
                content_rect.loc.x - shadow_margin,
                content_rect.loc.y - shadow_margin,
            )),
            Size::from((
                content_rect.size.w + shadow_margin * 2,
                content_rect.size.h + shadow_margin * 2,
            )),
        );

        Self {
            content_rect,
            shadow_margin,
            expanded_rect,
            kernel_radius,
            corner_radius: style_cfg.corner_radius as i32,
            is_focused,
            is_glass_card,
            sample_y_offset: 0,
        }
    }
}

/// Implements Regional Backdrop Damage Expansion (Section 7.1 of Technical Report):
///
/// Whenever an underlying layer produces a damage rectangle `under_damage` that
/// intersects a `BackdropEffectNode`'s `content_rect`, the damage region must be
/// dilated by `node.kernel_radius` and clipped to `node.content_rect` so blurred
/// or refracted pixels never leave stale artifacts.
///
/// $$D_{\text{expanded}} = (D_{\text{under}} \oplus B(R_{\text{eff}})) \cap \text{Bounds}(E)$$
pub fn expand_backdrop_damage(
    under_damage: &[Rectangle<i32, Logical>],
    effect_nodes: &[BackdropEffectNode],
) -> Vec<Rectangle<i32, Logical>> {
    let mut result = Vec::with_capacity(under_damage.len() + effect_nodes.len());
    result.extend_from_slice(under_damage);

    for node in effect_nodes {
        if node.kernel_radius <= 0 {
            continue;
        }
        for dmg in under_damage {
            if let Some(intersection) = dmg.intersection(node.content_rect) {
                let r = node.kernel_radius;
                let dilated = Rectangle::new(
                    Point::from((intersection.loc.x - r, intersection.loc.y - r)),
                    Size::from((intersection.size.w + r * 2, intersection.size.h + r * 2)),
                );
                if let Some(clipped) = dilated.intersection(node.content_rect) {
                    result.push(clipped);
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backdrop_node_geometry_and_damage_expansion() {
        let cfg = VisualStyleConfig::default(); // FrostedGlass, blur_radius=24, shadow_elevation=16
        let win_rect = Rectangle::new(Point::from((100, 100)), Size::from((800, 600)));
        let node = BackdropEffectNode::from_window_rect(win_rect, &cfg, true, false);

        // shadow_margin = 16 * 2 + 12 = 44
        assert_eq!(node.shadow_margin, 44);
        assert_eq!(node.expanded_rect.loc, Point::from((56, 56)));
        assert_eq!(node.expanded_rect.size, Size::from((888, 688)));
        // LiquidGlass kernel_radius = 24 * 2 = 48
        assert_eq!(node.kernel_radius, 48);

        // A small 10x10 cursor blink at (200, 200) inside the window bounds
        let under_dmg = [Rectangle::new(Point::from((200, 200)), Size::from((10, 10)))];
        let expanded = expand_backdrop_damage(&under_dmg, &[node]);
        assert_eq!(expanded.len(), 2);
        assert_eq!(
            expanded[1],
            Rectangle::new(Point::from((152, 152)), Size::from((106, 106)))
        );
    }
}

