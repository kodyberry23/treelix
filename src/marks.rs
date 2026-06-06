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

    fn save(&self) {
        if let Some(p) = &self.persist_path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut lines: Vec<String> =
                self.set.iter().map(|p| p.to_string_lossy().into_owned()).collect();
            lines.sort();
            let _ = std::fs::write(p, lines.join("\n"));
        }
    }
}
