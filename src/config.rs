//! `~/.config/svitek/config.toml`. Minimal by design: every key here has a
//! reason. Missing file or missing keys => defaults; a malformed file is an
//! error printed at start (and defaults are used).
//!
//! ```toml
//! # Width of the thumbnails in pixels; height follows the output's aspect ratio.
//! thumbnail_width = 240
//! # Where the panel sits: "center" (default) lays the workspaces out side by
//! # side in the middle of the screen; "left" / "right" stack them in a
//! # full-height column along that edge.
//! position = "center"
//! # Close the panel as soon as a workspace is clicked (default). With `false`
//! # a click switches to the workspace but leaves the panel open, so several
//! # can be visited in a row; Enter always closes.
//! close_on_select = true
//! # How the toggle key behaves: "toggle" (default) is a switch — press it once
//! # to open the panel, again to close it. "hold" is alt-tab: while the panel is
//! # up every further `svitek toggle` steps the selection one workspace on
//! # (wrapping), and letting go of the modifier (Super/Alt/Ctrl/Meta/Hyper)
//! # commits it, exactly as Enter would.
//! mode = "toggle"
//! # Hold mode only: whether the press that opens the panel already selects
//! # the *next* workspace (default, the alt-tab convention — a quick tap
//! # switches), or leaves the selection on the current one so the first press
//! # only opens the panel.
//! hold_selects_next = true
//!
//! [colors]
//! background = "#1e1e2ecc"   # panel background (RGBA hex allowed)
//! foreground = "#cdd6f4"     # window titles
//! dim        = "#a6adc8"     # app_id, workspace labels
//! focused    = "#89b4fa"     # border/marker of the focused workspace row
//! ```

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub thumbnail_width: u32,
    pub position: Position,
    pub close_on_select: bool,
    pub mode: Mode,
    /// Hold mode: does the opening press already step to the next workspace?
    /// True is alt-tab (a quick tap switches to the next workspace); false
    /// means the first press only opens the panel and the selection stays on
    /// the workspace the user is on. Ignored in toggle mode.
    pub hold_selects_next: bool,
    pub colors: Colors,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Left,
    Center,
    Right,
}

/// What the key bound to `svitek toggle` does.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// A switch: the binding opens the panel, and pressing it again closes it
    /// and goes back where the user came from.
    Toggle,
    /// Alt-tab: the binding opens the panel and every further press *steps* the
    /// selection one workspace on (wrapping), because sway consumes the key
    /// combination itself and the panel only ever sees another `toggle`.
    /// Releasing the modifier commits, the way Enter does.
    Hold,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Colors {
    pub background: String,
    pub foreground: String,
    pub dim: String,
    pub focused: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            thumbnail_width: 240,
            position: Position::Center,
            close_on_select: true,
            mode: Mode::Toggle,
            hold_selects_next: true,
            colors: Colors::default(),
        }
    }
}

impl Default for Colors {
    fn default() -> Self {
        Colors {
            background: "#1e1e2ecc".into(),
            foreground: "#cdd6f4".into(),
            dim: "#a6adc8".into(),
            focused: "#89b4fa".into(),
        }
    }
}

/// `$XDG_CONFIG_HOME/svitek/config.toml` (defaults to `~/.config/svitek/config.toml`).
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("svitek").join("config.toml"))
}

/// Load the config, falling back to defaults. Returns `(config, Some(error))`
/// when the file existed but could not be parsed.
pub fn load() -> (Config, Option<String>) {
    let Some(path) = config_path() else { return (Config::default(), None) };
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(format!("{}: {}", path.display(), e))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
        Err(e) => (Config::default(), Some(format!("{}: {}", path.display(), e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        assert_eq!(toml::from_str::<Config>("").unwrap(), Config::default());
    }

    #[test]
    fn partial_override() {
        let c: Config = toml::from_str("thumbnail_width = 320\n[colors]\nfocused = \"#fff\"\n").unwrap();
        assert_eq!(c.thumbnail_width, 320);
        assert_eq!(c.colors.focused, "#fff");
        assert_eq!(c.colors.dim, Colors::default().dim);
        assert_eq!(c.position, Position::Center);
        assert!(c.close_on_select);
        assert_eq!(c.mode, Mode::Toggle);
        assert!(c.hold_selects_next);
    }

    #[test]
    fn hold_selects_next_parses() {
        let c: Config = toml::from_str("mode = \"hold\"\nhold_selects_next = false\n").unwrap();
        assert_eq!(c.mode, Mode::Hold);
        assert!(!c.hold_selects_next);
    }

    #[test]
    fn mode_parses() {
        let c: Config = toml::from_str("mode = \"hold\"\n").unwrap();
        assert_eq!(c.mode, Mode::Hold);
        // The other keys are untouched by it.
        assert_eq!(c.position, Position::Center);
        let c: Config = toml::from_str("mode = \"toggle\"\n").unwrap();
        assert_eq!(c.mode, Mode::Toggle);
        // Only the two spellings, and only in lowercase.
        assert!(toml::from_str::<Config>("mode = \"Hold\"\n").is_err());
        assert!(toml::from_str::<Config>("mode = \"alt-tab\"\n").is_err());
    }

    #[test]
    fn position_and_close_on_select_parse() {
        let c: Config = toml::from_str("position = \"left\"\nclose_on_select = false\n").unwrap();
        assert_eq!(c.position, Position::Left);
        assert!(!c.close_on_select);
        let c: Config = toml::from_str("position = \"right\"\n").unwrap();
        assert_eq!(c.position, Position::Right);
        assert!(toml::from_str::<Config>("position = \"top\"\n").is_err());
    }

    #[test]
    fn unknown_key_is_error() {
        assert!(toml::from_str::<Config>("animations = true\n").is_err());
    }
}
