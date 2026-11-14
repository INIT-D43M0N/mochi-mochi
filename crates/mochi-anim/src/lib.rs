// SPDX-License-Identifier: GPL-3.0-or-later
//! Mochi Mochi Physical Spring & Timeline Animation Engine (`mochi-anim`)
//!
//! Provides interruptible damped harmonic oscillator (spring) solvers for
//! fluid window snapping, opening transitions, workspace sliding, and live
//! visual-style morphing.

use smithay::utils::{Logical, Point, Rectangle, Size};

/// Physical parameters for a damped harmonic oscillator:
/// $$\ddot{x} + 2 \zeta \omega_0 \dot{x} + \omega_0^2 (x - x_{\text{target}}) = 0$$
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringParams {
    /// Angular frequency squared ($\omega_0^2 = k / m$). Higher = faster snap.
    pub stiffness: f64,
    /// Damping ratio ($\zeta$). `1.0` = critically damped (no overshoot),
    /// `0.82..0.92` = subtle organic liquid bounce.
    pub damping_ratio: f64,
    /// Rest threshold for position and velocity.
    pub epsilon: f64,
}

impl SpringParams {
    /// Crisp, responsive spring for window snapping and maximize transitions.
    pub const fn window_snap() -> Self {
        Self {
            stiffness: 320.0,
            damping_ratio: 0.88,
            epsilon: 0.25,
        }
    }

    /// Smooth, fluid spring for workspace sliding and visual style cross-morphing.
    pub const fn workspace_slide() -> Self {
        Self {
            stiffness: 240.0,
            damping_ratio: 0.92,
            epsilon: 0.25,
        }
    }

    /// Gentle spring for scalar opacity/style blending.
    pub const fn style_morph() -> Self {
        Self {
            stiffness: 180.0,
            damping_ratio: 0.95,
            epsilon: 0.002,
        }
    }

    /// Surface-tension damped harmonic oscillator for soap-bubble inflation and buoyant settling.
    pub const fn soap_bubble() -> Self {
        Self {
            stiffness: 220.0,
            damping_ratio: 0.64,
            epsilon: 0.002,
        }
    }
}

impl Default for SpringParams {
    fn default() -> Self {
        Self::window_snap()
    }
}

/// 1D physical spring state with $C^1$ velocity continuity across mid-flight retargeting.
#[derive(Debug, Clone, Copy)]
pub struct Spring1D {
    pub current: f64,
    pub target: f64,
    pub velocity: f64,
    pub params: SpringParams,
}

impl Spring1D {
    pub fn new(initial: f64, params: SpringParams) -> Self {
        Self {
            current: initial,
            target: initial,
            velocity: 0.0,
            params,
        }
    }

    /// Sets a new target while preserving instantaneous velocity for smooth interruption.
    pub fn set_target(&mut self, target: f64) {
        self.target = target;
    }

    /// Teleports immediately to `value` and zeroes velocity (e.g., during direct mouse drag).
    pub fn snap_to(&mut self, value: f64) {
        self.current = value;
        self.target = value;
        self.velocity = 0.0;
    }

    /// Returns `true` if the spring is still in motion.
    pub fn is_animating(&self) -> bool {
        (self.current - self.target).abs() > self.params.epsilon
            || self.velocity.abs() > self.params.epsilon
    }

    /// Advances the spring physics by `dt_secs` using 4 sub-steps for unconditional stability.
    pub fn advance(&mut self, dt_secs: f64) -> f64 {
        if !self.is_animating() {
            self.current = self.target;
            self.velocity = 0.0;
            return self.current;
        }

        let dt = dt_secs.clamp(0.0, 0.05);
        let substeps = 4;
        let h = dt / (substeps as f64);
        let k = self.params.stiffness;
        let c = 2.0 * self.params.damping_ratio * k.sqrt();

        for _ in 0..substeps {
            let displacement = self.current - self.target;
            let accel = -k * displacement - c * self.velocity;
            self.velocity += accel * h;
            self.current += self.velocity * h;
        }

        if !self.is_animating() {
            self.current = self.target;
            self.velocity = 0.0;
        }

        self.current
    }
}

/// 2D physical spring (`x`, `y`) for window positions and camera offsets.
#[derive(Debug, Clone, Copy)]
pub struct Spring2D {
    pub x: Spring1D,
    pub y: Spring1D,
}

impl Spring2D {
    pub fn new(initial: Point<f64, Logical>, params: SpringParams) -> Self {
        Self {
            x: Spring1D::new(initial.x, params),
            y: Spring1D::new(initial.y, params),
        }
    }

    pub fn set_target(&mut self, target: Point<f64, Logical>) {
        self.x.set_target(target.x);
        self.y.set_target(target.y);
    }

    pub fn snap_to(&mut self, value: Point<f64, Logical>) {
        self.x.snap_to(value.x);
        self.y.snap_to(value.y);
    }

    pub fn is_animating(&self) -> bool {
        self.x.is_animating() || self.y.is_animating()
    }

    pub fn advance(&mut self, dt_secs: f64) -> Point<f64, Logical> {
        Point::from((self.x.advance(dt_secs), self.y.advance(dt_secs)))
    }

    pub fn current_i32(&self) -> Point<i32, Logical> {
        Point::from((self.x.current.round() as i32, self.y.current.round() as i32))
    }
}

/// Physics-based soap-bubble animation state modeling surface-tension inflation,
/// volume-preserving squash-and-stretch, and upward buoyancy drift.
#[derive(Debug, Clone, Copy)]
pub struct BubblePhysicsState {
    /// Radial inflation factor (`0.84 -> 1.0`).
    pub inflation: Spring1D,
    /// Coupled horizontal/vertical oscillation (`scale_x = inf * (1 + k)`, `scale_y = inf / (1 + k)`).
    pub aspect_osc: Spring1D,
    /// Upward buoyancy offset in logical pixels (`18.0 -> 0.0`).
    pub buoyancy_y: Spring1D,
}

impl BubblePhysicsState {
    /// Spawns a new forming bubble with gentle surface-tension inflation and upward buoyancy.
    pub fn new_forming() -> Self {
        let params = SpringParams::soap_bubble();
        let mut inflation = Spring1D::new(0.76, params);
        inflation.set_target(1.0);
        inflation.velocity = 1.8;

        let mut aspect_osc = Spring1D::new(0.08, params);
        aspect_osc.set_target(0.0);

        let mut buoyancy_y = Spring1D::new(8.0, SpringParams::workspace_slide());
        buoyancy_y.set_target(0.0);

        Self {
            inflation,
            aspect_osc,
            buoyancy_y,
        }
    }

    /// Advances bubble surface-tension and buoyancy physics by `dt_secs`.
    pub fn advance(&mut self, dt_secs: f64) {
        self.inflation.advance(dt_secs);
        self.aspect_osc.advance(dt_secs);
        self.buoyancy_y.advance(dt_secs);
    }

    /// Returns `true` while the bubble is still oscillating or rising.
    pub fn is_animating(&self) -> bool {
        self.inflation.is_animating()
            || self.aspect_osc.is_animating()
            || self.buoyancy_y.is_animating()
    }

    /// Returns volume-preserving `(scale_x, scale_y)` factors.
    pub fn scales(&self) -> (f64, f64) {
        let k = (1.0 + self.aspect_osc.current).clamp(0.85, 1.15);
        (self.inflation.current * k, self.inflation.current / k)
    }

    /// Applies current bubble scales to a base logical size.
    pub fn apply_to_size(&self, base: Size<i32, Logical>) -> Size<i32, Logical> {
        let (sx, sy) = self.scales();
        Size::from((
            ((base.w as f64) * sx).round().max(32.0) as i32,
            ((base.h as f64) * sy).round().max(24.0) as i32,
        ))
    }
}

/// Animated state for an individual window (position, backdrop size, and entrance opacity).
#[derive(Debug, Clone, Copy)]
pub struct WindowAnimationState {
    pub position: Spring2D,
    pub size_w: Spring1D,
    pub size_h: Spring1D,
    pub alpha: Spring1D,
    pub bubble: BubblePhysicsState,
    pub anim_tag: u32,
}

impl WindowAnimationState {
    /// Creates an entrance animation that blooms from the center with soap-bubble physics.
    pub fn new_opening(target_loc: Point<i32, Logical>, initial_size: Size<i32, Logical>) -> Self {
        let snap_params = SpringParams::window_snap();
        let pos = Spring2D::new(target_loc.to_f64(), snap_params);

        let mut alpha = Spring1D::new(0.18, SpringParams::style_morph());
        alpha.set_target(1.0);

        Self {
            position: pos,
            size_w: Spring1D::new(initial_size.w.max(120) as f64, snap_params),
            size_h: Spring1D::new(initial_size.h.max(80) as f64, snap_params),
            alpha,
            bubble: BubblePhysicsState::new_forming(),
            anim_tag: 0,
        }
    }

    pub fn advance(&mut self, dt_secs: f64) {
        self.position.advance(dt_secs);
        self.size_w.advance(dt_secs);
        self.size_h.advance(dt_secs);
        self.alpha.advance(dt_secs);
        self.bubble.advance(dt_secs);
    }

    pub fn is_animating(&self) -> bool {
        self.position.is_animating()
            || self.size_w.is_animating()
            || self.size_h.is_animating()
            || self.alpha.is_animating()
            || self.bubble.is_animating()
    }

    pub fn current_rect(&self, fallback_size: Size<i32, Logical>) -> Rectangle<i32, Logical> {
        let w = if self.size_w.is_animating() {
            self.size_w.current.round() as i32
        } else {
            fallback_size.w
        };
        let h = if self.size_h.is_animating() {
            self.size_h.current.round() as i32
        } else {
            fallback_size.h
        };
        let base_size = Size::from((w.max(64), h.max(48)));
        let (bubbled_size, dx, dy) = if self.bubble.is_animating() {
            let bs = self.bubble.apply_to_size(base_size);
            let dx = (base_size.w - bs.w) / 2;
            let dy = (base_size.h - bs.h) / 2 + self.bubble.buoyancy_y.current.round() as i32;
            (bs, dx, dy)
        } else {
            (base_size, 0, 0)
        };
        let pos = self.position.current_i32();
        Rectangle::new(Point::from((pos.x + dx, pos.y + dy)), bubbled_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spring1d_convergence_and_retargeting() {
        let mut s = Spring1D::new(0.0, SpringParams::window_snap());
        s.set_target(100.0);
        assert!(s.is_animating());

        // Advance half-way (~60ms)
        for _ in 0..4 {
            s.advance(1.0 / 60.0);
        }
        assert!(s.current > 10.0 && s.current < 95.0);
        let mid_vel = s.velocity;
        assert!(mid_vel > 0.0);

        // Interrupt mid-flight with a new target; velocity must remain identical at the instant of retarget
        s.set_target(200.0);
        assert_eq!(s.velocity, mid_vel);

        // Advance 1.5 seconds to rest
        for _ in 0..90 {
            s.advance(1.0 / 60.0);
        }
        assert!(!s.is_animating());
        assert_eq!(s.current, 200.0);
        assert_eq!(s.velocity, 0.0);
    }

    #[test]
    fn test_window_opening_animation() {
        let mut anim = WindowAnimationState::new_opening(Point::from((100, 100)), Size::from((600, 400)));
        assert!(anim.is_animating());
        assert_eq!(anim.position.current_i32(), Point::from((100, 100)));

        for _ in 0..90 {
            anim.advance(1.0 / 60.0);
        }
        assert!(!anim.is_animating());
        assert_eq!(anim.position.current_i32(), Point::from((100, 100)));
        assert!((anim.alpha.current - 1.0).abs() < 1e-4);
    }

    #[test]
    fn test_bubble_physics_volume_conservation() {
        let mut b = BubblePhysicsState::new_forming();
        assert!(b.is_animating());
        let (sx0, sy0) = b.scales();
        assert!((sx0 * sy0 - b.inflation.current * b.inflation.current).abs() < 1e-6);

        for _ in 0..120 {
            b.advance(1.0 / 60.0);
        }
        assert!(!b.is_animating());
        let (sx1, sy1) = b.scales();
        assert!((sx1 - 1.0).abs() < 1e-3 && (sy1 - 1.0).abs() < 1e-3);
    }
}

