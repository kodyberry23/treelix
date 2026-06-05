//! Reveal IPC: a Unix socket the running TUI listens on, so Helix's `A-r`
//! ("reveal current buffer") can tell treelix to expand to a path. Replaces
//! broot's `--listen`/`--send`.
//!
//! Wire protocol: newline-delimited commands. Currently `reveal <abspath>`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;

use crossbeam_channel::Sender;

use crate::editor;

/// Resolve the per-session reveal socket path, matching the dotfiles'
/// `launch-sidebar.sh`/`dispatch-to-sidebar.sh` derivation.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TREELIX_SOCKET_PATH") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let base = editor::runtime_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("treelix")
        .join(format!("{}.sock", editor::session_name()))
}

/// Removes the socket file when dropped.
pub struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind the reveal socket and serve in a background thread. Each `reveal <path>`
/// line is forwarded as a `PathBuf` on `sender`. Returns a guard that cleans up
/// the socket file on drop (and `None` if binding failed).
pub fn serve(sender: Sender<PathBuf>) -> Option<SocketGuard> {
    let path = socket_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Clear a stale socket from a crashed prior instance.
    let _ = std::fs::remove_file(&path);

    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("treelix: could not bind reveal socket {}: {e}", path.display());
            return None;
        }
    };

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            handle(stream, &sender);
        }
    });

    Some(SocketGuard { path })
}

fn handle(stream: UnixStream, sender: &Sender<PathBuf>) {
    let reader = BufReader::new(stream);
    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("reveal ") {
            let p = PathBuf::from(rest.trim());
            let _ = sender.send(p);
        }
    }
}

/// Client side: connect to a running treelix and ask it to reveal `path`.
/// Exits non-zero (after printing to stderr) when no instance is listening,
/// mirroring broot's `--send` behavior.
pub fn send_reveal(path: &str) -> std::io::Result<()> {
    let sock = socket_path();
    let mut stream = UnixStream::connect(&sock).map_err(|e| {
        eprintln!(
            "treelix reveal: no treelix socket at {} ({e})",
            sock.display()
        );
        e
    })?;
    // Absolutize so the receiving instance interprets it the same way.
    let abs = if std::path::Path::new(path).is_absolute() {
        path.to_string()
    } else {
        std::env::current_dir()
            .map(|c| c.join(path).to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string())
    };
    writeln!(stream, "reveal {abs}")?;
    stream.flush()
}
