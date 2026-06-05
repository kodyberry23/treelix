//! treelix configuration: optional `~/.config/treelix/config.toml`.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Theme name: a treelix theme file stem, the built-in `nord-aurora`, or
    /// the special value `helix` to derive colors from the active Helix theme.
    pub theme: String,
    /// Whether to render Nerd Font glyphs (set false for ASCII fallbacks).
    pub icons: bool,
    /// Show dotfiles by default.
    pub show_hidden: bool,
    /// Show git-ignored files by default.
    pub show_ignored: bool,
    /// Command used to open a file in the editor. `{mode}` is replaced with
    /// `open`/`vsplit`/`hsplit` and `{path}` with the absolute path. When unset,
    /// treelix uses its built-in Helix-socket dispatch (see `editor.rs`).
    pub open_command: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "nord-aurora".to_string(),
            icons: true,
            show_hidden: false,
            show_ignored: false,
            open_command: None,
        }
    }
}

impl Config {
    pub fn load() -> Config {
        if let Some(dir) = treelix_config_dir() {
            let path = dir.join("config.toml");
            if let Ok(s) = std::fs::read_to_string(&path) {
                match toml::from_str::<Config>(&s) {
                    Ok(cfg) => return cfg,
                    Err(e) => eprintln!("treelix: invalid config.toml: {e}"),
                }
            }
        }
        Config::default()
    }
}

/// `~/.config/treelix`, honoring `$XDG_CONFIG_HOME`.
pub fn treelix_config_dir() -> Option<PathBuf> {
    config_home().map(|c| c.join("treelix"))
}

/// `~/.config/helix`, honoring `$XDG_CONFIG_HOME`. On macOS Helix uses
/// `~/.config/helix`, not the platform application-support dir.
pub fn helix_config_dir() -> Option<PathBuf> {
    config_home().map(|c| c.join("helix"))
}

fn config_home() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
}
