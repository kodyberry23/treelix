//! Build ratatui list items from flattened tree rows.

use ratatui::text::{Line, Span};
use ratatui::widgets::ListItem;

use crate::theme::Theme;
use crate::tree::{NodeKind, Row};

use super::decorators::Decor;
use super::{decorators, icons};

pub struct RenderOpts {
    pub icons_enabled: bool,
    pub show_arrows: bool,
    /// Draw tree connector lines (│ ├ └). When false, use plain indentation
    /// with a chevron on directories (nvim-tree style).
    pub indent_markers: bool,
}

/// Build one `ListItem` per visible row.
pub fn build_items<'a>(
    rows: &[Row],
    theme: &Theme,
    opts: &RenderOpts,
    decor: &Decor,
) -> Vec<ListItem<'a>> {
    rows.iter()
        .map(|row| ListItem::new(build_line(row, theme, opts, decor)))
        .collect()
}

fn build_line<'a>(row: &Row, theme: &Theme, opts: &RenderOpts, decor: &Decor) -> Line<'a> {
    let mut spans: Vec<Span<'a>> = Vec::new();

    let is_dir = row.kind.is_dir();
    let expandable = is_dir && row.has_children;

    if opts.indent_markers {
        // Connector-line indentation. The last entry of `ancestor_last` is this
        // node's own connector; earlier entries are vertical guides for its
        // ancestors. An optional chevron is then drawn before the icon.
        let prefix = indent_prefix(&row.ancestor_last);
        if !prefix.is_empty() {
            spans.push(Span::styled(prefix, theme.indent_marker));
        }
        if opts.show_arrows && expandable {
            spans.push(Span::styled(
                format!("{} ", arrow_glyph(row, opts.icons_enabled)),
                theme.arrow,
            ));
        }
    } else {
        // nvim-tree style: plain whitespace indentation for ancestor levels,
        // then a chevron in this node's own cell (or blank padding so names stay
        // aligned with their chevroned siblings). No connector lines.
        if row.depth > 0 {
            spans.push(Span::raw("  ".repeat(row.depth)));
        }
        if opts.show_arrows && expandable {
            spans.push(Span::styled(
                format!("{} ", arrow_glyph(row, opts.icons_enabled)),
                theme.arrow,
            ));
        } else {
            spans.push(Span::raw("  "));
        }
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
    let name_style = decorators::name_style(row, theme, decor);
    spans.push(Span::styled(row.name.clone(), name_style));

    // Bookmark glyph.
    if decor.marks.contains(&row.path) {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(icons::BOOKMARK.to_string(), theme.bookmark));
    }

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

/// The expand/collapse chevron glyph for a directory row.
fn arrow_glyph(row: &Row, icons_enabled: bool) -> &'static str {
    if icons_enabled {
        if row.expanded {
            icons::ARROW_OPEN
        } else {
            icons::ARROW_CLOSED
        }
    } else if row.expanded {
        icons::ascii::ARROW_OPEN
    } else {
        icons::ascii::ARROW_CLOSED
    }
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
    use crate::clipboard::Clipboard;
    use crate::tree::Row;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::List;
    use ratatui::Terminal;
    use std::collections::HashSet;
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
            group_target: None,
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
        let opts = RenderOpts {
            icons_enabled: false,
            show_arrows: true,
            indent_markers: false,
        };
        let clipboard = Clipboard::default();
        let marks = HashSet::new();
        let selection = HashSet::new();
        let decor = Decor {
            clipboard: &clipboard,
            marks: &marks,
            selection: &selection,
            current_file: None,
            special_files: &[],
        };
        let items = build_items(&rows, &theme, &opts, &decor);

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

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn nvim_tree_style_uses_chevrons_not_connector_lines() {
        let theme = Theme::default();
        let opts = RenderOpts {
            icons_enabled: false,
            show_arrows: true,
            indent_markers: false,
        };
        let clipboard = Clipboard::default();
        let marks = HashSet::new();
        let selection = HashSet::new();
        let decor = Decor {
            clipboard: &clipboard,
            marks: &marks,
            selection: &selection,
            current_file: None,
            special_files: &[],
        };

        // Top-level expanded dir + a nested file one level deeper.
        let mut dir = row("lua", NodeKind::Directory, vec![false]);
        dir.expanded = true;
        dir.has_children = true;
        let file = row("init.lua", NodeKind::File, vec![false, true]);

        let dir_line = line_text(&build_line(&dir, &theme, &opts, &decor));
        let file_line = line_text(&build_line(&file, &theme, &opts, &decor));

        // The directory leads with its chevron; no connector glyphs anywhere.
        assert!(dir_line.starts_with("v "), "dir line: {dir_line:?}");
        for g in ["│", "├", "└"] {
            assert!(
                !dir_line.contains(g) && !file_line.contains(g),
                "connector {g:?} found in {dir_line:?} / {file_line:?}"
            );
        }
        // Nested file: 2 spaces of ancestor indent + a 2-space (blank chevron)
        // own cell, keeping it aligned under its siblings.
        assert!(file_line.starts_with("    "), "file line: {file_line:?}");
        assert!(file_line.contains("init.lua"));
    }

    #[test]
    fn indent_markers_mode_still_draws_connector_lines() {
        let theme = Theme::default();
        let opts = RenderOpts {
            icons_enabled: false,
            show_arrows: true,
            indent_markers: true,
        };
        let clipboard = Clipboard::default();
        let marks = HashSet::new();
        let selection = HashSet::new();
        let decor = Decor {
            clipboard: &clipboard,
            marks: &marks,
            selection: &selection,
            current_file: None,
            special_files: &[],
        };
        let file = row("init.lua", NodeKind::File, vec![false, true]);
        let line = line_text(&build_line(&file, &theme, &opts, &decor));
        assert!(line.contains('│') || line.contains('└'), "line: {line:?}");
    }
}
