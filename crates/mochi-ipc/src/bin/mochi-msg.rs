// SPDX-License-Identifier: GPL-3.0-or-later
//! `mochi-msg` — Command-Line IPC Control Utility for the Mochi Mochi Compositor

use mochi_ipc::{default_socket_path, send_request, IpcRequest};

fn print_usage() {
    println!(
        "mochi-msg v{} — Mochi Mochi Compositor IPC Control CLI\n\n\
         Usage:\n\
         \x20 mochi-msg status\n\
         \x20 mochi-msg windows\n\
         \x20 mochi-msg focus <app-id | title>\n\
         \x20 mochi-msg close\n\
         \x20 mochi-msg minimize\n\
         \x20 mochi-msg restore\n\
         \x20 mochi-msg style <normal-blur | frosted-glass | liquid-glass | cycle>\n\
         \x20 mochi-msg layout <floating | tiling | toggle>\n\
         \x20 mochi-msg master-ratio <+0.05 | -0.05 | 0.60 | grow | shrink>\n\
         \x20 mochi-msg master-count <inc | dec | 1..5>\n\
         \x20 mochi-msg swap <master | next | prev | up | down | left | right>\n\
         \x20 mochi-msg night-light [toggle | on | off | 3500 | 1000..10000]\n\
         \x20 mochi-msg workspace <1..N>\n\
         \x20 mochi-msg overview\n\
         \x20 mochi-msg scratchpad [toggle | move]\n\
         \x20 mochi-msg screenshot [path.ppm]\n\
         \x20 mochi-msg reload\n\
         \x20 mochi-msg spawn <COMMAND>\n\
         \x20 mochi-msg quit",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        print_usage();
        return Ok(());
    };

    let request = match cmd.as_str() {
        "status" => IpcRequest::Status,
        "windows" | "clients" | "list" => IpcRequest::ListWindows,
        "close" | "kill" => IpcRequest::CloseFocusedWindow,
        "minimize" | "min" => IpcRequest::Minimize,
        "restore" | "unminimize" => IpcRequest::Restore,
        "focus" | "activate" => {
            let rest: Vec<String> = args.collect();
            if rest.is_empty() {
                anyhow::bail!("Expected app-id or title substring (e.g. mochi-msg focus firefox)");
            }
            IpcRequest::FocusWindow(rest.join(" "))
        }
        "style" => {
            let style = args.next().unwrap_or_else(|| "cycle".to_string());
            IpcRequest::SetStyle(style)
        }
        "layout" => {
            let layout = args.next().unwrap_or_else(|| "toggle".to_string());
            IpcRequest::SetLayout(layout)
        }
        "master-ratio" | "ratio" => {
            let arg = args.next().unwrap_or_else(|| "+0.05".to_string());
            IpcRequest::AdjustMasterRatio(arg)
        }
        "grow-master" => IpcRequest::AdjustMasterRatio("+0.05".to_string()),
        "shrink-master" => IpcRequest::AdjustMasterRatio("-0.05".to_string()),
        "master-count" | "count" => {
            let arg = args.next().unwrap_or_else(|| "inc".to_string());
            IpcRequest::AdjustMasterCount(arg)
        }
        "swap" => {
            let dir = args.next().unwrap_or_else(|| "master".to_string());
            IpcRequest::SwapWindow(dir)
        }
        "night-light" | "nightlight" => {
            let sub = args.next().unwrap_or_else(|| "status".to_string());
            IpcRequest::NightLight(sub)
        }
        "workspace" | "ws" => {
            let ws_str = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("Expected workspace number (e.g. mochi-msg workspace 2)"))?;
            let ws: usize = ws_str.parse()?;
            IpcRequest::SwitchWorkspace(ws)
        }
        "overview" | "expose" => IpcRequest::ToggleOverview,
        "scratchpad" | "scratch" => {
            let sub = args.next().unwrap_or_else(|| "toggle".to_string());
            if sub.eq_ignore_ascii_case("move") || sub.eq_ignore_ascii_case("stash") {
                IpcRequest::MoveToScratchpad
            } else {
                IpcRequest::ToggleScratchpad
            }
        }
        "screenshot" | "shot" => {
            let path = args
                .next()
                .unwrap_or_else(|| "mochi-screenshot.ppm".to_string());
            IpcRequest::Screenshot(path)
        }
        "reload" => IpcRequest::ReloadConfig,
        "spawn" | "exec" => {
            let rest: Vec<String> = args.collect();
            if rest.is_empty() {
                anyhow::bail!("Expected command to spawn (e.g. mochi-msg spawn \"ptyxis --standalone\")");
            }
            IpcRequest::Spawn(rest.join(" "))
        }
        "quit" | "exit" => IpcRequest::Quit,
        "--help" | "-h" | "help" => {
            print_usage();
            return Ok(());
        }
        other => {
            anyhow::bail!("Unknown command '{other}'. Run 'mochi-msg --help' for usage.");
        }
    };

    let socket_path = default_socket_path();
    let response = send_request(&socket_path, &request)?;
    println!("{response}");
    Ok(())
}

