// SPDX-License-Identifier: GPL-3.0-or-later
//! Bare-Metal Session Integration: XDG config discovery + systemd user session wiring
//!
//! When Mochi Mochi is launched from a login manager (GDM / SDDM / greetd) the working
//! directory is `$HOME`, so the configuration must be discovered through the XDG Base
//! Directory specification instead of a repository-relative path. When running on real
//! hardware, the compositor also exports its environment to the systemd user manager /
//! D-Bus activation environment and starts `mochi-mochi-session.target` (which binds
//! `graphical-session.target`), so portals, notification daemons, and user services
//! started by systemd can reach `WAYLAND_DISPLAY`.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use tracing::{debug, info, warn};

use crate::state::MochiState;

/// Name of the systemd user target shipped in `resources/systemd/`.
pub const SESSION_TARGET: &str = "mochi-mochi-session.target";

/// Value exported as `XDG_CURRENT_DESKTOP` (matches `DesktopNames=` in the `.desktop` file).
pub const DESKTOP_NAME: &str = "MochiMochi";

/// Environment variables propagated to the systemd user manager and D-Bus activation env.
pub const EXPORTED_ENV_VARS: &[&str] = &[
    "WAYLAND_DISPLAY",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_DESKTOP",
    "MOCHI_IPC_SOCKET",
];

/// System-wide fallback locations searched after the per-user XDG paths.
const SYSTEM_CONFIG_FALLBACKS: &[&str] = &[
    "/usr/local/share/mochi-mochi/mochi.kdl",
    "/usr/share/mochi-mochi/mochi.kdl",
];

/// Resolves the KDL configuration path using the real process environment and filesystem.
///
/// Search order (first existing file wins):
/// 1. `--config <path>` (`explicit`), used verbatim even if it does not exist yet
/// 2. `$MOCHI_CONFIG`
/// 3. `$XDG_CONFIG_HOME/mochi/mochi.kdl` (default `~/.config/mochi/mochi.kdl`)
/// 4. `$XDG_CONFIG_DIRS/mochi/mochi.kdl` (default `/etc/xdg/mochi/mochi.kdl`)
/// 5. `/usr/local/share/mochi-mochi/mochi.kdl`, `/usr/share/mochi-mochi/mochi.kdl`
/// 6. `./config/mochi.kdl` (running from a source checkout)
///
/// If nothing exists, the per-user path is returned so that creating it later is picked up
/// by the live hot-reload watcher.
pub fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    resolve_config_path_with(explicit, |key| std::env::var(key).ok(), |p| p.is_file())
}

/// Pure, testable core of [`resolve_config_path`].
pub fn resolve_config_path_with(
    explicit: Option<PathBuf>,
    env: impl Fn(&str) -> Option<String>,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    if let Some(path) = env("MOCHI_CONFIG").filter(|s| !s.is_empty()) {
        return PathBuf::from(path);
    }

    let user_config_home = env("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| env("HOME").filter(|s| !s.is_empty()).map(|h| PathBuf::from(h).join(".config")));
    let user_path = user_config_home.map(|dir| dir.join("mochi").join("mochi.kdl"));

    let mut candidates: Vec<PathBuf> = Vec::new();
    candidates.extend(user_path.clone());

    let config_dirs = env("XDG_CONFIG_DIRS")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/etc/xdg".to_string());
    candidates.extend(
        config_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(|d| PathBuf::from(d).join("mochi").join("mochi.kdl")),
    );
    candidates.extend(SYSTEM_CONFIG_FALLBACKS.iter().map(PathBuf::from));
    candidates.push(PathBuf::from("config/mochi.kdl"));

    if let Some(found) = candidates.iter().find(|p| exists(p)) {
        return found.clone();
    }
    user_path.unwrap_or_else(|| PathBuf::from("config/mochi.kdl"))
}

/// Sets the session environment variables that clients and portals expect.
pub fn export_process_environment(state: &MochiState) {
    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
    std::env::set_var("MOCHI_IPC_SOCKET", &state.ipc_socket_path);
    std::env::set_var("XDG_SESSION_TYPE", "wayland");
    if std::env::var_os("XDG_CURRENT_DESKTOP").is_none() {
        std::env::set_var("XDG_CURRENT_DESKTOP", DESKTOP_NAME);
    }
    if std::env::var_os("XDG_SESSION_DESKTOP").is_none() {
        std::env::set_var("XDG_SESSION_DESKTOP", "mochi-mochi");
    }
}

/// Returns `true` unless the user opted out via `MOCHI_SYSTEMD=0`, or no systemd user
/// manager is reachable.
fn systemd_integration_enabled() -> bool {
    if matches!(std::env::var("MOCHI_SYSTEMD").as_deref(), Ok("0") | Ok("false") | Ok("no")) {
        return false;
    }
    Path::new("/run/systemd/system").exists()
}

fn run_quiet(program: &str, args: &[&str]) -> bool {
    match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => status.success(),
        Err(err) => {
            debug!(program, ?err, "Session helper not available");
            false
        }
    }
}

/// Exports the compositor environment to systemd / D-Bus and starts the session target.
/// Intended for the bare-metal (`udev`) backend only; nested sessions must not touch the
/// host user manager.
pub fn start_systemd_session(state: &MochiState) {
    export_process_environment(state);
    if !systemd_integration_enabled() {
        info!("systemd user session integration disabled or unavailable");
        return;
    }

    let mut dbus_args = vec!["--systemd"];
    dbus_args.extend_from_slice(EXPORTED_ENV_VARS);
    let imported = run_quiet("dbus-update-activation-environment", &dbus_args) || {
        let mut sd_args = vec!["--user", "import-environment"];
        sd_args.extend_from_slice(EXPORTED_ENV_VARS);
        run_quiet("systemctl", &sd_args)
    };
    if !imported {
        warn!("Failed to export session environment to systemd / D-Bus");
    }

    if run_quiet("systemctl", &["--user", "start", "--no-block", SESSION_TARGET]) {
        info!(target = SESSION_TARGET, "Started systemd user session target");
    } else {
        warn!(
            target = SESSION_TARGET,
            "Could not start systemd user session target (is it installed?)"
        );
    }
}

/// Exports `DISPLAY` to systemd user manager and D-Bus once XWayland is ready.
pub fn export_x11_display(display_number: u32) {
    let display_str = format!(":{}", display_number);
    std::env::set_var("DISPLAY", &display_str);
    if !systemd_integration_enabled() {
        return;
    }
    let _ = run_quiet(
        "dbus-update-activation-environment",
        &["--systemd", &format!("DISPLAY={}", display_str)],
    );
    let _ = run_quiet(
        "systemctl",
        &["--user", "set-environment", &format!("DISPLAY={}", display_str)],
    );
    info!(display = %display_str, "Exported DISPLAY to systemd user manager & D-Bus");
}

/// Stops the session target and removes the compositor's variables from the user manager,
/// so services do not try to reconnect to a dead `WAYLAND_DISPLAY` after logout.
pub fn stop_systemd_session() {
    if !systemd_integration_enabled() {
        return;
    }
    run_quiet("systemctl", &["--user", "stop", SESSION_TARGET]);
    let mut args = vec!["--user", "unset-environment"];
    args.extend_from_slice(&["WAYLAND_DISPLAY", "MOCHI_IPC_SOCKET", "DISPLAY"]);
    run_quiet("systemctl", &args);
    info!("Stopped systemd user session target");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn test_config_path_resolution_order() {
        // Explicit --config always wins
        let p = resolve_config_path_with(
            Some(PathBuf::from("/x/custom.kdl")),
            env_from(&[("MOCHI_CONFIG", "/y.kdl")]),
            |_| false,
        );
        assert_eq!(p, PathBuf::from("/x/custom.kdl"));

        // $MOCHI_CONFIG beats XDG paths
        let p = resolve_config_path_with(None, env_from(&[("MOCHI_CONFIG", "/y.kdl")]), |_| true);
        assert_eq!(p, PathBuf::from("/y.kdl"));

        // User XDG config preferred when it exists
        let env = env_from(&[("HOME", "/home/u")]);
        let p = resolve_config_path_with(None, &env, |_| true);
        assert_eq!(p, PathBuf::from("/home/u/.config/mochi/mochi.kdl"));

        // Falls back to /etc/xdg, then installed default
        let p = resolve_config_path_with(None, &env, |p| p.starts_with("/etc/xdg"));
        assert_eq!(p, PathBuf::from("/etc/xdg/mochi/mochi.kdl"));
        let p = resolve_config_path_with(None, &env, |p| p.starts_with("/usr/local/share"));
        assert_eq!(p, PathBuf::from("/usr/local/share/mochi-mochi/mochi.kdl"));

        // Nothing exists: return per-user path so hot-reload picks it up once created
        let env = env_from(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/cfg")]);
        let p = resolve_config_path_with(None, &env, |_| false);
        assert_eq!(p, PathBuf::from("/cfg/mochi/mochi.kdl"));
    }
}

