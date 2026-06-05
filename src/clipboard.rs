//! Cut/copy clipboard state for file operations.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipOp {
    Cut,
    Copy,
}

#[derive(Debug, Clone, Default)]
pub struct Clipboard {
    pub op: Option<ClipOp>,
    pub paths: Vec<PathBuf>,
}

impl Clipboard {
    pub fn set(&mut self, op: ClipOp, paths: Vec<PathBuf>) {
        self.op = Some(op);
        self.paths = paths;
    }

    pub fn clear(&mut self) {
        self.op = None;
        self.paths.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn is_cut(&self, path: &Path) -> bool {
        self.op == Some(ClipOp::Cut) && self.paths.iter().any(|p| p == path)
    }

    pub fn is_copied(&self, path: &Path) -> bool {
        self.op == Some(ClipOp::Copy) && self.paths.iter().any(|p| p == path)
    }
}
