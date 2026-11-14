// SPDX-License-Identifier: GPL-3.0-or-later
//! Mochi Mochi Wayland Compositor & Window Manager (`mochi-compositor`)
//!
//! Supports both nested development execution (`--backend winit`) and
//! bare-metal Linux TTY / DRM / KMS / `libinput` execution (`--backend udev`).

use std::path::PathBuf;

use calloop::EventLoop;
use mochi_core::{MochiConfig, MochiState};
use smithay::reexports::wayland_server::Display;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendChoice {
    Auto,
    Winit,
    Udev,
}

impl BackendChoice {
    fn resolve(self) -> Self {
        match self {
            Self::Auto => {
                let has_host_display = std::env::var_os("WAYLAND_DISPLAY").is_some()
                    || std::env::var_os("DISPLAY").is_some();
                if has_host_display {
                    Self::Winit
                } else {
                    Self::Udev
                }
            }
            explicit => explicit,
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let mut explicit_config: Option<PathBuf> = None;
    let mut initial_command: Option<String> = None;
    let mut check_only = false;
    let mut print_config_path = false;
    let mut backend_choice = BackendChoice::Auto;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                if let Some(p) = args.next() {
                    explicit_config = Some(PathBuf::from(p));
                }
            }
            "--print-config-path" => {
                print_config_path = true;
            }
            "--version" | "-V" => {
                println!("mochi-compositor {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--command" | "-c" => {
                initial_command = args.next();
            }
            "--backend" | "-b" => {
                if let Some(b) = args.next() {
                    match b.as_str() {
                        "auto" => backend_choice = BackendChoice::Auto,
                        "winit" | "nested" => backend_choice = BackendChoice::Winit,
                        "udev" | "drm" | "tty" | "bare-metal" => {
                            backend_choice = BackendChoice::Udev
                        }
                        other => {
                            anyhow::bail!(
                                "Unknown backend '{other}'. Expected 'auto', 'winit', or 'udev'."
                            );
                        }
                    }
                }
            }
            "--winit" => {
                backend_choice = BackendChoice::Winit;
            }
            "--udev" | "--tty" => {
                backend_choice = BackendChoice::Udev;
            }
            "--check-config" => {
                check_only = true;
            }
            "--help" | "-h" => {
                println!(
                    "Mochi Mochi Compositor v{}\n\
                     Usage: mochi-compositor [OPTIONS]\n\n\
                     Options:\n\
                     \x20 -b, --backend <MODE>   Backend mode: auto, winit (nested), or udev (bare-metal TTY/DRM)\n\
                     \x20 --winit                Shortcut for --backend winit\n\
                     \x20 --udev, --tty          Shortcut for --backend udev\n\
                     \x20 --config <PATH>        Path to KDL configuration file (default: XDG lookup)\n\
                     \x20 --print-config-path    Print the resolved configuration path and exit\n\
                     \x20 -c, --command <CMD>    Spawn an initial Wayland client after startup\n\
                     \x20 --check-config         Parse and validate KDL configuration and exit\n\
                     \x20 -V, --version          Print version and exit\n\
                     \x20 -h, --help             Show this help message\n\n\
                     Config lookup order:\n\
                     \x20 --config, $MOCHI_CONFIG, $XDG_CONFIG_HOME/mochi/mochi.kdl (~/.config/mochi/mochi.kdl),\n\
                     \x20 $XDG_CONFIG_DIRS/mochi/mochi.kdl (/etc/xdg/mochi/mochi.kdl),\n\
                     \x20 /usr/local/share/mochi-mochi/mochi.kdl, /usr/share/mochi-mochi/mochi.kdl, ./config/mochi.kdl\n\n\
                     Environment:\n\
                     \x20 MOCHI_CONFIG           Override configuration path\n\
                     \x20 MOCHI_SYSTEMD=0        Disable systemd user session integration (udev backend)",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            other => {
                tracing::warn!(arg = other, "Ignoring unrecognized CLI argument");
            }
        }
    }

    let config_path = mochi_core::session::resolve_config_path(explicit_config);
    if print_config_path {
        println!("{}", config_path.display());
        return Ok(());
    }

    let resolved_backend = backend_choice.resolve();
    let config = MochiConfig::load_from_path(&config_path);
    info!(
        version = env!("CARGO_PKG_VERSION"),
        backend = ?resolved_backend,
        config = %config_path.display(),
        style = config.visual_style.style.as_str(),
        blur_radius = config.visual_style.blur_radius,
        opacity = config.visual_style.opacity,
        corner_radius = config.visual_style.corner_radius,
        workspaces = config.workspace_count,
        xkb_layout = %config.keyboard.xkb_layout,
        "Loaded Mochi Mochi configuration"
    );

    if check_only {
        println!("Configuration OK: {:?}", config);
        return Ok(());
    }

    let mut event_loop: EventLoop<'static, MochiState> = EventLoop::try_new()?;
    let display: Display<MochiState> = Display::new()?;
    let mut state = MochiState::new_with_config_path(&mut event_loop, display, config, config_path)?;

    match resolved_backend {
        BackendChoice::Winit | BackendChoice::Auto => {
            mochi_backend::winit::run_winit(
                &mut event_loop,
                &mut state,
                initial_command.as_deref(),
            )?;
        }
        BackendChoice::Udev => {
            mochi_backend::udev::run_udev(
                &mut event_loop,
                &mut state,
                initial_command.as_deref(),
            )?;
        }
    }

    Ok(())
}
