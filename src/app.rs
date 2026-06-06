//! Application state and the main event loop.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::SystemTime;

use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver, Sender};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListState, Paragraph};
use ratatui::Terminal;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};

use crate::clipboard::{ClipOp, Clipboard};
use crate::config::Config;
use crate::editor::{self, OpenMode};
use crate::git::{self, GitData, GitStatus};
use crate::keymap::{self, Action};
use crate::marks::Marks;
use crate::render::{self, Decor, RenderOpts};
use crate::theme::Theme;
use crate::tree::{Row, SortMode, Tree, ViewOptions};
use crate::ui_overlays::{
    self, ConfirmKind, ConfirmState, InfoState, InputKind, InputState, Overlay,
};
use crate::{ipc, watcher};

/// Events that drive the loop, multiplexed onto one channel.
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Redraw,
    Fs,
    Git(GitData),
    Reveal(PathBuf),
}

pub struct App {
    tree: Tree,
    rows: Vec<Row>,
    list_state: ListState,
    list_area: Rect,
    theme: Theme,
    config: Config,
    clipboard: Clipboard,
    marks: Marks,
    selection: HashSet<PathBuf>,
    overlay: Overlay,
    pending: String,
    git: GitData,
    status: Option<String>,
    should_quit: bool,

    // View state
    sort: SortMode,
    files_first: bool,
    group_empty: bool,
    git_clean: bool,
    custom_active: bool,
    no_buffer: bool,
    no_bookmark: bool,
    live_filter: Option<String>,
    live_editing: bool,

    // Helix-aware state
    current_file: Option<PathBuf>,
    opened: HashSet<PathBuf>,

    matcher: Matcher,

    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,

    _watcher: Option<notify::RecommendedWatcher>,
    _socket: Option<ipc::SocketGuard>,
}

impl App {
    pub fn new(root: PathBuf, config: Config, theme: Theme) -> App {
        let mut tree = Tree::new(root.clone());
        tree.show_hidden = config.show_hidden;
        tree.show_ignored = config.show_ignored;
        tree.group_empty = config.group_empty;

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
            list_area: Rect::default(),
            theme,
            clipboard: Clipboard::default(),
            marks: Marks::load(config.bookmarks_persist),
            selection: HashSet::new(),
            overlay: Overlay::None,
            pending: String::new(),
            git: GitData::default(),
            status: None,
            should_quit: false,
            sort: SortMode::parse(&config.sort),
            files_first: config.files_first,
            group_empty: config.group_empty,
            git_clean: false,
            custom_active: false,
            no_buffer: false,
            no_bookmark: false,
            live_filter: None,
            live_editing: false,
            current_file: None,
            opened: HashSet::new(),
            matcher: Matcher::new(NucleoConfig::DEFAULT),
            config,
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

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        {
            let tx = self.tx.clone();
            thread::spawn(move || loop {
                match event::read() {
                    Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                        if tx.send(AppEvent::Key(k)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Mouse(m)) => {
                        let _ = tx.send(AppEvent::Mouse(m));
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
            AppEvent::Mouse(m) => self.on_mouse(m),
            AppEvent::Redraw => {}
            AppEvent::Fs => self.reload_from_disk(),
            AppEvent::Git(data) => {
                self.git = data;
                self.tree.apply_git(&self.git);
                self.refresh_rows(self.selected_path());
            }
            AppEvent::Reveal(path) => {
                // Helix told us its current buffer: mark it, reveal, highlight.
                self.current_file = Some(path.clone());
                self.opened.insert(path.clone());
                self.reveal(&path);
            }
        }
    }

    // ── Input ───────────────────────────────────────────────────────────────

    fn on_key(&mut self, key: KeyEvent) {
        match &self.overlay {
            Overlay::Input(_) => return self.on_input_key(key),
            Overlay::Confirm(_) => return self.on_confirm_key(key),
            Overlay::Info(_) => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::Help => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::None => {}
        }

        // Live-filter editing captures input.
        if self.live_editing {
            return self.on_live_key(key);
        }

        let (action, pending) = keymap::resolve(key, &self.pending);
        self.pending = pending;
        if action != Action::None {
            self.dispatch(action);
        }
    }

    fn on_live_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.live_filter = None;
                self.live_editing = false;
                self.refresh_rows(self.selected_path());
            }
            KeyCode::Enter => {
                self.live_editing = false; // keep the filter, resume nav
            }
            KeyCode::Backspace => {
                if let Some(q) = &mut self.live_filter {
                    q.pop();
                }
                self.refresh_rows(None);
            }
            KeyCode::Char(c) => {
                if let Some(q) = &mut self.live_filter {
                    q.push(c);
                }
                self.refresh_rows(None);
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, action: Action) {
        self.status = None;
        match action {
            Action::Quit => self.should_quit = true,
            Action::Down => self.move_selection(1),
            Action::Up => self.move_selection(-1),
            Action::FirstSibling => self.jump_sibling_edge(true),
            Action::LastSibling => self.jump_sibling_edge(false),
            Action::NextSibling => self.jump_sibling_step(1),
            Action::PrevSibling => self.jump_sibling_step(-1),
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
            Action::NextGit => self.jump_git(1),
            Action::PrevGit => self.jump_git(-1),
            Action::Preview => {
                if let Some(row) = self.current_row() {
                    if !row.kind.is_dir() {
                        let path = row.path.clone();
                        editor::preview(&path);
                        self.mark_current(&path);
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
            Action::Rename => self.start_rename(RenameKind::Basename2Full),
            Action::RenameBasename => self.start_rename(RenameKind::Basename),
            Action::RenameFull => self.start_rename(RenameKind::Full),
            Action::RenameOmitFilename => self.start_rename(RenameKind::OmitFilename),
            Action::Cut => self.clip(ClipOp::Cut),
            Action::Copy => self.clip(ClipOp::Copy),
            Action::Paste => self.paste(),
            Action::CopyFilename => self.copy_path_kind(PathKind::Filename),
            Action::CopyRelpath => self.copy_path_kind(PathKind::Relative),
            Action::CopyAbspath => self.copy_path_kind(PathKind::Absolute),
            Action::FileInfo => self.file_info(),
            Action::ToggleMark => self.toggle_mark(),
            Action::BulkDelete => self.bulk_remove(false),
            Action::BulkTrash => self.bulk_remove(true),
            Action::BulkMove => self.bulk_move(),
            Action::ToggleHidden => self.toggle_filter(Filter::Hidden),
            Action::ToggleIgnored => self.toggle_filter(Filter::Ignored),
            Action::ToggleGitClean => self.toggle_filter(Filter::GitClean),
            Action::ToggleCustom => self.toggle_filter(Filter::Custom),
            Action::ToggleNoBuffer => self.toggle_filter(Filter::NoBuffer),
            Action::ToggleNoBookmark => self.toggle_filter(Filter::NoBookmark),
            Action::ToggleGroupEmpty => self.toggle_group_empty(),
            Action::LiveFilterStart => {
                self.live_filter = Some(String::new());
                self.live_editing = true;
                self.refresh_rows(self.selected_path());
            }
            Action::LiveFilterClear => {
                self.live_filter = None;
                self.live_editing = false;
                self.refresh_rows(self.selected_path());
            }
            Action::SearchNode => {
                self.overlay = Overlay::Input(InputState {
                    prompt: " search ".into(),
                    buffer: String::new(),
                    kind: InputKind::Search,
                });
            }
            Action::Refresh => {
                self.reload_from_disk();
                self.status = Some("refreshed".into());
            }
            Action::Help => self.overlay = Overlay::Help,
            Action::ToggleSelect => self.toggle_select(),
            Action::ClearSelect => {
                if !self.selection.is_empty() {
                    self.selection.clear();
                    self.refresh_rows(self.selected_path());
                }
                self.pending.clear();
                self.status = None;
            }
            Action::None => {}
        }
    }

    // ── Overlay input ─────────────────────────────────────────────────────────

    fn on_input_key(&mut self, key: KeyEvent) {
        let Overlay::Input(state) = &mut self.overlay else {
            return;
        };
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
        let Overlay::Confirm(state) = &self.overlay else {
            return;
        };
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
        let value = state.buffer.trim().to_string();
        match state.kind {
            InputKind::Search => {
                if !value.is_empty() {
                    self.search(&value);
                }
                return;
            }
            _ if value.is_empty() => return,
            _ => {}
        }
        match state.kind {
            InputKind::Create { dir } => {
                let is_dir = value.ends_with('/');
                let clean = value.trim_end_matches('/');
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
            InputKind::Rename { path } => self.do_rename(&path, &value),
            InputKind::RenameBasename { path } => {
                let ext = extension(&file_name(&path));
                let final_name = if ext.is_empty() {
                    value
                } else {
                    format!("{value}.{ext}")
                };
                self.do_rename(&path, &final_name);
            }
            InputKind::RenameFull { path } => {
                // Interpret value relative to the tree root if not absolute.
                let target = if Path::new(&value).is_absolute() {
                    PathBuf::from(&value)
                } else {
                    self.tree.root.path.join(&value)
                };
                match crate::tree::ops::rename(&path, &target) {
                    Ok(()) => {
                        self.reload_from_disk();
                        self.reveal(&target);
                        self.status = Some(format!("moved to {value}"));
                    }
                    Err(e) => self.status = Some(format!("rename failed: {e}")),
                }
            }
            InputKind::Search => {}
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
        let result: std::io::Result<usize> = match kind {
            ConfirmKind::Delete(p) => crate::tree::ops::remove(&p).map(|_| 1),
            ConfirmKind::Trash(p) => crate::tree::ops::trash(&p).map(|_| 1),
            ConfirmKind::BulkDelete(paths) => {
                let n = paths.len();
                self.marks.remove_all(&paths);
                self.selection.clear();
                paths
                    .iter()
                    .try_for_each(|p| crate::tree::ops::remove(p))
                    .map(|_| n)
            }
            ConfirmKind::BulkTrash(paths) => {
                let n = paths.len();
                self.marks.remove_all(&paths);
                self.selection.clear();
                paths
                    .iter()
                    .try_for_each(|p| crate::tree::ops::trash(p))
                    .map(|_| n)
            }
        };
        match result {
            Ok(n) => {
                self.reload_from_disk();
                self.status = Some(format!("removed {n} item(s)"));
            }
            Err(e) => self.status = Some(format!("remove failed: {e}")),
        }
    }

    // ── Open / navigate ───────────────────────────────────────────────────────

    fn open_or_toggle(&mut self) {
        let Some(row) = self.current_row().cloned() else {
            return;
        };
        if row.kind.is_dir() {
            self.tree.toggle(&row.path);
            self.tree.apply_git(&self.git);
            self.refresh_rows(Some(row.path));
        } else {
            editor::open(&row.path, OpenMode::Open, &self.config);
            self.mark_current(&row.path);
        }
    }

    fn open_mode(&mut self, mode: OpenMode) {
        if let Some(row) = self.current_row().cloned() {
            if !row.kind.is_dir() {
                editor::open(&row.path, mode, &self.config);
                self.mark_current(&row.path);
            }
        }
    }

    fn mark_current(&mut self, path: &Path) {
        self.current_file = Some(path.to_path_buf());
        self.opened.insert(path.to_path_buf());
    }

    fn expand_current(&mut self) {
        let Some(row) = self.current_row().cloned() else {
            return;
        };
        if row.kind.is_dir() && !row.expanded {
            self.tree.expand(&row.path);
            self.tree.apply_git(&self.git);
            self.refresh_rows(Some(row.path));
        } else if row.kind.is_dir() {
            self.move_selection(1);
        }
    }

    fn collapse_or_parent(&mut self) {
        let Some(row) = self.current_row().cloned() else {
            return;
        };
        if row.kind.is_dir() && row.expanded {
            self.tree.collapse(&row.path);
            self.refresh_rows(Some(row.path));
        } else if let Some(parent) = row.path.parent() {
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
                self.set_root(row.dir_target().to_path_buf());
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

    // ── File ops ────────────────────────────────────────────────────────────

    fn start_create(&mut self) {
        let dir = self.current_dir_context();
        self.overlay = Overlay::Input(InputState {
            prompt: format!(" create in {}/ ", shorten(&dir)),
            buffer: String::new(),
            kind: InputKind::Create { dir },
        });
    }

    fn start_rename(&mut self, kind: RenameKind) {
        let Some(row) = self.current_row().cloned() else {
            return;
        };
        let (prompt, buffer, ikind) = match kind {
            RenameKind::Basename => (
                " rename basename ",
                stem(&row.name),
                InputKind::RenameBasename {
                    path: row.path.clone(),
                },
            ),
            RenameKind::Basename2Full => (
                " rename ",
                row.name.clone(),
                InputKind::Rename {
                    path: row.path.clone(),
                },
            ),
            RenameKind::Full => {
                let rel = row
                    .path
                    .strip_prefix(&self.tree.root.path)
                    .unwrap_or(&row.path)
                    .to_string_lossy()
                    .into_owned();
                (
                    " rename full path ",
                    rel,
                    InputKind::RenameFull {
                        path: row.path.clone(),
                    },
                )
            }
            RenameKind::OmitFilename => {
                // Pre-fill the relative directory, keeping the filename fixed.
                let rel_dir = row
                    .path
                    .parent()
                    .and_then(|p| p.strip_prefix(&self.tree.root.path).ok())
                    .map(|p| {
                        let s = p.to_string_lossy();
                        if s.is_empty() {
                            String::new()
                        } else {
                            format!("{s}/")
                        }
                    })
                    .unwrap_or_default();
                let fname = file_name(&row.path);
                (
                    " rename (dir only) ",
                    format!("{rel_dir}{fname}"),
                    InputKind::RenameFull {
                        path: row.path.clone(),
                    },
                )
            }
        };
        self.overlay = Overlay::Input(InputState {
            prompt: prompt.into(),
            buffer,
            kind: ikind,
        });
    }

    fn start_confirm_delete(&mut self, trash: bool) {
        let targets = self.op_targets();
        if targets.is_empty() {
            return;
        }
        let verb = if trash { "trash" } else { "delete" };
        let (prompt, kind) = if targets.len() == 1 {
            let p = targets[0].clone();
            let name = file_name(&p);
            let kind = if trash {
                ConfirmKind::Trash(p)
            } else {
                ConfirmKind::Delete(p)
            };
            (format!("{verb} {name}?"), kind)
        } else {
            let kind = if trash {
                ConfirmKind::BulkTrash(targets.clone())
            } else {
                ConfirmKind::BulkDelete(targets.clone())
            };
            (format!("{verb} {} selected items?", targets.len()), kind)
        };
        self.overlay = Overlay::Confirm(ConfirmState { prompt, kind });
    }

    fn clip(&mut self, op: ClipOp) {
        let targets = self.op_targets();
        if targets.is_empty() {
            return;
        }
        let n = targets.len();
        self.clipboard.set(op, targets);
        self.status = Some(format!(
            "{} {n} item(s)",
            if op == ClipOp::Cut { "cut" } else { "copied" }
        ));
        self.refresh_rows(self.selected_path());
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
        let Some(row) = self.current_row().cloned() else {
            return;
        };
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

    fn file_info(&mut self) {
        let Some(row) = self.current_row().cloned() else {
            return;
        };
        let mut lines = vec![format!("path: {}", row.path.display())];
        if let Ok(meta) = std::fs::symlink_metadata(&row.path) {
            let kind = if meta.is_dir() {
                "directory"
            } else if meta.file_type().is_symlink() {
                "symlink"
            } else {
                "file"
            };
            lines.push(format!("type: {kind}"));
            if meta.is_file() {
                lines.push(format!("size: {}", human_size(meta.len())));
            }
            if let Ok(m) = meta.modified() {
                lines.push(format!("modified: {}", human_ago(m)));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                lines.push(format!("perms: {:o}", meta.permissions().mode() & 0o777));
            }
        }
        if let Some(g) = row.git {
            lines.push(format!("git: {g:?}"));
        }
        if self.marks.contains(&row.path) {
            lines.push("bookmarked: yes".into());
        }
        self.overlay = Overlay::Info(InfoState {
            title: row.name.clone(),
            lines,
        });
    }

    // ── Marks / selection / bulk ──────────────────────────────────────────────

    fn toggle_mark(&mut self) {
        if let Some(row) = self.current_row().cloned() {
            let now = self.marks.toggle(&row.path);
            self.status = Some(format!(
                "{} bookmark: {}",
                if now { "added" } else { "removed" },
                row.name
            ));
            self.refresh_rows(Some(row.path));
        }
    }

    fn toggle_select(&mut self) {
        if let Some(row) = self.current_row().cloned() {
            if !self.selection.insert(row.path.clone()) {
                self.selection.remove(&row.path);
            }
            self.refresh_rows(Some(row.path.clone()));
            self.move_selection(1);
        }
    }

    /// Targets for delete/trash/cut/copy: the visual selection if any, else the
    /// current row.
    fn op_targets(&self) -> Vec<PathBuf> {
        if !self.selection.is_empty() {
            let mut v: Vec<PathBuf> = self.selection.iter().cloned().collect();
            v.sort();
            v
        } else {
            self.selected_path().into_iter().collect()
        }
    }

    fn bulk_remove(&mut self, trash: bool) {
        let paths: Vec<PathBuf> = self.marks.all().iter().cloned().collect();
        if paths.is_empty() {
            self.status = Some("no bookmarks".into());
            return;
        }
        let verb = if trash { "trash" } else { "delete" };
        let kind = if trash {
            ConfirmKind::BulkTrash(paths.clone())
        } else {
            ConfirmKind::BulkDelete(paths.clone())
        };
        self.overlay = Overlay::Confirm(ConfirmState {
            prompt: format!("{verb} {} bookmarked item(s)?", paths.len()),
            kind,
        });
    }

    fn bulk_move(&mut self) {
        let paths: Vec<PathBuf> = self.marks.all().iter().cloned().collect();
        if paths.is_empty() {
            self.status = Some("no bookmarks".into());
            return;
        }
        let dest = self.current_dir_context();
        let mut moved = 0;
        for src in &paths {
            let target = crate::tree::ops::paste_target(&dest, src);
            if crate::tree::ops::rename(src, &target).is_ok() {
                moved += 1;
            }
        }
        self.marks.remove_all(&paths);
        self.reload_from_disk();
        self.status = Some(format!("moved {moved} bookmarked item(s)"));
    }

    // ── Filters ───────────────────────────────────────────────────────────────

    fn toggle_filter(&mut self, f: Filter) {
        let label = match f {
            Filter::Hidden => {
                self.tree.show_hidden = !self.tree.show_hidden;
                ("hidden files", self.tree.show_hidden)
            }
            Filter::Ignored => {
                self.tree.show_ignored = !self.tree.show_ignored;
                ("git-ignored", self.tree.show_ignored)
            }
            Filter::GitClean => {
                self.git_clean = !self.git_clean;
                ("git-clean (changed only)", self.git_clean)
            }
            Filter::Custom => {
                self.custom_active = !self.custom_active;
                ("custom filter", self.custom_active)
            }
            Filter::NoBuffer => {
                self.no_buffer = !self.no_buffer;
                ("open-files only", self.no_buffer)
            }
            Filter::NoBookmark => {
                self.no_bookmark = !self.no_bookmark;
                ("bookmarked only", self.no_bookmark)
            }
        };
        self.status = Some(format!(
            "{}: {}",
            label.0,
            if label.1 { "on" } else { "off" }
        ));
        self.refresh_rows(self.selected_path());
    }

    fn toggle_group_empty(&mut self) {
        self.group_empty = !self.group_empty;
        self.tree.group_empty = self.group_empty;
        if self.group_empty {
            // Chain-expand currently-expanded directories.
            for p in self.tree.collect_expanded() {
                self.tree.expand(&p);
            }
            self.tree.apply_git(&self.git);
        }
        self.status = Some(format!(
            "group empty dirs: {}",
            if self.group_empty { "on" } else { "off" }
        ));
        self.refresh_rows(self.selected_path());
    }

    // ── Search ────────────────────────────────────────────────────────────────

    fn search(&mut self, query: &str) {
        let q = query.to_lowercase();
        let start = self.list_state.selected().map(|i| i + 1).unwrap_or(0);
        let n = self.rows.len();
        for off in 0..n {
            let i = (start + off) % n;
            if self.rows[i].name.to_lowercase().contains(&q) {
                self.list_state.select(Some(i));
                return;
            }
        }
        self.status = Some(format!("no match: {query}"));
    }

    // ── Reveal / reload ─────────────────────────────────────────────────────

    fn reveal(&mut self, path: &Path) {
        if self.tree.reveal(path) {
            self.tree.apply_git(&self.git);
            self.refresh_rows(None);
            self.select_path(path);
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

    // ── Selection / rows ──────────────────────────────────────────────────────

    fn refresh_rows(&mut self, preserve: Option<PathBuf>) {
        // Build restrict sets (kept alive for the flatten borrow).
        let live_query = self.live_filter.clone();
        let live_set = live_query.as_ref().map(|q| self.compute_match_set(q));
        let bookmark_set = if self.no_bookmark {
            Some(self.with_ancestors(self.marks.all()))
        } else {
            None
        };
        let buffer_set = if self.no_buffer {
            Some(self.with_ancestors(&self.opened))
        } else {
            None
        };
        let mut restricts: Vec<&HashSet<PathBuf>> = Vec::new();
        if let Some(s) = &live_set {
            restricts.push(s);
        }
        if let Some(s) = &bookmark_set {
            restricts.push(s);
        }
        if let Some(s) = &buffer_set {
            restricts.push(s);
        }

        let opts = ViewOptions {
            show_hidden: self.tree.show_hidden,
            show_ignored: self.tree.show_ignored,
            git_clean: self.git_clean,
            group_empty: self.group_empty,
            sort: self.sort,
            files_first: self.files_first,
            exclude: &self.config.exclude,
            custom_active: self.custom_active,
            restricts: &restricts,
        };
        self.rows = self.tree.flatten(&opts);

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

    fn compute_match_set(&mut self, query: &str) -> HashSet<PathBuf> {
        let root = self.tree.root.path.clone();
        let mut set = HashSet::new();
        let all = self.tree.all_paths();
        if query.is_empty() {
            for (p, _) in all {
                set.insert(p);
            }
            return set;
        }
        let pat = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        for (path, name) in all {
            if pat
                .score(Utf32Str::new(&name, &mut buf), &mut self.matcher)
                .is_some()
            {
                set.insert(path.clone());
                let mut cur = path.as_path();
                while let Some(parent) = cur.parent() {
                    if parent == root || !parent.starts_with(&root) {
                        break;
                    }
                    set.insert(parent.to_path_buf());
                    cur = parent;
                }
            }
        }
        set
    }

    fn with_ancestors(&self, base: &HashSet<PathBuf>) -> HashSet<PathBuf> {
        let root = &self.tree.root.path;
        let mut set = HashSet::new();
        for p in base {
            if !p.starts_with(root) {
                continue;
            }
            set.insert(p.clone());
            let mut cur = p.as_path();
            while let Some(parent) = cur.parent() {
                if parent == *root || !parent.starts_with(root) {
                    break;
                }
                set.insert(parent.to_path_buf());
                cur = parent;
            }
        }
        set
    }

    fn current_row(&self) -> Option<&Row> {
        self.list_state.selected().and_then(|i| self.rows.get(i))
    }

    fn selected_path(&self) -> Option<PathBuf> {
        self.current_row().map(|r| r.path.clone())
    }

    fn current_dir_context(&self) -> PathBuf {
        match self.current_row() {
            Some(row) if row.kind.is_dir() => row.dir_target().to_path_buf(),
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

    fn sibling_indices(&self) -> (Vec<usize>, usize) {
        let cur = match self.current_row() {
            Some(r) => r,
            None => return (Vec::new(), 0),
        };
        let parent = cur.path.parent().map(Path::to_path_buf);
        let indices: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.depth == cur.depth && r.path.parent().map(Path::to_path_buf) == parent
            })
            .map(|(i, _)| i)
            .collect();
        let cur_idx = self.list_state.selected().unwrap_or(0);
        (indices, cur_idx)
    }

    fn jump_sibling_edge(&mut self, first: bool) {
        let (indices, _) = self.sibling_indices();
        let target = if first {
            indices.first()
        } else {
            indices.last()
        };
        if let Some(&i) = target {
            self.list_state.select(Some(i));
        }
    }

    fn jump_sibling_step(&mut self, delta: isize) {
        let (indices, cur_idx) = self.sibling_indices();
        if let Some(pos) = indices.iter().position(|&i| i == cur_idx) {
            let np = pos as isize + delta;
            if np >= 0 && (np as usize) < indices.len() {
                self.list_state.select(Some(indices[np as usize]));
            }
        }
    }

    fn jump_git(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len();
        let cur = self.list_state.selected().unwrap_or(0);
        for off in 1..=n {
            let i = ((cur as isize + delta * off as isize).rem_euclid(n as isize)) as usize;
            let g = self.rows[i].git;
            if matches!(g, Some(s) if s != GitStatus::Ignored) {
                self.list_state.select(Some(i));
                return;
            }
        }
        self.status = Some("no git changes".into());
    }

    // ── Mouse ─────────────────────────────────────────────────────────────────

    fn on_mouse(&mut self, m: MouseEvent) {
        if !self.config.mouse {
            return;
        }
        match m.kind {
            MouseEventKind::ScrollDown => self.move_selection(1),
            MouseEventKind::ScrollUp => self.move_selection(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.list_area;
                if m.row >= area.y && m.row < area.y + area.height && !self.rows.is_empty() {
                    let offset = self.list_state.offset();
                    let idx = offset + (m.row - area.y) as usize;
                    if idx < self.rows.len() {
                        self.list_state.select(Some(idx));
                        self.open_or_toggle();
                    }
                }
            }
            _ => {}
        }
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

    fn draw<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        terminal.draw(|frame| {
            let area = frame.area();
            frame.render_widget(
                ratatui::widgets::Block::default().style(self.theme.background),
                area,
            );
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(area);

            frame.render_widget(
                Paragraph::new(render::root_header(&self.tree.root.path, &self.theme)),
                chunks[0],
            );

            self.list_area = chunks[1];
            let opts = RenderOpts {
                icons_enabled: self.config.icons,
                show_arrows: self.config.arrows,
            };
            let decor = Decor {
                clipboard: &self.clipboard,
                marks: self.marks.all(),
                selection: &self.selection,
                current_file: self.current_file.as_deref(),
                special_files: &self.config.special_files,
            };
            let items = render::build_items(&self.rows, &self.theme, &opts, &decor);
            let list = List::new(items)
                .style(self.theme.text)
                .highlight_style(self.theme.selection);
            frame.render_stateful_widget(list, chunks[1], &mut self.list_state);

            frame.render_widget(Paragraph::new(self.status_line()), chunks[2]);

            match &self.overlay {
                Overlay::Input(state) => ui_overlays::render_input(frame, area, &self.theme, state),
                Overlay::Confirm(state) => {
                    ui_overlays::render_confirm(frame, area, &self.theme, state)
                }
                Overlay::Info(state) => ui_overlays::render_info(frame, area, &self.theme, state),
                Overlay::Help => ui_overlays::render_help(frame, area, &self.theme),
                Overlay::None => {}
            }
        })?;
        Ok(())
    }

    fn status_line(&self) -> Line<'_> {
        // Live filter takes over the status line while active.
        if let Some(q) = &self.live_filter {
            let cursor = if self.live_editing { "▏" } else { "" };
            return Line::from(vec![
                Span::styled("filter: ", self.theme.filter_prefix),
                Span::styled(q.clone(), self.theme.text),
                Span::styled(cursor, self.theme.prompt),
            ]);
        }
        if let Some(msg) = &self.status {
            return Line::from(Span::styled(msg.clone(), self.theme.prompt));
        }
        let mut parts = vec![format!("{} items", self.rows.len())];
        if !self.selection.is_empty() {
            parts.push(format!("{} sel", self.selection.len()));
        }
        if self.git.toplevel.is_some() {
            parts.push("git".into());
        }
        let mut flags = String::new();
        if self.tree.show_hidden {
            flags.push('.');
        }
        if self.git_clean {
            flags.push('C');
        }
        if self.no_bookmark {
            flags.push('M');
        }
        if self.no_buffer {
            flags.push('B');
        }
        if !flags.is_empty() {
            parts.push(format!("[{flags}]"));
        }
        Line::from(Span::styled(parts.join("  "), self.theme.indent_marker))
    }
}

#[derive(Clone, Copy)]
enum PathKind {
    Filename,
    Relative,
    Absolute,
}

#[derive(Clone, Copy)]
enum Filter {
    Hidden,
    Ignored,
    GitClean,
    Custom,
    NoBuffer,
    NoBookmark,
}

#[derive(Clone, Copy)]
enum RenameKind {
    Basename,
    Basename2Full,
    Full,
    OmitFilename,
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

fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < 4 {
        f /= 1024.0;
        i += 1;
    }
    format!("{f:.1} {}", U[i])
}

fn human_ago(t: SystemTime) -> String {
    match t.elapsed() {
        Ok(d) => {
            let s = d.as_secs();
            if s < 60 {
                format!("{s}s ago")
            } else if s < 3600 {
                format!("{}m ago", s / 60)
            } else if s < 86400 {
                format!("{}h ago", s / 3600)
            } else {
                format!("{}d ago", s / 86400)
            }
        }
        Err(_) => "in the future".into(),
    }
}
