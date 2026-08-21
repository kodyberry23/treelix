//! Application state and the main event loop.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
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
use crate::diagnostics::{self, DiagnosticsData};
use crate::editor::{self, OpenMode};
use crate::git::{self, AfterScan, GitData, GitStatus, ScanSchedule};
use crate::keymap::{self, Action};
use crate::marks::Marks;
use crate::render::{self, Decor, RenderOpts};
use crate::theme::Theme;
use crate::tree::{Row, SortMode, Tree, ViewOptions};
use crate::ui_overlays::{
    self, ConfirmKind, ConfirmState, HelpState, InfoState, InputKind, InputState, Overlay,
};
use crate::{ipc, watcher};

/// Events that drive the loop, multiplexed onto one channel.
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Redraw,
    Fs(watcher::FsChange),
    /// A status scan finished; `None` when it failed or timed out.
    Git(Option<GitData>),
    Reveal(ipc::Reveal),
    Diagnostics(ipc::DiagnosticsUpdate),
}

/// Result of one live-filter scan of the tree on disk.
struct LiveScan {
    /// Inputs the scan depends on; a cached scan is reused only if they match.
    query: String,
    root: PathBuf,
    show_hidden: bool,
    show_ignored: bool,
    custom_active: bool,
    /// Paths to keep visible: matches, their ancestors, and (with
    /// `live_filter_show_folders`) every directory.
    visible: Rc<HashSet<PathBuf>>,
    /// Directories to expand so every match is on screen: the ancestor chains
    /// of matched entries, shallowest first, deduplicated.
    expand: Vec<PathBuf>,
    /// The walk hit its entry cap; matches beyond it are missing.
    truncated: bool,
}

/// Upper bound on directory entries examined per filter scan, so one keystroke
/// can't stall the UI walking a pathological tree (huge unignored vendor dirs).
const FILTER_WALK_CAP: usize = 50_000;
/// If a query would auto-expand more directories than this, skip the expansion
/// (the restrict filter still applies) — exploding thousands of directories for
/// a one-letter query helps nobody. Typing more letters narrows it under the cap.
const FILTER_EXPAND_CAP: usize = 500;

pub struct App {
    tree: Tree,
    rows: Vec<Row>,
    list_state: ListState,
    list_area: Rect,
    /// Set when the user just expanded the selected directory: the next draw
    /// nudges the offset so its first children are visible even when they
    /// were inserted below the fold (and even at scrolloff = 0, where
    /// scroll_padding alone keeps only the selected row in view).
    reveal_children: bool,
    theme: Theme,
    config: Config,
    clipboard: Clipboard,
    marks: Marks,
    selection: HashSet<PathBuf>,
    overlay: Overlay,
    pending: String,
    git: GitData,
    git_schedule: ScanSchedule,
    diagnostics: DiagnosticsData,
    diagnostics_mode: diagnostics::Mode,
    status: Option<String>,
    // When the transient status message should disappear on its own. Set
    // alongside `status` by set_status(); the event loop wakes at this
    // instant to clear it so a message never lingers while the user is off
    // in the editor and never touches the sidebar again.
    status_deadline: Option<Instant>,
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
    /// Cached result of the last live-filter disk scan, so ordinary row
    /// refreshes (git updates, redraws) don't re-walk the filesystem. Keyed
    /// by the scan inputs; invalidated explicitly on filesystem change.
    live_scan: Option<LiveScan>,
    /// Directories the live filter auto-expanded to surface matches (only those
    /// that were NOT already open when it did so). On clear, exactly these are
    /// collapsed again — so the filter's exploration is undone without
    /// disturbing anything the user had open before, or expanded/collapsed
    /// themselves while the filter was parked (those paths are removed from
    /// this set the moment the user touches them).
    filter_auto_expanded: HashSet<PathBuf>,
    /// True while `true`, expansion toggles are the filter's own doing and must
    /// not be recorded as user actions (prevents the auto-expand loop from
    /// immediately un-tracking what it just tracked).
    filter_expanding: bool,

    // Helix-aware state
    current_file: Option<PathBuf>,
    opened: HashSet<PathBuf>,
    /// Last input that ACTED on the tree, used to defer follow-reveals
    /// (see AppEvent::Reveal). Inert input (unmapped keys, stray clicks,
    /// pointer motion) does not count.
    last_input: Option<Instant>,
    /// A follow-reveal that arrived while the user was driving the tree;
    /// applied by handle_event once the grace window has passed. Helix
    /// pushes each path only once, so a deferred reveal must be kept, not
    /// dropped.
    pending_reveal: Option<PathBuf>,

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

        // File watcher → Fs events (changed-path sets or full-rescan markers).
        let (fs_tx, fs_rx) = unbounded::<watcher::FsChange>();
        let watcher = watcher::watch(root.clone(), fs_tx);
        {
            let tx = tx.clone();
            thread::spawn(move || {
                while let Ok(change) = fs_rx.recv() {
                    if tx.send(AppEvent::Fs(change)).is_err() {
                        break;
                    }
                }
            });
        }

        // IPC socket → Reveal / Diagnostics events.
        let (ipc_tx, ipc_rx) = unbounded::<ipc::Message>();
        let socket = ipc::serve(ipc_tx);
        {
            let tx = tx.clone();
            thread::spawn(move || {
                while let Ok(message) = ipc_rx.recv() {
                    let event = match message {
                        ipc::Message::Reveal(reveal) => AppEvent::Reveal(reveal),
                        ipc::Message::Diagnostics(update) => AppEvent::Diagnostics(update),
                    };
                    if tx.send(event).is_err() {
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
            reveal_children: false,
            theme,
            clipboard: Clipboard::default(),
            marks: Marks::load(config.bookmarks_persist),
            selection: HashSet::new(),
            overlay: Overlay::None,
            pending: String::new(),
            git: GitData::default(),
            git_schedule: ScanSchedule::default(),
            diagnostics: DiagnosticsData::default(),
            diagnostics_mode: diagnostics::Mode::parse(&config.diagnostics).unwrap_or_else(|| {
                eprintln!(
                    "treelix: unknown diagnostics = \"{}\" (off | errors | warnings); using warnings",
                    config.diagnostics
                );
                diagnostics::Mode::Warnings
            }),
            status: None,
            status_deadline: None,
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
            live_scan: None,
            filter_auto_expanded: HashSet::new(),
            filter_expanding: false,
            current_file: None,
            opened: HashSet::new(),
            last_input: None,
            pending_reveal: None,
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

    /// How long a transient status message stays on screen before the
    /// event loop clears it on its own.
    const STATUS_TIMEOUT: Duration = Duration::from_secs(4);

    /// Show a transient status message and arm its auto-clear deadline.
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
        self.status_deadline = Some(Instant::now() + Self::STATUS_TIMEOUT);
    }

    /// Clear the status message and disarm its deadline.
    fn clear_status(&mut self) {
        self.status = None;
        self.status_deadline = None;
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
            // Compute the next self-scheduled wake, if any. Two timers can fire
            // with no incoming event:
            //   - a transient status message that must auto-clear;
            //   - a deferred follow-reveal that must be applied once the input
            //     grace window lapses (otherwise it waits, possibly forever,
            //     for an unrelated event and then applies out of order).
            // Wake at whichever is sooner.
            let wake = self
                .status_deadline
                .into_iter()
                .chain(self.pending_reveal_deadline())
                .min();
            let received = match wake {
                Some(deadline) => self
                    .rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now())),
                None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(ev) => self.handle_event(ev),
                Err(RecvTimeoutError::Timeout) => {
                    // A timer fired. Clear an expired status, and flush a
                    // now-eligible deferred reveal (grace window lapsed).
                    if self.status_deadline.is_some_and(|d| Instant::now() >= d) {
                        self.clear_status();
                    }
                    if self.pending_reveal.is_some() && !self.recent_input() {
                        if let Some(path) = self.pending_reveal.take() {
                            self.reveal(&path);
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    /// When a deferred follow-reveal is pending, the instant its input grace
    /// window lapses (so the run loop can wake and apply it without waiting on
    /// an unrelated event). `None` when nothing is pending.
    fn pending_reveal_deadline(&self) -> Option<Instant> {
        self.pending_reveal.as_ref()?;
        // Wake just past the grace window measured from the last acting input;
        // if no input was recorded, the reveal is already eligible (wake now).
        Some(
            self.last_input
                .map(|t| t + Self::FOLLOW_INPUT_GRACE)
                .unwrap_or_else(Instant::now),
        )
    }

    fn handle_event(&mut self, ev: AppEvent) {
        // Apply a deferred follow-reveal once the user has been idle past
        // the grace window — before the event below is processed, so a
        // returning keypress lands on a tree already synced to Helix's
        // current buffer.
        if self.pending_reveal.is_some() && !self.recent_input() {
            if let Some(path) = self.pending_reveal.take() {
                self.reveal(&path);
            }
        }
        match ev {
            // Input bumps `last_input` inside on_key/on_mouse, and only when
            // it actually acts on the tree: unmapped keys, stray clicks, and
            // passive pointer motion (reported by any-event mouse tracking)
            // must not gate follow-reveals.
            AppEvent::Key(k) => self.on_key(k),
            AppEvent::Mouse(m) => self.on_mouse(m),
            AppEvent::Redraw => {}
            AppEvent::Fs(watcher::FsChange::Paths(paths)) => self.reload_from_paths(&paths),
            // The OS dropped events (FSEvents "must scan subdirs"): the paths
            // we have are incomplete, so re-read every expanded directory.
            AppEvent::Fs(watcher::FsChange::Rescan) => self.reload_from_disk(),
            AppEvent::Git(result) => {
                match self.git_schedule.finished(result.is_some()) {
                    AfterScan::Idle => {}
                    AfterScan::Rerun => self.start_git_scan(Duration::ZERO),
                    AfterScan::Retry(delay) => self.start_git_scan(delay),
                }
                let Some(data) = result else {
                    return;
                };
                // Only invalidate the filter scan when the statuses actually
                // changed. Every fs burst spawns a git scan; if it comes back
                // identical (the common case), re-walking the whole tree for
                // the filter is pure waste and doubles the per-burst cost.
                let changed = self.git.statuses != data.statuses;
                self.git = data;
                self.apply_overlays();
                if changed {
                    // The filter scan prunes on git-ignored status, so cached
                    // results computed against the old statuses are stale.
                    self.live_scan = None;
                }
                if changed && self.live_filter.is_some() {
                    // Re-expand to any matches the new statuses revealed.
                    self.refresh_filtered_view();
                } else {
                    self.refresh_rows(self.selected_path());
                }
            }
            AppEvent::Diagnostics(ipc::DiagnosticsUpdate { path, counts }) => {
                if self.diagnostics_mode == diagnostics::Mode::Off
                    || !self.diagnostics.update(path, counts)
                {
                    return;
                }
                self.apply_overlays();
                if self.live_filter.is_some() {
                    self.refresh_filtered_view();
                } else {
                    self.refresh_rows(self.selected_path());
                }
            }
            AppEvent::Reveal(ipc::Reveal { path, follow }) => {
                // Bring the incoming path into the tree's canonical namespace
                // so the highlight (current_file compared against row.path) and
                // the reveal below both match nodes keyed by canonical paths.
                let path = canonicalize_lenient(&path);
                // Helix told us its current buffer: mark it and highlight
                // (the highlight is styling derived from `current_file`, so
                // the regular post-event redraw picks it up — no row rebuild
                // needed here).
                self.current_file = Some(path.clone());
                self.opened.insert(path.clone());
                // Explicit reveals (A-r / space-f / `treelix reveal`) always
                // act. Automatic follow pushes may arrive as echoes of what
                // this pane just did (Enter-open, Tab-preview) — while the
                // user is driving the tree, defer instead of yanking their
                // cursor and expansion state; handle_event applies the
                // deferred path once they've been idle past the grace window.
                if follow && self.recent_input() {
                    self.pending_reveal = Some(path);
                } else {
                    self.pending_reveal = None;
                    self.reveal(&path);
                }
            }
        }
    }

    /// How long after an acting input a follow-reveal stays deferred.
    const FOLLOW_INPUT_GRACE: Duration = Duration::from_millis(1000);

    /// True while the user is actively driving treelix (last acted-on input
    /// within the grace window). Follow-reveals are deferred during this
    /// window; explicit reveals ignore it.
    fn recent_input(&self) -> bool {
        self.last_input
            .is_some_and(|t| t.elapsed() < Self::FOLLOW_INPUT_GRACE)
    }

    /// Record that the user just acted on the tree (gates follow-reveals).
    fn touch(&mut self) {
        self.last_input = Some(Instant::now());
    }

    // ── Input ───────────────────────────────────────────────────────────────

    fn on_key(&mut self, key: KeyEvent) {
        // Overlay/live-filter keystrokes and keys that resolve to an action
        // (or extend a chord) all count as driving the tree; a key that does
        // nothing (unmapped, dead prefix) must not gate follow-reveals.
        match &self.overlay {
            Overlay::Input(_) => {
                self.touch();
                return self.on_input_key(key);
            }
            Overlay::Confirm(_) => {
                self.touch();
                return self.on_confirm_key(key);
            }
            Overlay::Info(_) => {
                self.touch();
                self.overlay = Overlay::None;
                return;
            }
            Overlay::Help(_) => {
                self.touch();
                self.on_help_key(key);
                return;
            }
            Overlay::None => {}
        }

        // Live-filter editing captures input.
        if self.live_editing {
            self.touch();
            return self.on_live_key(key);
        }

        let (action, pending) = keymap::resolve(key, &self.pending);
        if action != Action::None || !pending.is_empty() {
            self.touch();
        }
        self.pending = pending;
        if action != Action::None {
            self.dispatch(action);
        }
    }

    fn on_help_key(&mut self, key: KeyEvent) {
        // Mirror render_help's geometry: it draws over the FULL pane (list area
        // + the 1-row header above and 1-row status below), insets the body by
        // pad_left+1 = 3 columns, and reserves 3 rows (1 top, 2 bottom). The
        // full pane height is list_area.height + 2, so the body height is
        // (list_area.height + 2) - 3 = list_area.height - 1.
        let body_w = (self.list_area.width as usize).saturating_sub(3).max(1);
        let body_h = self.list_area.height.saturating_sub(1);
        let total = ui_overlays::help_line_count(body_w);
        let max_scroll = total.saturating_sub(body_h);
        let page = body_h.max(1);
        let Overlay::Help(state) = &mut self.overlay else {
            return;
        };
        match key.code {
            // Dismiss.
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {
                self.overlay = Overlay::None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                state.scroll = (state.scroll + 1).min(max_scroll);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                state.scroll = state.scroll.saturating_sub(1);
            }
            KeyCode::Char('d') | KeyCode::PageDown | KeyCode::Char(' ') => {
                state.scroll = (state.scroll + page).min(max_scroll);
            }
            KeyCode::Char('u') | KeyCode::PageUp => {
                state.scroll = state.scroll.saturating_sub(page);
            }
            KeyCode::Char('g') | KeyCode::Home => state.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => state.scroll = max_scroll,
            // Any other key closes, preserving the old "any key dismisses" feel
            // for keys that aren't scroll controls.
            _ => self.overlay = Overlay::None,
        }
    }

    fn on_live_key(&mut self, key: KeyEvent) {
        // Ctrl-C quits from the filter as it does everywhere else; without this
        // the Char('c') arm below would type a literal 'c' into the query and
        // the app could never be quit while editing a filter.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.clear_live_filter();
                self.refresh_rows(self.selected_path());
            }
            KeyCode::Enter => {
                self.live_editing = false; // keep the filter, resume nav
            }
            KeyCode::Backspace => {
                if let Some(q) = &mut self.live_filter {
                    q.pop();
                }
                self.on_live_query_changed();
            }
            // Only unmodified characters extend the query; a Ctrl/Alt chord
            // must not land its base letter in the filter text.
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(q) = &mut self.live_filter {
                    q.push(c);
                }
                self.on_live_query_changed();
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, action: Action) {
        self.clear_status();
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
                // The user rearranged expansion wholesale; the filter no longer
                // owns any of it, so it must not auto-collapse on clear.
                self.filter_auto_expanded.clear();
                self.apply_overlays();
                self.refresh_rows(self.selected_path());
            }
            Action::CollapseAll => {
                self.tree.collapse_all();
                self.filter_auto_expanded.clear();
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
            Action::ToggleGitClean => self.toggle_filter(Filter::GitClean),
            Action::ToggleCustom => self.toggle_filter(Filter::Custom),
            Action::ToggleNoBuffer => self.toggle_filter(Filter::NoBuffer),
            Action::ToggleNoBookmark => self.toggle_filter(Filter::NoBookmark),
            Action::ToggleGroupEmpty => self.toggle_group_empty(),
            Action::LiveFilterStart => self.start_live_filter(),
            Action::LiveFilterClear => {
                self.clear_live_filter();
                self.refresh_rows(self.selected_path());
            }
            Action::SearchNode => {
                self.overlay = Overlay::Input(InputState::new(
                    " search ",
                    String::new(),
                    InputKind::Search,
                ));
            }
            Action::Refresh => {
                self.reload_from_disk();
                self.set_status("refreshed");
            }
            Action::Help => self.overlay = Overlay::Help(HelpState::default()),
            Action::ToggleSelect => self.toggle_select(),
            Action::ClearSelect => {
                // Esc clears, in priority: an active live filter, then a visual
                // selection, then any pending key sequence / status message.
                if self.live_filter.is_some() {
                    self.clear_live_filter();
                    self.refresh_rows(self.selected_path());
                    self.set_status("filter cleared");
                } else if !self.selection.is_empty() {
                    self.selection.clear();
                    self.refresh_rows(self.selected_path());
                    self.pending.clear();
                } else {
                    self.pending.clear();
                    self.clear_status();
                }
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
            KeyCode::Backspace => state.backspace(),
            KeyCode::Delete => state.delete(),
            KeyCode::Left => state.left(),
            KeyCode::Right => state.right(),
            KeyCode::Home => state.home(),
            KeyCode::End => state.end(),
            // Emacs-style line editing for terminal muscle memory.
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => state.left(),
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => state.right(),
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => state.home(),
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => state.end(),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => state.delete(),
            // Ignore other control/alt chords so they don't land in the buffer.
            KeyCode::Char(_)
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {}
            KeyCode::Char(c) => state.insert(c),
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
            InputKind::Create => {
                let is_dir = value.ends_with('/');
                let clean = value.trim_end_matches('/');
                // A bare directory prefix (e.g. "src/") with no name is a no-op.
                if clean.is_empty() {
                    return;
                }
                // Resolve relative to the tree root (absolute paths honored),
                // mirroring RenameFull, so editing the prefix down to nothing
                // creates at the root. ops::create makes intermediate dirs.
                // An explicitly absolute path is an intentional out-of-tree
                // action; a RELATIVE one must stay inside the root — `..`
                // segments would otherwise silently create escaped parents
                // the explorer can never show.
                let target = if Path::new(clean).is_absolute() {
                    PathBuf::from(clean)
                } else {
                    let t = normalize_lexical(&self.tree.root.path.join(clean));
                    if !t.starts_with(&self.tree.root.path) {
                        self.set_status("create rejected: path escapes the tree root");
                        return;
                    }
                    t
                };
                match crate::tree::ops::create(&target, is_dir) {
                    Ok(()) => {
                        self.reload_from_disk();
                        self.reveal(&target);
                        self.set_status(format!("created {clean}"));
                    }
                    Err(e) => self.set_status(format!("create failed: {e}")),
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
                // Interpret value relative to the tree root if not absolute;
                // relative values must stay inside the root (see Create).
                let target = if Path::new(&value).is_absolute() {
                    PathBuf::from(&value)
                } else {
                    let t = normalize_lexical(&self.tree.root.path.join(&value));
                    if !t.starts_with(&self.tree.root.path) {
                        self.set_status("rename rejected: path escapes the tree root");
                        return;
                    }
                    t
                };
                match crate::tree::ops::rename(&path, &target) {
                    Ok(()) => {
                        self.reload_from_disk();
                        self.reveal(&target);
                        self.set_status(format!("moved to {value}"));
                    }
                    Err(e) => self.set_status(format!("rename failed: {e}")),
                }
            }
            InputKind::Search => {}
        }
    }

    fn do_rename(&mut self, path: &Path, new_name: &str) {
        let parent = path.parent().unwrap_or(Path::new("/"));
        // The rename prompt takes a NAME (slashes allowed for subdir moves);
        // an absolute value or a `..` chain escaping the tree is a typo or an
        // injection, not a rename — reject rather than clobber something the
        // explorer can't show.
        if Path::new(new_name).is_absolute() {
            self.set_status("rename rejected: absolute path (use rename-full)");
            return;
        }
        let target = normalize_lexical(&parent.join(new_name));
        if !target.starts_with(&self.tree.root.path) {
            self.set_status("rename rejected: path escapes the tree root");
            return;
        }
        match crate::tree::ops::rename(path, &target) {
            Ok(()) => {
                self.reload_from_disk();
                self.reveal(&target);
                self.set_status(format!("renamed to {new_name}"));
            }
            Err(e) => self.set_status(format!("rename failed: {e}")),
        }
    }

    fn run_confirm(&mut self, kind: ConfirmKind) {
        let (paths, use_trash) = match kind {
            ConfirmKind::Delete(p) => (vec![p], false),
            ConfirmKind::Trash(p) => (vec![p], true),
            ConfirmKind::BulkDelete(paths) => (paths, false),
            ConfirmKind::BulkTrash(paths) => (paths, true),
        };
        // Run every op, then prune bookkeeping for the paths that actually
        // went away. Clearing marks/selection up front (or aborting the batch
        // on the first error) destroys the only record of what still exists —
        // a partial failure must leave the survivors marked and retryable.
        let total = paths.len();
        let mut removed: Vec<PathBuf> = Vec::new();
        let mut first_err: Option<String> = None;
        for p in &paths {
            let res = if use_trash {
                crate::tree::ops::trash(p)
            } else {
                crate::tree::ops::remove(p)
            };
            match res {
                Ok(()) => removed.push(p.clone()),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e.to_string());
                    }
                }
            }
        }
        self.marks.remove_all(&removed);
        for p in &removed {
            self.selection.remove(p);
        }
        self.reload_from_disk();
        match first_err {
            None => self.set_status(format!("removed {total} item(s)")),
            Some(e) => self.set_status(format!("removed {} of {total} ({e})", removed.len())),
        }
    }

    // ── Open / navigate ───────────────────────────────────────────────────────

    fn open_or_toggle(&mut self) {
        let Some(row) = self.current_row().cloned() else {
            return;
        };
        if row.kind.is_dir() {
            self.reveal_children = !row.expanded;
            self.tree.toggle(&row.path);
            self.note_user_expansion(&row.path);
            self.apply_overlays();
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
            self.reveal_children = true;
            self.tree.expand(&row.path);
            self.note_user_expansion(&row.path);
            self.apply_overlays();
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
            self.note_user_expansion(&row.path);
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
        // A live filter is scoped to the root it was typed under; carrying it
        // across a cd would later collapse the new tree's dirs by paths that
        // don't exist under it. Drop all filter state instead.
        self.live_filter = None;
        self.live_editing = false;
        self.live_scan = None;
        self.filter_auto_expanded.clear();
        self.tree.set_root(path);
        self.apply_overlays();
        self.refresh_rows(None);
        self.list_state.select(Some(0));
        self.spawn_git();
    }

    // ── File ops ────────────────────────────────────────────────────────────

    fn start_create(&mut self) {
        let dir = self.current_dir_context();
        // Prefill the editable buffer with the base directory (relative to the
        // tree root, trailing slash), nvim-tree style. The path is editable, so
        // it can be retargeted to any directory - including the project root
        // (clear the prefix) even when the cursor sits on a nested node. An
        // empty prefix means "create at the root". The trailing-slash dir rule
        // and intermediate-dir creation are handled on submit / in ops::create.
        let prefill = dir
            .strip_prefix(&self.tree.root.path)
            .ok()
            .map(|rel| rel.to_string_lossy())
            .filter(|rel| !rel.is_empty())
            .map(|rel| format!("{rel}/"))
            .unwrap_or_default();
        self.overlay = Overlay::Input(InputState::new(
            format!(" create in {}/ ", shorten(&self.tree.root.path)),
            prefill,
            InputKind::Create,
        ));
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
        self.overlay = Overlay::Input(InputState::new(prompt, buffer, ikind));
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
        self.set_status(format!(
            "{} {n} item(s)",
            if op == ClipOp::Cut { "cut" } else { "copied" }
        ));
        self.refresh_rows(self.selected_path());
    }

    fn paste(&mut self) {
        if self.clipboard.is_empty() {
            self.set_status("clipboard empty");
            return;
        }
        let dest = self.current_dir_context();
        let op = self.clipboard.op;
        let paths = self.clipboard.paths.clone();
        let total = paths.len();
        let mut last = None;
        let mut ok = 0usize;
        let mut moved: Vec<PathBuf> = Vec::new();
        let mut first_err: Option<String> = None;
        for src in &paths {
            let target = crate::tree::ops::paste_target(&dest, src);
            let res = match op {
                Some(ClipOp::Cut) => crate::tree::ops::rename(src, &target),
                _ => crate::tree::ops::copy(src, &target),
            };
            match res {
                Ok(()) => {
                    ok += 1;
                    if op == Some(ClipOp::Cut) {
                        // Bookmarks follow the file to its new home.
                        self.marks.remap(src, &target);
                        self.selection.remove(src);
                        moved.push(src.clone());
                    }
                    last = Some(target);
                }
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e.to_string());
                    }
                }
            }
        }
        if op == Some(ClipOp::Cut) {
            // Keep only the items that did NOT move, so the user can retry the
            // failures; a clean sweep clears the clipboard as before.
            self.clipboard.paths.retain(|p| !moved.contains(p));
            if self.clipboard.paths.is_empty() {
                self.clipboard.clear();
            }
        }
        // One truthful summary line instead of a per-item churn of statuses.
        match first_err {
            None => self.set_status(format!("pasted {total} item(s)")),
            Some(e) => self.set_status(format!("pasted {ok} of {total} ({e})")),
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
        self.set_status(format!("yanked: {text}"));
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
            self.set_status(format!(
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
            self.set_status("no bookmarks");
            return;
        }
        let verb = if trash { "trash" } else { "delete" };
        let kind = if trash {
            ConfirmKind::BulkTrash(paths.clone())
        } else {
            ConfirmKind::BulkDelete(paths.clone())
        };
        // Name what is about to be destroyed: bookmarks can be stale (set in
        // an earlier session, or pointing where a file was later recreated),
        // and a bare count gives the user nothing to catch that with.
        let mut names: Vec<String> = paths
            .iter()
            .take(3)
            .map(|p| {
                p.strip_prefix(&self.tree.root.path)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        if paths.len() > names.len() {
            names.push(format!("+{} more", paths.len() - names.len()));
        }
        self.overlay = Overlay::Confirm(ConfirmState {
            prompt: format!(
                "{verb} {} bookmarked item(s)? [{}]",
                paths.len(),
                names.join(", ")
            ),
            kind,
        });
    }

    fn bulk_move(&mut self) {
        let paths: Vec<PathBuf> = self.marks.all().iter().cloned().collect();
        if paths.is_empty() {
            self.set_status("no bookmarks");
            return;
        }
        let dest = self.current_dir_context();
        let total = paths.len();
        let mut moved: Vec<PathBuf> = Vec::new();
        let mut first_err: Option<String> = None;
        for src in &paths {
            let target = crate::tree::ops::paste_target(&dest, src);
            match crate::tree::ops::rename(src, &target) {
                Ok(()) => moved.push(src.clone()),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e.to_string());
                    }
                }
            }
        }
        // Only the bookmarks that actually moved are done with; failures stay
        // bookmarked so the user can see and retry them.
        self.marks.remove_all(&moved);
        self.reload_from_disk();
        match first_err {
            None => self.set_status(format!("moved {total} bookmarked item(s)")),
            Some(e) => self.set_status(format!(
                "moved {} of {total} bookmarked item(s) ({e})",
                moved.len()
            )),
        }
    }

    // ── Filters ───────────────────────────────────────────────────────────────

    fn toggle_filter(&mut self, f: Filter) {
        let label = match f {
            Filter::Hidden => {
                // One toggle for both dotfiles and git-ignored entries — keeping
                // them tied means revealing hidden files also reveals ignored
                // dirs (node_modules, dist), so navigating into them no longer
                // makes them vanish.
                let show = !self.tree.show_hidden;
                self.tree.show_hidden = show;
                self.tree.show_ignored = show;
                ("hidden + ignored", show)
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
        self.set_status(format!(
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
            self.apply_overlays();
        }
        self.set_status(format!(
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
        self.set_status(format!("no match: {query}"));
    }

    // ── Reveal / reload ─────────────────────────────────────────────────────

    fn reveal(&mut self, path: &Path) {
        // The tree root is canonicalized (main.rs), but reveal paths arrive
        // raw from the socket / CLI and may traverse a symlink (/tmp ->
        // /private/tmp on macOS, ~/work -> /Volumes/...). Compare in the same
        // namespace or every such reveal silently misses. Lenient: a follow
        // push can name a not-yet-saved buffer, so resolve the existing prefix
        // and keep the tail.
        let path = canonicalize_lenient(path);
        // Tree::reveal expands the ancestor chain regardless of whether the
        // leaf exists yet, so the row set must be rebuilt either way — skipping
        // the refresh leaves rows whose `expanded` flag contradicts the model
        // (the next toggle then inverts). The return value only decides whether
        // the cursor can move onto the revealed row.
        let landed = self.tree.reveal(&path);
        self.apply_overlays();
        self.refresh_rows(None);
        if landed {
            self.select_path(&path);
        }
    }

    fn reload_from_disk(&mut self) {
        let expanded = self.tree.collect_expanded();
        let sel = self.selected_path();
        self.tree.reload_preserving(&expanded);
        self.apply_overlays();
        self.live_scan = None; // disk changed: cached filter matches are stale
        if self.live_filter.is_some() {
            self.refresh_filtered_view();
        } else {
            self.refresh_rows(sel);
        }
        self.spawn_git();
    }

    /// Targeted reload for filesystem-watcher bursts: re-scan only the
    /// directories whose contents actually changed, instead of re-reading every
    /// expanded directory. This keeps churn cheap even when large trees are
    /// expanded (mirrors nvim-tree's per-directory refresh). Git is always
    /// re-scanned (off-thread, time-bounded) because a working-tree edit can
    /// change status even inside a collapsed directory.
    fn reload_from_paths(&mut self, changed: &HashSet<PathBuf>) {
        let sel = self.selected_path();
        // The directories whose listings may have changed are the parents of
        // each touched path (plus the paths themselves, in case a watched dir's
        // own entries changed).
        let mut dirs: HashSet<PathBuf> = HashSet::new();
        for p in changed {
            if let Some(parent) = p.parent() {
                dirs.insert(parent.to_path_buf());
            }
            dirs.insert(p.clone());
        }
        let mut touched = false;
        for d in &dirs {
            if self.tree.refresh_dir(d) {
                touched = true;
            }
        }
        // Disk changed: cached filter matches are stale even when no LOADED
        // directory was touched — the change may sit inside a never-expanded
        // directory that the filter's disk walk still covers.
        self.live_scan = None;
        if self.live_filter.is_some() {
            // Rebuild the filtered view and re-expand to any matches the change
            // surfaced (even inside a never-expanded directory the walk covers).
            if touched {
                self.apply_overlays();
            }
            self.refresh_filtered_view();
        } else if touched {
            self.apply_overlays();
            self.refresh_rows(sel);
        }
        self.spawn_git();
    }

    /// Ask for a status scan. One runs at a time (see `ScanSchedule`).
    fn spawn_git(&mut self) {
        if self.git_schedule.request() {
            self.start_git_scan(Duration::ZERO);
        }
    }

    fn start_git_scan(&self, delay: Duration) {
        let root = self.tree.root.path.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            if !delay.is_zero() {
                thread::sleep(delay);
            }
            // A failed/timed-out status is forwarded as None so the schedule
            // can retry; the last good statuses stay on screen meanwhile
            // (forwarding an empty result would blank every glyph and, under
            // the git-clean filter, empty the tree).
            let _ = tx.send(AppEvent::Git(git::scan(&root)));
        });
    }

    /// Re-apply every overlay (git status, diagnostics) to the tree after
    /// either the tree or an overlay changed.
    fn apply_overlays(&mut self) {
        self.tree.apply_git(&self.git);
        self.tree
            .apply_diagnostics(&self.diagnostics, self.diagnostics_mode);
    }

    // ── Selection / rows ──────────────────────────────────────────────────────

    fn refresh_rows(&mut self, preserve: Option<PathBuf>) {
        // Build restrict sets (kept alive for the flatten borrow). An empty
        // query is "no restriction yet" — scanning for it would walk the whole
        // tree only to admit everything (and, past the walk cap, silently hide
        // rows that were visible before `f` was even pressed).
        let live_query = self.live_filter.clone().filter(|q| !q.is_empty());
        let live_set = live_query.as_ref().map(|q| self.live_matches(q));
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
            restricts.push(s.as_ref());
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

        // Land the cursor on `preserve` if it survived; otherwise on its
        // nearest surviving ancestor (so a filter narrowing / delete keeps the
        // cursor NEAR where it was, in a related row). Only if neither exists
        // do we keep the prior index — never a stale raw offset onto an
        // unrelated file.
        //
        // View consistency is a two-part contract: this clamp keeps the
        // SELECTION valid for the new rows; the SCROLL OFFSET (and the
        // reveal-children nudge) is repaired in draw(), the only place the
        // true viewport height is known. Any new code that mutates
        // self.rows must route through here so both halves apply.
        let idx = preserve
            .as_deref()
            .and_then(|p| self.row_index_for(p))
            .or_else(|| self.list_state.selected())
            .unwrap_or(0);
        let clamped = idx.min(self.rows.len().saturating_sub(1));
        self.list_state.select(if self.rows.is_empty() {
            None
        } else {
            Some(clamped)
        });
    }

    /// Row index of `path` if present, else of its nearest ancestor that is a
    /// row (walking up toward the root). `None` when nothing on the path chain
    /// is visible.
    fn row_index_for(&self, path: &Path) -> Option<usize> {
        let mut cur = Some(path);
        while let Some(p) = cur {
            if let Some(i) = self.rows.iter().position(|r| r.path == p) {
                return Some(i);
            }
            cur = p
                .parent()
                .filter(|par| par.starts_with(&self.tree.root.path));
        }
        None
    }

    /// The live-filter restrict set for `query`, re-scanning the disk only when
    /// the cached scan no longer matches the current inputs. The set is behind
    /// an `Rc` so per-refresh reuse is a pointer copy, not a deep clone.
    fn live_matches(&mut self, query: &str) -> Rc<HashSet<PathBuf>> {
        self.ensure_live_scan(query);
        Rc::clone(&self.live_scan.as_ref().unwrap().visible)
    }

    fn ensure_live_scan(&mut self, query: &str) {
        let fresh = self.live_scan.as_ref().is_some_and(|s| {
            s.query == query
                && s.root == self.tree.root.path
                && s.show_hidden == self.tree.show_hidden
                && s.show_ignored == self.tree.show_ignored
                && s.custom_active == self.custom_active
        });
        if !fresh {
            self.live_scan = Some(self.scan_disk_for_filter(query));
        }
    }

    /// Walk the tree on disk from the root — not just the lazily-loaded nodes —
    /// honoring the active visibility toggles. This is what lets the filter
    /// find files inside directories that were never expanded. Returns the
    /// visible set (matches + ancestors + folder freebies) and the directories
    /// to expand so matches actually render.
    fn scan_disk_for_filter(&mut self, query: &str) -> LiveScan {
        let root = self.tree.root.path.clone();
        let show_hidden = self.tree.show_hidden;
        let show_ignored = self.tree.show_ignored;
        // Mirror nvim-tree's `always_show_folders`: keep every directory
        // visible so the tree stays navigable, matching only entries by name.
        let show_folders = self.config.live_filter_show_folders;
        let pat = (!query.is_empty())
            .then(|| Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart));

        // Git-ignored pruning only works when git data exists. On a non-git
        // root, statuses is permanently empty, so fall back to the watcher's
        // high-churn dir names — otherwise the walk burns its entry cap inside
        // node_modules/target and drops real matches past the cap. (For git
        // roots the pre-first-scan window is covered by the AppEvent::Git
        // cache invalidation: the scan simply reruns once statuses arrive.)
        let prune_components = self.git.toplevel.is_none();

        let mut visible = HashSet::new();
        let mut expand: Vec<PathBuf> = Vec::new();
        let mut seen_expand = HashSet::new();
        // Canonical targets of symlinked dirs already descended into, so a
        // symlink loop (a/link -> a) terminates.
        let mut visited_links = HashSet::new();
        let mut walked = 0usize;
        let mut truncated = false;
        let mut buf = Vec::new();
        let mut stack = vec![root.clone()];
        'walk: while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                walked += 1;
                if walked > FILTER_WALK_CAP {
                    truncated = true;
                    break 'walk;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                if !show_hidden && name.starts_with('.') {
                    continue;
                }
                if !show_ignored && self.git.statuses.get(&path) == Some(&GitStatus::Ignored) {
                    continue;
                }
                if self.custom_active
                    && self
                        .config
                        .exclude
                        .iter()
                        .any(|p| name.contains(p.as_str()))
                {
                    continue;
                }
                let ft = entry.file_type().ok();
                let is_plain_dir = ft.is_some_and(|t| t.is_dir());
                // Match the tree's NodeKind semantics: a symlink to a directory
                // is a directory for visibility and descent.
                let is_link_dir = ft.is_some_and(|t| t.is_symlink())
                    && std::fs::metadata(&path)
                        .map(|m| m.is_dir())
                        .unwrap_or(false);
                let is_dir = is_plain_dir || is_link_dir;
                let matched = pat.as_ref().is_none_or(|p| {
                    p.score(Utf32Str::new(&name, &mut buf), &mut self.matcher)
                        .is_some()
                });
                if matched || (is_dir && show_folders) {
                    visible.insert(path.clone());
                    let mut cur = path.as_path();
                    while let Some(parent) = cur.parent() {
                        if parent == root || !parent.starts_with(&root) {
                            break;
                        }
                        if !visible.insert(parent.to_path_buf()) {
                            break; // ancestors above are already recorded
                        }
                        cur = parent;
                    }
                }
                if matched && pat.is_some() {
                    // Expansion chain: every directory between the root and the
                    // match, so the match renders. Shallow-first ordering falls
                    // out of sorting below.
                    let mut cur = path.parent();
                    while let Some(p) = cur {
                        if !p.starts_with(&root) {
                            break;
                        }
                        if !seen_expand.insert(p.to_path_buf()) {
                            break; // this chain is already recorded to the root
                        }
                        expand.push(p.to_path_buf());
                        if p == root {
                            break;
                        }
                        cur = p.parent();
                    }
                }
                if is_dir {
                    if prune_components && watcher::is_high_churn_name(&name) {
                        continue; // shown/matched above, but never descended
                    }
                    if is_link_dir {
                        // Descend only through symlinks whose target we have
                        // not walked yet — unresolvable or repeated targets
                        // (cycles) are skipped.
                        if let Ok(real) = std::fs::canonicalize(&path) {
                            if visited_links.insert(real) {
                                stack.push(path);
                            }
                        }
                    } else {
                        stack.push(path);
                    }
                }
            }
        }
        expand.sort_by_key(|p| p.components().count());
        LiveScan {
            query: query.to_string(),
            root,
            show_hidden,
            show_ignored,
            custom_active: self.custom_active,
            visible: Rc::new(visible),
            expand,
            truncated,
        }
    }

    /// Called whenever the live-filter query text changes: rebuild the filtered
    /// view (rescan + expand to matches), preserving the cursor.
    fn on_live_query_changed(&mut self) {
        self.refresh_filtered_view();
    }

    /// Rebuild the filtered view: ensure a fresh scan, expand the tree to the
    /// matches (bounded), surface any truncation, and refresh the rows. This is
    /// the SINGLE place expansion-to-matches happens, so every rescan path
    /// (query edit, git update, fs change, toggle) re-expands to its matches —
    /// not just an edit of the query text.
    fn refresh_filtered_view(&mut self) {
        let Some(query) = self.live_filter.clone() else {
            return;
        };
        if query.is_empty() {
            // No restriction yet — nothing to scan or expand.
            self.live_scan = None;
            self.refresh_rows(self.selected_path());
            return;
        }
        let preserve = self.selected_path();
        self.ensure_live_scan(&query);
        let scan = self.live_scan.as_ref().unwrap();
        let truncated = scan.truncated;
        let over_cap = scan.expand.len() > FILTER_EXPAND_CAP;
        let n_dirs = scan.expand.len();
        let expand: Vec<PathBuf> = if over_cap {
            Vec::new()
        } else {
            scan.expand.clone()
        };
        if !expand.is_empty() {
            let already = self.tree.collect_expanded();
            self.filter_expanding = true;
            for dir in expand.iter().filter(|d| !already.contains(*d)) {
                self.tree.expand(dir);
                // Remember what the filter opened, so clear() can undo exactly
                // this and nothing the user had open.
                self.filter_auto_expanded.insert(dir.clone());
            }
            self.filter_expanding = false;
            self.apply_overlays();
        }
        if truncated {
            self.set_status("filter: tree too large, results incomplete");
        } else if over_cap {
            // Matches exist but span too many directories to explode open;
            // without this message the mostly-collapsed view reads as "no
            // results".
            self.set_status(format!(
                "filter: matches in {n_dirs} dirs — type more to narrow"
            ));
        } else {
            // Under the caps now — clear any stale over-cap/truncation notice
            // left over from a broader query.
            self.clear_status();
        }
        self.refresh_rows(preserve);
    }

    /// Arm the live filter.
    fn start_live_filter(&mut self) {
        self.live_filter = Some(String::new());
        self.live_editing = true;
        self.filter_auto_expanded.clear();
        self.refresh_rows(self.selected_path());
    }

    /// Drop the live filter, collapsing exactly the directories the filter
    /// auto-expanded (deepest first) and leaving everything the user had open —
    /// or opened/closed themselves while filtering — untouched.
    fn clear_live_filter(&mut self) {
        self.live_filter = None;
        self.live_editing = false;
        self.live_scan = None;
        let mut to_collapse: Vec<PathBuf> = self.filter_auto_expanded.drain().collect();
        // Collapse deepest-first so a parent collapse doesn't hide a child we
        // still need to touch (collapse only flips the flag, but order keeps
        // the intent clear and is robust to future changes).
        to_collapse.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        for dir in to_collapse {
            self.tree.collapse(&dir);
        }
        self.apply_overlays();
    }

    /// Record that the user (not the filter) just toggled `path`'s expansion,
    /// so clear_live_filter won't collapse it — nor any of its ancestors, which
    /// are now load-bearing for the user's navigation into this subtree (a
    /// filter-opened parent the user has since descended into must stay open on
    /// clear, not collapse and hide the user's own deeper expansion). A no-op
    /// unless a filter is active and the toggle came from the user.
    fn note_user_expansion(&mut self, path: &Path) {
        if self.live_filter.is_none() || self.filter_expanding {
            return;
        }
        self.filter_auto_expanded.remove(path);
        let mut cur = path.parent();
        while let Some(p) = cur {
            if !p.starts_with(&self.tree.root.path) {
                break;
            }
            self.filter_auto_expanded.remove(p);
            if p == self.tree.root.path {
                break;
            }
            cur = p.parent();
        }
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
        self.set_status("no git changes");
    }

    // ── Mouse ─────────────────────────────────────────────────────────────────

    fn on_mouse(&mut self, m: MouseEvent) {
        if !self.config.mouse {
            return;
        }
        // touch() only where the event actually acts on the tree: passive
        // pointer motion, drags, other buttons, and clicks that land outside
        // the list must not gate follow-reveals.
        match m.kind {
            MouseEventKind::ScrollDown => {
                self.touch();
                self.move_selection(1);
            }
            MouseEventKind::ScrollUp => {
                self.touch();
                self.move_selection(-1);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.list_area;
                if m.row >= area.y && m.row < area.y + area.height && !self.rows.is_empty() {
                    let offset = self.list_state.offset();
                    let idx = offset + (m.row - area.y) as usize;
                    if idx < self.rows.len() {
                        self.touch();
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
                indent_markers: self.config.indent_markers,
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
                .highlight_style(self.theme.selection)
                .scroll_padding(self.config.scrolloff);
            // Ratatui scrolls only far enough to keep the selection visible
            // and clamps a stale offset to len-1, never to len-height. When
            // the row count shrinks (collapse, filter) or the pane grows, the
            // leftover offset top-anchors the tail of the tree over a pane of
            // blank rows until the cursor crawls back up one row per press.
            // Enforce the missing invariant here — the one place the true
            // viewport height for this frame is known. (The selection half of
            // the contract lives in refresh_rows; the reveal-children nudge
            // below is the third piece of the same view-consistency story.)
            let max_offset = self.rows.len().saturating_sub(chunks[1].height as usize);
            if self.list_state.offset() > max_offset {
                *self.list_state.offset_mut() = max_offset;
            }
            // A directory the user just expanded must show its first children
            // even when they were inserted below the fold. scroll_padding
            // covers this only for scrolloff >= 1 (it keeps rows around the
            // SELECTED row visible); at scrolloff = 0 the selected dir row
            // hasn't moved, so ratatui has no reason to scroll and the pane
            // shows an open chevron over nothing.
            if std::mem::take(&mut self.reveal_children) {
                if let (Some(sel), h @ 1..) =
                    (self.list_state.selected(), chunks[1].height as usize)
                {
                    if let Some(dir) = self.rows.get(sel) {
                        let children = self.rows[sel + 1..]
                            .iter()
                            .take_while(|r| r.depth > dir.depth)
                            .count();
                        let want = children.min(self.config.scrolloff.max(1));
                        // Smallest offset that keeps row sel+want inside the
                        // viewport; never scroll the selected row itself out.
                        let need = (sel + want + 1).saturating_sub(h);
                        if self.list_state.offset() < need {
                            *self.list_state.offset_mut() = need.min(max_offset).min(sel);
                        }
                    }
                }
            }
            frame.render_stateful_widget(list, chunks[1], &mut self.list_state);

            frame.render_widget(Paragraph::new(self.status_line()), chunks[2]);

            match &self.overlay {
                Overlay::Input(state) => ui_overlays::render_input(frame, area, &self.theme, state),
                Overlay::Confirm(state) => {
                    ui_overlays::render_confirm(frame, area, &self.theme, state)
                }
                Overlay::Info(state) => ui_overlays::render_info(frame, area, &self.theme, state),
                Overlay::Help(state) => ui_overlays::render_help(frame, area, &self.theme, state),
                Overlay::None => {}
            }
        })?;
        Ok(())
    }

    fn status_line(&self) -> Line<'_> {
        // Live filter takes over the status line while active — but a transient
        // filter message (truncation / "matches in N dirs") is appended so the
        // notices set during filtering are actually visible; without this the
        // early return swallowed every one of them.
        if let Some(q) = &self.live_filter {
            let cursor = if self.live_editing { "▏" } else { "" };
            let mut spans = vec![
                Span::styled("filter: ", self.theme.filter_prefix),
                Span::styled(q.clone(), self.theme.text),
                Span::styled(cursor, self.theme.prompt),
            ];
            if let Some(msg) = &self.status {
                spans.push(Span::styled("  ", self.theme.text));
                spans.push(Span::styled(msg.clone(), self.theme.prompt));
            }
            return Line::from(spans);
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
        // The two toggles that silently reshape the view: without a
        // persistent flag, a stray `L` (shift-adjacent to `l` while
        // expanding) or `U` announced itself only in a 4s transient message.
        if self.custom_active {
            flags.push('U');
        }
        if self.tree.group_empty {
            flags.push('L');
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

/// Canonicalize a path whose leaf may not exist yet: resolve the deepest
/// existing ancestor (following symlinks) and re-attach the remaining
/// components. Used to bring incoming reveal paths into the same namespace as
/// the canonicalized tree root. Falls back to the input if nothing resolves.
fn canonicalize_lenient(p: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(p) {
        return real;
    }
    for anc in p.ancestors().skip(1) {
        if let Ok(real) = std::fs::canonicalize(anc) {
            if let Ok(rest) = p.strip_prefix(anc) {
                return real.join(rest);
            }
        }
    }
    p.to_path_buf()
}

/// Lexically resolve `.` and `..` components (no filesystem access), so a
/// user-typed relative path can be containment-checked against the tree root
/// before anything touches the disk.
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unique_tmpdir() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("treelix-apptest-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    /// App rooted at a fresh temp tree:
    ///   root/
    ///     sub/deep.txt   (sub collapsed at startup -> deep.txt not in `rows`)
    ///     a.txt
    /// The root is canonicalized exactly as main.rs does at startup, so paths
    /// derived from it are in the same namespace as the tree nodes AND as the
    /// canonicalize_lenient() applied to reveal paths (on macOS /tmp and
    /// std::env::temp_dir() resolve under /private). Returns (app, root, deep).
    fn app_with_tree() -> (App, PathBuf, PathBuf) {
        let root = fs::canonicalize(unique_tmpdir()).unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/deep.txt"), b"x").unwrap();
        fs::write(root.join("a.txt"), b"x").unwrap();
        let app = App::new(root.clone(), Config::default(), Theme::default());
        let deep = root.join("sub").join("deep.txt");
        (app, root, deep)
    }

    fn diag_of(app: &App, path: &Path) -> Option<crate::diagnostics::Diag> {
        app.rows
            .iter()
            .find(|r| r.path == path)
            .and_then(|r| r.diag)
    }

    #[test]
    fn diagnostics_color_the_file_and_its_collapsed_folder_until_cleared() {
        use crate::diagnostics::{Counts, Diag, Severity};
        let (mut app, root, deep) = app_with_tree();
        let sub = root.join("sub");
        assert!(!deep_visible(&app, &deep), "sub starts collapsed");

        app.handle_event(AppEvent::Diagnostics(ipc::DiagnosticsUpdate {
            path: deep.clone(),
            counts: Counts {
                errors: 2,
                warnings: 1,
            },
        }));
        assert_eq!(
            diag_of(&app, &sub),
            Some(Diag {
                severity: Severity::Error,
                count: 0
            }),
            "a collapsed folder shows the worst severity inside it"
        );
        assert_eq!(diag_of(&app, &root.join("a.txt")), None);

        app.handle_event(AppEvent::Reveal(ipc::Reveal {
            path: deep.clone(),
            follow: false,
        }));
        assert_eq!(
            diag_of(&app, &deep),
            Some(Diag {
                severity: Severity::Error,
                count: 2
            }),
            "the file shows its error count once visible"
        );

        app.handle_event(AppEvent::Diagnostics(ipc::DiagnosticsUpdate {
            path: deep.clone(),
            counts: Counts {
                errors: 0,
                warnings: 1,
            },
        }));
        assert_eq!(
            diag_of(&app, &deep).map(|d| d.severity),
            Some(Severity::Warning)
        );
        assert_eq!(
            diag_of(&app, &sub).map(|d| d.severity),
            Some(Severity::Warning)
        );

        app.handle_event(AppEvent::Diagnostics(ipc::DiagnosticsUpdate {
            path: deep.clone(),
            counts: Counts::default(),
        }));
        assert_eq!(diag_of(&app, &deep), None, "cleared");
        assert_eq!(diag_of(&app, &sub), None);
    }

    #[test]
    fn diagnostics_mode_errors_hides_warnings_and_off_ignores_everything() {
        use crate::diagnostics::{Counts, Severity};
        for (mode, expect_warning, expect_error) in
            [("errors", None, Some(Severity::Error)), ("off", None, None)]
        {
            let root = fs::canonicalize(unique_tmpdir()).unwrap();
            fs::write(root.join("a.txt"), b"x").unwrap();
            let config = Config {
                diagnostics: mode.to_string(),
                ..Config::default()
            };
            let mut app = App::new(root.clone(), config, Theme::default());
            let file = root.join("a.txt");
            app.handle_event(AppEvent::Diagnostics(ipc::DiagnosticsUpdate {
                path: file.clone(),
                counts: Counts {
                    errors: 0,
                    warnings: 4,
                },
            }));
            assert_eq!(
                diag_of(&app, &file).map(|d| d.severity),
                expect_warning,
                "{mode}"
            );
            app.handle_event(AppEvent::Diagnostics(ipc::DiagnosticsUpdate {
                path: file.clone(),
                counts: Counts {
                    errors: 1,
                    warnings: 4,
                },
            }));
            assert_eq!(
                diag_of(&app, &file).map(|d| d.severity),
                expect_error,
                "{mode}"
            );
        }
    }

    /// Is `deep` an actual visible row (i.e. `sub` is expanded)?
    fn deep_visible(app: &App, deep: &Path) -> bool {
        app.rows.iter().any(|r| r.path == *deep)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// List viewport height in the standard test terminal: TEST_H rows minus
    /// the 1-row header and 1-row status line.
    const VIEWPORT: usize = (TEST_H - 2) as usize;
    const TEST_W: u16 = 40;
    const TEST_H: u16 = 20;

    fn test_terminal() -> Terminal<ratatui::backend::TestBackend> {
        Terminal::new(ratatui::backend::TestBackend::new(TEST_W, TEST_H)).unwrap()
    }

    /// The text of one terminal row of the last-drawn frame.
    fn row_text(terminal: &Terminal<ratatui::backend::TestBackend>, y: u16) -> String {
        crate::test_util::buffer_row_text(terminal.backend().buffer(), y)
    }

    /// Regression: ratatui clamps a stale ListState offset to len-1, not
    /// len-height, so shrinking the row count while scrolled deep used to
    /// leave the last row top-anchored over a pane of blanks (and each `k`
    /// recovered exactly one row). The draw() clamp must bottom-anchor it.
    #[test]
    fn shrinking_rows_keeps_viewport_bottom_anchored() {
        let root = fs::canonicalize(unique_tmpdir()).unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        for i in 0..40 {
            fs::write(root.join(format!("dir/f{i:02}.txt")), b"x").unwrap();
        }
        for i in 0..30 {
            fs::write(root.join(format!("file{i:02}.txt")), b"x").unwrap();
        }
        let mut app = App::new(root.clone(), Config::default(), Theme::default());
        let mut terminal = test_terminal();
        // Dirs sort first, so row 0 is `dir`. Expand it (71 rows), select the
        // last row, and render so ratatui drives the offset deep.
        app.dispatch(Action::Expand);
        assert_eq!(app.rows.len(), 71);
        app.list_state.select(Some(app.rows.len() - 1));
        app.draw(&mut terminal).unwrap();
        assert!(app.list_state.offset() > 33, "offset went deep");
        // Collapse everything: 31 rows remain, selection preserved on the
        // last file, the stale offset (>33) now exceeds len - viewport.
        app.dispatch(Action::CollapseAll);
        app.draw(&mut terminal).unwrap();
        assert_eq!(app.rows.len(), 31);
        assert_eq!(app.list_state.offset(), app.rows.len() - VIEWPORT);
        assert!(
            row_text(&terminal, VIEWPORT as u16).contains("file29"),
            "last row pinned to the pane bottom: {:?}",
            row_text(&terminal, VIEWPORT as u16)
        );
        assert!(
            !row_text(&terminal, 1).trim().is_empty(),
            "no blank rows at the top of the pane"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Regression: expanding a directory whose row sits on the bottom of the
    /// viewport inserted every child below the fold — open chevron, zero
    /// visible children. scroll_padding must pull the first children into view.
    #[test]
    fn expanding_a_dir_on_the_bottom_row_reveals_children() {
        let root = fs::canonicalize(unique_tmpdir()).unwrap();
        for d in 0..25 {
            let dir = root.join(format!("d{d:02}"));
            fs::create_dir(&dir).unwrap();
            for f in 0..5 {
                fs::write(dir.join(format!("c{f}.txt")), b"x").unwrap();
            }
        }
        let mut app = App::new(root.clone(), Config::default(), Theme::default());
        let mut terminal = test_terminal();
        // Select the LAST row (a dir) and render: it sits on the bottom row.
        app.list_state.select(Some(app.rows.len() - 1));
        app.draw(&mut terminal).unwrap();
        assert!(
            row_text(&terminal, VIEWPORT as u16).contains("d24"),
            "d24 on the bottom row"
        );
        // Expand it. Its five children must not all land below the fold.
        app.dispatch(Action::Expand);
        app.draw(&mut terminal).unwrap();
        let screen: String = (1..=VIEWPORT as u16)
            .map(|y| row_text(&terminal, y))
            .collect();
        assert!(
            screen.contains("c0.txt"),
            "children of the expanded dir are visible: {screen}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Regression: the reveal must not depend on scroll_padding. With
    /// scrolloff = 0 (a legitimate config value), ratatui has no reason to
    /// scroll — the selected dir row hasn't moved — so without the explicit
    /// reveal nudge the expanded dir showed an open chevron over nothing.
    #[test]
    fn expanding_at_bottom_reveals_a_child_even_with_scrolloff_zero() {
        let root = fs::canonicalize(unique_tmpdir()).unwrap();
        for d in 0..25 {
            let dir = root.join(format!("d{d:02}"));
            fs::create_dir(&dir).unwrap();
            for f in 0..5 {
                fs::write(dir.join(format!("c{f}.txt")), b"x").unwrap();
            }
        }
        let config = Config {
            scrolloff: 0,
            ..Config::default()
        };
        let mut app = App::new(root.clone(), config, Theme::default());
        let mut terminal = test_terminal();
        app.list_state.select(Some(app.rows.len() - 1));
        app.draw(&mut terminal).unwrap();
        assert!(
            row_text(&terminal, VIEWPORT as u16).contains("d24"),
            "d24 on the bottom row"
        );
        app.dispatch(Action::Expand);
        app.draw(&mut terminal).unwrap();
        let screen: String = (1..=VIEWPORT as u16)
            .map(|y| row_text(&terminal, y))
            .collect();
        assert!(
            screen.contains("c0.txt"),
            "at least the first child is visible at scrolloff 0: {screen}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn help_scrolls_and_only_non_scroll_keys_dismiss() {
        let (mut app, _root, _deep) = app_with_tree();
        // Give the help a small viewport so its content is scrollable.
        app.list_area = ratatui::layout::Rect::new(0, 1, 30, 6);
        app.dispatch(Action::Help);
        assert!(matches!(app.overlay, Overlay::Help(_)), "help opened");

        // j scrolls down without dismissing.
        app.on_key(key(KeyCode::Char('j')));
        let s1 = match &app.overlay {
            Overlay::Help(h) => h.scroll,
            _ => panic!("j must not dismiss help"),
        };
        assert!(s1 >= 1, "j scrolled down");
        // k scrolls back up.
        app.on_key(key(KeyCode::Char('k')));
        match &app.overlay {
            Overlay::Help(h) => assert_eq!(h.scroll, s1 - 1),
            _ => panic!("k must not dismiss help"),
        }
        // G jumps to the bottom; g back to the top.
        app.on_key(key(KeyCode::Char('G')));
        let bottom = match &app.overlay {
            Overlay::Help(h) => h.scroll,
            _ => panic!("G must not dismiss"),
        };
        assert!(bottom >= 1, "G scrolled to a positive max");
        app.on_key(key(KeyCode::Char('g')));
        match &app.overlay {
            Overlay::Help(h) => assert_eq!(h.scroll, 0, "g returned to top"),
            _ => panic!("g must not dismiss"),
        }
        // q dismisses.
        app.on_key(key(KeyCode::Char('q')));
        assert!(matches!(app.overlay, Overlay::None), "q closes help");

        // A non-scroll key (e.g. 'z') also dismisses (old any-key feel).
        app.dispatch(Action::Help);
        app.on_key(key(KeyCode::Char('z')));
        assert!(
            matches!(app.overlay, Overlay::None),
            "unrelated key closes help"
        );
    }

    // Regression guard for f9744e4: set_status/clear_status must assign the
    // field, not call themselves. If the recursion returns, this test blows the
    // stack and aborts instead of asserting cleanly.
    #[test]
    fn set_and_clear_status_do_not_recurse() {
        let (mut app, _root, _deep) = app_with_tree();
        app.set_status("hello");
        assert_eq!(app.status.as_deref(), Some("hello"));
        assert!(app.status_deadline.is_some(), "deadline should be armed");
        app.clear_status();
        assert_eq!(app.status, None);
        assert!(app.status_deadline.is_none(), "deadline should be disarmed");
    }

    #[test]
    fn explicit_reveal_applies_immediately() {
        let (mut app, _root, deep) = app_with_tree();
        assert!(!deep_visible(&app, &deep), "sub collapsed at startup");
        app.handle_event(AppEvent::Reveal(ipc::Reveal {
            path: deep.clone(),
            follow: false,
        }));
        assert_eq!(app.current_file.as_ref(), Some(&deep));
        assert!(
            app.pending_reveal.is_none(),
            "explicit reveal is never deferred"
        );
        assert!(deep_visible(&app, &deep), "sub expanded, deep revealed");
        assert_eq!(app.selected_path().as_ref(), Some(&deep));
    }

    #[test]
    fn follow_reveal_is_deferred_while_driving() {
        let (mut app, _root, deep) = app_with_tree();
        app.touch(); // user just acted -> within the grace window
        app.handle_event(AppEvent::Reveal(ipc::Reveal {
            path: deep.clone(),
            follow: true,
        }));
        // Highlight updates immediately...
        assert_eq!(app.current_file.as_ref(), Some(&deep));
        // ...but the expand + cursor move is held back.
        assert_eq!(app.pending_reveal.as_ref(), Some(&deep));
        assert!(!deep_visible(&app, &deep), "deferred: sub stays collapsed");
    }

    #[test]
    fn follow_reveal_applies_when_idle() {
        let (mut app, _root, deep) = app_with_tree();
        app.last_input = None; // not driving the tree
        app.handle_event(AppEvent::Reveal(ipc::Reveal {
            path: deep.clone(),
            follow: true,
        }));
        assert!(app.pending_reveal.is_none());
        assert!(deep_visible(&app, &deep), "idle follow reveals immediately");
    }

    #[test]
    fn deferred_reveal_applies_on_next_event_once_idle() {
        let (mut app, _root, deep) = app_with_tree();
        app.touch();
        app.handle_event(AppEvent::Reveal(ipc::Reveal {
            path: deep.clone(),
            follow: true,
        }));
        assert_eq!(app.pending_reveal.as_ref(), Some(&deep), "deferred");
        assert!(!deep_visible(&app, &deep));

        // User goes idle; the next event (any kind) flushes the pending reveal
        // before it is handled.
        app.last_input = None;
        app.handle_event(AppEvent::Redraw);
        assert!(app.pending_reveal.is_none(), "pending reveal consumed");
        assert!(deep_visible(&app, &deep), "tree synced to Helix's buffer");
    }

    fn type_filter(app: &mut App, query: &str) {
        app.on_key(key(KeyCode::Char('f'))); // LiveFilterStart
        for c in query.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn live_filter_finds_file_in_collapsed_dir() {
        // Regression: the filter used to search only lazily-loaded nodes and
        // never expanded anything, so with `sub` collapsed (its children not
        // even loaded), filtering for "deep" showed nothing.
        let (mut app, _root, deep) = app_with_tree();
        assert!(!deep_visible(&app, &deep), "sub collapsed at startup");
        type_filter(&mut app, "deep");
        assert!(
            deep_visible(&app, &deep),
            "filter must scan disk and auto-expand to the match; rows: {:?}",
            app.rows.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        // The non-matching sibling file is filtered out.
        assert!(!app.rows.iter().any(|r| r.name == "a.txt"));
    }

    #[test]
    fn filter_hides_folders_without_matches() {
        // Default behavior: a folder appears in the filtered view only if its
        // own name matches or it is an ancestor of a match — unrelated folders
        // (and folders with no matching files) are noise and stay hidden.
        let (mut app, root, deep) = app_with_tree();
        fs::create_dir(root.join("unrelated")).unwrap();
        fs::write(root.join("unrelated/other.txt"), b"x").unwrap();
        app.handle_event(AppEvent::Fs(watcher::FsChange::Rescan)); // pick up new dir
        type_filter(&mut app, "deep");
        assert!(deep_visible(&app, &deep), "match shown");
        assert!(
            app.rows.iter().any(|r| r.name == "sub"),
            "ancestor of the match stays visible"
        );
        assert!(
            !app.rows.iter().any(|r| r.name == "unrelated"),
            "folder with no matches is hidden"
        );
    }

    #[test]
    fn clearing_filter_restores_expansion_state() {
        let (mut app, _root, deep) = app_with_tree();
        type_filter(&mut app, "deep");
        assert!(deep_visible(&app, &deep), "filter expanded sub");
        app.on_key(key(KeyCode::Esc)); // clear while editing
        assert!(app.live_filter.is_none());
        assert!(
            !deep_visible(&app, &deep),
            "sub must be collapsed again after the filter is cleared"
        );
        assert!(
            app.rows.iter().any(|r| r.name == "a.txt"),
            "full tree is back"
        );
    }

    #[test]
    fn narrowing_query_after_enter_persists() {
        // Enter keeps the filter active for navigation; rows stay restricted.
        let (mut app, _root, deep) = app_with_tree();
        type_filter(&mut app, "deep");
        app.on_key(key(KeyCode::Enter));
        assert!(app.live_filter.is_some(), "Enter keeps the filter");
        assert!(deep_visible(&app, &deep));
        // F (LiveFilterClear) restores the pre-filter tree.
        app.on_key(key(KeyCode::Char('F')));
        assert!(app.live_filter.is_none());
        assert!(!deep_visible(&app, &deep));
    }

    #[test]
    fn clear_only_undoes_filter_opened_dirs() {
        // A second top-level dir the user opens BEFORE filtering must survive a
        // filter+clear cycle, while the dir the filter itself opened collapses.
        let (mut app, root, _deep) = app_with_tree();
        fs::create_dir(root.join("other")).unwrap();
        fs::write(root.join("other/note.txt"), b"x").unwrap();
        app.handle_event(AppEvent::Fs(watcher::FsChange::Rescan));

        // User opens `other` by hand (pre-filter).
        app.select_path(&root.join("other"));
        app.dispatch(Action::Expand);
        assert!(app.tree.collect_expanded().contains(&root.join("other")));

        // Filter opens `sub` to reach sub/deep.txt.
        type_filter(&mut app, "deep");
        assert!(app.tree.collect_expanded().contains(&root.join("sub")));

        // Clear (Esc, since we're still in edit mode): filter-opened `sub`
        // collapses; user-opened `other` stays open.
        app.on_key(key(KeyCode::Esc));
        let expanded = app.tree.collect_expanded();
        assert!(
            expanded.contains(&root.join("other")),
            "pre-existing user expansion survives clear"
        );
        assert!(
            !expanded.contains(&root.join("sub")),
            "filter-opened dir collapses on clear"
        );
    }

    #[test]
    fn clear_does_not_reopen_a_dir_the_user_collapsed_while_parked() {
        // Regression vs the old wholesale-restore: collapsing a dir while the
        // filter is parked must stick after clear, not spring back open.
        let (mut app, root, _deep) = app_with_tree();
        // Open `sub` by hand pre-filter so it's part of the "before" state.
        app.select_path(&root.join("sub"));
        app.dispatch(Action::Expand);
        type_filter(&mut app, "deep");
        app.on_key(key(KeyCode::Enter)); // park
                                         // User collapses `sub` while parked.
        app.select_path(&root.join("sub"));
        app.dispatch(Action::CollapseOrParent);
        assert!(!app.tree.collect_expanded().contains(&root.join("sub")));
        // Clear must NOT re-expand it.
        app.on_key(key(KeyCode::Char('F')));
        assert!(
            !app.tree.collect_expanded().contains(&root.join("sub")),
            "a dir collapsed while parked stays collapsed after clear"
        );
    }

    #[test]
    fn cursor_falls_back_to_nearest_ancestor_not_raw_index() {
        // Cursor on sub/deep.txt; filtering to a query it no longer matches
        // must land the cursor near it (on sub, its ancestor), not on an
        // arbitrary row at the old index.
        let (mut app, root, deep) = app_with_tree();
        app.reveal(&deep);
        assert_eq!(app.selected_path().as_ref(), Some(&deep));
        type_filter(&mut app, "a.txt"); // matches only root/a.txt, not deep
                                        // deep.txt is gone from the view; the cursor should be on `sub`
                                        // (nearest surviving ancestor of deep) or a.txt — never a stale index
                                        // pointing at an unrelated row.
        let sel = app.selected_path().unwrap();
        assert!(
            sel == root.join("sub") || sel == root.join("a.txt"),
            "cursor landed sensibly, got {sel:?}"
        );
    }

    #[test]
    fn empty_filter_query_restricts_nothing() {
        // Pressing `f` alone must not scan or hide anything: an empty query is
        // "no restriction yet", and on huge trees a truncated scan-of-everything
        // used as a restrict set would hide rows that were just visible.
        let (mut app, _root, _deep) = app_with_tree();
        let before: Vec<PathBuf> = app.rows.iter().map(|r| r.path.clone()).collect();
        app.on_key(key(KeyCode::Char('f')));
        assert!(app.live_scan.is_none(), "no scan for an empty query");
        let after: Vec<PathBuf> = app.rows.iter().map(|r| r.path.clone()).collect();
        assert_eq!(before, after, "rows unchanged by an empty filter");
    }

    #[test]
    fn cd_drops_live_filter_state() {
        // A filter parked with Enter must not survive a cd: restoring the old
        // root's expansion snapshot onto the new tree would collapse everything.
        let (mut app, root, deep) = app_with_tree();
        type_filter(&mut app, "deep");
        app.on_key(key(KeyCode::Enter));
        assert!(app.live_filter.is_some());
        app.set_root(root.join("sub"));
        assert!(app.live_filter.is_none(), "cd clears the filter");
        assert!(
            app.filter_auto_expanded.is_empty(),
            "cd drops the tracked auto-expansions"
        );
        assert!(
            app.rows.iter().any(|r| r.path == deep),
            "new root renders normally after cd"
        );
    }

    #[test]
    fn bulk_delete_partial_failure_keeps_survivor_marks() {
        // Regression: marks/selection were cleared BEFORE the ops ran and the
        // batch aborted on the first error, so a partial failure destroyed
        // the record of what still exists.
        let (mut app, root, _deep) = app_with_tree();
        let a = root.join("a.txt");
        let ghost = root.join("ghost.txt"); // never exists on disk
        app.marks.toggle(&a);
        app.marks.toggle(&ghost);
        app.run_confirm(ConfirmKind::BulkDelete(vec![ghost.clone(), a.clone()]));
        assert!(!a.exists(), "existing file removed despite earlier failure");
        assert!(!app.marks.contains(&a), "removed path unmarked");
        assert!(
            app.marks.contains(&ghost),
            "failed path keeps its mark for retry"
        );
        assert!(
            app.status.as_deref().unwrap_or("").contains("1 of 2"),
            "status reports partial result: {:?}",
            app.status
        );
    }

    #[test]
    fn create_rejects_root_escape() {
        let (mut app, root, _deep) = app_with_tree();
        app.submit_input(InputState::new(
            " create ",
            "../escaped.txt".to_string(),
            InputKind::Create,
        ));
        assert!(
            !root.parent().unwrap().join("escaped.txt").exists(),
            "no file may be created outside the root"
        );
        assert!(
            app.status.as_deref().unwrap_or("").contains("rejected"),
            "status explains the rejection: {:?}",
            app.status
        );
    }

    #[test]
    fn rename_rejects_root_escape_and_existing_target() {
        let (mut app, root, _deep) = app_with_tree();
        // Escape via `..` in the rename prompt.
        app.do_rename(&root.join("a.txt"), "../stolen.txt");
        assert!(root.join("a.txt").exists(), "source untouched");
        assert!(!root.parent().unwrap().join("stolen.txt").exists());
        // Renaming onto an existing file is refused (ops-level guard).
        fs::write(root.join("b.txt"), b"other").unwrap();
        app.do_rename(&root.join("a.txt"), "b.txt");
        assert_eq!(
            fs::read(root.join("b.txt")).unwrap(),
            b"other",
            "no overwrite"
        );
        assert!(root.join("a.txt").exists());
    }

    #[test]
    fn fs_rescan_reloads_expanded_tree() {
        // An OS-level event overflow (FSEvents "must scan subdirs") arrives as
        // FsChange::Rescan with no usable paths; the whole expanded tree must
        // be re-read or the missed changes stay invisible forever.
        let (mut app, root, deep) = app_with_tree();
        app.handle_event(AppEvent::Reveal(ipc::Reveal {
            path: deep.clone(),
            follow: false,
        }));
        assert!(deep_visible(&app, &deep), "sub expanded");

        // Change the tree on disk with no per-path notification.
        fs::write(root.join("sub/missed.txt"), b"x").unwrap();
        app.handle_event(AppEvent::Fs(watcher::FsChange::Rescan));
        assert!(
            app.rows
                .iter()
                .any(|r| r.path == root.join("sub/missed.txt")),
            "rescan re-reads expanded dirs and surfaces the missed file"
        );
    }

    #[test]
    fn acting_key_arms_grace_but_inert_key_does_not() {
        let (mut app, _root, _deep) = app_with_tree();
        app.last_input = None;
        app.on_key(key(KeyCode::Char('j'))); // resolves to Down -> acts
        assert!(
            app.last_input.is_some(),
            "a mapped key must arm the grace window"
        );

        let (mut app2, _r2, _d2) = app_with_tree();
        app2.last_input = None;
        app2.on_key(key(KeyCode::F(9))); // unmapped, no pending chord -> inert
        assert!(
            app2.last_input.is_none(),
            "an unmapped key must not arm the grace window"
        );
    }

    #[test]
    fn chord_prefix_arms_grace() {
        // A dead prefix (`g`) resolves to no action but a pending chord; it is
        // the user driving the tree, so it must arm the window.
        let (mut app, _root, _deep) = app_with_tree();
        app.last_input = None;
        app.on_key(key(KeyCode::Char('g')));
        assert!(!app.pending.is_empty(), "g starts a multi-key sequence");
        assert!(
            app.last_input.is_some(),
            "a chord prefix arms the grace window"
        );
    }
}
