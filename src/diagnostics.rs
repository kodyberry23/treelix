//! LSP diagnostics pushed by the editor, shown as file-name colors.
//!
//! The patched Helix sends one `diagnostics <errors> <warnings> <abspath>` line
//! over the reveal socket whenever a file's counts change (see `ipc.rs`), for
//! every file its language servers report on, open in a buffer or not. treelix
//! keeps the counts per file, colors the name by the worst severity, and
//! propagates that severity up to every ancestor directory so a problem inside
//! a collapsed folder is still visible.

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

    /// What to show for these counts at `min` and above.
    pub fn shown(self, min: Severity) -> Option<Diag> {
        if self.errors > 0 {
            Some(Diag {
                severity: Severity::Error,
                count: self.errors,
            })
        } else if self.warnings > 0 && min <= Severity::Warning {
            Some(Diag {
                severity: Severity::Warning,
                count: self.warnings,
            })
        } else {
            None
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

/// Every file the editor reported diagnostics for, keyed by absolute path.
#[derive(Default, Debug)]
pub struct DiagnosticsData {
    files: HashMap<PathBuf, Counts>,
}

impl DiagnosticsData {
    /// Record the counts for `path`; zero counts forget it. Returns whether
    /// anything changed, so callers can skip a redraw for repeated pushes.
    pub fn update(&mut self, path: PathBuf, counts: Counts) -> bool {
        if counts.is_empty() {
            self.files.remove(&path).is_some()
        } else {
            self.files.insert(path, counts) != Some(counts)
        }
    }

    /// What a file row shows at `min` and above.
    pub fn for_file(&self, path: &Path, min: Severity) -> Option<Diag> {
        self.files.get(path).and_then(|counts| counts.shown(min))
    }

    /// The worst severity of any file under `dir` (at `min` and above). Looks
    /// at the reported paths rather than the tree's loaded children, so a
    /// directory never expanded still reflects what it contains.
    pub fn worst_under(&self, dir: &Path, min: Severity) -> Option<Diag> {
        self.files
            .iter()
            .filter(|(path, _)| path.starts_with(dir) && path.as_path() != dir)
            .filter_map(|(_, counts)| counts.shown(min))
            .map(|diag| diag.severity)
            .max()
            .map(|severity| Diag { severity, count: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(errors: u32, warnings: u32) -> Counts {
        Counts { errors, warnings }
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
    }

    #[test]
    fn directories_inherit_the_worst_severity_below_them() {
        let mut data = DiagnosticsData::default();
        data.update(PathBuf::from("/r/src/deep/a.rs"), counts(0, 1));
        data.update(PathBuf::from("/r/src/b.rs"), counts(0, 2));
        data.update(PathBuf::from("/r/other/c.rs"), counts(3, 0));
        let src = Path::new("/r/src");
        assert_eq!(
            data.worst_under(src, Severity::Warning).map(|d| d.severity),
            Some(Severity::Warning)
        );
        assert_eq!(data.worst_under(src, Severity::Error), None);
        assert_eq!(
            data.worst_under(Path::new("/r"), Severity::Warning)
                .map(|d| d.severity),
            Some(Severity::Error)
        );
        assert_eq!(
            data.worst_under(Path::new("/r/src/deep/a.rs"), Severity::Warning),
            None,
            "a file is not under itself"
        );
        assert_eq!(
            data.worst_under(Path::new("/r/srcx"), Severity::Warning),
            None,
            "prefix must be a path component"
        );
    }
}
