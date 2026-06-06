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
    for (key, desc) in HELP_ENTRIES {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{key:<8}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(*desc, theme.text),
        ]));
    }

    let body_para = Paragraph::new(lines)
        .style(theme.help)
        .wrap(Wrap { trim: false });
    frame.render_widget(body_para, body);

    let footer_para = Paragraph::new(Line::from(Span::styled(
        "press any key to close",
        theme.indent_marker,
    )))
    .style(theme.help);
    frame.render_widget(footer_para, footer);
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
