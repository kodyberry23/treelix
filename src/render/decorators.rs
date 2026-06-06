//! Decorator styling for a node's name, applied in nvim-tree's precedence:
//! special → git → cut → copy → opened → selected → hidden.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ratatui::style::{Modifier, Style};

use crate::clipboard::Clipboard;
use crate::theme::Theme;
use crate::tree::{NodeKind, Row};

/// References needed to decorate a row.
pub struct Decor<'a> {
    pub clipboard: &'a Clipboard,
    pub marks: &'a HashSet<PathBuf>,
    pub selection: &'a HashSet<PathBuf>,
    pub current_file: Option<&'a Path>,
    pub special_files: &'a [String],
}

impl Decor<'_> {
    fn is_special(&self, row: &Row) -> bool {
        if row.kind.is_dir() {
            return false;
        }
        let lower = row.name.to_lowercase();
        self.special_files.iter().any(|s| s == &lower)
    }
}

/// Compute the style for a row's name span.
pub fn name_style(row: &Row, theme: &Theme, decor: &Decor) -> Style {
    let mut style = match row.kind {
        NodeKind::Directory | NodeKind::Symlink { to_dir: true } => theme.directory,
        NodeKind::Symlink { to_dir: false } => theme.symlink,
        NodeKind::File => {
            if decor.is_special(row) {
                theme.special
            } else if row.executable {
                theme.executable
            } else {
                theme.text
            }
        }
    };

    if let Some(status) = row.git {
        style = style.patch(theme.git_style(status));
    }

    if decor.clipboard.is_cut(&row.path) {
        style = style.patch(theme.cut);
    } else if decor.clipboard.is_copied(&row.path) {
        style = style.patch(theme.copy);
    }

    if decor.current_file == Some(row.path.as_path()) {
        style = style.patch(theme.opened);
    }

    if decor.selection.contains(&row.path) {
        style = style.patch(theme.selected);
    }

    if row.name.starts_with('.') {
        style = style.add_modifier(Modifier::DIM);
    }

    style
}
