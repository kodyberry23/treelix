//! Reveal IPC: a Unix socket the running TUI listens on, so Helix can tell
//! treelix to expand to a path. Replaces broot's `--listen`/`--send`.
//!
//! Wire protocol: newline-delimited commands.
//!   `reveal <abspath>`        — explicit user reveal (A-r / space-f /
//!                               `treelix reveal`); always applied in full.
//!   `reveal-follow <abspath>` — automatic push from the patched Helix on a
//!                               focused-buffer change; the app may defer it
//!                               while the user is driving the tree.
//!   `diagnostics <errors> <warnings> <abspath>`
//!                             — one file's current LSP diagnostic counts; both
//!                               zero clears the file. Counts come first so the
//!                               path can contain spaces.
//!   `diagnostics-begin [<sender>] <seq>` … `diagnostics-end <seq>`
//!                             — a complete snapshot: the `diagnostics` lines in
//!                               between are every file that has any, applied
//!                               atomically at `end`; files not listed are
//!                               clear. `sender` identifies the editor process
//!                               (`hx-<pid>-<start>`; omitted by older editors,
//!                               which then share the legacy sender ""), and
//!                               `seq` grows with each snapshot it produces.
//!                               Snapshots are kept per sender: one not newer
//!                               than that sender's last is ignored
//!                               (connections are served on separate threads),
//!                               while a sender's first is always applied, so a
//!                               restarted editor shows at once. The tree shows
//!                               the union of every sender's files. This is
//!                               what the patched Helix sends.
//!   `diagnostics-bye <sender> <seq>`
//!                             — the editor is exiting: retract its snapshot.
//!                               An empty snapshot for that sender only, under
//!                               the same ordering rules; two editors can share
//!                               one sidebar, and one leaving must not wipe the
//!                               other's markers. Helix appends it to its final
//!                               snapshot on one connection, so the two are
//!                               always read in order. A sender whose process
//!                               has exited without a farewell is pruned (see
//!                               `diagnostics::Sources`).
//! The path is taken verbatim up to the newline (no trimming — trailing
//! whitespace is legal in file names).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;

use crossbeam_channel::Sender;

use crate::diagnostics::Counts;
use crate::editor;

/// One parsed reveal command.
#[derive(Debug, PartialEq, Eq)]
pub struct Reveal {
    pub path: PathBuf,
    /// True for `reveal-follow` (automatic push), false for an explicit
    /// user-requested reveal.
    pub follow: bool,
}

/// A file's diagnostic counts as reported by the editor.
#[derive(Debug, PartialEq, Eq)]
pub struct DiagnosticsUpdate {
    pub path: PathBuf,
    pub counts: Counts,
}

/// One message for the app: a wire line, or a whole snapshot batch.
pub enum Message {
    Reveal(Reveal),
    Diagnostics(DiagnosticsUpdate),
    DiagnosticsSnapshot {
        /// Who sent it; empty when the sender did not say.
        sender: String,
        seq: u64,
        files: Vec<(PathBuf, Counts)>,
    },
    /// The sender is exiting and retracts its snapshot (see the module doc).
    DiagnosticsBye {
        sender: String,
        seq: u64,
    },
}

/// One parsed wire line.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Reveal(Reveal),
    Diagnostics(DiagnosticsUpdate),
    Begin { sender: String, seq: u64 },
    End(u64),
    Bye { sender: String, seq: u64 },
}

/// A snapshot batch being read on one connection.
struct Batch {
    sender: String,
    seq: u64,
    files: Vec<(PathBuf, Counts)>,
}

/// Fold one line into the connection's batch state; returns the message to
/// deliver, if the line completes one. A `diagnostics` line outside a batch
/// is delivered on its own; inside, it joins the batch. A batch is delivered
/// only when its `end` carries the same sequence as its `begin`.
fn fold_line(batch: &mut Option<Batch>, line: Line) -> Option<Message> {
    match line {
        Line::Reveal(reveal) => Some(Message::Reveal(reveal)),
        // Helix only sends a farewell after a `diagnostics-end`, so no batch
        // is open; an open one is left alone either way, like a reveal.
        Line::Bye { sender, seq } => Some(Message::DiagnosticsBye { sender, seq }),
        Line::Begin { sender, seq } => {
            *batch = Some(Batch {
                sender,
                seq,
                files: Vec::new(),
            });
            None
        }
        Line::Diagnostics(update) => match batch {
            Some(batch) => {
                batch.files.push((update.path, update.counts));
                None
            }
            None => Some(Message::Diagnostics(update)),
        },
        Line::End(seq) => match batch.take() {
            Some(batch) if batch.seq == seq => Some(Message::DiagnosticsSnapshot {
                sender: batch.sender,
                seq,
                files: batch.files,
            }),
            _ => None,
        },
    }
}

/// The reveal socket path: `TREELIX_SOCKET_PATH` when set, otherwise a
/// per-session path. The helix-files launchers always export the per-tab path,
/// so the session-level derivation here is only the fallback when no
/// environment is set (treelix run outside the launchers).
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
pub fn serve(sender: Sender<Message>) -> Option<SocketGuard> {
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

fn handle(stream: UnixStream, sender: &Sender<Message>) {
    // Bound the read so a peer that connects and then stalls (never sending a
    // newline, never closing) frees this thread instead of leaking it.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let reader = BufReader::new(stream);
    let mut batch: Option<Batch> = None;
    for line in reader.lines().map_while(Result::ok) {
        if let Some(message) = parse_line(&line).and_then(|line| fold_line(&mut batch, line)) {
            let _ = sender.send(message);
        }
    }
}

/// Parse one wire line. The path is everything after the command word (and,
/// for `diagnostics`, the two counts), verbatim — `lines()` has already
/// removed the newline terminator, and trailing whitespace is a legal part of
/// a file name. Unknown commands are ignored, so an older treelix paired with
/// a newer Helix just misses the feature.
fn parse_line(line: &str) -> Option<Line> {
    if let Some(rest) = line.strip_prefix("diagnostics-begin ") {
        // `<seq>` alone, or `<sender> <seq>`.
        let (sender, seq) = match rest.rsplit_once(' ') {
            Some((sender, seq)) => (sender.to_string(), seq),
            None => (String::new(), rest),
        };
        return seq.parse().ok().map(|seq| Line::Begin { sender, seq });
    }
    if let Some(seq) = line.strip_prefix("diagnostics-end ") {
        return seq.parse().ok().map(Line::End);
    }
    if let Some(rest) = line.strip_prefix("diagnostics-bye ") {
        // Both fields are mandatory, unlike `diagnostics-begin`'s optional
        // sender: a farewell that does not say whose it is means nothing.
        let (sender, seq) = rest.rsplit_once(' ')?;
        if sender.is_empty() {
            return None;
        }
        return seq.parse().ok().map(|seq| Line::Bye {
            sender: sender.to_string(),
            seq,
        });
    }
    if let Some(rest) = line.strip_prefix("diagnostics ") {
        let mut parts = rest.splitn(3, ' ');
        let errors = parts.next()?.parse().ok()?;
        let warnings = parts.next()?.parse().ok()?;
        let path = parts.next()?;
        return (!path.is_empty()).then(|| {
            Line::Diagnostics(DiagnosticsUpdate {
                path: PathBuf::from(path),
                counts: Counts { errors, warnings },
            })
        });
    }
    let (rest, follow) = if let Some(rest) = line.strip_prefix("reveal-follow ") {
        (rest, true)
    } else {
        (line.strip_prefix("reveal ")?, false)
    };
    (!rest.is_empty()).then(|| {
        Line::Reveal(Reveal {
            path: PathBuf::from(rest),
            follow,
        })
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
        let Line::Reveal(r) = parse_line("reveal /a/b.txt").unwrap() else {
            panic!("expected a reveal");
        };
        assert_eq!(r.path, PathBuf::from("/a/b.txt"));
        assert!(!r.follow);

        let Line::Reveal(r) = parse_line("reveal-follow /a/b.txt").unwrap() else {
            panic!("expected a reveal");
        };
        assert_eq!(r.path, PathBuf::from("/a/b.txt"));
        assert!(r.follow);
    }

    #[test]
    fn parse_preserves_path_whitespace() {
        // Trailing whitespace is a legal part of a unix file name.
        let Line::Reveal(r) = parse_line("reveal /a/trailing ").unwrap() else {
            panic!("expected a reveal");
        };
        assert_eq!(r.path, PathBuf::from("/a/trailing "));
        // Interior spaces too.
        let Line::Reveal(r) = parse_line("reveal-follow /a/has space/f.txt").unwrap() else {
            panic!("expected a reveal");
        };
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

    #[test]
    fn parses_diagnostics_with_counts_first_and_verbatim_path() {
        let Line::Diagnostics(d) = parse_line("diagnostics 2 5 /a/has space/f.rs ").unwrap() else {
            panic!("expected diagnostics");
        };
        assert_eq!(d.path, PathBuf::from("/a/has space/f.rs "));
        assert_eq!(
            d.counts,
            Counts {
                errors: 2,
                warnings: 5
            }
        );
        let Line::Diagnostics(d) = parse_line("diagnostics 0 0 /a/f.rs").unwrap() else {
            panic!("expected diagnostics");
        };
        assert!(d.counts.is_empty(), "both zero clears the file");
        assert!(
            parse_line("diagnostics 1 /a/f.rs").is_none(),
            "missing a count"
        );
        assert!(
            parse_line("diagnostics x 1 /a/f.rs").is_none(),
            "not a number"
        );
        assert!(parse_line("diagnostics 1 1 ").is_none(), "empty path");
        assert!(parse_line("diagnostics 1 1").is_none());
        assert!(
            parse_line("unknown-command /a").is_none(),
            "ignored, not an error"
        );
    }

    #[test]
    fn snapshot_batches_are_delivered_whole_and_only_when_well_formed() {
        let counts = |e, w| Counts {
            errors: e,
            warnings: w,
        };
        let mut batch: Option<Batch> = None;
        assert!(fold_line(&mut batch, parse_line("diagnostics-begin 7").unwrap()).is_none());
        assert!(fold_line(&mut batch, parse_line("diagnostics 1 0 /a.rs").unwrap()).is_none());
        assert!(fold_line(&mut batch, parse_line("diagnostics 0 2 /b c.rs").unwrap()).is_none());
        let Some(Message::DiagnosticsSnapshot { sender, seq, files }) =
            fold_line(&mut batch, parse_line("diagnostics-end 7").unwrap())
        else {
            panic!("expected a snapshot");
        };
        assert_eq!(seq, 7);
        assert_eq!(sender, "", "no sender given");
        assert_eq!(
            files,
            vec![
                (PathBuf::from("/a.rs"), counts(1, 0)),
                (PathBuf::from("/b c.rs"), counts(0, 2))
            ]
        );
        assert!(batch.is_none(), "the batch is consumed");

        // An empty snapshot is still a snapshot (everything clear).
        fold_line(&mut batch, parse_line("diagnostics-begin 8").unwrap());
        assert!(matches!(
            fold_line(&mut batch, parse_line("diagnostics-end 8").unwrap()),
            Some(Message::DiagnosticsSnapshot { seq: 8, files, .. }) if files.is_empty()
        ));

        // With a sender: `diagnostics-begin <sender> <seq>`.
        assert_eq!(
            parse_line("diagnostics-begin hx-4242-17 12"),
            Some(Line::Begin {
                sender: "hx-4242-17".into(),
                seq: 12
            })
        );

        // A mismatched end drops the batch instead of applying a partial one.
        fold_line(&mut batch, parse_line("diagnostics-begin 9").unwrap());
        fold_line(&mut batch, parse_line("diagnostics 1 0 /a.rs").unwrap());
        assert!(fold_line(&mut batch, parse_line("diagnostics-end 3").unwrap()).is_none());
        assert!(batch.is_none());
        assert!(parse_line("diagnostics-begin x").is_none());
        assert!(parse_line("diagnostics-end").is_none());

        // Outside a batch, a diagnostics line stands on its own.
        assert!(matches!(
            fold_line(&mut batch, parse_line("diagnostics 1 0 /a.rs").unwrap()),
            Some(Message::Diagnostics(_))
        ));
    }

    #[test]
    fn a_farewell_names_its_sender_and_sequence() {
        assert_eq!(
            parse_line("diagnostics-bye hx-4242-17 12"),
            Some(Line::Bye {
                sender: "hx-4242-17".into(),
                seq: 12
            })
        );
        // Both fields are required: an anonymous farewell retracts nothing.
        assert!(parse_line("diagnostics-bye 12").is_none());
        assert!(parse_line("diagnostics-bye hx-1").is_none());
        assert!(parse_line("diagnostics-bye  12").is_none(), "empty sender");
        assert!(parse_line("diagnostics-bye hx-1 x").is_none());
        assert!(parse_line("diagnostics-bye").is_none());
        assert!(parse_line("diagnostics-bye ").is_none());
    }

    #[test]
    fn a_farewell_follows_its_snapshot_on_the_same_connection() {
        // Exactly the bytes helix writes on exit: the final snapshot, then
        // the farewell, one connection.
        let mut batch: Option<Batch> = None;
        assert!(fold_line(
            &mut batch,
            parse_line("diagnostics-begin hx-1-2 4").unwrap()
        )
        .is_none());
        assert!(fold_line(&mut batch, parse_line("diagnostics 1 0 /a.rs").unwrap()).is_none());
        assert!(matches!(
            fold_line(&mut batch, parse_line("diagnostics-end 4").unwrap()),
            Some(Message::DiagnosticsSnapshot { seq: 4, .. })
        ));
        assert!(matches!(
            fold_line(&mut batch, parse_line("diagnostics-bye hx-1-2 5").unwrap()),
            Some(Message::DiagnosticsBye { ref sender, seq: 5 }) if sender == "hx-1-2"
        ));
        assert!(batch.is_none());

        // Inside a batch it is passed through and the batch survives.
        fold_line(
            &mut batch,
            parse_line("diagnostics-begin hx-1-2 6").unwrap(),
        );
        assert!(matches!(
            fold_line(&mut batch, parse_line("diagnostics-bye hx-1-2 7").unwrap()),
            Some(Message::DiagnosticsBye { .. })
        ));
        assert!(batch.is_some());
    }
}
