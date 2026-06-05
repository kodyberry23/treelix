//! Filesystem watching. Watches the root recursively and coalesces bursts of
//! events into a single debounced "something changed" signal, mirroring
//! nvim-tree's `filesystem_watchers` (default 50ms debounce).

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{unbounded, RecvTimeoutError, Sender};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

const DEBOUNCE: Duration = Duration::from_millis(75);

/// Directories whose internal churn we don't want to react to.
const IGNORE_COMPONENTS: &[&str] = &["node_modules", "target", ".ccls-cache", ".zig-cache"];

/// Begin watching `root`. Coalesced change notifications are sent as `()` on
/// `sender`. The returned watcher must be kept alive for watching to continue.
pub fn watch(root: PathBuf, sender: Sender<()>) -> Option<RecommendedWatcher> {
    let (raw_tx, raw_rx) = unbounded::<()>();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            if event.paths.iter().all(|p| is_ignored(p)) && !event.paths.is_empty() {
                return;
            }
            let _ = raw_tx.send(());
        }
    })
    .ok()?;

    if watcher.watch(&root, RecursiveMode::Recursive).is_err() {
        return None;
    }

    thread::spawn(move || loop {
        // Block for the first event of a burst.
        if raw_rx.recv().is_err() {
            break;
        }
        // Drain until things go quiet.
        loop {
            match raw_rx.recv_timeout(DEBOUNCE) {
                Ok(()) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if sender.send(()).is_err() {
            break;
        }
    });

    Some(watcher)
}

fn is_ignored(path: &Path) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str();
        IGNORE_COMPONENTS.iter().any(|ig| s == std::ffi::OsStr::new(ig))
    })
}
