//! Nerd Font file/folder icons, a curated subset of nvim-web-devicons resolved
//! by filename then extension.
//!
//! Glyphs are written as explicit `\u{...}` escapes (Nerd Font Private-Use-Area
//! codepoints) rather than raw bytes, so they survive copy/paste and editor
//! round-trips — pasting the literal glyphs is how this table previously ended
//! up as empty strings (and silently rendered no icons at all).

use crate::tree::NodeKind;

pub const FOLDER_CLOSED: &str = "\u{f07b}"; //
pub const FOLDER_OPEN: &str = "\u{f07c}"; //
pub const FILE_DEFAULT: &str = "\u{f15b}"; //
pub const SYMLINK: &str = "\u{f481}"; //

pub const ARROW_CLOSED: &str = "\u{f105}"; //  angle-right
pub const ARROW_OPEN: &str = "\u{f107}"; //  angle-down

pub const BOOKMARK: &str = "\u{f02e}"; //

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
        "cargo.toml" | "cargo.lock" => "\u{e7a8}",            //  rust
        "package.json" | "package-lock.json" => "\u{e718}",   //  node
        "yarn.lock" => "\u{e718}",                            //  node
        "dockerfile" | ".dockerignore" => "\u{f308}",         //  docker
        "docker-compose.yml" | "docker-compose.yaml" => "\u{f308}",
        "makefile" => "\u{e673}",                             //  make
        ".gitignore" | ".gitattributes" | ".gitmodules" => "\u{f1d3}", //  git
        ".gitconfig" => "\u{f1d3}",                           //  git
        "readme.md" | "readme" | "readme.txt" => "\u{f02d}",  //  book
        "license" | "license.md" | "license.txt" => "\u{f02d}", //  book
        ".env" | ".env.local" => "\u{f013}",                  //  gear
        ".zshrc" | ".bashrc" | ".bash_profile" | ".profile" => "\u{f489}", //  terminal
        "config.toml" | "config.kdl" => "\u{e615}",           //  config
        ".editorconfig" => "\u{e615}",                        //  config
        "flake.nix" | "default.nix" | "shell.nix" => "\u{f313}", //  nix
        _ => return None,
    })
}

fn by_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "\u{e7a8}",                                       //  rust
        "go" => "\u{e627}",                                       //  go
        "py" | "pyc" | "pyw" => "\u{e606}",                       //  python
        "js" | "mjs" | "cjs" => "\u{e74e}",                       //  javascript
        "ts" => "\u{e628}",                                       //  typescript
        "jsx" => "\u{e7ba}",                                      //  react
        "tsx" => "\u{e7ba}",                                      //  react
        "html" | "htm" => "\u{e736}",                            //  html5
        "css" => "\u{e749}",                                      //  css3
        "scss" | "sass" => "\u{e603}",                           //  sass
        "json" | "jsonc" => "\u{e60b}",                          //  json
        "toml" => "\u{e615}",                                     //  config
        "yaml" | "yml" => "\u{e615}",                            //  config
        "md" | "markdown" => "\u{e73e}",                         //  markdown
        "lua" => "\u{e620}",                                      //  lua
        "vim" => "\u{e62b}",                                      //  vim
        "sh" | "bash" | "zsh" | "fish" => "\u{f489}",            //  terminal
        "c" | "h" => "\u{e61e}",                                  //  c
        "cpp" | "cc" | "cxx" | "hpp" => "\u{e61d}",              //  c++
        "java" => "\u{e738}",                                     //  java
        "kt" | "kts" => "\u{e634}",                              //  kotlin
        "rb" => "\u{e739}",                                       //  ruby
        "php" => "\u{e73d}",                                      //  php
        "ex" | "exs" => "\u{e62d}",                              //  elixir
        "erl" | "hrl" => "\u{e7b1}",                            //  erlang
        "hs" => "\u{e777}",                                       //  haskell
        "nix" => "\u{f313}",                                      //  nix
        "kdl" => "\u{e615}",                                      //  config
        "sql" => "\u{f1c0}",                                      //  database
        "txt" | "log" => "\u{f15c}",                             //  text
        "pdf" => "\u{f1c1}",                                      //  pdf
        "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "7z" | "rar" => "\u{f1c6}", //  archive
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" => "\u{f1c5}", //  image
        "svg" => "\u{f1c5}",                                      //  image
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => "\u{f1c7}",    //  audio
        "mp4" | "mkv" | "mov" | "avi" | "webm" => "\u{f1c8}",    //  video
        "ttf" | "otf" | "woff" | "woff2" => "\u{f031}",         //  font
        "git" => "\u{f1d3}",                                      //  git
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
        assert_eq!(by_name("main.rs"), "\u{e7a8}");
        assert_eq!(by_name("notes.md"), "\u{e73e}");
        assert_eq!(by_name("init.lua"), "\u{e620}");
        assert_eq!(by_name("data.json"), "\u{e60b}");
    }

    #[test]
    fn resolves_by_filename_over_extension() {
        // Cargo.toml hits the filename table, not the generic .toml extension.
        assert_eq!(by_name("Cargo.toml"), "\u{e7a8}");
        assert_ne!(by_name("Cargo.toml"), by_name("config.toml"));
    }

    #[test]
    fn unknown_falls_back() {
        assert_eq!(by_name("mystery.qqq"), FILE_DEFAULT);
    }

    #[test]
    fn icons_are_never_empty() {
        // Guard against the table regressing to empty strings (which renders no
        // icons at all). Every structural glyph and a representative spread of
        // file types must resolve to a non-empty glyph.
        for g in [FOLDER_CLOSED, FOLDER_OPEN, FILE_DEFAULT, SYMLINK, ARROW_CLOSED, ARROW_OPEN, BOOKMARK] {
            assert!(!g.is_empty());
        }
        for f in [
            "main.rs", "app.ts", "index.js", "page.tsx", "style.css", "data.json",
            "notes.md", "init.lua", "build.gradle.kts", "server.go", "script.py",
            "Cargo.toml", "package.json", "Dockerfile", "Makefile", ".gitignore",
            "README.md", "photo.png", "archive.zip", "song.mp3",
        ] {
            assert!(!by_name(f).is_empty(), "empty icon for {f}");
        }
    }
}
