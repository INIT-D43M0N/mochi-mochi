// SPDX-License-Identifier: GPL-3.0-or-later
//! Mochi Mochi IPC Protocol & Client/Server Transport (`mochi-ipc`)
//!
//! Provides a lightweight, zero-dependency line-delimited protocol over a Unix domain
//! socket (`$XDG_RUNTIME_DIR/mochi-mochi.sock`) used by the `mochi-msg` CLI and
//! external status bars or desktop environment components.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

/// Returns the default Unix domain socket path for Mochi Mochi IPC.
pub fn default_socket_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("MOCHI_IPC_SOCKET") {
        return PathBuf::from(explicit);
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("mochi-mochi.sock");
    }
    PathBuf::from("/tmp/mochi-mochi.sock")
}

/// Requests sent from `mochi-msg` or external tools to the running Mochi Mochi compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcRequest {
    /// Query live compositor status (active workspace, layout mode, visual style, window count).
    Status,
    /// Set or cycle the active visual style (`normal-blur`, `frosted-glass`, `liquid-glass`, `cycle`).
    SetStyle(String),
    /// Set or toggle the active workspace layout (`floating`, `tiling`, `toggle`).
    SetLayout(String),
    /// Switch to a 1-based workspace number (`1..=9`).
    SwitchWorkspace(usize),
    /// Hot-reload `config/mochi.kdl` from disk immediately.
    ReloadConfig,
    /// Spawn a Wayland client command inside Mochi Mochi.
    Spawn(String),
    /// Toggle the spring-animated Expose / Window Overview grid (`Mod+W`).
    ToggleOverview,
    /// Toggle showing/hiding the dropdown scratchpad window (`Mod+Minus`).
    ToggleScratchpad,
    /// Move the currently focused window into/out of the scratchpad (`Mod+Shift+Minus`).
    MoveToScratchpad,
    /// Capture the composited OpenGL ES framebuffer directly to a `.ppm` image file (`Mod+P`).
    Screenshot(String),
    /// List all mapped windows across all workspaces with JSON metadata (`mochi-msg windows`).
    ListWindows,
    /// Close the currently focused window (`mochi-msg close`).
    CloseFocusedWindow,
    /// Focus and raise a window matching the given `app-id` or `title` substring (`mochi-msg focus <query>`).
    FocusWindow(String),
    /// Adjust Master column width ratio (`mochi-msg master-ratio <delta|val>`).
    AdjustMasterRatio(String),
    /// Adjust Master window count on active workspace (`mochi-msg master-count <inc|dec|N>`).
    AdjustMasterCount(String),
    /// Directionally swap the focused window (`mochi-msg swap <master|next|prev|up|down|left|right>`).
    SwapWindow(String),
    /// Control Night Light warmth / color temperature (`mochi-msg night-light [toggle|on|off|<kelvin>]`).
    NightLight(String),
    /// Minimize the currently focused window with spring flight (`mochi-msg minimize`).
    Minimize,
    /// Restore the most recently minimized window on the active workspace (`mochi-msg restore`).
    Restore,
    /// Cleanly shut down Mochi Mochi.
    Quit,
}

impl IpcRequest {
    /// Encodes the request as a single line for transmission over the Unix domain socket.
    pub fn to_wire(&self) -> String {
        match self {
            Self::Status => "STATUS\n".to_string(),
            Self::SetStyle(s) => format!("STYLE {}\n", s.trim()),
            Self::SetLayout(l) => format!("LAYOUT {}\n", l.trim()),
            Self::SwitchWorkspace(ws) => format!("WORKSPACE {}\n", ws),
            Self::ReloadConfig => "RELOAD\n".to_string(),
            Self::Spawn(cmd) => format!("SPAWN {}\n", cmd.trim()),
            Self::ToggleOverview => "OVERVIEW\n".to_string(),
            Self::ToggleScratchpad => "SCRATCHPAD TOGGLE\n".to_string(),
            Self::MoveToScratchpad => "SCRATCHPAD MOVE\n".to_string(),
            Self::Screenshot(path) => format!("SCREENSHOT {}\n", path.trim()),
            Self::ListWindows => "WINDOWS\n".to_string(),
            Self::CloseFocusedWindow => "CLOSE\n".to_string(),
            Self::FocusWindow(query) => format!("FOCUS {}\n", query.trim()),
            Self::AdjustMasterRatio(r) => format!("MASTER_RATIO {}\n", r.trim()),
            Self::AdjustMasterCount(c) => format!("MASTER_COUNT {}\n", c.trim()),
            Self::SwapWindow(dir) => format!("SWAP {}\n", dir.trim()),
            Self::NightLight(arg) => format!("NIGHT_LIGHT {}\n", arg.trim()),
            Self::Minimize => "MINIMIZE\n".to_string(),
            Self::Restore => "RESTORE\n".to_string(),
            Self::Quit => "QUIT\n".to_string(),
        }
    }

    /// Parses a single-line wire command into an [`IpcRequest`].
    pub fn from_wire(line: &str) -> Result<Self, String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err("Empty IPC request".to_string());
        }
        let mut parts = trimmed.splitn(2, ' ');
        let verb = parts.next().unwrap_or("").to_ascii_uppercase();
        let arg = parts.next().map(str::trim).unwrap_or("");

        match verb.as_str() {
            "STATUS" => Ok(Self::Status),
            "STYLE" => {
                if arg.is_empty() {
                    Ok(Self::SetStyle("cycle".to_string()))
                } else {
                    Ok(Self::SetStyle(arg.to_string()))
                }
            }
            "LAYOUT" => {
                if arg.is_empty() {
                    Ok(Self::SetLayout("toggle".to_string()))
                } else {
                    Ok(Self::SetLayout(arg.to_string()))
                }
            }
            "WORKSPACE" => {
                let ws: usize = arg
                    .parse()
                    .map_err(|_| format!("Invalid workspace number '{arg}'"))?;
                if ws == 0 {
                    return Err("Workspace number is 1-based (1..N)".to_string());
                }
                Ok(Self::SwitchWorkspace(ws))
            }
            "RELOAD" => Ok(Self::ReloadConfig),
            "SPAWN" => {
                if arg.is_empty() {
                    return Err("SPAWN requires a command string".to_string());
                }
                Ok(Self::Spawn(arg.to_string()))
            }
            "OVERVIEW" => Ok(Self::ToggleOverview),
            "SCRATCHPAD" => {
                if arg.eq_ignore_ascii_case("move") || arg.eq_ignore_ascii_case("stash") {
                    Ok(Self::MoveToScratchpad)
                } else {
                    Ok(Self::ToggleScratchpad)
                }
            }
            "SCREENSHOT" => {
                let path = if arg.is_empty() {
                    "mochi-screenshot.ppm".to_string()
                } else {
                    arg.to_string()
                };
                Ok(Self::Screenshot(path))
            }
            "WINDOWS" | "CLIENTS" => Ok(Self::ListWindows),
            "CLOSE" => Ok(Self::CloseFocusedWindow),
            "MINIMIZE" => Ok(Self::Minimize),
            "RESTORE" => Ok(Self::Restore),
            "FOCUS" => {
                if arg.is_empty() {
                    return Err("FOCUS requires an app-id or title substring".to_string());
                }
                Ok(Self::FocusWindow(arg.to_string()))
            }
            "MASTER_RATIO" => {
                let r = if arg.is_empty() { "+0.05" } else { arg };
                Ok(Self::AdjustMasterRatio(r.to_string()))
            }
            "MASTER_COUNT" => {
                let c = if arg.is_empty() { "inc" } else { arg };
                Ok(Self::AdjustMasterCount(c.to_string()))
            }
            "SWAP" => {
                let dir = if arg.is_empty() { "master" } else { arg };
                Ok(Self::SwapWindow(dir.to_string()))
            }
            "NIGHT_LIGHT" | "NIGHTLIGHT" => {
                let a = if arg.is_empty() { "status" } else { arg };
                Ok(Self::NightLight(a.to_string()))
            }
            "QUIT" => Ok(Self::Quit),
            other => Err(format!("Unknown IPC command '{other}'")),
        }
    }
}

/// Sends an [`IpcRequest`] to the running Mochi Mochi compositor and returns the JSON response string.
pub fn send_request(socket_path: &Path, request: &IpcRequest) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(socket_path).map_err(|e| {
        anyhow::anyhow!(
            "Could not connect to Mochi Mochi IPC socket at {}: {e}",
            socket_path.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;

    let payload = request.to_wire();
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipc_wire_roundtrip() {
        let requests = [
            IpcRequest::Status,
            IpcRequest::SetStyle("liquid-glass".to_string()),
            IpcRequest::SetLayout("tiling".to_string()),
            IpcRequest::SwitchWorkspace(3),
            IpcRequest::ReloadConfig,
            IpcRequest::Spawn("ptyxis --standalone".to_string()),
            IpcRequest::Screenshot("capture.ppm".to_string()),
            IpcRequest::ListWindows,
            IpcRequest::CloseFocusedWindow,
            IpcRequest::Minimize,
            IpcRequest::Restore,
            IpcRequest::FocusWindow("firefox".to_string()),
            IpcRequest::AdjustMasterRatio("+0.05".to_string()),
            IpcRequest::AdjustMasterCount("inc".to_string()),
            IpcRequest::SwapWindow("next".to_string()),
            IpcRequest::Quit,
        ];

        for req in requests {
            let wire = req.to_wire();
            let parsed = IpcRequest::from_wire(&wire).unwrap();
            assert_eq!(parsed, req);
        }
    }
}

