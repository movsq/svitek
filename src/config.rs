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
    pub colors: Colors,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Left,
    Center,
    Right,
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
