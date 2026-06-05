//! Nerd Font file/folder icons, a curated subset of nvim-web-devicons resolved
//! by filename then extension.

use crate::tree::NodeKind;

pub const FOLDER_CLOSED: &str = "";
pub const FOLDER_OPEN: &str = "";
pub const FILE_DEFAULT: &str = "";
pub const SYMLINK: &str = "";

pub const ARROW_CLOSED: &str = "";
pub const ARROW_OPEN: &str = "";

/// Resolve the glyph for a node. Directories use folder/arrow glyphs handled by
/// the builder; this returns file icons (and a default folder icon).
pub fn file_icon(name: &str, kind: NodeKind, expanded: bool) -> &'static str {
    match kind {
        NodeKind::Directory => {
            if expanded {
                FOLDER_OPEN
            } else {
                FOLDER_CLOSED
            }
        }
        NodeKind::Symlink { to_dir: true } => {
            if expanded {
                FOLDER_OPEN
            } else {
                FOLDER_CLOSED
            }
        }
        NodeKind::Symlink { to_dir: false } => SYMLINK,
        NodeKind::File => by_name(name),
    }
}

fn by_name(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if let Some(icon) = by_filename(&lower) {
        return icon;
    }
    if let Some(ext) = lower.rsplit_once('.').map(|(_, e)| e) {
        if let Some(icon) = by_extension(ext) {
            return icon;
        }
    }
    FILE_DEFAULT
}

fn by_filename(name: &str) -> Option<&'static str> {
    Some(match name {
        "cargo.toml" | "cargo.lock" => "",
        "package.json" | "package-lock.json" => "",
        "dockerfile" | ".dockerignore" => "",
        "makefile" => "",
        ".gitignore" | ".gitattributes" | ".gitmodules" => "",
        ".gitconfig" => "",
        "readme.md" | "readme" | "readme.txt" => "",
        "license" | "license.md" | "license.txt" => "",
        ".env" | ".env.local" => "",
        ".zshrc" | ".bashrc" | ".bash_profile" | ".profile" => "",
        "config.toml" | "config.kdl" => "",
        ".editorconfig" => "",
        "flake.nix" | "default.nix" | "shell.nix" => "",
        _ => return None,
    })
}

fn by_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "",
        "go" => "",
        "py" | "pyc" | "pyw" => "",
        "js" | "mjs" | "cjs" => "",
        "ts" => "",
        "jsx" => "",
        "tsx" => "",
        "html" | "htm" => "",
        "css" => "",
        "scss" | "sass" => "",
        "json" | "jsonc" => "",
        "toml" => "",
        "yaml" | "yml" => "",
        "md" | "markdown" => "",
        "lua" => "",
        "vim" => "",
        "sh" | "bash" | "zsh" | "fish" => "",
        "c" | "h" => "",
        "cpp" | "cc" | "cxx" | "hpp" => "",
        "java" => "",
        "kt" | "kts" => "",
        "rb" => "",
        "php" => "",
        "ex" | "exs" => "",
        "erl" | "hrl" => "",
        "hs" => "",
        "nix" => "",
        "kdl" => "",
        "sql" => "",
        "txt" | "log" => "",
        "pdf" => "",
        "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "7z" | "rar" => "",
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" => "",
        "svg" => "ﰟ",
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => "",
        "mp4" | "mkv" | "mov" | "avi" | "webm" => "",
        "ttf" | "otf" | "woff" | "woff2" => "",
        "git" => "",
        _ => return None,
    })
}

/// ASCII fallback icons when Nerd Fonts are disabled.
pub mod ascii {
    pub const FOLDER_CLOSED: &str = "▸";
    pub const FOLDER_OPEN: &str = "▾";
    pub const FILE_DEFAULT: &str = " ";
    pub const SYMLINK: &str = "→";
    pub const ARROW_CLOSED: &str = ">";
    pub const ARROW_OPEN: &str = "v";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_by_extension() {
        assert_eq!(by_name("main.rs"), "");
        assert_eq!(by_name("notes.md"), "");
    }

    #[test]
    fn resolves_by_filename_over_extension() {
        // Cargo.toml hits the filename table, not the generic .toml extension.
        assert_eq!(by_name("Cargo.toml"), "");
    }

    #[test]
    fn unknown_falls_back() {
        assert_eq!(by_name("mystery.qqq"), FILE_DEFAULT);
    }
}
