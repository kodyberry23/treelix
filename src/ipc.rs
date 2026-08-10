//! Reveal IPC: a Unix socket the running TUI listens on, so Helix can tell
//! treelix to expand to a path. Replaces broot's `--listen`/`--send`.
//!
//! Wire protocol: newline-delimited commands.
//!   `reveal <abspath>`        — explicit user reveal (A-r / space-f /
//!                               `treelix reveal`); always applied in full.
//!   `reveal-follow <abspath>` — automatic push from the patched Helix on a
//!                               focused-buffer change; the app may defer it
//!                               while the user is driving the tree.
//! The path is taken verbatim up to the newline (no trimming — trailing
//! whitespace is legal in file names).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;

use crossbeam_channel::Sender;

use crate::editor;

/// One parsed reveal command.
pub struct Reveal {
    pub path: PathBuf,
    /// True for `reveal-follow` (automatic push), false for an explicit
    /// user-requested reveal.
    pub follow: bool,
}

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

/// Removes the socket file when dropped — but only if it is still the SAME
/// socket this instance bound. Another instance that later rebound the path
/// (after a restart race) must not have its live socket unlinked out from under
/// it, which would leave the real sidebar dark with no error anywhere.
pub struct SocketGuard {
    path: PathBuf,
    /// (dev, ino) of the socket file this guard created; the drop compares
    /// against the path's current identity before unlinking.
    identity: Option<(u64, u64)>,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if socket_identity(&self.path) == self.identity {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn socket_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn socket_identity(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Bind the reveal socket and serve in a background thread. Each parsed
/// reveal line is forwarded on `sender`. Returns a guard that cleans up the
/// socket file on drop (and `None` if binding failed or another live
/// instance already owns the socket).
pub fn serve(sender: Sender<Reveal>) -> Option<SocketGuard> {
    let path = socket_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A live instance may already own this socket (connect succeeds while a
    // listener is bound, even a busy one). Stealing it would silently send
    // every future reveal here AND unlink the file on our exit, leaving the
    // real sidebar dark — refuse instead. A stale socket from a crashed
    // instance has no listener, fails the connect, and is cleaned up below.
    if UnixStream::connect(&path).is_ok() {
        eprintln!(
            "treelix: another instance is serving {}; reveal socket disabled here",
            path.display()
        );
        return None;
    }
    let _ = std::fs::remove_file(&path);

    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "treelix: could not bind reveal socket {}: {e}",
                path.display()
            );
            return None;
        }
    };

    let identity = socket_identity(&path);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            // Handle each connection on its own thread. A single client that
            // connects and holds the socket open (a stray `nc -U`, or a helix
            // sender whose process is paused mid-write) must not park the
            // accept loop and freeze every subsequent reveal.
            let sender = sender.clone();
            thread::spawn(move || handle(stream, &sender));
        }
    });

    Some(SocketGuard { path, identity })
}

fn handle(stream: UnixStream, sender: &Sender<Reveal>) {
    // Bound the read so a peer that connects and then stalls (never sending a
    // newline, never closing) frees this thread instead of leaking it.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let reader = BufReader::new(stream);
    for line in reader.lines().map_while(Result::ok) {
        if let Some(reveal) = parse_line(&line) {
            let _ = sender.send(reveal);
        }
    }
}

/// Parse one wire line. The path is everything after the command word,
/// verbatim — `lines()` has already removed the newline terminator, and
/// trailing whitespace is a legal part of a file name.
fn parse_line(line: &str) -> Option<Reveal> {
    let (rest, follow) = if let Some(rest) = line.strip_prefix("reveal-follow ") {
        (rest, true)
    } else {
        (line.strip_prefix("reveal ")?, false)
    };
    (!rest.is_empty()).then(|| Reveal {
        path: PathBuf::from(rest),
        follow,
    })
}

/// Client side: connect to a running treelix and ask it to reveal `path`.
/// Exits non-zero (after printing to stderr) when no instance is listening,
/// mirroring broot's `--send` behavior.
pub fn send_reveal(path: &str) -> std::io::Result<()> {
    // The protocol is newline-delimited; a newline in the path would smuggle a
    // second command onto the wire. Reject rather than corrupt the stream
    // (mirrors helix's sender guard in sidebar_follow.rs).
    if path.contains('\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "reveal path contains a newline",
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_explicit_and_follow() {
        let r = parse_line("reveal /a/b.txt").unwrap();
        assert_eq!(r.path, PathBuf::from("/a/b.txt"));
        assert!(!r.follow);

        let r = parse_line("reveal-follow /a/b.txt").unwrap();
        assert_eq!(r.path, PathBuf::from("/a/b.txt"));
        assert!(r.follow);
    }

    #[test]
    fn parse_preserves_path_whitespace() {
        // Trailing whitespace is a legal part of a unix file name.
        let r = parse_line("reveal /a/trailing ").unwrap();
        assert_eq!(r.path, PathBuf::from("/a/trailing "));
        // Interior spaces too.
        let r = parse_line("reveal-follow /a/has space/f.txt").unwrap();
        assert_eq!(r.path, PathBuf::from("/a/has space/f.txt"));
    }

    #[test]
    fn parse_rejects_junk() {
        assert!(parse_line("").is_none());
        assert!(parse_line("reveal").is_none());
        assert!(parse_line("reveal ").is_none());
        assert!(parse_line("reveal-follow ").is_none());
        assert!(parse_line("revealx /a").is_none());
        assert!(parse_line("open /a").is_none());
    }
}
