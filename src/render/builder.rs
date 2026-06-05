//! Build ratatui list items from flattened tree rows.

use ratatui::text::{Line, Span};
use ratatui::widgets::ListItem;

use crate::clipboard::Clipboard;
use crate::theme::Theme;
use crate::tree::{NodeKind, Row};

use super::{decorators, icons};

pub struct RenderOpts {
    pub icons_enabled: bool,
    pub show_arrows: bool,
}

/// Build one `ListItem` per visible row.
pub fn build_items<'a>(
    rows: &[Row],
    theme: &Theme,
    opts: &RenderOpts,
    clipboard: &Clipboard,
) -> Vec<ListItem<'a>> {
    rows.iter()
        .map(|row| ListItem::new(build_line(row, theme, opts, clipboard)))
        .collect()
}

fn build_line<'a>(
    row: &Row,
    theme: &Theme,
    opts: &RenderOpts,
    clipboard: &Clipboard,
) -> Line<'a> {
    let mut spans: Vec<Span<'a>> = Vec::new();

    // Indent markers. The last entry of `ancestor_last` is this node's own
    // connector; earlier entries are vertical guides for its ancestors.
    let prefix = indent_prefix(&row.ancestor_last);
    if !prefix.is_empty() {
        spans.push(Span::styled(prefix, theme.indent_marker));
    }

    let is_dir = row.kind.is_dir();

    // Optional expand/collapse arrow for directories.
    if opts.show_arrows && is_dir && row.has_children {
        let arrow = if opts.icons_enabled {
            if row.expanded { icons::ARROW_OPEN } else { icons::ARROW_CLOSED }
        } else if row.expanded {
            icons::ascii::ARROW_OPEN
        } else {
            icons::ascii::ARROW_CLOSED
        };
        spans.push(Span::styled(format!("{arrow} "), theme.arrow));
    }

    // Icon.
    let icon = icon_for(row, opts.icons_enabled);
    if !icon.is_empty() {
        let icon_style = if is_dir {
            theme.folder_icon
        } else if matches!(row.kind, NodeKind::Symlink { .. }) {
            theme.symlink
        } else {
            theme.icon
        };
        spans.push(Span::styled(format!("{icon} "), icon_style));
    }

    // Name.
    let name_style = decorators::name_style(row, theme, clipboard);
    spans.push(Span::styled(row.name.clone(), name_style));

    // Symlink destination.
    if let Some(target) = &row.link_to {
        spans.push(Span::styled(
            format!(" → {}", target.display()),
            theme.symlink,
        ));
    }

    // Trailing git glyph.
    if let Some(status) = row.git {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            status.glyph().to_string(),
            theme.git_style(status),
        ));
    }

    Line::from(spans)
}

fn indent_prefix(ancestor_last: &[bool]) -> String {
    let mut prefix = String::new();
    let n = ancestor_last.len();
    for (i, &last) in ancestor_last.iter().enumerate() {
        if i + 1 == n {
            prefix.push_str(if last { "└ " } else { "├ " });
        } else {
            prefix.push_str(if last { "  " } else { "│ " });
        }
    }
    prefix
}

fn icon_for(row: &Row, icons_enabled: bool) -> &'static str {
    if icons_enabled {
        icons::file_icon(&row.name, row.kind, row.expanded)
    } else {
        match row.kind {
            NodeKind::Directory | NodeKind::Symlink { to_dir: true } => {
                if row.expanded {
                    icons::ascii::FOLDER_OPEN
                } else {
                    icons::ascii::FOLDER_CLOSED
                }
            }
            NodeKind::Symlink { to_dir: false } => icons::ascii::SYMLINK,
            NodeKind::File => icons::ascii::FILE_DEFAULT,
        }
    }
}

/// Build the styled root-header line (the tree's root path, `~`-shortened).
pub fn root_header<'a>(root: &std::path::Path, theme: &Theme) -> Line<'a> {
    let display = shorten_home(root);
    Line::from(Span::styled(display, theme.root))
}

fn shorten_home(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        if let Ok(rel) = path.strip_prefix(&home) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Row;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::List;
    use ratatui::Terminal;
    use std::path::PathBuf;

    #[test]
    fn indent_markers() {
        assert_eq!(indent_prefix(&[]), "");
        assert_eq!(indent_prefix(&[true]), "└ ");
        assert_eq!(indent_prefix(&[false]), "├ ");
        assert_eq!(indent_prefix(&[false, true]), "│ └ ");
        assert_eq!(indent_prefix(&[true, false]), "  ├ ");
    }

    fn row(name: &str, kind: NodeKind, ancestor_last: Vec<bool>) -> Row {
        Row {
            path: PathBuf::from("/demo").join(name),
            name: name.to_string(),
            kind,
            depth: ancestor_last.len().saturating_sub(1),
            expanded: false,
            has_children: kind.is_dir(),
            executable: false,
            git: None,
            link_to: None,
            is_last: ancestor_last.last().copied().unwrap_or(false),
            ancestor_last,
        }
    }

    #[test]
    fn renders_names_to_buffer() {
        let rows = vec![
            row("src", NodeKind::Directory, vec![false]),
            row("README.md", NodeKind::File, vec![true]),
        ];
        let theme = Theme::default();
        let opts = RenderOpts { icons_enabled: false, show_arrows: false };
        let clipboard = Clipboard::default();
        let items = build_items(&rows, &theme, &opts, &clipboard);

        let backend = TestBackend::new(30, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| f.render_widget(List::new(items), f.area()))
            .unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("src"), "buffer was: {text:?}");
        assert!(text.contains("README.md"), "buffer was: {text:?}");
    }
}
