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
    /// Render expand/collapse chevrons (▸/▾) on directories, nvim-tree style.
    pub arrows: bool,
    /// Draw tree connector lines (│ ├ └) for indentation. When false (default),
    /// indentation is plain whitespace with a chevron on each directory, exactly
    /// like nvim-tree with `renderer.indent_markers.enable = false`.
    pub indent_markers: bool,
    /// Show dotfiles by default.
    pub show_hidden: bool,
    /// Show git-ignored files by default.
    pub show_ignored: bool,
    /// Sort mode: name | modified | extension | filetype.
    pub sort: String,
    /// Place files before directories.
    pub files_first: bool,
    /// Collapse chains of sole-child directories into one line by default.
    pub group_empty: bool,
    /// Enable mouse support (click to open/cd, scroll).
    pub mouse: bool,
    /// Persist bookmarks to `~/.config/treelix/bookmarks`.
    pub bookmarks_persist: bool,
    /// Keep folders visible during a live filter (`f`), matching only files by
    /// name — nvim-tree's `live_filter.always_show_folders`. Set false to also
    /// hide non-matching folders.
    pub live_filter_show_folders: bool,
    /// Substring patterns hidden when the custom filter (`U`) is active.
    pub exclude: Vec<String>,
    /// Filenames (lowercase) highlighted as "special".
    pub special_files: Vec<String>,
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
            arrows: true,
            indent_markers: false,
            show_hidden: false,
            show_ignored: false,
            sort: "name".to_string(),
            files_first: false,
            group_empty: false,
            mouse: true,
            bookmarks_persist: false,
            live_filter_show_folders: true,
            exclude: Vec::new(),
            special_files: default_special_files(),
            open_command: None,
        }
    }
}

fn default_special_files() -> Vec<String> {
    [
        "cargo.toml",
        "makefile",
        "readme.md",
        "readme",
        "license",
        "license.md",
        "package.json",
        "dockerfile",
        "flake.nix",
        ".gitignore",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
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
