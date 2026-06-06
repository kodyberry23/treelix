//! Key → Action mapping, following nvim-tree's default bindings.
//!
//! Multi-key sequences (`g`, `]`, `[`, `b` prefixes) are handled by threading a
//! `pending` accumulator string through `resolve`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    // Navigation
    Down,
    Up,
    FirstSibling,
    LastSibling,
    NextSibling,
    PrevSibling,
    OpenOrToggle,
    Expand,
    CollapseOrParent,
    CursorParent,
    CdInto,
    RootParent,
    ExpandAll,
    CollapseAll,
    NextGit,
    PrevGit,
    // Open targets
    Preview,
    VSplit,
    HSplit,
    SystemOpen,
    // File ops
    Create,
    Delete,
    Trash,
    Rename,
    RenameBasename,
    RenameFull,
    RenameOmitFilename,
    Cut,
    Copy,
    Paste,
    CopyFilename,
    CopyRelpath,
    CopyAbspath,
    FileInfo,
    // Marks
    ToggleMark,
    BulkDelete,
    BulkTrash,
    BulkMove,
    // Filters / view
    ToggleHidden,
    ToggleIgnored,
    ToggleGitClean,
    ToggleCustom,
    ToggleNoBuffer,
    ToggleNoBookmark,
    ToggleGroupEmpty,
    LiveFilterStart,
    LiveFilterClear,
    SearchNode,
    Refresh,
    Help,
    // Selection
    ToggleSelect,
    ClearSelect,
}

/// Resolve a key press to an action, threading multi-key `pending` state.
/// Returns the action and the new pending accumulator (empty when no sequence
/// is in progress).
pub fn resolve(key: KeyEvent, pending: &str) -> (Action, String) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let none = String::new();

    // Continue an in-progress multi-key sequence.
    if !pending.is_empty() {
        return resolve_pending(pending, key);
    }

    // Ctrl-chord bindings.
    if ctrl {
        let action = match key.code {
            KeyCode::Char('v') => Action::VSplit,
            KeyCode::Char('x') => Action::HSplit,
            KeyCode::Char(']') => Action::CdInto,
            KeyCode::Char('r') => Action::RenameOmitFilename,
            KeyCode::Char('k') => Action::FileInfo,
            _ => Action::None,
        };
        return (action, none);
    }

    match key.code {
        // Enter multi-key prefixes.
        KeyCode::Char('g') => (Action::None, "g".into()),
        KeyCode::Char(']') => (Action::None, "]".into()),
        KeyCode::Char('[') => (Action::None, "[".into()),
        KeyCode::Char('b') => (Action::None, "b".into()),

        KeyCode::Char('q') => (Action::Quit, none),

        KeyCode::Char('j') | KeyCode::Down => (Action::Down, none),
        KeyCode::Char('k') | KeyCode::Up => (Action::Up, none),
        KeyCode::Char('K') => (Action::FirstSibling, none),
        KeyCode::Char('J') => (Action::LastSibling, none),
        KeyCode::Char('>') => (Action::NextSibling, none),
        KeyCode::Char('<') => (Action::PrevSibling, none),

        KeyCode::Enter | KeyCode::Char('o') => (Action::OpenOrToggle, none),
        KeyCode::Char('l') | KeyCode::Right => (Action::Expand, none),
        KeyCode::Char('h') | KeyCode::Backspace | KeyCode::Left => (Action::CollapseOrParent, none),
        KeyCode::Char('P') => (Action::CursorParent, none),
        KeyCode::Char('-') => (Action::RootParent, none),
        KeyCode::Char('E') => (Action::ExpandAll, none),
        KeyCode::Char('W') => (Action::CollapseAll, none),
        KeyCode::Char('L') => (Action::ToggleGroupEmpty, none),

        KeyCode::Tab => (Action::Preview, none),
        KeyCode::Char('s') => (Action::SystemOpen, none),

        KeyCode::Char('a') => (Action::Create, none),
        KeyCode::Char('d') | KeyCode::Delete => (Action::Delete, none),
        KeyCode::Char('D') => (Action::Trash, none),
        KeyCode::Char('r') => (Action::Rename, none),
        KeyCode::Char('e') => (Action::RenameBasename, none),
        KeyCode::Char('u') => (Action::RenameFull, none),
        KeyCode::Char('x') => (Action::Cut, none),
        KeyCode::Char('c') => (Action::Copy, none),
        KeyCode::Char('p') => (Action::Paste, none),
        KeyCode::Char('y') => (Action::CopyFilename, none),
        KeyCode::Char('Y') => (Action::CopyRelpath, none),

        KeyCode::Char('m') => (Action::ToggleMark, none),

        KeyCode::Char('.') => (Action::ToggleHidden, none),
        KeyCode::Char('I') => (Action::ToggleIgnored, none),
        KeyCode::Char('C') => (Action::ToggleGitClean, none),
        KeyCode::Char('U') => (Action::ToggleCustom, none),
        KeyCode::Char('B') => (Action::ToggleNoBuffer, none),
        KeyCode::Char('M') => (Action::ToggleNoBookmark, none),

        KeyCode::Char('f') => (Action::LiveFilterStart, none),
        KeyCode::Char('F') => (Action::LiveFilterClear, none),
        KeyCode::Char('S') => (Action::SearchNode, none),

        KeyCode::Char('v') => (Action::ToggleSelect, none),
        KeyCode::Esc => (Action::ClearSelect, none),

        KeyCode::Char('R') => (Action::Refresh, none),

        _ => (Action::None, none),
    }
}

fn resolve_pending(pending: &str, key: KeyEvent) -> (Action, String) {
    let none = String::new();
    match pending {
        "g" => {
            let action = match key.code {
                KeyCode::Char('y') => Action::CopyAbspath,
                KeyCode::Char('e') => Action::CopyFilename, // ge: copy basename
                KeyCode::Char('?') => Action::Help,
                _ => Action::None,
            };
            (action, none)
        }
        "]" => {
            let action = match key.code {
                KeyCode::Char('c') => Action::NextGit,
                _ => Action::None,
            };
            (action, none)
        }
        "[" => {
            let action = match key.code {
                KeyCode::Char('c') => Action::PrevGit,
                _ => Action::None,
            };
            (action, none)
        }
        "b" => match key.code {
            KeyCode::Char('d') => (Action::BulkDelete, none),
            KeyCode::Char('t') => (Action::BulkTrash, none),
            KeyCode::Char('m') => (Action::None, "bm".into()), // bmv in progress
            _ => (Action::None, none),
        },
        "bm" => {
            let action = match key.code {
                KeyCode::Char('v') => Action::BulkMove,
                _ => Action::None,
            };
            (action, none)
        }
        _ => (Action::None, none),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn basic_keys() {
        assert_eq!(resolve(key('j'), "").0, Action::Down);
        assert_eq!(resolve(key('a'), "").0, Action::Create);
        assert_eq!(resolve(ctrl('v'), "").0, Action::VSplit);
        assert_eq!(resolve(key('.'), "").0, Action::ToggleHidden);
        assert_eq!(resolve(key('f'), "").0, Action::LiveFilterStart);
    }

    #[test]
    fn g_prefix() {
        let (action, pending) = resolve(key('g'), "");
        assert_eq!(action, Action::None);
        assert_eq!(pending, "g");
        assert_eq!(resolve(key('y'), "g").0, Action::CopyAbspath);
        assert_eq!(resolve(key('?'), "g").0, Action::Help);
    }

    #[test]
    fn git_nav_prefixes() {
        assert_eq!(resolve(key(']'), "").1, "]");
        assert_eq!(resolve(key('c'), "]").0, Action::NextGit);
        assert_eq!(resolve(key('c'), "[").0, Action::PrevGit);
    }

    #[test]
    fn bulk_sequences() {
        assert_eq!(resolve(key('b'), "").1, "b");
        assert_eq!(resolve(key('d'), "b").0, Action::BulkDelete);
        assert_eq!(resolve(key('t'), "b").0, Action::BulkTrash);
        // bmv is three keys.
        assert_eq!(resolve(key('m'), "b").1, "bm");
        assert_eq!(resolve(key('v'), "bm").0, Action::BulkMove);
    }
}
