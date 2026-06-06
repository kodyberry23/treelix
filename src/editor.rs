//! Routing a selected file into the running Helix instance.
//!
//! Primary path: reuse the dotfiles' `dispatch-to-editor.sh`, which sends
//! `:open`/`:vsplit <path>` to Helix over its per-session Unix socket
//! (helix-editor/helix PR #13896) and focuses the editor pane, with a fallback
//! that spawns a fresh `hx` pane. If that script isn't present, treelix does the
//! same dispatch itself.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;

#[derive(Debug, Clone, Copy)]
pub enum OpenMode {
    Open,
    VSplit,
    HSplit,
}

impl OpenMode {
    fn helix_cmd(self) -> &'static str {
        match self {
            OpenMode::Open => ":open",
            OpenMode::VSplit => ":vsplit",
            OpenMode::HSplit => ":hsplit",
        }
    }
    fn dispatch_arg(self) -> &'static str {
        match self {
            OpenMode::Open => "open",
            OpenMode::VSplit => "vsplit",
            // The dispatch script only knows open/vsplit; hsplit goes internal.
            OpenMode::HSplit => "open",
        }
    }
}

/// Open `path` in Helix using the configured strategy.
pub fn open(path: &Path, mode: OpenMode, config: &Config) {
    let abs = absolutize(path);

    // 1. Explicit user template: `open_command` with {mode}/{path}.
    if let Some(tmpl) = &config.open_command {
        let cmd = tmpl
            .replace("{mode}", mode.dispatch_arg())
            .replace("{path}", &abs.to_string_lossy());
        let _ = Command::new("sh").arg("-c").arg(cmd).status();
        return;
    }

    // 2. dotfiles dispatcher (open/vsplit only).
    if !matches!(mode, OpenMode::HSplit) {
        if let Some(script) = dispatch_script() {
            let status = Command::new(&script)
                .arg(mode.dispatch_arg())
                .arg(&abs)
                .status();
            if matches!(status, Ok(s) if s.success()) {
                return;
            }
        }
    }

    // 3. Internal dispatch.
    internal_dispatch(&abs, mode);
}

/// Open `path` with the system handler (`open` on macOS).
pub fn system_open(path: &Path) {
    let _ = Command::new("open").arg(path).status();
}

/// Preview: send `:open <path>` to Helix over its socket WITHOUT shifting focus,
/// so the cursor stays in treelix. No-op if no socket is available.
pub fn preview(path: &Path) {
    let abs = absolutize(path);
    if let Some(sock) = helix_socket_path().filter(|s| is_socket(s)) {
        let _ = send_to_socket(&sock, &format!(":open {}", abs.display()));
    }
}

fn internal_dispatch(abs: &Path, mode: OpenMode) {
    let sock = helix_socket_path();
    if let Some(sock) = sock.filter(|s| is_socket(s)) {
        let line = format!("{} {}", mode.helix_cmd(), abs.display());
        if send_to_socket(&sock, &line).is_ok() {
            focus_editor_pane();
            return;
        }
    }
    // Fallback: spawn a fresh helix pane (in zellij) or a bare `hx`.
    if std::env::var_os("ZELLIJ").is_some() {
        let _ = Command::new("zellij")
            .args([
                "action",
                "new-pane",
                "--direction",
                "right",
                "--name",
                "editor",
                "--",
            ])
            .arg("hx")
            .arg(abs)
            .status();
    } else {
        let _ = Command::new("hx").arg(abs).status();
    }
}

fn send_to_socket(sock: &Path, line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(sock)?;
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

/// Shift zellij focus to the pane named `editor`.
fn focus_editor_pane() {
    if std::env::var_os("ZELLIJ").is_none() {
        return;
    }
    if let Some(id) = resolve_pane_id("editor") {
        let _ = Command::new("zellij")
            .args(["action", "focus-pane-id"])
            .arg(id)
            .status();
    }
}

/// Parse `zellij action list-panes` for the terminal pane titled `name`.
fn resolve_pane_id(name: &str) -> Option<String> {
    let out = Command::new("zellij")
        .args(["action", "list-panes"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let id = f.next();
        let kind = f.next();
        let title = f.next();
        if kind == Some("terminal") && title == Some(name) {
            return id.map(|s| s.to_string());
        }
    }
    None
}

/// Per-session Helix socket path, matching the dotfiles' `launch-editor.sh`.
pub fn helix_socket_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HELIX_SOCKET_PATH") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let base = runtime_dir()?.join("helix");
    let session = session_name();
    Some(base.join(format!("{session}.sock")))
}

fn dispatch_script() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TREELIX_DISPATCH_TO_EDITOR") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let home = std::env::var_os("HOME")?;
    let candidate = PathBuf::from(home).join("projects/helix-files/scripts/dispatch-to-editor.sh");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Sanitized zellij session name (alphanumerics + `-`/`_`), or `default`.
pub fn session_name() -> String {
    let raw = std::env::var("ZELLIJ_SESSION_NAME").unwrap_or_else(|_| "default".to_string());
    sanitize(&raw)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn runtime_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !x.is_empty() {
            return Some(PathBuf::from(x));
        }
    }
    Some(PathBuf::from("/tmp"))
}

fn is_socket(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::symlink_metadata(path)
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_session() {
        assert_eq!(sanitize("my project!"), "my_project_");
        assert_eq!(sanitize("ok-name_1"), "ok-name_1");
    }
}
