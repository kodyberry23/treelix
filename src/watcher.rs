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

/// One coalesced filesystem notification.
pub enum FsChange {
    /// The set of paths touched during the burst; the consumer reloads only
    /// the directories that actually changed.
    Paths(HashSet<PathBuf>),
    /// The OS reported that events were dropped or coalesced beyond recovery
    /// (FSEvents `kFSEventStreamEventFlagMustScanSubDirs`, delivered by notify
    /// as the rescan flag). The event's paths no longer describe everything
    /// that changed, so only a full re-scan of the expanded tree restores
    /// accuracy — ignoring it silently leaves stale directories behind.
    Rescan,
}

enum RawEvent {
    Paths(Vec<PathBuf>),
    Rescan,
}

/// Begin watching `root`. Coalesced change notifications are sent on `sender`.
/// The returned watcher must be kept alive for watching to continue.
pub fn watch(root: PathBuf, sender: Sender<FsChange>) -> Option<RecommendedWatcher> {
    let (raw_tx, raw_rx) = unbounded::<RawEvent>();

    let cb_root = root.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            // An overflow/rescan event's paths are not a complete description
            // of what changed, so it normally escalates to a full re-scan.
            // Exception: FSEvents scopes the flag to the subtree that must be
            // rescanned, and when that subtree lies wholly within an ignored
            // high-churn directory (an `npm install` overflowing the queue),
            // everything lost was churn we drop anyway — escalating would
            // re-read the whole expanded tree once per debounce burst for the
            // duration of the install.
            if event.need_rescan() {
                let confined_to_ignored = !event.paths.is_empty()
                    && event.paths.iter().all(|p| inside_ignored(&cb_root, p));
                if !confined_to_ignored {
                    let _ = raw_tx.send(RawEvent::Rescan);
                }
                return;
            }
            // Drop events confined entirely to high-churn ignored directories.
            if !event.paths.is_empty() && event.paths.iter().all(|p| is_ignored(&cb_root, p)) {
                return;
            }
            let _ = raw_tx.send(RawEvent::Paths(event.paths));
        }
    })
    .ok()?;

    if watcher.watch(&root, RecursiveMode::Recursive).is_err() {
        return None;
    }

    thread::spawn(move || {
        // Block for the first event of each burst.
        while let Ok(first) = raw_rx.recv() {
            let mut changed: HashSet<PathBuf> = HashSet::new();
            let mut rescan = false;
            match first {
                RawEvent::Paths(paths) => changed.extend(paths),
                RawEvent::Rescan => rescan = true,
            }
            // Drain until things go quiet. A rescan anywhere in the burst
            // upgrades the whole burst: the accumulated paths are incomplete
            // by definition, so the consumer must do a full re-scan.
            loop {
                match raw_rx.recv_timeout(DEBOUNCE) {
                    Ok(RawEvent::Paths(paths)) => changed.extend(paths),
                    Ok(RawEvent::Rescan) => rescan = true,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            let msg = if rescan {
                FsChange::Rescan
            } else {
                FsChange::Paths(changed)
            };
            if sender.send(msg).is_err() {
                break;
            }
        }
    });

    Some(watcher)
}

/// True when `path` lies strictly INSIDE an ignored directory that is itself
/// BELOW the watch root. An event whose final component IS the ignored
/// directory (its creation, deletion, or rename) must pass through — dropping
/// it would leave the parent listing stale, hiding a new `build/` or showing a
/// deleted `node_modules/` forever. Only the churn within such directories is
/// noise.
///
/// The root prefix is stripped first: components ABOVE the root (a project that
/// happens to live under a directory literally named `build` or `target`) must
/// never mark paths as ignored — that would silently drop every event and
/// freeze the whole tree.
fn is_ignored(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false; // outside the tree: not our concern, don't drop it
    };
    let mut comps = rel.components();
    comps.next_back(); // the entry itself may BE the ignored dir — keep those
    comps.any(|c| is_high_churn_os(c.as_os_str()))
}

/// Whether `path` is inside a high-churn ignored directory below the root,
/// counting the final component too (used for the rescan-confinement check,
/// where a rescan scoped to `node_modules/` itself is still ignorable churn).
fn inside_ignored(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    rel.components().any(|c| is_high_churn_os(c.as_os_str()))
}

fn is_high_churn_os(s: &std::ffi::OsStr) -> bool {
    IGNORE_COMPONENTS
        .iter()
        .any(|ig| s == std::ffi::OsStr::new(ig))
}

/// Whether `name` is one of the high-churn directory names treelix refuses to
/// track (also used by the live filter to avoid walking them on non-git roots).
pub fn is_high_churn_name(name: &str) -> bool {
    IGNORE_COMPONENTS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_only_paths_inside_ignored_dirs() {
        let r = Path::new("/r");
        // Interior churn is dropped...
        assert!(is_ignored(r, Path::new("/r/node_modules/pkg")));
        assert!(is_ignored(r, Path::new("/r/node_modules/pkg/index.js")));
        assert!(is_ignored(r, Path::new("/r/a/target/debug/bin")));
        // ...but the ignored directory's own lifecycle events pass through,
        // so its parent listing stays truthful.
        assert!(!is_ignored(r, Path::new("/r/node_modules")));
        assert!(!is_ignored(r, Path::new("/r/a/target")));
        assert!(!is_ignored(r, Path::new("/r/build")));
        // Ordinary paths are never dropped.
        assert!(!is_ignored(r, Path::new("/r/src/main.rs")));
        // A FILE named like an ignored dir is the final component — kept.
        assert!(!is_ignored(r, Path::new("/r/src/build")));
    }

    #[test]
    fn root_prefix_components_are_not_treated_as_ignored() {
        // Regression: the whole absolute path was scanned, so a project living
        // under a directory literally named `build`/`target`/`node_modules`
        // matched on every event and silently froze the entire watcher.
        let root = Path::new("/Users/kody/build/app");
        assert!(
            !is_ignored(root, Path::new("/Users/kody/build/app/src/main.rs")),
            "the `build` ABOVE the root must not mark paths ignored"
        );
        assert!(
            !inside_ignored(root, Path::new("/Users/kody/build/app/src/main.rs")),
            "rescan confinement must also ignore the root prefix"
        );
        // A real node_modules BELOW this awkward root is still ignored.
        assert!(is_ignored(
            root,
            Path::new("/Users/kody/build/app/node_modules/x/i.js")
        ));
    }
}
