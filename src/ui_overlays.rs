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
    Help,
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

/// Render the help panel listing the keybindings.
pub fn render_help(frame: &mut Frame, area: Rect, theme: &Theme) {
    let popup = centered_rect(area, 64, 90);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.help_title)
        .title(Span::styled(" treelix — keybindings ", theme.help_title));

    let mut lines: Vec<Line> = Vec::new();
    for (key, desc) in HELP_ENTRIES {
        lines.push(Line::from(vec![
            Span::styled(format!("{key:<8}"), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(*desc, theme.text),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "press any key to close",
        theme.indent_marker,
    )));

    let para = Paragraph::new(lines).block(block).style(theme.help);
    frame.render_widget(para, popup);
}

pub const HELP_ENTRIES: &[(&str, &str)] = &[
    ("j / k", "down / up"),
    ("K / J", "first / last sibling"),
    ("<CR> o", "open file / toggle dir"),
    ("l", "expand dir"),
    ("h <BS>", "collapse / parent"),
    ("P", "move cursor to parent"),
    ("C-]", "cd into dir (re-root)"),
    ("-", "re-root to parent"),
    ("E / W", "expand all / collapse all"),
    ("<Tab>", "preview in Helix (no focus)"),
    ("C-v C-x", "open in vsplit / hsplit"),
    ("s", "system open"),
    ("a", "create (trailing / = dir)"),
    ("d <Del>", "delete (confirm)"),
    ("D", "trash"),
    ("r / e", "rename / rename basename"),
    ("x c p", "cut / copy / paste"),
    ("y Y gy", "copy name / relpath / abspath"),
    (". / I", "toggle hidden / git-ignored"),
    ("R", "refresh"),
    ("g?", "this help"),
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
