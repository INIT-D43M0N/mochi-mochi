// SPDX-License-Identifier: GPL-3.0-or-later
//! Mochi Mochi Compositor Core (`mochi-core`)
//!
//! Coordinates Wayland protocol state, seat/input pipelines, KDL configuration,
//! and delegates window management policy to `mochi-wm`.

pub mod config;
pub mod foreign_toplevel;
pub mod gamma_control;
pub mod glass;
pub mod grabs;
pub mod input;
pub mod output_management;
pub mod protocols;
pub mod screencopy;
pub mod session;
pub mod state;
pub mod workspaces;

pub use config::MochiConfig;
pub use state::MochiState;

