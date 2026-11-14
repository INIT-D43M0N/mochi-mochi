// SPDX-License-Identifier: GPL-3.0-or-later
//! Declarative KDL Configuration Parser (`mochi.kdl`)
//!
//! Parses visual style settings, workspace layout defaults, XKB keyboard settings,
//! live-reloadable keybindings (`binds { ... }`), and per-window placement/styling
//! rules (`window-rule`).

use std::{fs, path::Path};

use kdl::{KdlDocument, KdlNode};
use mochi_wm::{
    LayoutMode, OutputConfig, SnapZone, SwapDirection, VisualStyle, VisualStyleConfig, WindowRule,
};
use smithay::{
    input::keyboard::{keysyms, ModifiersState},
    utils::Point,
};
use tracing::{info, warn};

/// Compositor actions triggered by global keybindings (`binds { ... }` in `mochi.kdl`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    Quit,
    SpawnTerminal,
    SpawnCommand(String),
    CloseActiveWindow,
    CycleVisualStyle,
    CycleFocus,
    ToggleFullscreen,
    ToggleLayout,
    ToggleOverview,
    ToggleScratchpad,
    MoveToScratchpad,
    Screenshot,
    SwapWithMaster,
    AdjustMasterRatio(i32),
    AdjustMasterCount(i32),
    SwapWindow(SwapDirection),
    ReloadConfig,
    SnapActiveWindow(SnapZone),
    SwitchWorkspace(usize),
    MoveWindowToWorkspace(usize),
    SwitchVt(i32),
    ToggleNightLight,
    SetNightLightTemperature(u32),
}

impl KeyAction {
    /// Parses an `action="..."` attribute value from `mochi.kdl` into a [`KeyAction`].
    pub fn from_action_str(raw: &str) -> Option<Self> {
        let norm = raw.trim().to_ascii_lowercase();
        match norm.as_str() {
            "quit" | "exit" => Some(Self::Quit),
            "spawn-terminal" | "terminal" => Some(Self::SpawnTerminal),
            "close-window" | "close" => Some(Self::CloseActiveWindow),
            "cycle-visual-style" | "cycle-style" => Some(Self::CycleVisualStyle),
            "cycle-focus" | "next-window" => Some(Self::CycleFocus),
            "toggle-fullscreen" | "fullscreen" => Some(Self::ToggleFullscreen),
            "toggle-layout-tiling-floating" | "toggle-layout" | "tiling" => {
                Some(Self::ToggleLayout)
            }
            "toggle-overview" | "overview" | "expose" => Some(Self::ToggleOverview),
            "toggle-scratchpad" | "scratchpad" => Some(Self::ToggleScratchpad),
            "move-to-scratchpad" | "stash-scratchpad" => Some(Self::MoveToScratchpad),
            "screenshot" | "take-screenshot" => Some(Self::Screenshot),
            "swap-with-master" | "swap-master" => Some(Self::SwapWithMaster),
            "swap-next" => Some(Self::SwapWindow(SwapDirection::Next)),
            "swap-prev" | "swap-previous" => Some(Self::SwapWindow(SwapDirection::Prev)),
            "swap-up" => Some(Self::SwapWindow(SwapDirection::Up)),
            "swap-down" => Some(Self::SwapWindow(SwapDirection::Down)),
            "swap-left" => Some(Self::SwapWindow(SwapDirection::Left)),
            "swap-right" => Some(Self::SwapWindow(SwapDirection::Right)),
            "decrease-master-ratio" | "shrink-master" | "shrink-master-ratio" => {
                Some(Self::AdjustMasterRatio(-5))
            }
            "increase-master-ratio" | "grow-master" | "grow-master-ratio" => {
                Some(Self::AdjustMasterRatio(5))
            }
            "inc-master-count" | "increase-master-count" => Some(Self::AdjustMasterCount(1)),
            "dec-master-count" | "decrease-master-count" => Some(Self::AdjustMasterCount(-1)),
            "reload-config" | "reload" => Some(Self::ReloadConfig),
            "toggle-night-light" | "toggle-nightlight" | "night-light" | "nightlight" => {
                Some(Self::ToggleNightLight)
            }
            "snap-left" => Some(Self::SnapActiveWindow(SnapZone::LeftHalf)),
            "snap-right" => Some(Self::SnapActiveWindow(SnapZone::RightHalf)),
            "snap-maximize" | "maximize" => Some(Self::SnapActiveWindow(SnapZone::Maximize)),
            "snap-center" | "center-restore" => {
                Some(Self::SnapActiveWindow(SnapZone::CenterRestore))
            }
            other => {
                if let Some(rest) = other.strip_prefix("workspace-") {
                    let num: usize = rest.parse().ok()?;
                    return Some(Self::SwitchWorkspace(num.saturating_sub(1)));
                }
                if let Some(rest) = other
                    .strip_prefix("move-to-workspace-")
                    .or_else(|| other.strip_prefix("move-window-to-workspace-"))
                {
                    let num: usize = rest.parse().ok()?;
                    return Some(Self::MoveWindowToWorkspace(num.saturating_sub(1)));
                }
                if let Some(cmd) = raw.trim().strip_prefix("spawn:") {
                    return Some(Self::SpawnCommand(cmd.trim().to_string()));
                }
                None
            }
        }
    }
}

/// Logical key trigger independent of `Shift`-transformed keysyms (e.g. `Shift+1` -> `!`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTrigger {
    Return,
    Escape,
    Tab,
    Left,
    Right,
    Up,
    Down,
    BracketLeft,
    BracketRight,
    Minus,
    Digit(u8),
    Char(char),
}

impl KeyTrigger {
    pub fn from_token(token: &str) -> Option<Self> {
        let low = token.trim().to_ascii_lowercase();
        match low.as_str() {
            "return" | "enter" | "kp_enter" => Some(Self::Return),
            "escape" | "esc" => Some(Self::Escape),
            "tab" => Some(Self::Tab),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "[" | "bracketleft" => Some(Self::BracketLeft),
            "]" | "bracketright" => Some(Self::BracketRight),
            "-" | "minus" | "underscore" => Some(Self::Minus),
            _ if low.len() == 1 => {
                let ch = low.chars().next()?;
                if ch.is_ascii_digit() && ch != '0' {
                    Some(Self::Digit(ch as u8 - b'0'))
                } else if ch.is_ascii_lowercase() {
                    Some(Self::Char(ch))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn matches_keysym(self, keysym: u32) -> bool {
        match self {
            Self::Return => matches!(keysym, keysyms::KEY_Return | keysyms::KEY_KP_Enter),
            Self::Escape => keysym == keysyms::KEY_Escape,
            Self::Tab => matches!(keysym, keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab),
            Self::Left => keysym == keysyms::KEY_Left,
            Self::Right => keysym == keysyms::KEY_Right,
            Self::Up => keysym == keysyms::KEY_Up,
            Self::Down => keysym == keysyms::KEY_Down,
            Self::BracketLeft => matches!(keysym, keysyms::KEY_bracketleft | keysyms::KEY_braceleft),
            Self::BracketRight => {
                matches!(keysym, keysyms::KEY_bracketright | keysyms::KEY_braceright)
            }
            Self::Minus => matches!(
                keysym,
                keysyms::KEY_minus | keysyms::KEY_underscore | keysyms::KEY_KP_Subtract
            ),
            Self::Digit(1) => matches!(keysym, keysyms::KEY_1 | keysyms::KEY_exclam),
            Self::Digit(2) => matches!(keysym, keysyms::KEY_2 | keysyms::KEY_at),
            Self::Digit(3) => matches!(keysym, keysyms::KEY_3 | keysyms::KEY_numbersign),
            Self::Digit(4) => matches!(keysym, keysyms::KEY_4 | keysyms::KEY_dollar),
            Self::Digit(5) => matches!(keysym, keysyms::KEY_5 | keysyms::KEY_percent),
            Self::Digit(6) => matches!(
                keysym,
                keysyms::KEY_6 | keysyms::KEY_asciicircum | keysyms::KEY_dead_diaeresis
            ),
            Self::Digit(7) => matches!(keysym, keysyms::KEY_7 | keysyms::KEY_ampersand),
            Self::Digit(8) => matches!(keysym, keysyms::KEY_8 | keysyms::KEY_asterisk),
            Self::Digit(9) => matches!(keysym, keysyms::KEY_9 | keysyms::KEY_parenleft),
            Self::Digit(_) => false,
            Self::Char(ch) => {
                let lower = ch as u32;
                let upper = ch.to_ascii_uppercase() as u32;
                keysym == lower || keysym == upper
            }
        }
    }
}

/// A single compiled keybinding rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    pub mod_logo_or_alt: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub trigger: KeyTrigger,
    pub action: KeyAction,
}

impl KeyBinding {
    /// Parses a chord string (such as `"Mod+Shift+Return"` or `"Mod+W"`) and [`KeyAction`].
    pub fn parse(chord: &str, action: KeyAction) -> Option<Self> {
        let mut mod_logo_or_alt = false;
        let mut shift = false;
        let mut ctrl = false;
        let mut trigger = None;

        for part in chord.split('+') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            match trimmed.to_ascii_lowercase().as_str() {
                "mod" | "super" | "logo" | "alt" | "meta" => mod_logo_or_alt = true,
                "shift" => shift = true,
                "ctrl" | "control" => ctrl = true,
                _ => {
                    trigger = KeyTrigger::from_token(trimmed);
                }
            }
        }

        Some(Self {
            mod_logo_or_alt,
            shift,
            ctrl,
            trigger: trigger?,
            action,
        })
    }

    /// Evaluates whether this keybinding matches the active `modifiers` and `keysym`.
    pub fn matches(&self, modifiers: &ModifiersState, keysym: u32) -> bool {
        let mod_active = modifiers.logo || modifiers.alt;
        if self.mod_logo_or_alt != mod_active {
            return false;
        }
        if self.shift != modifiers.shift {
            return false;
        }
        if self.ctrl != modifiers.ctrl {
            return false;
        }
        self.trigger.matches_keysym(keysym)
    }
}

/// Returns the default Mochi Mochi keybinding table (used when `binds` is omitted or as base).
pub fn default_keybindings() -> Vec<KeyBinding> {
    let pairs: &[(&str, KeyAction)] = &[
        ("Mod+Escape", KeyAction::Quit),
        ("Mod+Return", KeyAction::SpawnTerminal),
        ("Mod+Q", KeyAction::CloseActiveWindow),
        ("Mod+S", KeyAction::CycleVisualStyle),
        ("Mod+T", KeyAction::ToggleLayout),
        ("Mod+W", KeyAction::ToggleOverview),
        ("Mod+Minus", KeyAction::ToggleScratchpad),
        ("Mod+Shift+Minus", KeyAction::MoveToScratchpad),
        ("Mod+P", KeyAction::Screenshot),
        ("Mod+Shift+Return", KeyAction::SwapWithMaster),
        ("Mod+Shift+R", KeyAction::ReloadConfig),
        ("Mod+H", KeyAction::AdjustMasterRatio(-5)),
        ("Mod+L", KeyAction::AdjustMasterRatio(5)),
        ("Mod+[", KeyAction::AdjustMasterRatio(-5)),
        ("Mod+]", KeyAction::AdjustMasterRatio(5)),
        ("Mod+I", KeyAction::AdjustMasterCount(1)),
        ("Mod+D", KeyAction::AdjustMasterCount(-1)),
        ("Mod+Shift+H", KeyAction::SwapWindow(SwapDirection::Left)),
        ("Mod+Shift+L", KeyAction::SwapWindow(SwapDirection::Right)),
        ("Mod+Shift+J", KeyAction::SwapWindow(SwapDirection::Down)),
        ("Mod+Shift+K", KeyAction::SwapWindow(SwapDirection::Up)),
        ("Mod+Tab", KeyAction::CycleFocus),
        ("Mod+F", KeyAction::ToggleFullscreen),
        ("Mod+Left", KeyAction::SnapActiveWindow(SnapZone::LeftHalf)),
        ("Mod+Right", KeyAction::SnapActiveWindow(SnapZone::RightHalf)),
        ("Mod+Up", KeyAction::SnapActiveWindow(SnapZone::Maximize)),
        ("Mod+Down", KeyAction::SnapActiveWindow(SnapZone::CenterRestore)),
        ("Mod+1", KeyAction::SwitchWorkspace(0)),
        ("Mod+2", KeyAction::SwitchWorkspace(1)),
        ("Mod+3", KeyAction::SwitchWorkspace(2)),
        ("Mod+4", KeyAction::SwitchWorkspace(3)),
        ("Mod+Shift+1", KeyAction::MoveWindowToWorkspace(0)),
        ("Mod+Shift+2", KeyAction::MoveWindowToWorkspace(1)),
        ("Mod+Shift+3", KeyAction::MoveWindowToWorkspace(2)),
        ("Mod+Shift+4", KeyAction::MoveWindowToWorkspace(3)),
    ];

    pairs
        .iter()
        .filter_map(|(chord, act)| KeyBinding::parse(chord, act.clone()))
        .collect()
}

#[derive(Debug, Clone)]
pub struct KeyboardConfig {
    pub xkb_layout: String,
    pub repeat_rate: i32,
    pub repeat_delay: i32,
}

impl Default for KeyboardConfig {
    fn default() -> Self {
        Self {
            xkb_layout: "us".to_string(),
            repeat_rate: 25,
            repeat_delay: 600,
        }
    }
}

/// Declarative configuration for the Night Light / Color Temperature engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NightLightConfig {
    pub enabled: bool,
    pub temperature: u32,
    pub schedule_start: Option<(u8, u8)>,
    pub schedule_end: Option<(u8, u8)>,
}

impl Default for NightLightConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            temperature: 3800,
            schedule_start: None,
            schedule_end: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MochiConfig {
    pub visual_style: VisualStyleConfig,
    pub workspace_count: usize,
    pub default_layout: LayoutMode,
    pub keyboard: KeyboardConfig,
    pub keybindings: Vec<KeyBinding>,
    pub window_rules: Vec<WindowRule>,
    pub outputs: Vec<OutputConfig>,
    pub startup_commands: Vec<String>,
    pub night_light: NightLightConfig,
}

impl Default for MochiConfig {
    fn default() -> Self {
        Self {
            visual_style: VisualStyleConfig::default(),
            workspace_count: 4,
            default_layout: LayoutMode::Floating,
            keyboard: KeyboardConfig::default(),
            keybindings: default_keybindings(),
            window_rules: Vec::new(),
            outputs: Vec::new(),
            startup_commands: Vec::new(),
            night_light: NightLightConfig::default(),
        }
    }
}

impl MochiConfig {
    /// Loads configuration from a KDL file, falling back gracefully to defaults.
    pub fn load_from_path(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let Ok(raw) = fs::read_to_string(path) else {
            info!(?path, "No KDL config file found; using default Mochi Mochi settings");
            return Self::default();
        };

        Self::parse_kdl(&raw)
    }

    pub fn parse_kdl(source: &str) -> Self {
        let mut cfg = Self::default();
        let doc: KdlDocument = match source.parse() {
            Ok(d) => d,
            Err(err) => {
                warn!(?err, "Failed to parse KDL configuration; using defaults");
                return cfg;
            }
        };

        if let Some(vs_node) = doc.get("visual-style") {
            if let Some(style_name) = vs_node.entries().first().and_then(|e| e.value().as_string())
            {
                cfg.visual_style.style = VisualStyle::from_name(style_name);
            }
            if let Some(children) = vs_node.children() {
                if let Some(r) = child_i128(children, "blur-radius") {
                    cfg.visual_style.blur_radius = r.clamp(0, 128) as u32;
                }
                if let Some(op) = child_f64(children, "opacity") {
                    cfg.visual_style.opacity = (op as f32).clamp(0.1, 1.0);
                }
                if let Some(cr) = child_i128(children, "corner-radius") {
                    cfg.visual_style.corner_radius = cr.clamp(0, 64) as u32;
                }
                if let Some(sh) = child_i128(children, "shadow-elevation") {
                    cfg.visual_style.shadow_elevation = sh.clamp(0, 64) as u32;
                }
                if let Some(gap) = child_i128(children, "window-gap") {
                    cfg.visual_style.window_gap = gap.clamp(0, 64) as i32;
                }
                if let Some(tb) = child_i128(children, "titlebar-height") {
                    cfg.visual_style.titlebar_height = tb.clamp(0, 64) as i32;
                }
                if let Some(refr) = child_f64(children, "refraction-strength") {
                    cfg.visual_style.refraction_strength = (refr as f32).clamp(0.0, 0.25);
                }
                if let Some(noise) = child_f64(children, "noise-intensity") {
                    cfg.visual_style.noise_intensity = (noise as f32).clamp(0.0, 0.25);
                }
                if let Some(spec) = child_f64(children, "specular-intensity") {
                    cfg.visual_style.specular_intensity = (spec as f32).clamp(0.0, 2.0);
                }
            }
        }

        if let Some(ws_node) = doc.get("workspaces") {
            if let Some(children) = ws_node.children() {
                if let Some(count) = child_i128(children, "count") {
                    cfg.workspace_count = (count as usize).clamp(1, 32);
                }
                if let Some(layout_str) = child_str(children, "default-layout") {
                    cfg.default_layout = LayoutMode::from_name(layout_str);
                }
                if let Some(gap) = child_i128(children, "window-gap") {
                    cfg.visual_style.window_gap = gap.clamp(0, 64) as i32;
                }
            }
        }

        if let Some(input_node) = doc.get("input") {
            if let Some(input_children) = input_node.children() {
                if let Some(kb_node) = input_children.get("keyboard") {
                    if let Some(kb_children) = kb_node.children() {
                        if let Some(layout) = child_str(kb_children, "xkb-layout") {
                            cfg.keyboard.xkb_layout = layout.to_string();
                        }
                        if let Some(rate) = child_i128(kb_children, "repeat-rate") {
                            cfg.keyboard.repeat_rate = rate.clamp(1, 100) as i32;
                        }
                        if let Some(delay) = child_i128(kb_children, "repeat-delay") {
                            cfg.keyboard.repeat_delay = delay.clamp(100, 2000) as i32;
                        }
                    }
                }
            }
        }

        if let Some(nl_node) = doc.get("night-light") {
            if let Some(children) = nl_node.children() {
                if let Some(en) = child_bool(children, "enabled") {
                    cfg.night_light.enabled = en;
                }
                if let Some(temp) = child_i128(children, "temperature") {
                    cfg.night_light.temperature = (temp as u32).clamp(1000, 10000);
                }
                if let Some(sched_node) = children.get("schedule") {
                    let vals: Vec<&str> = sched_node
                        .entries()
                        .iter()
                        .filter_map(|e| e.value().as_string())
                        .collect();
                    if vals.len() >= 2 {
                        cfg.night_light.schedule_start = parse_time_hm(vals[0]);
                        cfg.night_light.schedule_end = parse_time_hm(vals[1]);
                    }
                }
            }
        }

        if let Some(binds_node) = doc.get("binds") {
            if let Some(children) = binds_node.children() {
                for node in children.nodes() {
                    for parsed_bind in parse_bind_node(node) {
                        // Replace any existing binding with the same chord (modifiers + trigger)
                        cfg.keybindings.retain(|b| {
                            !(b.mod_logo_or_alt == parsed_bind.mod_logo_or_alt
                                && b.shift == parsed_bind.shift
                                && b.ctrl == parsed_bind.ctrl
                                && b.trigger == parsed_bind.trigger)
                        });
                        cfg.keybindings.push(parsed_bind);
                    }
                }
            }
        }

        if let Some(startup_node) = doc.get("startup") {
            if let Some(children) = startup_node.children() {
                for node in children.nodes() {
                    if matches!(node.name().value(), "spawn" | "exec" | "command") {
                        let args: Vec<&str> = node
                            .entries()
                            .iter()
                            .filter_map(|e| e.value().as_string())
                            .collect();
                        let joined = args.join(" ");
                        if !joined.trim().is_empty() {
                            cfg.startup_commands.push(joined);
                        }
                    }
                }
            }
        }

        for node in doc.nodes() {
            if node.name().value() == "window-rule" {
                if let Some(rule) = parse_window_rule_node(node) {
                    cfg.window_rules.push(rule);
                }
            } else if node.name().value() == "output" {
                if let Some(out_cfg) = parse_output_node(node) {
                    cfg.outputs.push(out_cfg);
                }
            } else if node.name().value() == "spawn-at-startup" {
                let args: Vec<&str> = node
                    .entries()
                    .iter()
                    .filter_map(|e| e.value().as_string())
                    .collect();
                let joined = args.join(" ");
                if !joined.trim().is_empty() {
                    cfg.startup_commands.push(joined);
                }
            }
        }

        cfg
    }
}

fn parse_output_node(node: &KdlNode) -> Option<OutputConfig> {
    let name = node
        .entries()
        .iter()
        .find(|e| e.name().is_none())
        .and_then(|e| e.value().as_string())
        .or_else(|| node_prop_str(node, "name"))?
        .to_string();

    let mut position = None;
    let mut scale = None;

    if let Some(children) = node.children() {
        if let Some(pos_node) = children.get("position") {
            let x = node_prop_i128(pos_node, "x")
                .or_else(|| pos_node.entries().first()?.value().as_integer())
                .unwrap_or(0) as i32;
            let y = node_prop_i128(pos_node, "y")
                .or_else(|| pos_node.entries().get(1)?.value().as_integer())
                .unwrap_or(0) as i32;
            position = Some(Point::from((x, y)));
        }
        if let Some(sc) = child_f64(children, "scale") {
            scale = Some(sc.clamp(0.5, 4.0));
        } else if let Some(sc_i) = child_i128(children, "scale") {
            scale = Some((sc_i as f64).clamp(0.5, 4.0));
        }
    }

    Some(OutputConfig {
        name,
        position,
        scale,
    })
}

fn parse_bind_node(node: &KdlNode) -> Vec<KeyBinding> {
    let chord_str = if node.name().value() == "bind" {
        node.entries()
            .iter()
            .find(|e| e.name().is_none())
            .and_then(|e| e.value().as_string())
            .unwrap_or("")
    } else {
        node.name().value()
    };

    if chord_str.is_empty() {
        return Vec::new();
    }

    let action_prop = node_prop_str(node, "action");
    let spawn_prop = node_prop_str(node, "spawn");

    // Expand range shorthand such as `"Mod+Shift+1..4" action="move-window-to-workspace"`
    if chord_str.ends_with("1..4") {
        let prefix = chord_str.trim_end_matches("1..4");
        let is_move = action_prop
            .map(|a| a.contains("move"))
            .unwrap_or(false);
        return (1..=4usize)
            .filter_map(|n| {
                let single_chord = format!("{prefix}{n}");
                let act = if is_move {
                    KeyAction::MoveWindowToWorkspace(n - 1)
                } else {
                    KeyAction::SwitchWorkspace(n - 1)
                };
                KeyBinding::parse(&single_chord, act)
            })
            .collect();
    }

    let action = if let Some(cmd) = spawn_prop {
        Some(KeyAction::SpawnCommand(cmd.to_string()))
    } else if let Some(act_name) = action_prop {
        KeyAction::from_action_str(act_name)
    } else {
        None
    };

    let Some(action) = action else {
        return Vec::new();
    };

    KeyBinding::parse(chord_str, action).into_iter().collect()
}

fn parse_window_rule_node(node: &KdlNode) -> Option<WindowRule> {
    let mut rule = WindowRule {
        app_id_match: node_prop_str(node, "app-id").map(ToString::to_string),
        title_match: node_prop_str(node, "title").map(ToString::to_string),
        ..Default::default()
    };

    if let Some(children) = node.children() {
        if rule.app_id_match.is_none() {
            rule.app_id_match = child_str(children, "app-id").map(ToString::to_string);
        }
        if rule.title_match.is_none() {
            rule.title_match = child_str(children, "title").map(ToString::to_string);
        }
        if let Some(ws) = child_i128(children, "workspace") {
            let ws_1 = (ws as usize).clamp(1, 32);
            rule.target_workspace = Some(ws_1 - 1);
        }
        if let Some(fl) = child_bool(children, "floating") {
            rule.floating = Some(fl);
        }
        if let Some(op) = child_f64(children, "opacity") {
            rule.opacity = Some((op as f32).clamp(0.1, 1.0));
        }
    }

    if rule.app_id_match.is_some() || rule.title_match.is_some() {
        Some(rule)
    } else {
        None
    }
}

fn node_prop_str<'a>(node: &'a KdlNode, prop_name: &str) -> Option<&'a str> {
    node.entries()
        .iter()
        .find(|e| e.name().map(|n| n.value()) == Some(prop_name))?
        .value()
        .as_string()
}

fn node_prop_i128(node: &KdlNode, prop_name: &str) -> Option<i128> {
    node.entries()
        .iter()
        .find(|e| e.name().map(|n| n.value()) == Some(prop_name))?
        .value()
        .as_integer()
}

fn child_bool(doc: &KdlDocument, name: &str) -> Option<bool> {
    doc.get(name)?.entries().first()?.value().as_bool()
}

fn child_i128(doc: &KdlDocument, name: &str) -> Option<i128> {
    doc.get(name)?.entries().first()?.value().as_integer()
}

fn child_f64(doc: &KdlDocument, name: &str) -> Option<f64> {
    doc.get(name)?.entries().first()?.value().as_float()
}

fn child_str<'a>(doc: &'a KdlDocument, name: &str) -> Option<&'a str> {
    doc.get(name)?.entries().first()?.value().as_string()
}

/// Parses "HH:MM" (e.g. "20:30") into `(hour, minute)`.
pub fn parse_time_hm(s: &str) -> Option<(u8, u8)> {
    let mut parts = s.trim().split(':');
    let h: u8 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    if h < 24 && m < 60 {
        Some((h, m))
    } else {
        None
    }
}

/// Checks whether `now` (hour, min) falls within the schedule from `start` to `end`.
/// Handles overnight intervals (e.g., 20:00 -> 06:30) correctly.
pub fn is_time_in_schedule(start: (u8, u8), end: (u8, u8), now: (u8, u8)) -> bool {
    let to_mins = |(h, m): (u8, u8)| (h as u32) * 60 + (m as u32);
    let s = to_mins(start);
    let e = to_mins(end);
    let n = to_mins(now);
    if s <= e {
        n >= s && n < e
    } else {
        // Over midnight, e.g. 20:00 to 06:30
        n >= s || n < e
    }
}

/// Queries the current system local time as `(hour, minute)` via POSIX `localtime_r`.
pub fn current_local_time_hm() -> (u8, u8) {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    let res = unsafe { libc::localtime_r(&now, tm.as_mut_ptr()) };
    if res.is_null() {
        (12, 0)
    } else {
        let tm = unsafe { tm.assume_init() };
        (tm.tm_hour as u8, tm.tm_min as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kdl_config_parsing() {
        let kdl_src = r#"
            visual-style "liquid-glass" {
                blur-radius 32
                opacity 0.75
                corner-radius 18
                shadow-elevation 24
                window-gap 12
                refraction-strength 0.045
            }
            workspaces {
                count 6
                default-layout "tiling"
            }
            input {
                keyboard {
                    xkb-layout "br"
                    repeat-rate 30
                    repeat-delay 400
                }
            }
            output "HDMI-A-1" {
                position x=1920 y=0
                scale 1.25
            }
            window-rule app-id="pavucontrol" {
                floating #true
                opacity 0.90
            }
            window-rule app-id="firefox" title="Private" {
                workspace 2
            }
            startup {
                spawn "waybar"
                spawn "mako --default-timeout 5000"
            }
            binds {
                "Mod+D" spawn="fuzzel"
                "Mod+W" action="toggle-overview"
                "Mod+Minus" action="toggle-scratchpad"
                "Mod+Shift+Minus" action="move-to-scratchpad"
            }
        "#;

        let cfg = MochiConfig::parse_kdl(kdl_src);
        assert_eq!(cfg.visual_style.style, VisualStyle::LiquidGlass);
        assert_eq!(cfg.visual_style.blur_radius, 32);
        assert!((cfg.visual_style.opacity - 0.75).abs() < 1e-5);
        assert_eq!(cfg.visual_style.corner_radius, 18);
        assert_eq!(cfg.visual_style.shadow_elevation, 24);
        assert_eq!(cfg.visual_style.window_gap, 12);
        assert!((cfg.visual_style.refraction_strength - 0.045).abs() < 1e-5);
        assert_eq!(cfg.workspace_count, 6);
        assert_eq!(cfg.default_layout, LayoutMode::MasterStack);
        assert_eq!(cfg.keyboard.xkb_layout, "br");
        assert_eq!(cfg.keyboard.repeat_rate, 30);
        assert_eq!(cfg.keyboard.repeat_delay, 400);

        assert_eq!(cfg.outputs.len(), 1);
        assert_eq!(cfg.outputs[0].name, "HDMI-A-1");
        assert_eq!(cfg.outputs[0].position, Some(Point::from((1920, 0))));
        assert_eq!(cfg.outputs[0].scale, Some(1.25));

        assert_eq!(cfg.window_rules.len(), 2);
        assert_eq!(cfg.window_rules[0].app_id_match.as_deref(), Some("pavucontrol"));
        assert_eq!(cfg.window_rules[0].floating, Some(true));
        assert_eq!(cfg.window_rules[1].target_workspace, Some(1));

        assert_eq!(
            cfg.startup_commands,
            vec![
                "waybar".to_string(),
                "mako --default-timeout 5000".to_string()
            ]
        );

        assert!(cfg.keybindings.iter().any(|b| {
            b.trigger == KeyTrigger::Char('d')
                && b.action == KeyAction::SpawnCommand("fuzzel".to_string())
        }));
        assert!(cfg.keybindings.iter().any(|b| {
            b.trigger == KeyTrigger::Char('w') && b.action == KeyAction::ToggleOverview
        }));
    }

    #[test]
    fn test_night_light_schedule_and_parsing() {
        assert_eq!(parse_time_hm("20:30"), Some((20, 30)));
        assert_eq!(parse_time_hm("06:00"), Some((6, 0)));
        assert_eq!(parse_time_hm("invalid"), None);
        assert_eq!(parse_time_hm("25:00"), None);

        // Standard daytime interval: 08:00 to 18:00
        assert!(is_time_in_schedule((8, 0), (18, 0), (12, 30)));
        assert!(!is_time_in_schedule((8, 0), (18, 0), (20, 0)));
        assert!(!is_time_in_schedule((8, 0), (18, 0), (7, 59)));

        // Overnight interval: 20:00 to 06:30
        assert!(is_time_in_schedule((20, 0), (6, 30), (22, 15)));
        assert!(is_time_in_schedule((20, 0), (6, 30), (2, 0)));
        assert!(is_time_in_schedule((20, 0), (6, 30), (6, 29)));
        assert!(!is_time_in_schedule((20, 0), (6, 30), (6, 30)));
        assert!(!is_time_in_schedule((20, 0), (6, 30), (12, 0)));
        assert!(!is_time_in_schedule((20, 0), (6, 30), (19, 59)));

        let kdl = r#"
            night-light {
                enabled #true
                temperature 3500
                schedule "21:00" "07:00"
            }
        "#;
        let cfg = MochiConfig::parse_kdl(kdl);
        assert!(cfg.night_light.enabled);
        assert_eq!(cfg.night_light.temperature, 3500);
        assert_eq!(cfg.night_light.schedule_start, Some((21, 0)));
        assert_eq!(cfg.night_light.schedule_end, Some((7, 0)));
    }
}
