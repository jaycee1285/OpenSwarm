//! Configuration management for OpenSwarm.

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

use crate::agent::types::AgentType;
use crate::ipc::proto::{ModelCatalogEntry, ModelOption};

/// Application configuration persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Whether WebSocket remote access is enabled.
    #[serde(default)]
    pub ws_enabled: bool,
    /// Password for WebSocket authentication.
    #[serde(default)]
    pub ws_password: String,
    /// Port for WebSocket server.
    #[serde(default = "default_ws_port")]
    pub ws_port: u16,
    /// Selected terminal color scheme.
    #[serde(default = "default_terminal_scheme")]
    pub terminal_scheme: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            ws_enabled: false,
            ws_password: String::new(),
            ws_port: default_ws_port(),
            terminal_scheme: default_terminal_scheme(),
        }
    }
}

fn default_ws_port() -> u16 {
    9384
}

fn default_terminal_scheme() -> String {
    "catppuccin-latte".to_string()
}

/// Returns the path to the config file.
pub fn config_path() -> PathBuf {
    if let Ok(config_dir) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(config_dir).join("openswarm/config.json")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config/openswarm/config.json")
    } else {
        PathBuf::from("openswarm-config.json")
    }
}

/// Load configuration from disk, returning defaults if not found.
pub fn load_config() -> AppConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save configuration to disk.
pub fn save_config(config: &AppConfig) -> std::io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

// Prompt templates — hardcoded for v0.0.1, extractable to config later

pub const PROMPT_TEMPLATES: &[(&str, Option<&str>)] = &[
    (
        "What's the state of this repo using TASKBOARD.md",
        Some("What's the state of this repo using TASKBOARD.md"),
    ),
    (
        "Set up a 30 minute session using TASKS.md",
        Some("Set up a 30 minute session using TASKS.md"),
    ),
    ("Ask Me", None),
];

// Default OpenCode model override
pub const OPENCODE_DEFAULT_MODEL: &str = "opencode/minimax-m2.1-free";

// Model options per agent type (id, display label)

pub fn model_options(agent_type: AgentType) -> &'static [(&'static str, &'static str)] {
    match agent_type {
        AgentType::ClaudeCode => &[
            ("haiku", "Haiku"),
            ("sonnet", "Sonnet"),
            ("opus", "Opus"),
        ],
        AgentType::Codex => &[
            ("gpt-5.3-codex", "GPT-5.3 Codex"),
            ("gpt-5.4-codex", "GPT-5.4 Codex"),
            ("gpt-5.2-codex", "GPT-5.2 Codex"),
            ("gpt-5-codex", "GPT-5 Codex"),
            ("gpt-5.1-codex-mini", "Codex Mini"),
        ],
        AgentType::OpenCode => &[
            ("opencode/minimax-m2.1-free", "Minimax M2.1 (free)"),
            ("openrouter/moonshotai/kimi-k2-thinking", "Kimi K2"),
        ],
    }
}

pub fn default_model(agent_type: AgentType) -> &'static str {
    match agent_type {
        AgentType::ClaudeCode => "sonnet",
        AgentType::Codex => "gpt-5.3-codex",
        AgentType::OpenCode => "openrouter/moonshotai/kimi-k2-thinking",
    }
}
pub fn model_catalog() -> Vec<ModelCatalogEntry> {
    AgentType::all()
        .iter()
        .copied()
        .map(|agent_type| ModelCatalogEntry {
            agent_type,
            default_model: default_model(agent_type).to_string(),
            options: model_options(agent_type)
                .iter()
                .map(|(id, label)| ModelOption {
                    id: (*id).to_string(),
                    label: (*label).to_string(),
                })
                .collect(),
        })
        .collect()
}


// Token pricing per million tokens (input_rate, output_rate) in USD.
// Only applicable to pay-per-token models (OpenRouter, etc.).
// Subscription models (Claude, Codex) return None.
pub fn token_rates(model: &str) -> Option<(f64, f64)> {
    // OpenRouter published rates
    match model {
        "opencode/minimax-m2.1-free" => Some((0.0, 0.0)), // free tier
        "openrouter/moonshotai/kimi-k2-thinking" => Some((0.60, 2.00)),
        _ => {
            // Try prefix matching for openrouter models
            if model.starts_with("openrouter/") || model.starts_with("opencode/") {
                // Unknown OpenRouter model — return None, show raw tokens
                None
            } else {
                // Subscription model (Claude, Codex) — no per-token cost
                None
            }
        }
    }
}

/// Calculate dollar cost from token counts and model rates.
/// Returns None for subscription models.
pub fn calculate_cost(model: &str, input_tokens: u64, output_tokens: u64) -> Option<f64> {
    let (in_rate, out_rate) = token_rates(model)?;
    Some(
        input_tokens as f64 * in_rate / 1_000_000.0
            + output_tokens as f64 * out_rate / 1_000_000.0,
    )
}

// Initial PTY size — updated dynamically once VTE computes its allocation
pub const DEFAULT_PTY_ROWS: u16 = 30;
pub const DEFAULT_PTY_COLS: u16 = 120;

// Terminal color themes

#[derive(Clone, Copy, Debug)]
pub struct TerminalThemeOption {
    pub id: &'static str,
    pub label: &'static str,
    pub file_name: &'static str,
}

pub const TERMINAL_THEME_OPTIONS: &[TerminalThemeOption] = &[
    TerminalThemeOption {
        id: "catppuccin-latte",
        label: "Catppuccin Latte",
        file_name: "catppuccin-latte.tmTheme",
    },
    TerminalThemeOption {
        id: "catppuccin-macchiato",
        label: "Catppuccin Macchiato",
        file_name: "catppuccin-macchiato.tmTheme",
    },
    TerminalThemeOption {
        id: "gruvbox-material-light-hard",
        label: "Gruvbox Material Light",
        file_name: "gruvbox-material-light-hard.tmTheme",
    },
    TerminalThemeOption {
        id: "gruvbox-material-dark-hard",
        label: "Gruvbox Material Dark",
        file_name: "gruvbox-material-dark-hard.tmTheme",
    },
    TerminalThemeOption {
        id: "ayu-light",
        label: "Ayu Light",
        file_name: "Ayu Light.tmTheme",
    },
    TerminalThemeOption {
        id: "ayu-mirage",
        label: "Ayu Mirage",
        file_name: "Ayu Mirage.tmTheme",
    },
];

use gtk4::gdk;
use vte4::prelude::*;

#[derive(Clone, Copy)]
struct TerminalPalette {
    fg: gdk::RGBA,
    bg: gdk::RGBA,
    cursor: gdk::RGBA,
    selection: gdk::RGBA,
    palette: [gdk::RGBA; 16],
}

#[derive(Default)]
struct ThemeEntry {
    name: Option<String>,
    foreground: Option<gdk::RGBA>,
    background: Option<gdk::RGBA>,
    caret: Option<gdk::RGBA>,
    selection: Option<gdk::RGBA>,
    invisibles: Option<gdk::RGBA>,
}

fn rgba(hex: &str) -> gdk::RGBA {
    let h = hex.trim_start_matches('#').as_bytes();
    let r = hex_byte(h[0], h[1]) as f32 / 255.0;
    let g = hex_byte(h[2], h[3]) as f32 / 255.0;
    let b = hex_byte(h[4], h[5]) as f32 / 255.0;
    gdk::RGBA::new(r, g, b, 1.0)
}

fn hex_byte(hi: u8, lo: u8) -> u8 {
    (hex_nibble(hi) << 4) | hex_nibble(lo)
}

fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

fn hex_to_rgba(value: &str) -> Option<gdk::RGBA> {
    let trimmed = value.trim();
    if trimmed.len() != 7 || !trimmed.starts_with('#') {
        return None;
    }
    Some(rgba(trimmed))
}

fn tmtheme_root() -> PathBuf {
    if let Ok(data_home) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(data_home).join("themes/tmthemes")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/themes/tmthemes")
    } else {
        PathBuf::from(".")
    }
}

fn find_theme_option(id: &str) -> TerminalThemeOption {
    TERMINAL_THEME_OPTIONS
        .iter()
        .copied()
        .find(|option| option.id == id)
        .unwrap_or(TERMINAL_THEME_OPTIONS[0])
}

fn parse_tmtheme(contents: &str) -> Vec<ThemeEntry> {
    let mut entries = Vec::new();
    let mut in_settings_array = false;
    let mut pending_key: Option<String> = None;
    let mut dict_depth = 0usize;
    let mut current: Option<ThemeEntry> = None;

    for raw_line in contents.lines() {
        let line = raw_line.trim();

        if !in_settings_array {
            if line == "<key>settings</key>" {
                pending_key = Some("settings".to_string());
            } else if line == "<array>" && pending_key.as_deref() == Some("settings") {
                in_settings_array = true;
                pending_key = None;
            }
            continue;
        }

        if line == "<dict>" {
            dict_depth += 1;
            if dict_depth == 1 {
                current = Some(ThemeEntry::default());
            }
            continue;
        }

        if line == "</dict>" {
            if dict_depth == 1 {
                if let Some(entry) = current.take() {
                    entries.push(entry);
                }
            }
            dict_depth = dict_depth.saturating_sub(1);
            continue;
        }

        if line == "</array>" && dict_depth == 0 {
            break;
        }

        if dict_depth == 0 {
            continue;
        }

        if let Some(key) = parse_tag_value(line, "key") {
            pending_key = Some(key);
            continue;
        }

        if let Some(value) = parse_tag_value(line, "string") {
            let Some(ref mut entry) = current else {
                continue;
            };
            let Some(key) = pending_key.take() else {
                continue;
            };
            match key.as_str() {
                "name" => entry.name = Some(value),
                "foreground" => entry.foreground = hex_to_rgba(&value),
                "background" => entry.background = hex_to_rgba(&value),
                "caret" => entry.caret = hex_to_rgba(&value),
                "selection" => entry.selection = hex_to_rgba(&value),
                "invisibles" => entry.invisibles = hex_to_rgba(&value),
                _ => {}
            }
        }
    }

    entries
}

fn parse_tag_value(line: &str, tag: &str) -> Option<String> {
    let prefix = format!("<{tag}>");
    let suffix = format!("</{tag}>");
    let value = line.strip_prefix(&prefix)?.strip_suffix(&suffix)?;
    Some(value.to_string())
}

fn mix(left: gdk::RGBA, right: gdk::RGBA, ratio: f32) -> gdk::RGBA {
    let inv = 1.0 - ratio;
    gdk::RGBA::new(
        left.red() * inv + right.red() * ratio,
        left.green() * inv + right.green() * ratio,
        left.blue() * inv + right.blue() * ratio,
        1.0,
    )
}

fn relative_luminance(color: gdk::RGBA) -> f32 {
    0.2126 * color.red() + 0.7152 * color.green() + 0.0722 * color.blue()
}

fn brighten(color: gdk::RGBA, bg: gdk::RGBA, fg: gdk::RGBA) -> gdk::RGBA {
    if relative_luminance(bg) < 0.5 {
        mix(color, fg, 0.22)
    } else {
        mix(color, bg, 0.22)
    }
}

fn theme_entry<'a>(entries: &'a [ThemeEntry], name: &str) -> Option<&'a ThemeEntry> {
    entries.iter().find(|entry| entry.name.as_deref() == Some(name))
}

fn theme_color(entries: &[ThemeEntry], name: &str, fallback: gdk::RGBA) -> gdk::RGBA {
    theme_entry(entries, name)
        .and_then(|entry| entry.foreground)
        .unwrap_or(fallback)
}

fn load_terminal_palette(id: &str) -> Option<TerminalPalette> {
    let option = find_theme_option(id);
    let path = tmtheme_root().join(option.file_name);
    let contents = std::fs::read_to_string(path).ok()?;
    let entries = parse_tmtheme(&contents);
    let root = entries.first()?;

    let fg = root.foreground?;
    let bg = root.background?;
    let cursor = root.caret.unwrap_or(fg);
    let selection = root.selection.unwrap_or(mix(bg, fg, 0.18));
    let comments = root
        .invisibles
        .or_else(|| theme_entry(&entries, "Comments").and_then(|entry| entry.foreground))
        .unwrap_or(mix(fg, bg, 0.45));
    let red = theme_color(&entries, "Variables", fg);
    let green = theme_color(&entries, "Strings", fg);
    let yellow = theme_color(&entries, "Classes", theme_color(&entries, "Numbers", fg));
    let blue = theme_color(&entries, "Functions", fg);
    let magenta = theme_color(&entries, "Keywords", fg);
    let cyan = theme_color(&entries, "Support", fg);
    let bright_red = theme_color(&entries, "Numbers", brighten(red, bg, fg));
    let bright_blue = theme_color(&entries, "Methods", brighten(blue, bg, fg));

    Some(TerminalPalette {
        fg,
        bg,
        cursor,
        selection,
        palette: [
            fg,
            red,
            green,
            yellow,
            blue,
            magenta,
            cyan,
            bg,
            comments,
            brighten(bright_red, bg, fg),
            brighten(green, bg, fg),
            brighten(yellow, bg, fg),
            brighten(bright_blue, bg, fg),
            brighten(magenta, bg, fg),
            brighten(cyan, bg, fg),
            mix(bg, fg, 0.08),
        ],
    })
}

fn fallback_terminal_palette() -> TerminalPalette {
    let fg = rgba("#4c4f69");
    let bg = rgba("#eff1f5");
    let red = rgba("#d20f39");
    let green = rgba("#40a02b");
    let yellow = rgba("#df8e1d");
    let blue = rgba("#1e66f5");
    let magenta = rgba("#8839ef");
    let cyan = rgba("#179299");
    let comments = rgba("#9ca0b0");
    TerminalPalette {
        fg,
        bg,
        cursor: fg,
        selection: rgba("#bcc0cc"),
        palette: [
            fg,
            red,
            green,
            yellow,
            blue,
            magenta,
            cyan,
            bg,
            comments,
            brighten(red, bg, fg),
            brighten(green, bg, fg),
            brighten(yellow, bg, fg),
            brighten(blue, bg, fg),
            brighten(magenta, bg, fg),
            brighten(cyan, bg, fg),
            mix(bg, fg, 0.08),
        ],
    }
}

pub fn apply_terminal_theme_by_id(terminal: &vte4::Terminal, id: &str) {
    let theme = load_terminal_palette(id).unwrap_or_else(fallback_terminal_palette);
    let palette_refs: Vec<&gdk::RGBA> = theme.palette.iter().collect();
    terminal.set_colors(Some(&theme.fg), Some(&theme.bg), &palette_refs);
    terminal.set_color_cursor(Some(&theme.cursor));
    terminal.set_color_cursor_foreground(Some(&theme.bg));
    terminal.set_color_highlight(Some(&theme.selection));
    terminal.set_color_highlight_foreground(Some(&theme.fg));
}

pub fn apply_terminal_theme(terminal: &vte4::Terminal) {
    let config = load_config();
    apply_terminal_theme_by_id(terminal, &config.terminal_scheme);
}
