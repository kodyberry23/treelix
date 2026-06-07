//! Overlay UI state and rendering: create/rename prompts, delete/trash confirm,
//! and the help panel.

use std::path::PathBuf;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum Overlay {
    None,
    Input(InputState),
    Confirm(ConfirmState),
    Info(InfoState),
    Help,
}

#[derive(Debug, Clone)]
pub struct InfoState {
    pub title: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InputState {
    pub prompt: String,
    pub buffer: String,
    pub kind: InputKind,
}

#[derive(Debug, Clone)]
pub enum InputKind {
    /// Create relative to this directory (trailing `/` in buffer = directory).
    Create { dir: PathBuf },
    /// Rename the full basename of this path.
    Rename { path: PathBuf },
    /// Rename only the stem, keeping the extension.
    RenameBasename { path: PathBuf },
    /// Rename to a full (possibly relative) path.
    RenameFull { path: PathBuf },
    /// Search for a node by name (case-insensitive substring).
    Search,
}

#[derive(Debug, Clone)]
pub struct ConfirmState {
    pub prompt: String,
    pub kind: ConfirmKind,
}

#[derive(Debug, Clone)]
pub enum ConfirmKind {
    Delete(PathBuf),
    Trash(PathBuf),
    BulkDelete(Vec<PathBuf>),
    BulkTrash(Vec<PathBuf>),
}

/// Render an input prompt as a one-line bordered popup near the bottom.
pub fn render_input(frame: &mut Frame, area: Rect, theme: &Theme, state: &InputState) {
    let popup = bottom_popup(area, 3);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.prompt)
        .title(Span::styled(state.prompt.clone(), theme.prompt));
    let line = Line::from(vec![
        Span::styled(&state.buffer, theme.text),
        Span::styled("▏", theme.prompt), // cursor
    ]);
    let para = Paragraph::new(line).block(block);
    frame.render_widget(para, popup);
}

/// Render a yes/no confirmation popup.
pub fn render_confirm(frame: &mut Frame, area: Rect, theme: &Theme, state: &ConfirmState) {
    let popup = bottom_popup(area, 4);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.prompt);
    let text = vec![
        Line::from(Span::styled(state.prompt.clone(), theme.text)),
        Line::from(Span::styled("[y]es  [n]o", theme.prompt)),
    ];
    let para = Paragraph::new(text).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, popup);
}

/// Render a file-info popup.
pub fn render_info(frame: &mut Frame, area: Rect, theme: &Theme, state: &InfoState) {
    let popup = centered_rect(area, 60, 50);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.help_title)
        .title(Span::styled(format!(" {} ", state.title), theme.help_title));
    let lines: Vec<Line> = state
        .lines
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), theme.text)))
        .collect();
    let para = Paragraph::new(lines).block(block).style(theme.help);
    frame.render_widget(para, popup);
}

/// Render the help panel listing the keybindings. Fills the entire pane
/// (rather than floating) since the sidebar is typically narrow; any key
/// dismisses it and restores the previous view.
pub fn render_help(frame: &mut Frame, area: Rect, theme: &Theme) {
    frame.render_widget(Clear, area);
    // Paint the whole pane with the help background first.
    frame.render_widget(Block::default().style(theme.help), area);

    // Inset the content: left/top padding so it isn't flush against the border,
    // and reserve the bottom line for the "press any key" footer.
    let pad_left = 2u16;
    let inner_x = area.x + pad_left;
    let inner_w = area.width.saturating_sub(pad_left + 1);
    let body = Rect {
        x: inner_x,
        y: area.y + 1,
        width: inner_w,
        height: area.height.saturating_sub(3),
    };
    let footer = Rect {
        x: inner_x,
        y: area.y + area.height.saturating_sub(1),
        width: inner_w,
        height: 1,
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled("treelix — keybindings", theme.help_title)));
    lines.push(Line::from(""));

    // Two-column layout: a fixed-width key column on the left and descriptions
    // on the right that wrap *within their own column* instead of spilling back
    // under the keybinding. We pre-wrap each description to the right column's
    // width and print the key only on the first row (blank cell on the rest),
    // so we don't rely on paragraph wrapping (which would reset to column 0).
    let key_col = HELP_ENTRIES
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(8)
        + 1; // one-space gutter between the columns
    let desc_w = (body.width as usize).saturating_sub(key_col).max(1);

    for (key, desc) in HELP_ENTRIES {
        for (i, seg) in wrap_words(desc, desc_w).into_iter().enumerate() {
            let key_cell = if i == 0 {
                format!("{key:<key_col$}")
            } else {
                " ".repeat(key_col)
            };
            lines.push(Line::from(vec![
                Span::styled(key_cell, Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(seg, theme.text),
            ]));
        }
    }

    // Descriptions are already wrapped to fit, so render without paragraph wrap.
    let body_para = Paragraph::new(lines).style(theme.help);
    frame.render_widget(body_para, body);

    let footer_para = Paragraph::new(Line::from(Span::styled(
        "press any key to close",
        theme.indent_marker,
    )))
    .style(theme.help);
    frame.render_widget(footer_para, footer);
}

/// Greedy word-wrap `text` into segments no wider than `width` columns. A word
/// longer than `width` is placed on its own line (it may overflow), which is
/// fine for the short help descriptions. Always returns at least one segment.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub const HELP_ENTRIES: &[(&str, &str)] = &[
    ("j / k", "down / up"),
    ("K / J", "first / last sibling"),
    ("> / <", "next / prev sibling"),
    ("<CR> o", "open file / toggle dir"),
    ("l / h", "expand / collapse · parent"),
    ("P", "move cursor to parent"),
    ("C-]", "cd into dir (re-root)"),
    ("-", "re-root to parent"),
    ("E / W", "expand all / collapse all"),
    ("L", "toggle group-empty dirs"),
    ("]c [c", "next / prev git change"),
    ("<Tab>", "preview in Helix (no focus)"),
    ("C-v C-x", "open in vsplit / hsplit"),
    ("s", "system open"),
    ("a", "create (trailing / = dir)"),
    ("d <Del>", "delete (confirm)"),
    ("D", "trash"),
    ("r e u", "rename / basename / full-path"),
    ("C-r", "rename omit filename"),
    ("x c p", "cut / copy / paste"),
    ("y Y gy", "copy name / relpath / abspath"),
    ("C-k", "file info"),
    ("m", "toggle bookmark"),
    ("bd bt bmv", "bulk delete / trash / move"),
    ("v", "select node (multi-select)"),
    ("f / F", "live filter (files; E first for all) / clear"),
    ("Esc", "clear filter · selection · pending"),
    ("S", "search node"),
    (".", "toggle hidden + git-ignored"),
    ("C", "toggle git-clean (changed only)"),
    ("U / B / M", "custom / no-buffer / no-bookmark"),
    ("R", "refresh"),
    ("? g?", "this help"),
    ("q", "quit"),
];

fn bottom_popup(area: Rect, height: u16) -> Rect {
    let h = height.min(area.height);
    Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(h),
        width: area.width,
        height: h,
    }
}

fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_words_keeps_segments_within_width() {
        let segs = wrap_words("live filter (files; E first for all) / clear", 12);
        assert!(segs.len() > 1, "long desc should wrap to multiple rows");
        for s in &segs {
            assert!(s.chars().count() <= 12, "segment {s:?} exceeds width");
        }
        // No information is lost: joining segments reproduces the words.
        assert_eq!(
            segs.join(" ").split_whitespace().collect::<Vec<_>>(),
            "live filter (files; E first for all) / clear"
                .split_whitespace()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn wrap_words_short_desc_is_single_segment() {
        assert_eq!(wrap_words("down / up", 40), vec!["down / up".to_string()]);
    }

    #[test]
    fn wrap_words_long_word_gets_its_own_line() {
        // A single token wider than the column is placed alone rather than dropped.
        let segs = wrap_words("supercalifragilistic word", 8);
        assert_eq!(segs[0], "supercalifragilistic");
        assert_eq!(segs[1], "word");
    }
}
