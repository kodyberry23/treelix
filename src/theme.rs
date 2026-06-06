//! Theming. treelix owns its theme: it loads its own theme file (or a built-in
//! one), with an optional opt-in import that derives colors from the active
//! Helix theme.

use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};

use crate::git::GitStatus;

/// The built-in default theme, embedded so treelix looks right with zero config.
const BUILTIN_NORD_AURORA: &str = include_str!("../themes/nord-aurora.toml");

#[derive(Debug, Clone)]
pub struct Theme {
    pub text: Style,
    pub background: Style,
    pub selection: Style,
    pub directory: Style,
    pub root: Style,
    pub indent_marker: Style,
    pub arrow: Style,
    pub icon: Style,
    pub folder_icon: Style,
    pub symlink: Style,
    pub executable: Style,
    pub git_staged: Style,
    pub git_dirty: Style,
    pub git_deleted: Style,
    pub git_renamed: Style,
    pub git_conflict: Style,
    pub git_untracked: Style,
    pub git_ignored: Style,
    pub cut: Style,
    pub copy: Style,
    pub special: Style,
    pub opened: Style,
    pub bookmark: Style,
    pub selected: Style,
    pub help: Style,
    pub help_title: Style,
    pub prompt: Style,
    pub filter_prefix: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::from_toml_str(BUILTIN_NORD_AURORA).expect("built-in theme parses")
    }
}

impl Theme {
    pub fn git_style(&self, status: GitStatus) -> Style {
        match status {
            GitStatus::Staged => self.git_staged,
            GitStatus::Dirty => self.git_dirty,
            GitStatus::Deleted => self.git_deleted,
            GitStatus::Renamed => self.git_renamed,
            GitStatus::Conflict => self.git_conflict,
            GitStatus::Untracked => self.git_untracked,
            GitStatus::Ignored => self.git_ignored,
        }
    }

    /// Load a theme by name: first `~/.config/treelix/themes/<name>.toml`, then
    /// the built-in of that name, else the built-in default. `name == "helix"`
    /// derives the theme from the active Helix theme.
    pub fn load(name: &str) -> Theme {
        if name == "helix" {
            if let Some(t) = crate::theme::helix::import() {
                return t;
            }
            return Theme::default();
        }
        if let Some(dir) = crate::config::treelix_config_dir() {
            let path = dir.join("themes").join(format!("{name}.toml"));
            if let Ok(s) = std::fs::read_to_string(&path) {
                if let Some(t) = Theme::from_toml_str(&s) {
                    return t;
                }
            }
        }
        // Built-in named themes (only nord-aurora today).
        if name == "nord-aurora" {
            return Theme::default();
        }
        Theme::default()
    }

    /// Parse a treelix theme TOML string.
    pub fn from_toml_str(s: &str) -> Option<Theme> {
        let value: toml::Value = toml::from_str(s).ok()?;
        let palette = parse_palette(value.get("palette"));
        let styles = value.get("styles").and_then(|v| v.as_table());

        let get = |key: &str, fallback: Style| -> Style {
            styles
                .and_then(|t| t.get(key))
                .and_then(|v| v.as_str())
                .and_then(|spec| parse_style(spec, &palette))
                .unwrap_or(fallback)
        };

        let text = get("text", Style::default());
        Some(Theme {
            text,
            background: get("background", Style::default()),
            selection: get("selection", Style::default()),
            directory: get("directory", text),
            root: get("root", text),
            indent_marker: get("indent_marker", text),
            arrow: get("arrow", text),
            icon: get("icon", text),
            folder_icon: get("folder_icon", text),
            symlink: get("symlink", text),
            executable: get("executable", text),
            git_staged: get("git_staged", text),
            git_dirty: get("git_dirty", text),
            git_deleted: get("git_deleted", text),
            git_renamed: get("git_renamed", text),
            git_conflict: get("git_conflict", text),
            git_untracked: get("git_untracked", text),
            git_ignored: get("git_ignored", text),
            cut: get("cut", text),
            copy: get("copy", text),
            special: get("special", text),
            opened: get("opened", text),
            bookmark: get("bookmark", text),
            selected: get("selected", text),
            help: get("help", text),
            help_title: get("help_title", text),
            prompt: get("prompt", text),
            filter_prefix: get("filter_prefix", text),
        })
    }
}

fn parse_palette(value: Option<&toml::Value>) -> HashMap<String, Color> {
    let mut map = HashMap::new();
    if let Some(table) = value.and_then(|v| v.as_table()) {
        for (k, v) in table {
            if let Some(s) = v.as_str() {
                if let Some(c) = parse_hex(s) {
                    map.insert(k.clone(), c);
                }
            }
        }
    }
    map
}

/// Parse a `"fg / bg / mods"` style spec.
fn parse_style(spec: &str, palette: &HashMap<String, Color>) -> Option<Style> {
    let mut parts = spec.split('/');
    let fg = parts.next().map(str::trim).unwrap_or("");
    let bg = parts.next().map(str::trim).unwrap_or("");
    let mods = parts.next().map(str::trim).unwrap_or("");

    let mut style = Style::default();
    if let Some(c) = resolve_color(fg, palette) {
        style = style.fg(c);
    }
    if let Some(c) = resolve_color(bg, palette) {
        style = style.bg(c);
    }
    for m in mods.split(',') {
        match m.trim().to_lowercase().as_str() {
            "bold" => style = style.add_modifier(Modifier::BOLD),
            "italic" => style = style.add_modifier(Modifier::ITALIC),
            "underline" | "underlined" => style = style.add_modifier(Modifier::UNDERLINED),
            "dim" => style = style.add_modifier(Modifier::DIM),
            "reversed" | "reverse" => style = style.add_modifier(Modifier::REVERSED),
            _ => {}
        }
    }
    Some(style)
}

/// Resolve a color token: palette name, `#hex`, or `none`/empty → no color.
fn resolve_color(token: &str, palette: &HashMap<String, Color>) -> Option<Color> {
    let token = token.trim();
    if token.is_empty() || token.eq_ignore_ascii_case("none") {
        return None;
    }
    if let Some(c) = palette.get(token) {
        return Some(*c);
    }
    parse_hex(token)
}

fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

/// Optional Helix theme import.
pub mod helix {
    use super::*;

    /// Derive a treelix theme from Helix's active theme. Best effort: any scope
    /// that can't be resolved falls back to the built-in default.
    pub fn import() -> Option<Theme> {
        let helix_dir = crate::config::helix_config_dir()?;
        let config = std::fs::read_to_string(helix_dir.join("config.toml")).ok()?;
        let cfg: toml::Value = toml::from_str(&config).ok()?;
        let theme_name = cfg.get("theme").and_then(|v| v.as_str())?;

        let theme_value = load_helix_theme(theme_name, &helix_dir, 0)?;
        let palette = parse_palette(theme_value.get("palette"));

        // Resolve a Helix scope's fg color (scope may be a bare color string or
        // a table with `fg`).
        let scope_fg = |scope: &str| -> Option<Color> {
            let v = theme_value.get(scope)?;
            let token = match v {
                toml::Value::String(s) => s.as_str(),
                toml::Value::Table(t) => t.get("fg").and_then(|f| f.as_str())?,
                _ => return None,
            };
            resolve_color(token, &palette)
        };

        let mut theme = Theme::default();
        let style_of =
            |c: Option<Color>, base: Style| c.map(|c| Style::default().fg(c)).unwrap_or(base);

        if let Some(c) = scope_fg("ui.text") {
            theme.text = Style::default().fg(c);
            theme.icon = theme.text;
        }
        // Helix has no Directory scope; `function` reads as the cyan accent in
        // typical themes (and matches the old broot sidebar look).
        theme.directory =
            style_of(scope_fg("function"), theme.directory).add_modifier(Modifier::BOLD);
        theme.folder_icon = style_of(scope_fg("function"), theme.folder_icon);
        theme.root = style_of(scope_fg("ui.text.focus"), theme.root).add_modifier(Modifier::BOLD);
        theme.indent_marker = style_of(scope_fg("comment"), theme.indent_marker);
        theme.arrow = theme.indent_marker;
        theme.symlink = style_of(scope_fg("string.special"), theme.symlink);
        theme.executable = style_of(scope_fg("string"), theme.executable);
        theme.git_staged = style_of(scope_fg("diff.plus"), theme.git_staged);
        theme.git_dirty = style_of(scope_fg("diff.delta"), theme.git_dirty);
        theme.git_renamed = theme.git_dirty;
        theme.git_deleted = style_of(scope_fg("diff.minus"), theme.git_deleted);
        theme.git_conflict = theme.git_deleted;
        theme.git_untracked = style_of(scope_fg("special"), theme.git_untracked);
        theme.git_ignored = style_of(scope_fg("comment"), theme.git_ignored);
        theme.special = style_of(scope_fg("constant"), theme.special).add_modifier(Modifier::BOLD);
        theme.opened =
            style_of(scope_fg("ui.text.focus"), theme.opened).add_modifier(Modifier::BOLD);
        theme.bookmark = style_of(scope_fg("keyword"), theme.bookmark);
        theme.filter_prefix = style_of(scope_fg("keyword"), theme.filter_prefix);
        // Selection: use ui.selection bg.
        if let Some(t) = theme_value.get("ui.selection").and_then(|v| v.as_table()) {
            if let Some(c) = t
                .get("bg")
                .and_then(|b| b.as_str())
                .and_then(|s| resolve_color(s, &palette))
            {
                theme.selection = Style::default().bg(c);
            }
        }
        Some(theme)
    }

    fn load_helix_theme(name: &str, helix_dir: &PathBuf, depth: u8) -> Option<toml::Value> {
        if depth > 8 {
            return None;
        }
        let candidates = [
            helix_dir.join("themes").join(format!("{name}.toml")),
            helix_runtime_themes().map(|p| p.join(format!("{name}.toml")))?,
        ];
        let mut content = None;
        for cand in candidates {
            if let Ok(s) = std::fs::read_to_string(&cand) {
                content = Some(s);
                break;
            }
        }
        let value: toml::Value = toml::from_str(&content?).ok()?;

        // Resolve `inherits` by merging the parent underneath this theme.
        if let Some(parent_name) = value.get("inherits").and_then(|v| v.as_str()) {
            if let Some(parent) = load_helix_theme(parent_name, helix_dir, depth + 1) {
                return Some(merge_themes(parent, value));
            }
        }
        Some(value)
    }

    fn helix_runtime_themes() -> Option<PathBuf> {
        if let Some(rt) = std::env::var_os("HELIX_RUNTIME") {
            return Some(PathBuf::from(rt).join("themes"));
        }
        // Common build-from-source location used by the user's setup.
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("projects/helix/runtime/themes"))
    }

    /// Child keys override parent keys.
    fn merge_themes(mut parent: toml::Value, child: toml::Value) -> toml::Value {
        if let (Some(p), Some(c)) = (parent.as_table_mut(), child.as_table()) {
            for (k, v) in c {
                p.insert(k.clone(), v.clone());
            }
        }
        parent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_parses() {
        let t = Theme::default();
        // directory should be the frost1 cyan, bold.
        assert_eq!(t.directory.fg, Some(Color::Rgb(0x74, 0xBC, 0xD9)));
        assert!(t.directory.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn parse_style_spec() {
        let mut pal = HashMap::new();
        pal.insert("red".to_string(), Color::Rgb(255, 0, 0));
        let s = parse_style("red / none / bold,italic", &pal).unwrap();
        assert_eq!(s.fg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(s.bg, None);
        assert!(s.add_modifier.contains(Modifier::BOLD));
        assert!(s.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("#1A1F28"), Some(Color::Rgb(0x1A, 0x1F, 0x28)));
        assert_eq!(parse_hex("bad"), None);
    }
}
