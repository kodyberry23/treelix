//! Routing a selected file into the running Helix instance.
//!
//! Primary path: reuse the dotfiles' `dispatch-to-editor.sh` (mode arg
//! open|vsplit|hsplit), which sends the window-picker commands
//! `:open-pick`/`:vsplit-pick`/`:hsplit-pick <path>` to Helix over its
//! per-session Unix socket (helix-editor/helix PR #13896) and focuses the editor
//! pane, with a fallback that spawns a fresh `hx` pane. If that script isn't
//! present (or rejects the mode), treelix does the same dispatch itself
//! (`*-pick` under zellij, plain `:open`/`:vsplit`/`:hsplit` otherwise).

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
        // The `-pick` variants (helix local-patch) prompt with per-split labels
        // when multiple splits exist, and fall back to the plain action when
        // there is only one. Used by the internal socket path under zellij,
        // where we can focus the editor pane so the labels are reachable; the
        // dotfiles dispatch script sends the same commands for open/vsplit.
        match self {
            OpenMode::Open => ":open-pick",
            OpenMode::VSplit => ":vsplit-pick",
            OpenMode::HSplit => ":hsplit-pick",
        }
    }
    fn helix_cmd_plain(self) -> &'static str {
        // Non-picker variants, used when there is no zellij to move focus to
        // the editor pane — a picker would be armed but unreachable.
        match self {
            OpenMode::Open => ":open",
            OpenMode::VSplit => ":vsplit",
            OpenMode::HSplit => ":hsplit",
        }
    }
    /// Faithful mode name: the `{mode}` value for user `open_command` templates
    /// and the mode argument passed to the dotfiles dispatch script (which
    /// accepts open|vsplit|hsplit).
    fn name(self) -> &'static str {
        match self {
            OpenMode::Open => "open",
            OpenMode::VSplit => "vsplit",
            OpenMode::HSplit => "hsplit",
        }
    }
}

/// Open `path` in Helix using the configured strategy.
///
/// Runs on a DETACHED thread: the dispatch shells out to zellij
/// (`list-panes`/`focus-pane-id`) and the dotfiles script, any of which can
/// block for an unbounded time if the zellij server is wedged. Doing that on
/// the UI event loop would freeze the whole TUI with no way to quit, so we fire
/// and forget — a failed open is a no-op, never a hang.
pub fn open(path: &Path, mode: OpenMode, config: &Config) {
    let abs = absolutize(path);
    let open_command = config.open_command.clone();
    std::thread::spawn(move || {
        // 1. Explicit user template: `open_command` with {mode}/{path}.
        if let Some(tmpl) = &open_command {
            // POSIX-single-quote the path before interpolating into `sh -c`.
            // Without this, a filename like `a$(rm -rf ~).md` executes as a
            // command substitution, and spaces/globs split into extra args.
            // `{mode}` is a fixed keyword (open|vsplit|hsplit), so it needs no
            // quoting; users must NOT hand-quote {path} in their template.
            let cmd = tmpl
                .replace("{mode}", mode.name())
                .replace("{path}", &posix_quote(&abs));
            let _ = Command::new("sh").arg("-c").arg(cmd).status();
            return;
        }

        // 2. dotfiles dispatcher. An older script that predates hsplit support
        // rejects it with a non-zero exit, in which case we fall through to the
        // internal socket dispatch below — graceful under version skew.
        if let Some(script) = dispatch_script() {
            let status = Command::new(&script).arg(mode.name()).arg(&abs).status();
            if matches!(status, Ok(s) if s.success()) {
                return;
            }
        }

        // 3. Internal dispatch.
        internal_dispatch(&abs, mode);
    });
}

/// Open `path` with the system handler (`open` on macOS). Detached: `open` can
/// block (e.g. launching a cold application) and must not stall the UI thread.
pub fn system_open(path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = Command::new("open").arg(&path).status();
    });
}

/// POSIX-single-quote an arbitrary path for safe interpolation into a `sh -c`
/// string: wrap in single quotes and replace each embedded `'` with `'\''`.
/// This keeps EVERY byte literal — no command substitution, globbing, or word
/// splitting — for any filename.
fn posix_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}

/// Preview: send `:open <path>` to Helix over its socket WITHOUT shifting focus,
/// so the cursor stays in treelix. No-op if no socket is available. Detached:
/// a wedged helix that accepts the connection but never reads could otherwise
/// block the write (and thus the UI thread) until the write timeout.
pub fn preview(path: &Path) {
    let abs = absolutize(path);
    std::thread::spawn(move || {
        if let Some(sock) = helix_socket_path().filter(|s| is_socket(s)) {
            // Quote so paths with spaces parse as one argument on the helix side.
            let _ = send_to_socket(&sock, &format!(":open {}", helix_quote(&abs)));
        }
    });
}

fn internal_dispatch(abs: &Path, mode: OpenMode) {
    let sock = helix_socket_path();
    if let Some(sock) = sock.filter(|s| is_socket(s)) {
        // Use the picker variants only under zellij (where focus_editor_pane can
        // bring the labels into view); otherwise send the plain command. Quote
        // the path so filenames with spaces parse as a single argument.
        let cmd = if std::env::var_os("ZELLIJ").is_some() {
            mode.helix_cmd()
        } else {
            mode.helix_cmd_plain()
        };
        let line = format!("{} {}", cmd, helix_quote(abs));
        if send_to_socket(&sock, &line).is_ok() {
            focus_editor_pane();
            return;
        }
    }
    // Fallback: under zellij, open the file in a fresh editor pane. Without
    // zellij there is no separate pane to route to — spawning `hx` here would
    // inherit treelix's own stdin/stdout and draw a second TUI over the file
    // tree on the same terminal, leaving a scrambled screen when it exits. A
    // sidebar has no business seizing the terminal, so that path is a no-op.
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
    }
}

/// Single-quote a path for helix's command line, escaping embedded single
/// quotes by doubling them. Single quotes keep the path LITERAL — unlike double
/// quotes, which route the token through helix's `%`/`%sh{}` expansion (breaking
/// filenames with `%` and risking `%sh{...}` execution from a crafted filename).
fn helix_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "''"))
}

fn send_to_socket(sock: &Path, line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(sock)?;
    // Bound the write: a helix that accepted the connection but stopped reading
    // must not park this thread forever (these run detached, but a leaked
    // blocked thread per open still accumulates).
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
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

    #[test]
    fn helix_quote_escapes_for_command_line() {
        // Plain path: wrapped in single quotes, unchanged inside.
        assert_eq!(helix_quote(Path::new("/a/b.txt")), "'/a/b.txt'");
        // Spaces stay inside the single quotes (one argument on the helix side).
        assert_eq!(helix_quote(Path::new("/a b/c d.txt")), "'/a b/c d.txt'");
        // `%` must stay literal — single quotes prevent helix's %/%sh{} expansion.
        assert_eq!(helix_quote(Path::new("/x/50%.txt")), "'/x/50%.txt'");
        assert_eq!(
            helix_quote(Path::new("/x/%sh{touch X}.txt")),
            "'/x/%sh{touch X}.txt'"
        );
        // Embedded single quote is doubled (helix single-quote escaping).
        assert_eq!(helix_quote(Path::new("/x/it's.txt")), "'/x/it''s.txt'");
        assert_eq!(helix_quote(Path::new("/x/a''b.txt")), "'/x/a''''b.txt'");
        // A double quote inside a single-quoted token is literal, no escaping.
        assert_eq!(helix_quote(Path::new("/x/a\"b.txt")), "'/x/a\"b.txt'");
    }

    #[test]
    fn posix_quote_neutralizes_shell_metacharacters() {
        // Plain path unchanged inside single quotes.
        assert_eq!(posix_quote(Path::new("/a/b.txt")), "'/a/b.txt'");
        // Command substitution and separators are inert inside single quotes.
        assert_eq!(
            posix_quote(Path::new("/x/a$(rm -rf ~).md")),
            "'/x/a$(rm -rf ~).md'"
        );
        assert_eq!(posix_quote(Path::new("/x/a;b.md")), "'/x/a;b.md'");
        assert_eq!(posix_quote(Path::new("/x/a b*.md")), "'/x/a b*.md'");
        // An embedded single quote closes, escapes a literal quote, reopens —
        // the classic '\'' sequence — so the break-out attempt stays literal.
        assert_eq!(posix_quote(Path::new("/x/a'b.md")), "'/x/a'\\''b.md'");
        assert_eq!(
            posix_quote(Path::new("/x/'; rm -rf ~ #.md")),
            "'/x/'\\''; rm -rf ~ #.md'"
        );
    }
}
