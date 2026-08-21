//! LSP diagnostics pushed by the editor, shown as file-name colors.
//!
//! The patched Helix sends its diagnostics over the reveal socket (see
//! `ipc.rs`): normally a whole snapshot (`diagnostics-begin <seq>`, one
//! `diagnostics <errors> <warnings> <abspath>` line per file that has any,
//! `diagnostics-end <seq>`), for every file its language servers report on,
//! open in a buffer or not. treelix keeps the counts per file, colors the name
//! by the worst severity, and colors every ancestor directory by the worst
//! severity below it, so a problem inside a collapsed folder is still visible.
//! Paths are expected canonical (the app canonicalizes them on arrival, as it
//! does for reveals), because tree nodes are keyed by canonical paths.

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
#[derive(Default, Debug)]
pub struct DiagnosticsData {
    files: HashMap<PathBuf, Counts>,
    dirs: HashMap<PathBuf, Below>,
}

impl DiagnosticsData {
    /// Record the counts for `path`; zero counts forget it. Returns whether
    /// anything changed, so callers can skip a redraw for repeated pushes.
    pub fn update(&mut self, path: PathBuf, counts: Counts) -> bool {
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
        if next.files == self.files {
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
        self.files.get(path).and_then(|counts| counts.shown(min))
    }

    /// The worst severity of any file under `dir` (at `min` and above). Based
    /// on the reported paths, not the tree's loaded children, so a directory
    /// never expanded still reflects what it contains.
    pub fn worst_under(&self, dir: &Path, min: Severity) -> Option<Diag> {
        let below = self.dirs.get(dir)?;
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
}
