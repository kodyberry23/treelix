//! Application state and the main event loop.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver, Sender};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListState, Paragraph};
use ratatui::Terminal;

use crate::clipboard::{ClipOp, Clipboard};
use crate::config::Config;
use crate::editor::{self, OpenMode};
use crate::git::{self, GitData};
use crate::keymap::{self, Action};
use crate::render::{self, RenderOpts};
use crate::theme::Theme;
use crate::tree::{Row, Tree};
use crate::ui_overlays::{
    self, ConfirmKind, ConfirmState, InputKind, InputState, Overlay,
};
use crate::{ipc, watcher};

/// Events that drive the loop, multiplexed onto one channel.
pub enum AppEvent {
    Key(KeyEvent),
    Redraw,
    Fs,
    Git(GitData),
    Reveal(PathBuf),
}

pub struct App {
    tree: Tree,
    rows: Vec<Row>,
    list_state: ListState,
    theme: Theme,
    config: Config,
    clipboard: Clipboard,
    overlay: Overlay,
    pending_g: bool,
    git: GitData,
    status: Option<String>,
    should_quit: bool,

    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,

    // Kept alive for the duration of the run.
    _watcher: Option<notify::RecommendedWatcher>,
    _socket: Option<ipc::SocketGuard>,
}

impl App {
    pub fn new(root: PathBuf, config: Config, theme: Theme) -> App {
        let mut tree = Tree::new(root.clone());
        tree.show_hidden = config.show_hidden;
        tree.show_ignored = config.show_ignored;

        let (tx, rx) = unbounded();

        // File watcher → Fs events.
        let (fs_tx, fs_rx) = unbounded::<()>();
        let watcher = watcher::watch(root.clone(), fs_tx);
        {
            let tx = tx.clone();
            thread::spawn(move || {
                while fs_rx.recv().is_ok() {
                    if tx.send(AppEvent::Fs).is_err() {
                        break;
                    }
                }
            });
        }

        // Reveal IPC socket → Reveal events.
        let (rev_tx, rev_rx) = unbounded::<PathBuf>();
        let socket = ipc::serve(rev_tx);
        {
            let tx = tx.clone();
            thread::spawn(move || {
                while let Ok(p) = rev_rx.recv() {
                    if tx.send(AppEvent::Reveal(p)).is_err() {
                        break;
                    }
                }
            });
        }

        let mut app = App {
            tree,
            rows: Vec::new(),
            list_state: ListState::default(),
            theme,
            config,
            clipboard: Clipboard::default(),
            overlay: Overlay::None,
            pending_g: false,
            git: GitData::default(),
            status: None,
            should_quit: false,
            tx,
            rx,
            _watcher: watcher,
            _socket: socket,
        };
        app.refresh_rows(None);
        app.list_state.select(Some(0));
        app.spawn_git();
        app
    }

    /// Spawn the input-reading thread, then run the event loop.
    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        // Input thread.
        {
            let tx = self.tx.clone();
            thread::spawn(move || loop {
                match event::read() {
                    Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                        if tx.send(AppEvent::Key(k)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Resize(..)) => {
                        let _ = tx.send(AppEvent::Redraw);
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            });
        }

        loop {
            self.draw(terminal)?;
            match self.rx.recv() {
                Ok(ev) => self.handle_event(ev),
                Err(_) => break,
            }
            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Key(k) => self.on_key(k),
            AppEvent::Redraw => {}
            AppEvent::Fs => self.reload_from_disk(),
            AppEvent::Git(data) => {
                self.git = data;
                self.tree.apply_git(&self.git);
                self.refresh_rows(self.selected_path());
            }
            AppEvent::Reveal(path) => self.reveal(&path),
        }
    }

    // ── Input ─────────────────────────────────────────────────────────────

    fn on_key(&mut self, key: KeyEvent) {
        // Overlays consume input.
        match &self.overlay {
            Overlay::Input(_) => return self.on_input_key(key),
            Overlay::Confirm(_) => return self.on_confirm_key(key),
            Overlay::Help => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::None => {}
        }

        if key.code == KeyCode::Esc {
            self.pending_g = false;
            self.status = None;
            return;
        }

        let (action, pending_g) = keymap::resolve(key, self.pending_g);
        self.pending_g = pending_g;
        if action != Action::None {
            self.dispatch(action);
        }
    }

    fn dispatch(&mut self, action: Action) {
        self.status = None;
        match action {
            Action::Quit => self.should_quit = true,
            Action::Down => self.move_selection(1),
            Action::Up => self.move_selection(-1),
            Action::FirstSibling => self.jump_sibling(true),
            Action::LastSibling => self.jump_sibling(false),
            Action::OpenOrToggle => self.open_or_toggle(),
            Action::Expand => self.expand_current(),
            Action::CollapseOrParent => self.collapse_or_parent(),
            Action::CursorParent => self.cursor_parent(),
            Action::CdInto => self.cd_into(),
            Action::RootParent => self.root_parent(),
            Action::ExpandAll => {
                self.tree.expand_all();
                self.tree.apply_git(&self.git);
                self.refresh_rows(self.selected_path());
            }
            Action::CollapseAll => {
                self.tree.collapse_all();
                self.refresh_rows(self.selected_path());
            }
            Action::Preview => {
                if let Some(row) = self.current_row() {
                    if !row.kind.is_dir() {
                        editor::preview(&row.path);
                    }
                }
            }
            Action::VSplit => self.open_mode(OpenMode::VSplit),
            Action::HSplit => self.open_mode(OpenMode::HSplit),
            Action::SystemOpen => {
                if let Some(row) = self.current_row() {
                    editor::system_open(&row.path);
                }
            }
            Action::Create => self.start_create(),
            Action::Delete => self.start_confirm_delete(false),
            Action::Trash => self.start_confirm_delete(true),
            Action::Rename => self.start_rename(false),
            Action::RenameBasename => self.start_rename(true),
            Action::Cut => self.clip(ClipOp::Cut),
            Action::Copy => self.clip(ClipOp::Copy),
            Action::Paste => self.paste(),
            Action::CopyFilename => self.copy_path_kind(PathKind::Filename),
            Action::CopyRelpath => self.copy_path_kind(PathKind::Relative),
            Action::CopyAbspath => self.copy_path_kind(PathKind::Absolute),
            Action::ToggleHidden => {
                self.tree.show_hidden = !self.tree.show_hidden;
                self.refresh_rows(self.selected_path());
                self.status = Some(format!(
                    "hidden files: {}",
                    if self.tree.show_hidden { "shown" } else { "hidden" }
                ));
            }
            Action::ToggleIgnored => {
                self.tree.show_ignored = !self.tree.show_ignored;
                self.refresh_rows(self.selected_path());
                self.status = Some(format!(
                    "git-ignored: {}",
                    if self.tree.show_ignored { "shown" } else { "hidden" }
                ));
            }
            Action::Refresh => {
                self.reload_from_disk();
                self.status = Some("refreshed".into());
            }
            Action::Help => self.overlay = Overlay::Help,
            Action::None => {}
        }
    }

    // ── Overlay input ─────────────────────────────────────────────────────

    fn on_input_key(&mut self, key: KeyEvent) {
        let Overlay::Input(state) = &mut self.overlay else { return };
        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Enter => {
                let state = state.clone();
                self.overlay = Overlay::None;
                self.submit_input(state);
            }
            KeyCode::Backspace => {
                state.buffer.pop();
            }
            KeyCode::Char(c) => state.buffer.push(c),
            _ => {}
        }
    }

    fn on_confirm_key(&mut self, key: KeyEvent) {
        let Overlay::Confirm(state) = &self.overlay else { return };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let kind = state.kind.clone();
                self.overlay = Overlay::None;
                self.run_confirm(kind);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.overlay = Overlay::None;
            }
            _ => {}
        }
    }

    fn submit_input(&mut self, state: InputState) {
        let name = state.buffer.trim().to_string();
        if name.is_empty() {
            return;
        }
        match state.kind {
            InputKind::Create { dir } => {
                let is_dir = name.ends_with('/');
                let clean = name.trim_end_matches('/');
                let target = dir.join(clean);
                match crate::tree::ops::create(&target, is_dir) {
                    Ok(()) => {
                        self.reload_from_disk();
                        self.reveal(&target);
                        self.status = Some(format!("created {clean}"));
                    }
                    Err(e) => self.status = Some(format!("create failed: {e}")),
                }
            }
            InputKind::Rename { path } => {
                self.do_rename(&path, &name);
            }
            InputKind::RenameBasename { path } => {
                // `name` is the new stem; re-append the original extension.
                let ext = extension(&file_name(&path));
                let final_name = if ext.is_empty() {
                    name
                } else {
                    format!("{name}.{ext}")
                };
                self.do_rename(&path, &final_name);
            }
        }
    }

    fn do_rename(&mut self, path: &Path, new_name: &str) {
        let parent = path.parent().unwrap_or(Path::new("/"));
        let target = parent.join(new_name);
        match crate::tree::ops::rename(path, &target) {
            Ok(()) => {
                self.reload_from_disk();
                self.reveal(&target);
                self.status = Some(format!("renamed to {new_name}"));
            }
            Err(e) => self.status = Some(format!("rename failed: {e}")),
        }
    }

    fn run_confirm(&mut self, kind: ConfirmKind) {
        let (path, result) = match kind {
            ConfirmKind::Delete(p) => {
                let r = crate::tree::ops::remove(&p);
                (p, r)
            }
            ConfirmKind::Trash(p) => {
                let r = crate::tree::ops::trash(&p);
                (p, r)
            }
        };
        match result {
            Ok(()) => {
                self.reload_from_disk();
                self.status = Some(format!("removed {}", file_name(&path)));
            }
            Err(e) => self.status = Some(format!("remove failed: {e}")),
        }
    }

    // ── Actions ───────────────────────────────────────────────────────────

    fn open_or_toggle(&mut self) {
        let Some(row) = self.current_row().cloned() else { return };
        if row.kind.is_dir() {
            self.tree.toggle(&row.path);
            self.tree.apply_git(&self.git);
            self.refresh_rows(Some(row.path));
        } else {
            editor::open(&row.path, OpenMode::Open, &self.config);
        }
    }

    fn open_mode(&mut self, mode: OpenMode) {
        if let Some(row) = self.current_row() {
            if !row.kind.is_dir() {
                editor::open(&row.path, mode, &self.config);
            }
        }
    }

    fn expand_current(&mut self) {
        let Some(row) = self.current_row().cloned() else { return };
        if row.kind.is_dir() && !row.expanded {
            self.tree.expand(&row.path);
            self.tree.apply_git(&self.git);
            self.refresh_rows(Some(row.path));
        } else if row.kind.is_dir() {
            self.move_selection(1);
        }
    }

    fn collapse_or_parent(&mut self) {
        let Some(row) = self.current_row().cloned() else { return };
        if row.kind.is_dir() && row.expanded {
            self.tree.collapse(&row.path);
            self.refresh_rows(Some(row.path));
        } else if let Some(parent) = row.path.parent() {
            // Select the parent row, if visible.
            if parent != self.tree.root.path {
                self.select_path(parent);
            }
        }
    }

    fn cursor_parent(&mut self) {
        if let Some(row) = self.current_row().cloned() {
            if let Some(parent) = row.path.parent() {
                self.select_path(parent);
            }
        }
    }

    fn cd_into(&mut self) {
        if let Some(row) = self.current_row().cloned() {
            if row.kind.is_dir() {
                self.set_root(row.path);
            }
        }
    }

    fn root_parent(&mut self) {
        if let Some(parent) = self.tree.root.path.parent().map(Path::to_path_buf) {
            let old_root = self.tree.root.path.clone();
            self.set_root(parent);
            self.select_path(&old_root);
        }
    }

    fn set_root(&mut self, path: PathBuf) {
        self.tree.set_root(path);
        self.tree.apply_git(&self.git);
        self.refresh_rows(None);
        self.list_state.select(Some(0));
        self.spawn_git();
    }

    fn start_create(&mut self) {
        let dir = self.current_dir_context();
        self.overlay = Overlay::Input(InputState {
            prompt: format!(" create in {}/ ", shorten(&dir)),
            buffer: String::new(),
            kind: InputKind::Create { dir },
        });
    }

    fn start_rename(&mut self, basename_only: bool) {
        let Some(row) = self.current_row().cloned() else { return };
        let (buffer, kind) = if basename_only {
            (
                stem(&row.name),
                InputKind::RenameBasename { path: row.path.clone() },
            )
        } else {
            (row.name.clone(), InputKind::Rename { path: row.path.clone() })
        };
        self.overlay = Overlay::Input(InputState {
            prompt: " rename ".into(),
            buffer,
            kind,
        });
    }

    fn start_confirm_delete(&mut self, trash: bool) {
        let Some(row) = self.current_row().cloned() else { return };
        let verb = if trash { "trash" } else { "delete" };
        let kind = if trash {
            ConfirmKind::Trash(row.path.clone())
        } else {
            ConfirmKind::Delete(row.path.clone())
        };
        self.overlay = Overlay::Confirm(ConfirmState {
            prompt: format!("{verb} {}?", row.name),
            kind,
        });
    }

    fn clip(&mut self, op: ClipOp) {
        if let Some(row) = self.current_row().cloned() {
            self.clipboard.set(op, vec![row.path.clone()]);
            self.status = Some(format!(
                "{} {}",
                if op == ClipOp::Cut { "cut" } else { "copied" },
                row.name
            ));
            self.refresh_rows(self.selected_path());
        }
    }

    fn paste(&mut self) {
        if self.clipboard.is_empty() {
            self.status = Some("clipboard empty".into());
            return;
        }
        let dest = self.current_dir_context();
        let op = self.clipboard.op;
        let paths = self.clipboard.paths.clone();
        let mut last = None;
        for src in &paths {
            let target = crate::tree::ops::paste_target(&dest, src);
            let res = match op {
                Some(ClipOp::Cut) => crate::tree::ops::rename(src, &target),
                _ => crate::tree::ops::copy(src, &target),
            };
            match res {
                Ok(()) => last = Some(target),
                Err(e) => self.status = Some(format!("paste failed: {e}")),
            }
        }
        if op == Some(ClipOp::Cut) {
            self.clipboard.clear();
        }
        self.reload_from_disk();
        if let Some(t) = last {
            self.reveal(&t);
        }
    }

    fn copy_path_kind(&mut self, kind: PathKind) {
        let Some(row) = self.current_row().cloned() else { return };
        let text = match kind {
            PathKind::Filename => row.name.clone(),
            PathKind::Relative => row
                .path
                .strip_prefix(&self.tree.root.path)
                .unwrap_or(&row.path)
                .to_string_lossy()
                .into_owned(),
            PathKind::Absolute => row.path.to_string_lossy().into_owned(),
        };
        copy_to_clipboard(&text);
        self.status = Some(format!("yanked: {text}"));
    }

    // ── Reveal / reload ───────────────────────────────────────────────────

    fn reveal(&mut self, path: &Path) {
        // Resolve symlinks so the path matches the canonicalized tree root
        // (e.g. macOS /tmp -> /private/tmp). Falls back to the raw path if the
        // target doesn't exist.
        let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if self.tree.reveal(&resolved) {
            self.tree.apply_git(&self.git);
            self.refresh_rows(None);
            self.select_path(&resolved);
        }
    }

    fn reload_from_disk(&mut self) {
        let expanded = self.tree.collect_expanded();
        let sel = self.selected_path();
        self.tree.reload_preserving(&expanded);
        self.tree.apply_git(&self.git);
        self.refresh_rows(sel);
        self.spawn_git();
    }

    fn spawn_git(&self) {
        let root = self.tree.root.path.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let data = git::scan(&root);
            let _ = tx.send(AppEvent::Git(data));
        });
    }

    // ── Selection / rows ──────────────────────────────────────────────────

    fn refresh_rows(&mut self, preserve: Option<PathBuf>) {
        self.rows = self.tree.flatten();
        let idx = preserve
            .and_then(|p| self.rows.iter().position(|r| r.path == p))
            .or_else(|| self.list_state.selected())
            .unwrap_or(0);
        let clamped = idx.min(self.rows.len().saturating_sub(1));
        self.list_state.select(if self.rows.is_empty() {
            None
        } else {
            Some(clamped)
        });
    }

    fn current_row(&self) -> Option<&Row> {
        self.list_state.selected().and_then(|i| self.rows.get(i))
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.current_row().map(|r| r.path.clone())
    }

    /// Directory context for create/paste: the selected dir, else its parent.
    fn current_dir_context(&self) -> PathBuf {
        match self.current_row() {
            Some(row) if row.kind.is_dir() => row.path.clone(),
            Some(row) => row
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.tree.root.path.clone()),
            None => self.tree.root.path.clone(),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        let max = self.rows.len() as isize - 1;
        let next = (cur + delta).clamp(0, max) as usize;
        self.list_state.select(Some(next));
    }

    fn select_path(&mut self, path: &Path) {
        if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.list_state.select(Some(i));
        }
    }

    fn jump_sibling(&mut self, first: bool) {
        let Some(cur) = self.current_row().cloned() else { return };
        let parent = cur.path.parent();
        let mut indices: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.depth == cur.depth && r.path.parent() == parent)
            .map(|(i, _)| i)
            .collect();
        if first {
            if let Some(&i) = indices.first() {
                self.list_state.select(Some(i));
            }
        } else if let Some(&i) = indices.last() {
            self.list_state.select(Some(i));
        }
        indices.clear();
    }

    // ── Rendering ─────────────────────────────────────────────────────────

    fn draw<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        terminal.draw(|frame| {
            let area = frame.area();
            // Base background (transparent by default → terminal/Ghostty shows through).
            frame.render_widget(
                ratatui::widgets::Block::default().style(self.theme.background),
                area,
            );
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // header
                    Constraint::Min(1),    // tree
                    Constraint::Length(1), // status
                ])
                .split(area);

            // Header.
            frame.render_widget(
                Paragraph::new(render::root_header(&self.tree.root.path, &self.theme)),
                chunks[0],
            );

            // Tree list.
            let opts = RenderOpts {
                icons_enabled: self.config.icons,
                show_arrows: false,
            };
            let items = render::build_items(&self.rows, &self.theme, &opts, &self.clipboard);
            let list = List::new(items)
                .style(self.theme.text)
                .highlight_style(self.theme.selection);
            frame.render_stateful_widget(list, chunks[1], &mut self.list_state);

            // Status line.
            let status_line = self.status_line();
            frame.render_widget(Paragraph::new(status_line), chunks[2]);

            // Overlays.
            match &self.overlay {
                Overlay::Input(state) => {
                    ui_overlays::render_input(frame, area, &self.theme, state)
                }
                Overlay::Confirm(state) => {
                    ui_overlays::render_confirm(frame, area, &self.theme, state)
                }
                Overlay::Help => ui_overlays::render_help(frame, area, &self.theme),
                Overlay::None => {}
            }
        })?;
        Ok(())
    }

    fn status_line(&self) -> Line<'_> {
        if let Some(msg) = &self.status {
            return Line::from(Span::styled(msg.clone(), self.theme.prompt));
        }
        let git = self
            .git
            .toplevel
            .as_ref()
            .map(|_| " git")
            .unwrap_or("");
        let count = format!("{} items{}", self.rows.len(), git);
        Line::from(Span::styled(count, self.theme.indent_marker))
    }
}

#[derive(Clone, Copy)]
enum PathKind {
    Filename,
    Relative,
    Absolute,
}

fn copy_to_clipboard(text: &str) {
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn shorten(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn stem(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s.to_string(),
        _ => name.to_string(),
    }
}

fn extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => e.to_string(),
        _ => String::new(),
    }
}
