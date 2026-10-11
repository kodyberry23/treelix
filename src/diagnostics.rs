//! LSP diagnostics pushed by the editor, shown as file-name colors.
//!
//! The patched Helix sends its diagnostics over the reveal socket (see
//! `ipc.rs`): normally a whole snapshot (`diagnostics-begin <sender> <seq>`,
//! one `diagnostics <errors> <warnings> <abspath>` line per file that has any,
//! `diagnostics-end <seq>`), for every file its language servers report on,
//! open in a buffer or not. Several editors can push to one sidebar (the pane
//! beside it plus any `hx` started in a shell of the same tab), so `Sources`
//! keeps the latest snapshot of each sender and the tree shows their union.
//! An exiting editor's `diagnostics-bye` retracts only its own snapshot, and a
//! sender whose process is gone (killed, or too old to say goodbye) is pruned.
//! treelix keeps the counts per file, colors the name by the worst severity,
//! and colors every ancestor directory by the worst severity below it, so a
//! problem inside a collapsed folder is still visible. Paths are expected
//! canonical (the app canonicalizes them on arrival, as it does for reveals),
//! because tree nodes are keyed by canonical paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Worst severity worth showing, lowest first so `max` picks the dominant one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Error,
}

/// Which severities color the tree: the `diagnostics` config key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Errors,
    /// Errors and warnings (the default; matches VS Code's explorer).
    Warnings,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "off" => Some(Mode::Off),
            "errors" => Some(Mode::Errors),
            "warnings" => Some(Mode::Warnings),
            _ => None,
        }
    }

    /// The least severe level still shown, or `None` when the feature is off.
    pub fn min_severity(self) -> Option<Severity> {
        match self {
            Mode::Off => None,
            Mode::Errors => Some(Severity::Error),
            Mode::Warnings => Some(Severity::Warning),
        }
    }
}

/// Diagnostic counts for one file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub errors: u32,
    pub warnings: u32,
}

impl Counts {
    pub fn is_empty(self) -> bool {
        self.errors == 0 && self.warnings == 0
    }

    /// The file's own severity, before the configured floor is applied.
    fn severity(self) -> Option<Severity> {
        if self.errors > 0 {
            Some(Severity::Error)
        } else if self.warnings > 0 {
            Some(Severity::Warning)
        } else {
            None
        }
    }

    /// What to show for these counts at `min` and above.
    pub fn shown(self, min: Severity) -> Option<Diag> {
        match self.severity() {
            Some(Severity::Error) => Some(Diag {
                severity: Severity::Error,
                count: self.errors,
            }),
            Some(Severity::Warning) if min <= Severity::Warning => Some(Diag {
                severity: Severity::Warning,
                count: self.warnings,
            }),
            _ => None,
        }
    }
}

/// What a row displays: the dominant severity and, for files, how many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Diag {
    pub severity: Severity,
    /// Number of diagnostics at `severity` for a file; 0 for a directory,
    /// whose severity is inherited from what it contains.
    pub count: u32,
}

/// How many files below a directory carry each severity. Maintained as files
/// change so a directory's color is one lookup, not a scan of every file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Below {
    with_errors: u32,
    with_warnings: u32,
}

/// Every file the editor reported diagnostics for, keyed by absolute path.
///
/// Keys are case-folded: macOS filesystems are case-insensitive by default, so
/// the editor may know a file by different casing than the directory listing
/// (TypeScript imports and `hx SRC/file` resolve case-insensitively). Without
/// folding, the folder colors (ancestor tallies match) while the file itself
/// misses its exact-match lookup: a folder stuck red with a plain file in it.
#[derive(Default, Debug)]
pub struct DiagnosticsData {
    files: HashMap<PathBuf, Counts>,
    dirs: HashMap<PathBuf, Below>,
}

/// The comparison form of a path: NFC-normalized and Unicode-lowercased when
/// it is UTF-8 (the overwhelming case), byte-identical otherwise. NFC matters
/// on macOS too: the filesystem may hand back decomposed names (NFD) while an
/// editor reports the composed form, which is the same folder-red/file-plain
/// symptom as a case difference.
fn fold(path: &Path) -> PathBuf {
    use unicode_normalization::UnicodeNormalization as _;
    match path.to_str() {
        Some(utf8) => PathBuf::from(utf8.nfc().collect::<String>().to_lowercase()),
        None => path.to_path_buf(),
    }
}

impl DiagnosticsData {
    /// Record the counts for `path`; zero counts forget it. Returns whether
    /// anything changed, so callers can skip a redraw for repeated pushes.
    pub fn update(&mut self, path: PathBuf, counts: Counts) -> bool {
        let path = fold(&path);
        let previous = self.files.get(&path).copied();
        let next = (!counts.is_empty()).then_some(counts);
        if previous == next {
            return false;
        }
        if let Some(previous) = previous {
            self.account(&path, previous, -1);
        }
        match next {
            Some(counts) => {
                self.account(&path, counts, 1);
                self.files.insert(path, counts);
            }
            None => {
                self.files.remove(&path);
            }
        }
        true
    }

    /// Replace everything with a complete snapshot. Returns whether the
    /// result differs from what was held.
    pub fn replace(&mut self, files: impl IntoIterator<Item = (PathBuf, Counts)>) -> bool {
        let mut next = DiagnosticsData::default();
        for (path, counts) in files {
            next.update(path, counts);
        }
        // `dirs` is derived from `files` by `account`, so comparing the
        // files is comparing everything; the assertion keeps that true.
        if next.files == self.files {
            debug_assert_eq!(next.dirs, self.dirs, "dirs are derived from files");
            return false;
        }
        *self = next;
        true
    }

    /// Add (`sign` = 1) or remove (`sign` = -1) a file's contribution to the
    /// per-directory tallies of all its ancestors.
    fn account(&mut self, path: &Path, counts: Counts, sign: i32) {
        let Some(severity) = counts.severity() else {
            return;
        };
        for dir in path.ancestors().skip(1) {
            let below = self.dirs.entry(dir.to_path_buf()).or_default();
            let slot = match severity {
                Severity::Error => &mut below.with_errors,
                Severity::Warning => &mut below.with_warnings,
            };
            *slot = slot.saturating_add_signed(sign);
            if *below == Below::default() {
                self.dirs.remove(dir);
            }
        }
    }

    /// What a file row shows at `min` and above.
    pub fn for_file(&self, path: &Path, min: Severity) -> Option<Diag> {
        self.files
            .get(&fold(path))
            .and_then(|counts| counts.shown(min))
    }

    /// The worst severity of any file under `dir` (at `min` and above). Based
    /// on the reported paths, not the tree's loaded children, so a directory
    /// never expanded still reflects what it contains.
    pub fn worst_under(&self, dir: &Path, min: Severity) -> Option<Diag> {
        let below = self.dirs.get(&fold(dir))?;
        let severity = if below.with_errors > 0 {
            Severity::Error
        } else if below.with_warnings > 0 && min <= Severity::Warning {
            Severity::Warning
        } else {
            return None;
        };
        Some(Diag { severity, count: 0 })
    }
}

/// The latest diagnostics of every editor pushing to this sidebar.
///
/// Each sender's snapshots are ordered by its own `seq`; senders never order
/// against each other, because each counts from 1 in its own process. What the
/// tree shows is `merged`, the union of every sender's files.
#[derive(Default, Debug)]
pub struct Sources {
    senders: HashMap<String, Source>,
}

/// One sender's latest state.
#[derive(Debug)]
struct Source {
    /// The newest snapshot or farewell applied; `None` while the legacy
    /// sender has only sent bare lines. Anything not newer is a straggler
    /// served late on another connection and is ignored.
    seq: Option<u64>,
    /// Its files with any diagnostics, keyed by `fold(path)`. Empty after a
    /// farewell: the entry then stays as a tombstone so a straggler cannot
    /// repaint what the farewell cleared.
    files: HashMap<PathBuf, Counts>,
    /// The editor's process, from a sender id of the form `hx-<pid>-<start>`.
    /// `None` (the legacy sender `""`, or an id in another form) is never
    /// pruned, because there is no process to check.
    pid: Option<u32>,
}

/// The pid in a Helix sender id (`hx-<pid>-<start>`), if it has that form and
/// names a real process id. Pid 0 and values beyond `pid_t` are rejected:
/// `kill` would read them as a process group, not a process.
fn sender_pid(sender: &str) -> Option<u32> {
    let (pid, start) = sender.strip_prefix("hx-")?.split_once('-')?;
    if start.is_empty() || !start.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pid: u32 = pid.parse().ok()?;
    (pid > 0 && i32::try_from(pid).is_ok()).then_some(pid)
}

/// Fold and keep only files that have something to show. A file listed twice
/// keeps its worst counts.
fn fold_files(files: impl IntoIterator<Item = (PathBuf, Counts)>) -> HashMap<PathBuf, Counts> {
    let mut folded = HashMap::new();
    for (path, counts) in files {
        if !counts.is_empty() {
            merge_max(&mut folded, fold(&path), counts);
        }
    }
    folded
}

/// Record `counts` for `key`, keeping the larger error and warning counts.
fn merge_max(into: &mut HashMap<PathBuf, Counts>, key: PathBuf, counts: Counts) {
    let slot = into.entry(key).or_default();
    slot.errors = slot.errors.max(counts.errors);
    slot.warnings = slot.warnings.max(counts.warnings);
}

impl Sources {
    /// Apply a complete snapshot from `sender`. A sender's first snapshot is
    /// always accepted (a restarted editor counts from 1 again under a new
    /// id); after that only a newer `seq` is. Returns whether it was applied.
    pub fn snapshot(&mut self, sender: String, seq: u64, files: Vec<(PathBuf, Counts)>) -> bool {
        if self
            .senders
            .get(&sender)
            .and_then(|source| source.seq)
            .is_some_and(|last| seq <= last)
        {
            return false;
        }
        let pid = sender_pid(&sender);
        let files = fold_files(files);
        self.senders.insert(
            sender,
            Source {
                seq: Some(seq),
                files,
                pid,
            },
        );
        true
    }

    /// `sender` is exiting: clear its files and leave a tombstone at `seq`,
    /// under the same ordering as a snapshot. Other senders are untouched. A
    /// farewell from a sender never seen still leaves the tombstone, so its
    /// snapshot arriving late on another connection is rejected. Returns
    /// whether it was applied.
    pub fn bye(&mut self, sender: String, seq: u64) -> bool {
        self.snapshot(sender, seq, Vec::new())
    }

    /// A bare `diagnostics` line (an older editor without snapshots): update
    /// one file of the legacy sender `""`. Returns whether that changed it.
    pub fn single(&mut self, path: PathBuf, counts: Counts) -> bool {
        let source = self.senders.entry(String::new()).or_insert(Source {
            seq: None,
            files: HashMap::new(),
            pid: None,
        });
        let key = fold(&path);
        if counts.is_empty() {
            source.files.remove(&key).is_some()
        } else {
            source.files.insert(key, counts) != Some(counts)
        }
    }

    /// Drop every sender whose process `alive` says is gone, files and
    /// tombstone alike. Returns whether a dropped sender had files, that is,
    /// whether `merged` may have changed.
    pub fn prune(&mut self, alive: impl Fn(u32) -> bool) -> bool {
        let mut shown = false;
        self.senders.retain(|_, source| {
            let keep = source.pid.is_none_or(&alive);
            shown |= !keep && !source.files.is_empty();
            keep
        });
        shown
    }

    /// The union of every sender's files, keyed by folded path. Per file it
    /// takes the larger error and warning counts, not their sum: two editors
    /// on one workspace each run their own language server and report the
    /// same diagnostics, which a sum would double.
    pub fn merged(&self) -> Vec<(PathBuf, Counts)> {
        let mut union = HashMap::new();
        for source in self.senders.values() {
            for (path, counts) in &source.files {
                merge_max(&mut union, path.clone(), *counts);
            }
        }
        union.into_iter().collect()
    }

    /// Whether any sender that can be pruned has files on display, which is
    /// when the app needs its periodic liveness check.
    pub fn has_live_marks(&self) -> bool {
        self.senders
            .values()
            .any(|source| source.pid.is_some() && !source.files.is_empty())
    }
}

/// Whether process `pid` exists. `kill(pid, 0)` delivers nothing and only
/// checks: success or `EPERM` (it exists but belongs to another user) means
/// it is running. A zombie (exited, not yet reaped by its parent) also answers,
/// which does not arise under zellij, where the pane's shell reaps at once.
/// A recycled pid reads as alive, so a dead editor's marks can
/// outlive it until the process that took its pid exits too.
pub fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence and permission checks.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(errors: u32, warnings: u32) -> Counts {
        Counts { errors, warnings }
    }

    fn severity_under(data: &DiagnosticsData, dir: &str, min: Severity) -> Option<Severity> {
        data.worst_under(Path::new(dir), min).map(|d| d.severity)
    }

    #[test]
    fn mode_parses_and_gates_severity() {
        assert_eq!(Mode::parse("off"), Some(Mode::Off));
        assert_eq!(Mode::parse("errors"), Some(Mode::Errors));
        assert_eq!(Mode::parse("warnings"), Some(Mode::Warnings));
        assert_eq!(Mode::parse("hints"), None);
        assert_eq!(Mode::Off.min_severity(), None);
        assert_eq!(Mode::Errors.min_severity(), Some(Severity::Error));
        assert_eq!(Mode::Warnings.min_severity(), Some(Severity::Warning));
    }

    #[test]
    fn errors_dominate_and_warnings_respect_the_floor() {
        let both = counts(2, 5);
        assert_eq!(
            both.shown(Severity::Warning),
            Some(Diag {
                severity: Severity::Error,
                count: 2
            })
        );
        let warn = counts(0, 3);
        assert_eq!(
            warn.shown(Severity::Warning),
            Some(Diag {
                severity: Severity::Warning,
                count: 3
            })
        );
        assert_eq!(
            warn.shown(Severity::Error),
            None,
            "errors-only mode hides it"
        );
        assert_eq!(counts(0, 0).shown(Severity::Warning), None);
    }

    #[test]
    fn update_reports_changes_and_forgets_clean_files() {
        let mut data = DiagnosticsData::default();
        let file = PathBuf::from("/r/src/a.rs");
        assert!(data.update(file.clone(), counts(1, 0)));
        assert!(
            !data.update(file.clone(), counts(1, 0)),
            "same counts: no change"
        );
        assert!(data.update(file.clone(), counts(1, 2)));
        assert!(data.update(file.clone(), counts(0, 0)), "cleared");
        assert_eq!(data.for_file(&file, Severity::Warning), None);
        assert!(
            !data.update(file, counts(0, 0)),
            "clearing twice: no change"
        );
        assert!(data.dirs.is_empty(), "no tallies left behind");
    }

    #[test]
    fn directories_inherit_the_worst_severity_below_them() {
        let mut data = DiagnosticsData::default();
        data.update(PathBuf::from("/r/src/deep/a.rs"), counts(0, 1));
        data.update(PathBuf::from("/r/src/b.rs"), counts(0, 2));
        data.update(PathBuf::from("/r/other/c.rs"), counts(3, 0));
        assert_eq!(
            severity_under(&data, "/r/src", Severity::Warning),
            Some(Severity::Warning)
        );
        assert_eq!(severity_under(&data, "/r/src", Severity::Error), None);
        assert_eq!(
            severity_under(&data, "/r", Severity::Warning),
            Some(Severity::Error)
        );
        assert_eq!(
            severity_under(&data, "/r/src/deep/a.rs", Severity::Warning),
            None,
            "a file is not under itself"
        );
        assert_eq!(
            severity_under(&data, "/r/srcx", Severity::Warning),
            None,
            "prefix must be a path component"
        );

        // Tallies follow changes: an error turning into a warning, then clearing.
        data.update(PathBuf::from("/r/other/c.rs"), counts(0, 1));
        assert_eq!(
            severity_under(&data, "/r", Severity::Warning),
            Some(Severity::Warning)
        );
        data.update(PathBuf::from("/r/other/c.rs"), counts(0, 0));
        assert_eq!(severity_under(&data, "/r/other", Severity::Warning), None);
        assert_eq!(
            severity_under(&data, "/r", Severity::Warning),
            Some(Severity::Warning)
        );
    }

    #[test]
    fn lookups_ignore_case_differences_between_editor_and_disk() {
        let mut data = DiagnosticsData::default();
        data.update(PathBuf::from("/r/src/header.tsx"), counts(1, 0));
        assert_eq!(
            data.for_file(Path::new("/r/src/Header.tsx"), Severity::Warning)
                .map(|d| d.severity),
            Some(Severity::Error),
            "the on-disk casing finds the editor's lowercase report"
        );
        assert_eq!(
            severity_under(&data, "/r/SRC", Severity::Warning),
            Some(Severity::Error)
        );
        assert!(
            !data.update(PathBuf::from("/r/src/HEADER.tsx"), counts(1, 0)),
            "the same file under other casing is the same entry"
        );
        assert!(data.update(PathBuf::from("/r/src/Header.tsx"), counts(0, 0)));
        assert_eq!(
            data.for_file(Path::new("/r/src/header.tsx"), Severity::Warning),
            None
        );
    }

    #[test]
    fn lookups_ignore_unicode_normalization_differences() {
        let mut data = DiagnosticsData::default();
        // NFD "café.rs" (e + combining acute) as a filesystem might list it...
        data.update(PathBuf::from("/r/src/cafe\u{301}.rs"), counts(1, 0));
        // ...found under the NFC form an editor reports (and vice versa).
        assert_eq!(
            data.for_file(Path::new("/r/src/caf\u{e9}.rs"), Severity::Warning)
                .map(|d| d.severity),
            Some(Severity::Error)
        );
        assert!(
            !data.update(PathBuf::from("/r/src/CAF\u{c9}.rs"), counts(1, 0)),
            "composed uppercase is still the same entry"
        );
    }

    #[test]
    fn replace_swaps_the_whole_set_and_reports_whether_it_changed() {
        let mut data = DiagnosticsData::default();
        data.update(PathBuf::from("/r/a.rs"), counts(1, 0));
        data.update(PathBuf::from("/r/b.rs"), counts(0, 1));
        let same = vec![
            (PathBuf::from("/r/b.rs"), counts(0, 1)),
            (PathBuf::from("/r/a.rs"), counts(1, 0)),
            (PathBuf::from("/r/clean.rs"), counts(0, 0)),
        ];
        assert!(!data.replace(same), "same files and counts: no change");
        let next = vec![(PathBuf::from("/r/sub/c.rs"), counts(0, 2))];
        assert!(data.replace(next));
        assert_eq!(data.for_file(Path::new("/r/a.rs"), Severity::Warning), None);
        assert_eq!(
            severity_under(&data, "/r/sub", Severity::Warning),
            Some(Severity::Warning)
        );
        assert_eq!(
            severity_under(&data, "/r", Severity::Warning),
            Some(Severity::Warning),
            "tallies rebuilt from the snapshot"
        );
        assert!(data.replace(Vec::new()), "emptying is a change");
        assert!(data.dirs.is_empty());
    }

    fn file(path: &str, errors: u32, warnings: u32) -> (PathBuf, Counts) {
        (PathBuf::from(path), counts(errors, warnings))
    }

    /// `merged` as a sorted list, for comparisons.
    fn union(sources: &Sources) -> Vec<(PathBuf, Counts)> {
        let mut merged = sources.merged();
        merged.sort_by(|a, b| a.0.cmp(&b.0));
        merged
    }

    #[test]
    fn sender_ids_name_a_pid_only_in_the_helix_form() {
        assert_eq!(sender_pid("hx-4242-1760000000000000"), Some(4242));
        assert_eq!(sender_pid(""), None, "legacy sender");
        assert_eq!(sender_pid("hx-1"), None, "no start time");
        assert_eq!(sender_pid("hx-old"), None);
        assert_eq!(sender_pid("hx-12-"), None);
        assert_eq!(sender_pid("hx-12-3x"), None);
        assert_eq!(sender_pid("hx-0-1"), None, "pid 0 is a process group");
        assert_eq!(
            sender_pid("hx-4294967295-1"),
            None,
            "beyond pid_t would be a negative pid"
        );
        assert_eq!(sender_pid("vim-12-1"), None);
    }

    #[test]
    fn this_process_is_alive_and_a_reaped_child_is_not() {
        assert!(process_alive(std::process::id()));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!process_alive(pid), "a reaped child no longer exists");
        assert!(!process_alive(0), "never a process group");
        assert!(!process_alive(u32::MAX));
    }

    #[test]
    fn the_union_keeps_the_larger_counts_not_their_sum() {
        let mut sources = Sources::default();
        assert!(sources.snapshot(
            "hx-1-1".into(),
            1,
            vec![file("/r/a.rs", 2, 0), file("/r/b.rs", 0, 1)],
        ));
        assert!(sources.snapshot(
            "hx-2-1".into(),
            1,
            vec![file("/r/a.rs", 2, 3), file("/r/c.rs", 1, 0)],
        ));
        assert_eq!(
            union(&sources),
            vec![
                file("/r/a.rs", 2, 3),
                file("/r/b.rs", 0, 1),
                file("/r/c.rs", 1, 0),
            ],
            "the same file reported twice is not doubled"
        );
    }

    #[test]
    fn senders_merge_on_the_folded_path() {
        let mut sources = Sources::default();
        sources.snapshot("hx-1-1".into(), 1, vec![file("/r/src/Header.tsx", 1, 0)]);
        sources.snapshot("hx-2-1".into(), 1, vec![file("/r/src/header.tsx", 0, 2)]);
        sources.snapshot(
            "hx-3-1".into(),
            1,
            vec![file("/r/src/cafe\u{301}.rs", 1, 0)],
        );
        sources.snapshot("hx-4-1".into(), 1, vec![file("/r/src/caf\u{e9}.rs", 3, 0)]);
        assert_eq!(
            union(&sources),
            vec![
                file("/r/src/caf\u{e9}.rs", 3, 0),
                file("/r/src/header.tsx", 1, 2),
            ],
            "case and normalization variants are one file"
        );
    }

    #[test]
    fn each_sender_orders_only_its_own_snapshots() {
        let mut sources = Sources::default();
        assert!(sources.snapshot("hx-1-1".into(), 900, vec![file("/r/a.rs", 1, 0)]));
        assert!(
            sources.snapshot("hx-2-1".into(), 1, Vec::new()),
            "another sender's first snapshot applies whatever its seq"
        );
        assert_eq!(union(&sources), vec![file("/r/a.rs", 1, 0)]);
        assert!(!sources.snapshot("hx-2-1".into(), 1, vec![file("/r/b.rs", 1, 0)]));
        assert!(!sources.snapshot("hx-1-1".into(), 899, Vec::new()));
        assert!(sources.snapshot("hx-2-1".into(), 2, vec![file("/r/b.rs", 0, 1)]));
        assert_eq!(
            union(&sources),
            vec![file("/r/a.rs", 1, 0), file("/r/b.rs", 0, 1)]
        );
    }

    #[test]
    fn a_farewell_retracts_only_its_own_sender() {
        let mut sources = Sources::default();
        sources.snapshot("hx-1-1".into(), 3, vec![file("/r/a.rs", 1, 0)]);
        sources.snapshot("hx-2-1".into(), 7, vec![file("/r/b.rs", 0, 1)]);
        assert!(sources.bye("hx-2-1".into(), 8));
        assert_eq!(union(&sources), vec![file("/r/a.rs", 1, 0)]);
        assert!(
            !sources.bye("hx-1-1".into(), 3),
            "a farewell not newer than the snapshot is stale"
        );
        assert_eq!(union(&sources), vec![file("/r/a.rs", 1, 0)]);
        assert!(sources.bye("hx-1-1".into(), 4));
        assert!(sources.merged().is_empty());
    }

    #[test]
    fn a_straggler_after_a_farewell_is_rejected() {
        let mut sources = Sources::default();
        sources.snapshot("hx-1-1".into(), 2, vec![file("/r/a.rs", 1, 0)]);
        sources.bye("hx-1-1".into(), 3);
        assert!(
            !sources.snapshot("hx-1-1".into(), 2, vec![file("/r/a.rs", 1, 0)]),
            "the superseded snapshot arriving late"
        );
        assert!(sources.merged().is_empty());

        // A farewell that overtook the sender's only snapshot still wins.
        assert!(sources.bye("hx-9-1".into(), 2));
        assert!(!sources.snapshot("hx-9-1".into(), 1, vec![file("/r/z.rs", 1, 0)]));
        assert!(sources.merged().is_empty());
    }

    #[test]
    fn prune_drops_a_dead_senders_files_and_tombstone() {
        let mut sources = Sources::default();
        sources.snapshot("hx-10-1".into(), 1, vec![file("/r/a.rs", 1, 0)]);
        sources.snapshot("hx-20-1".into(), 1, vec![file("/r/b.rs", 1, 0)]);
        sources.bye("hx-30-1".into(), 5);
        assert!(sources.has_live_marks());
        assert!(!sources.prune(|_| true), "everyone alive: nothing dropped");

        assert!(sources.prune(|pid| pid != 10), "a sender with files died");
        assert_eq!(union(&sources), vec![file("/r/b.rs", 1, 0)]);
        assert!(
            !sources.prune(|pid| pid == 20),
            "dropping only a tombstone changes nothing on display"
        );
        assert!(
            !sources.senders.contains_key("hx-30-1"),
            "the tombstone went with its process"
        );
        assert!(sources.prune(|_| false));
        assert!(!sources.has_live_marks());
        assert!(sources.senders.is_empty());
    }

    #[test]
    fn senders_without_a_pid_are_never_pruned() {
        let mut sources = Sources::default();
        sources.snapshot(String::new(), 1, vec![file("/r/a.rs", 1, 0)]);
        sources.snapshot("hx-1".into(), 1, vec![file("/r/b.rs", 1, 0)]);
        assert!(
            !sources.has_live_marks(),
            "nothing to check, so no timer is needed"
        );
        assert!(!sources.prune(|_| false));
        assert_eq!(
            union(&sources),
            vec![file("/r/a.rs", 1, 0), file("/r/b.rs", 1, 0)]
        );
    }

    #[test]
    fn bare_lines_update_the_legacy_sender() {
        let mut sources = Sources::default();
        assert!(sources.single(PathBuf::from("/r/a.rs"), counts(1, 0)));
        assert!(
            !sources.single(PathBuf::from("/r/A.rs"), counts(1, 0)),
            "same file, same counts"
        );
        assert!(sources.single(PathBuf::from("/r/b.rs"), counts(0, 2)));
        assert!(
            !sources.single(PathBuf::from("/r/c.rs"), counts(0, 0)),
            "clearing an unknown file"
        );
        assert_eq!(
            union(&sources),
            vec![file("/r/a.rs", 1, 0), file("/r/b.rs", 0, 2)]
        );
        assert!(sources.single(PathBuf::from("/r/a.rs"), counts(0, 0)));
        assert_eq!(union(&sources), vec![file("/r/b.rs", 0, 2)]);

        // A legacy snapshot (no sender) replaces the bare lines, and its
        // first one applies even at seq 0.
        assert!(sources.snapshot(String::new(), 0, vec![file("/r/c.rs", 1, 0)]));
        assert_eq!(union(&sources), vec![file("/r/c.rs", 1, 0)]);
        assert!(!sources.snapshot(String::new(), 0, Vec::new()));
        // Another sender's union still includes the legacy files.
        sources.snapshot("hx-5-1".into(), 1, vec![file("/r/d.rs", 0, 1)]);
        assert_eq!(
            union(&sources),
            vec![file("/r/c.rs", 1, 0), file("/r/d.rs", 0, 1)]
        );
    }
}
