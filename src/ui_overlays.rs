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
    /// Byte offset of the insertion point within `buffer` (always on a char boundary).
    pub cursor: usize,
}

impl InputState {
    /// Create an input prompt with the cursor positioned at the end of `buffer`.
    pub fn new(prompt: impl Into<String>, buffer: String, kind: InputKind) -> Self {
        let cursor = buffer.len();
        Self {
            prompt: prompt.into(),
            buffer,
            kind,
            cursor,
        }
    }

    /// Insert a character at the cursor and advance past it.
    pub fn insert(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the character before the cursor (Backspace).
    pub fn backspace(&mut self) {
        if let Some(c) = self.buffer[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.buffer.remove(self.cursor);
        }
    }

    /// Delete the character at the cursor (Delete).
    pub fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
        }
    }

    /// Move the cursor one character left.
    pub fn left(&mut self) {
        if let Some(c) = self.buffer[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    /// Move the cursor one character right.
    pub fn right(&mut self) {
        if let Some(c) = self.buffer[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    /// Move the cursor to the start of the buffer.
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the buffer.
    pub fn end(&mut self) {
        self.cursor = self.buffer.len();
    }
}

#[derive(Debug, Clone)]
pub enum InputKind {
    /// Create at a path resolved against the tree root (the buffer is prefilled
    /// with the editable base directory; trailing `/` = directory).
    Create,
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
    // Block cursor: draw the character at the insertion point as a reverse-
    // video block in the prompt's accent (orange in nord-aurora), matching the
    // input field. At end-of-input there's no character there, so render a
    // trailing space as the block.
    let cursor_style = theme.prompt.add_modifier(Modifier::REVERSED);
    let (before, rest) = state.buffer.split_at(state.cursor);
    let mut spans = vec![Span::styled(before, theme.text)];
    // Display width of the cursor block itself (1 for the trailing-space
    // block at end-of-input, 2 for a wide char under the cursor).
    let cursor_cell;
    match rest.chars().next() {
        Some(c) => {
            let len = c.len_utf8();
            let span = Span::styled(&rest[..len], cursor_style);
            cursor_cell = span.width().max(1);
            spans.push(span);
            spans.push(Span::styled(&rest[len..], theme.text));
        }
        None => {
            cursor_cell = 1;
            spans.push(Span::styled(" ", cursor_style));
        }
    }
    // Horizontal scroll: keep the cursor block in view. Paragraph clips at
    // the right border, so without this a name longer than the (narrow
    // sidebar) popup pushes the insertion point off-screen and you type
    // blind. Scroll just enough that the cursor sits at the right edge.
    //
    // The offset must land on a character-cell boundary: asked to start
    // mid-way through a double-width char, ratatui's truncator renders the
    // whole char instead, shifting the line right one cell and clipping the
    // cursor block off the border again. Round up to the next boundary.
    let inner_width = popup.width.saturating_sub(2) as usize; // borders
    let cursor_col = Span::raw(before).width();
    let desired = (cursor_col + cursor_cell).saturating_sub(inner_width);
    let mut scroll = 0usize;
    for c in before.chars() {
        if scroll >= desired {
            break;
        }
        let mut buf = [0u8; 4];
        scroll += Span::raw(&*c.encode_utf8(&mut buf)).width();
    }
    let line = Line::from(spans);
    let para = Paragraph::new(line)
        .block(block)
        .scroll((0, u16::try_from(scroll).unwrap_or(u16::MAX)));
    frame.render_widget(para, popup);
}

#[cfg(test)]
mod input_scroll_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render an input prompt into a width x 3 frame; return the popup's
    /// content row text and whether the reverse-video cursor block is visible.
    fn render_row(width: u16, state: &InputState) -> (String, bool) {
        let backend = TestBackend::new(width, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|f| render_input(f, f.area(), &theme, state))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row: String = (0..width)
            .map(|x| buf.cell((x, 1)).unwrap().symbol().to_string())
            .collect();
        let cursor_visible = (0..width).any(|x| {
            buf.cell((x, 1))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        });
        (row, cursor_visible)
    }

    #[test]
    fn short_input_renders_from_start() {
        let state = InputState::new("New file", "notes.md".into(), InputKind::Create);
        let (row, cursor_visible) = render_row(20, &state);
        assert!(row.contains("notes.md"), "row was: {row:?}");
        assert!(cursor_visible);
    }

    #[test]
    fn long_input_scrolls_to_keep_cursor_visible() {
        // 29 chars in a popup with 18 inner columns: without horizontal
        // scroll the tail (and the end-of-input cursor) would be clipped at
        // the right border and typing would be invisible.
        let state = InputState::new(
            "New file",
            "src/deeply/nested/longname.rs".into(),
            InputKind::Create,
        );
        let (row, cursor_visible) = render_row(20, &state);
        assert!(
            row.contains("longname.rs"),
            "tail near the cursor must be visible, row was: {row:?}"
        );
        assert!(
            !row.contains("src/"),
            "start should have scrolled out of view, row was: {row:?}"
        );
        assert!(cursor_visible);
    }

    #[test]
    fn cursor_moved_to_start_scrolls_back_to_show_beginning() {
        let mut state = InputState::new(
            "Rename",
            "src/deeply/nested/longname.rs".into(),
            InputKind::Rename {
                path: PathBuf::from("x"),
            },
        );
        state.cursor = 0;
        let (row, _) = render_row(20, &state);
        assert!(
            row.contains("src/deeply"),
            "with the cursor at the start the beginning must be visible, row was: {row:?}"
        );
    }

    #[test]
    fn ascii_cursor_visible_at_every_width_and_length() {
        for width in 5..=24u16 {
            for n in 1..=40usize {
                let text: String = "abcdefghij".chars().cycle().take(n).collect();
                let state = InputState::new("New", text, InputKind::Create);
                let (row, cursor_visible) = render_row(width, &state);
                assert!(cursor_visible, "cursor clipped: width={width} n={n} row={row:?}");
            }
        }
    }

    #[test]
    fn wide_char_cursor_visible_at_every_width_and_length() {
        // Double-width chars exercise the boundary-alignment path: an
        // unaligned scroll offset makes ratatui shift the line one cell
        // right and clip the cursor block.
        for width in 6..=24u16 {
            for n in 1..=14usize {
                let text: String = "日本語のファイル名です前".chars().cycle().take(n).collect();
                let state = InputState::new("New", text.clone(), InputKind::Create);
                let (row, cursor_visible) = render_row(width, &state);
                assert!(
                    cursor_visible,
                    "cursor clipped: width={width} n={n} buffer={text:?} row={row:?}"
                );
            }
        }
    }
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
