//! Filesystem watching. Watches the root recursively and coalesces bursts of
//! events into a single debounced notification carrying the set of changed
//! paths, mirroring nvim-tree's `filesystem_watchers` (default ~50ms debounce).
//!
//! macOS note: `notify`'s default backend is FSEvents, where a recursive watch
//! is a *single* event-stream registration over the directory hierarchy — cheap
//! and essentially independent of how many files live underneath (so watching a
//! root containing a 100k-file `node_modules` is fine). notify offers no built-in
//! path exclusion, so we filter high-churn directories out of the events
//! ourselves below. (On Linux, `RecursiveMode::Recursive` would instead add one
//! inotify watch per subdirectory and could exhaust `max_user_watches`; treelix
//! targets macOS, but that's the caveat if it's ever ported.)

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{unbounded, RecvTimeoutError, Sender};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

const DEBOUNCE: Duration = Duration::from_millis(75);

/// Directories whose internal churn we don't want to react to.
const IGNORE_COMPONENTS: &[&str] = &[
    "node_modules",
    "target",
    "build",
    ".ccls-cache",
    ".zig-cache",
];

/// Begin watching `root`. Coalesced change notifications are sent on `sender` as
/// the set of paths touched during the burst. The returned watcher must be kept
/// alive for watching to continue.
pub fn watch(root: PathBuf, sender: Sender<HashSet<PathBuf>>) -> Option<RecommendedWatcher> {
    let (raw_tx, raw_rx) = unbounded::<Vec<PathBuf>>();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            // Drop events confined entirely to high-churn ignored directories.
            if !event.paths.is_empty() && event.paths.iter().all(|p| is_ignored(p)) {
                return;
            }
            let _ = raw_tx.send(event.paths);
        }
    })
    .ok()?;

    if watcher.watch(&root, RecursiveMode::Recursive).is_err() {
        return None;
    }

    thread::spawn(move || {
        // Block for the first event of each burst.
        while let Ok(first) = raw_rx.recv() {
            let mut changed: HashSet<PathBuf> = first.into_iter().collect();
            // Drain until things go quiet, accumulating every affected path so
            // the consumer can reload only the directories that actually changed.
            loop {
                match raw_rx.recv_timeout(DEBOUNCE) {
                    Ok(paths) => changed.extend(paths),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            if sender.send(changed).is_err() {
                break;
            }
        }
    });

    Some(watcher)
}

fn is_ignored(path: &Path) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str();
        IGNORE_COMPONENTS
            .iter()
            .any(|ig| s == std::ffi::OsStr::new(ig))
    })
}
