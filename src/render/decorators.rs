//! Decorator styling for a node's name, applied in nvim-tree's precedence:
//! git → cut → copy → hidden.

use ratatui::style::{Modifier, Style};

use crate::clipboard::Clipboard;
use crate::theme::Theme;
use crate::tree::{NodeKind, Row};

/// Compute the style for a row's name span.
pub fn name_style(row: &Row, theme: &Theme, clipboard: &Clipboard) -> Style {
    // Base by node kind.
    let mut style = match row.kind {
        NodeKind::Directory | NodeKind::Symlink { to_dir: true } => theme.directory,
        NodeKind::Symlink { to_dir: false } => theme.symlink,
        NodeKind::File => {
            if row.executable {
                theme.executable
            } else {
                theme.text
            }
        }
    };

    // Git coloring overrides the base foreground.
    if let Some(status) = row.git {
        style = style.patch(theme.git_style(status));
    }

    // Clipboard state takes precedence for the name.
    if clipboard.is_cut(&row.path) {
        style = style.patch(theme.cut);
    } else if clipboard.is_copied(&row.path) {
        style = style.patch(theme.copy);
    }

    // Hidden files dim slightly when shown.
    if row.name.starts_with('.') {
        style = style.add_modifier(Modifier::DIM);
    }

    style
}
