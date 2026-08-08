//! Bookmarks (marks). Optionally persisted to a plain newline-separated file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Marks {
    set: HashSet<PathBuf>,
    persist_path: Option<PathBuf>,
}

impl Marks {
    /// Load bookmarks, optionally from `~/.config/treelix/bookmarks`.
    pub fn load(persist: bool) -> Marks {
        let persist_path = if persist {
            crate::config::treelix_config_dir().map(|d| d.join("bookmarks"))
        } else {
            None
        };
        let mut set = HashSet::new();
        if let Some(p) = &persist_path {
            if let Ok(content) = std::fs::read_to_string(p) {
                for line in content.lines() {
                    let line = line.trim();
                    if !line.is_empty() {
                        set.insert(PathBuf::from(line));
                    }
                }
            }
        }
        Marks { set, persist_path }
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.set.contains(path)
    }

    pub fn all(&self) -> &HashSet<PathBuf> {
        &self.set
    }

    pub fn toggle(&mut self, path: &Path) -> bool {
        let now_marked = if self.set.contains(path) {
            self.set.remove(path);
            false
        } else {
            self.set.insert(path.to_path_buf());
            true
        };
        self.save();
        now_marked
    }

    /// Remove a set of paths (after a bulk operation moved/deleted them).
    pub fn remove_all(&mut self, paths: &[PathBuf]) {
        for p in paths {
            self.set.remove(p);
        }
        self.save();
    }

    /// Point a bookmark at a path's new location (after a move), so the mark
    /// follows the file instead of dangling on — and later deleting — whatever
    /// is recreated at the old path.
    pub fn remap(&mut self, old: &Path, new: &Path) {
        if self.set.remove(old) {
            self.set.insert(new.to_path_buf());
            self.save();
        }
    }

    fn save(&self) {
        if let Some(p) = &self.persist_path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut lines: Vec<String> = self
                .set
                .iter()
                // The store is newline-delimited; a path containing a newline
                // would corrupt it (and split into phantom bookmarks on load).
                // Such paths are exceedingly rare — skip them rather than
                // write a file that can't be read back.
                .filter(|p| !p.as_os_str().to_string_lossy().contains('\n'))
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            lines.sort();
            // Write to a unique temp file and atomically rename into place, so
            // a concurrent instance's save can't interleave with ours and
            // leave a truncated/half-written bookmarks file. The temp name is
            // pid-scoped to avoid two instances colliding on the temp path.
            let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
            if std::fs::write(&tmp, lines.join("\n")).is_ok() && std::fs::rename(&tmp, p).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}
