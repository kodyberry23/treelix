//! The tree node model. Mirrors nvim-tree: directories load children lazily,
//! collapse keeps the cached subtree.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::git::GitStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Directory,
    /// Symlink; `to_dir` is true when it resolves to a directory.
    Symlink { to_dir: bool },
}

impl NodeKind {
    pub fn is_dir(self) -> bool {
        matches!(
            self,
            NodeKind::Directory | NodeKind::Symlink { to_dir: true }
        )
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub kind: NodeKind,
    pub expanded: bool,
    /// Whether children have been read from disk yet (directories only).
    pub loaded: bool,
    pub children: Vec<Node>,
    pub executable: bool,
    /// Per-file status, or propagated highest-priority status for directories.
    pub git: Option<GitStatus>,
    /// Symlink destination, for display.
    pub link_to: Option<PathBuf>,
    /// File size in bytes (0 for directories).
    pub len: u64,
    /// Last-modified time, if available.
    pub mtime: Option<SystemTime>,
}

impl Node {
    pub fn new(path: PathBuf, kind: NodeKind, executable: bool, link_to: Option<PathBuf>) -> Self {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Node {
            path,
            name,
            kind,
            expanded: false,
            loaded: false,
            children: Vec::new(),
            executable,
            git: None,
            link_to,
            len: 0,
            mtime: None,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }

    pub fn is_hidden(&self) -> bool {
        self.name.starts_with('.')
    }

    /// Find a descendant (or self) node by absolute path, mutably.
    pub fn find_mut(&mut self, target: &Path) -> Option<&mut Node> {
        if self.path == target {
            return Some(self);
        }
        if !target.starts_with(&self.path) {
            return None;
        }
        for child in &mut self.children {
            if let Some(found) = child.find_mut(target) {
                return Some(found);
            }
        }
        None
    }
}
