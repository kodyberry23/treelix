//! Key → Action mapping, following nvim-tree's default bindings (core subset).

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
    OpenOrToggle,
    Expand,
    CollapseOrParent,
    CursorParent,
    CdInto,
    RootParent,
    ExpandAll,
    CollapseAll,
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
    Cut,
    Copy,
    Paste,
    CopyFilename,
    CopyRelpath,
    CopyAbspath,
    // Filters / misc
    ToggleHidden,
    ToggleIgnored,
    Refresh,
    Help,
}

/// Resolve a key press to an action, threading the `g`-prefix pending state.
pub fn resolve(key: KeyEvent, pending_g: bool) -> (Action, bool) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if pending_g {
        let action = match key.code {
            KeyCode::Char('y') => Action::CopyAbspath,
            KeyCode::Char('e') => Action::CopyFilename, // ge: copy basename
            KeyCode::Char('?') => Action::Help,
            _ => Action::None,
        };
        return (action, false);
    }

    // Ctrl-chord bindings.
    if ctrl {
        let action = match key.code {
            KeyCode::Char('v') => Action::VSplit,
            KeyCode::Char('x') => Action::HSplit,
            KeyCode::Char(']') => Action::CdInto,
            _ => Action::None,
        };
        return (action, false);
    }

    match key.code {
        KeyCode::Char('g') => return (Action::None, true), // enter g-prefix

        KeyCode::Char('q') => (Action::Quit, false),

        KeyCode::Char('j') | KeyCode::Down => (Action::Down, false),
        KeyCode::Char('k') | KeyCode::Up => (Action::Up, false),
        KeyCode::Char('K') => (Action::FirstSibling, false),
        KeyCode::Char('J') => (Action::LastSibling, false),

        KeyCode::Enter | KeyCode::Char('o') => (Action::OpenOrToggle, false),
        KeyCode::Char('l') | KeyCode::Right => (Action::Expand, false),
        KeyCode::Char('h') | KeyCode::Backspace | KeyCode::Left => {
            (Action::CollapseOrParent, false)
        }
        KeyCode::Char('P') => (Action::CursorParent, false),
        KeyCode::Char('-') => (Action::RootParent, false),
        KeyCode::Char('E') => (Action::ExpandAll, false),
        KeyCode::Char('W') => (Action::CollapseAll, false),

        KeyCode::Tab => (Action::Preview, false),
        KeyCode::Char('s') => (Action::SystemOpen, false),

        KeyCode::Char('a') => (Action::Create, false),
        KeyCode::Char('d') | KeyCode::Delete => (Action::Delete, false),
        KeyCode::Char('D') => (Action::Trash, false),
        KeyCode::Char('r') => (Action::Rename, false),
        KeyCode::Char('e') => (Action::RenameBasename, false),
        KeyCode::Char('x') => (Action::Cut, false),
        KeyCode::Char('c') => (Action::Copy, false),
        KeyCode::Char('p') => (Action::Paste, false),
        KeyCode::Char('y') => (Action::CopyFilename, false),
        KeyCode::Char('Y') => (Action::CopyRelpath, false),

        KeyCode::Char('.') => (Action::ToggleHidden, false),
        KeyCode::Char('I') => (Action::ToggleIgnored, false),
        KeyCode::Char('R') => (Action::Refresh, false),

        _ => (Action::None, false),
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
        assert_eq!(resolve(key('j'), false).0, Action::Down);
        assert_eq!(resolve(key('a'), false).0, Action::Create);
        assert_eq!(resolve(ctrl('v'), false).0, Action::VSplit);
    }

    #[test]
    fn g_prefix() {
        let (action, pending) = resolve(key('g'), false);
        assert_eq!(action, Action::None);
        assert!(pending);
        assert_eq!(resolve(key('y'), true).0, Action::CopyAbspath);
        assert_eq!(resolve(key('?'), true).0, Action::Help);
    }
}
