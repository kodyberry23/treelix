//! Git integration: shell out to the `git` CLI and parse porcelain status into
//! per-path status, mirroring nvim-tree's approach. Runs off the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A single effective git status category for a file (or the propagated
/// highest-priority status for a directory). Ordered by nvim-tree's icon
/// priority so `max` picks the dominant status when propagating to folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitStatus {
    /// Lowest priority — only shown when explicitly toggled on.
    Ignored,
    Untracked,
    Conflict,
    Deleted,
    Renamed,
    Dirty,
    Staged,
}

impl GitStatus {
    /// nvim-tree default glyphs.
    pub fn glyph(self) -> &'static str {
        match self {
            GitStatus::Staged => "✓",
            GitStatus::Dirty => "✗",
            GitStatus::Renamed => "➜",
            GitStatus::Deleted => "",
            GitStatus::Conflict => "",
            GitStatus::Untracked => "★",
            GitStatus::Ignored => "◌",
        }
    }
}

/// Classify a porcelain v1 two-character XY code into one effective status.
fn classify(x: u8, y: u8) -> GitStatus {
    // Merge conflicts / unmerged states.
    if x == b'U' || y == b'U' || (x == b'A' && y == b'A') || (x == b'D' && y == b'D') {
        return GitStatus::Conflict;
    }
    if x == b'?' && y == b'?' {
        return GitStatus::Untracked;
    }
    if x == b'!' && y == b'!' {
        return GitStatus::Ignored;
    }
    if x == b'R' || y == b'R' {
        return GitStatus::Renamed;
    }
    // Index (staged) changes take priority over worktree changes.
    if x != b' ' && x != b'?' {
        return GitStatus::Staged;
    }
    match y {
        b'D' => GitStatus::Deleted,
        _ => GitStatus::Dirty,
    }
}

/// Result of a git status scan for a working tree.
#[derive(Debug, Clone, Default)]
pub struct GitData {
    /// Repository top-level, if the root is inside a git repo.
    pub toplevel: Option<PathBuf>,
    /// Absolute path -> effective status.
    pub statuses: HashMap<PathBuf, GitStatus>,
}

/// Discover the git top-level for `root`, or `None` if not a repo.
pub fn toplevel(root: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Run `git status` for the repo containing `root` and return per-path statuses.
/// Returns an empty (toplevel: None) result when `root` is not a git repo.
pub fn scan(root: &Path) -> GitData {
    let Some(top) = toplevel(root) else {
        return GitData::default();
    };

    let out = Command::new("git")
        .arg("-C")
        .arg(&top)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            // `matching` (not the default `traditional`) collapses a fully
            // ignored directory into a single `dir/` entry instead of listing
            // every file inside it. With `traditional` + `--untracked-files=all`
            // git emits the *contents* of ignored dirs but not the dir itself,
            // so an ignored folder (node_modules/, dist/) is only recognized as
            // ignored once its children are loaded on expand — making it flash
            // in and then vanish. `matching` tags the directory up front while
            // still listing untracked files individually. (This mirrors
            // nvim-tree.)
            "--ignored=matching",
            "--untracked-files=all",
        ])
        .output();

    let mut statuses = HashMap::new();
    if let Ok(out) = out {
        if out.status.success() {
            parse_porcelain_z(&out.stdout, &top, &mut statuses);
        }
    }

    GitData {
        toplevel: Some(top),
        statuses,
    }
}

/// Parse NUL-separated `git status --porcelain=v1 -z` output.
///
/// Each record is `XY<space>PATH`. For renames/copies the record is followed by
/// a second NUL-terminated field holding the original path (which we skip).
fn parse_porcelain_z(buf: &[u8], top: &Path, out: &mut HashMap<PathBuf, GitStatus>) {
    let mut fields = buf.split(|&b| b == 0);
    while let Some(rec) = fields.next() {
        if rec.len() < 3 {
            continue;
        }
        let x = rec[0];
        let y = rec[1];
        // rec[2] is the separating space.
        let path_bytes = &rec[3..];
        let status = classify(x, y);

        // Renames/copies carry an extra "orig path" field after the NUL.
        if x == b'R' || y == b'R' || x == b'C' || y == b'C' {
            let _ = fields.next();
        }

        let rel = String::from_utf8_lossy(path_bytes);
        let rel = rel.trim_end_matches('/');
        out.insert(top.join(rel), status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_codes() {
        assert_eq!(classify(b'?', b'?'), GitStatus::Untracked);
        assert_eq!(classify(b'!', b'!'), GitStatus::Ignored);
        assert_eq!(classify(b' ', b'M'), GitStatus::Dirty);
        assert_eq!(classify(b'M', b' '), GitStatus::Staged);
        assert_eq!(classify(b'M', b'M'), GitStatus::Staged);
        assert_eq!(classify(b' ', b'D'), GitStatus::Deleted);
        assert_eq!(classify(b'R', b' '), GitStatus::Renamed);
        assert_eq!(classify(b'U', b'U'), GitStatus::Conflict);
        assert_eq!(classify(b'A', b' '), GitStatus::Staged);
    }

    #[test]
    fn priority_order() {
        assert!(GitStatus::Staged > GitStatus::Dirty);
        assert!(GitStatus::Dirty > GitStatus::Untracked);
        assert!(GitStatus::Untracked > GitStatus::Ignored);
    }

    #[test]
    fn parse_simple() {
        let top = Path::new("/repo");
        let mut map = HashMap::new();
        // " M file.txt\0?? new.rs\0"
        let buf = b" M file.txt\0?? new.rs\0";
        parse_porcelain_z(buf, top, &mut map);
        assert_eq!(
            map.get(Path::new("/repo/file.txt")),
            Some(&GitStatus::Dirty)
        );
        assert_eq!(
            map.get(Path::new("/repo/new.rs")),
            Some(&GitStatus::Untracked)
        );
    }

    #[test]
    fn ignored_directory_is_tagged_directly() {
        // Regression: a fully git-ignored directory must be reported as a single
        // `dir/` entry (so it's recognized as ignored without loading children),
        // while untracked files are still listed individually. This depends on
        // `--ignored=matching` in `scan()`.
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("treelix-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("node_modules/foo")).unwrap();
        std::fs::create_dir_all(dir.join("untracked_dir")).unwrap();
        std::fs::write(dir.join(".gitignore"), b"node_modules/\n").unwrap();
        std::fs::write(dir.join("node_modules/foo/index.js"), b"x").unwrap();
        std::fs::write(dir.join("untracked_dir/new.txt"), b"y").unwrap();
        std::fs::write(dir.join("tracked.txt"), b"z").unwrap();
        // Canonicalize: on macOS /tmp is a symlink to /private/tmp, and git
        // reports the canonical toplevel, so status keys use the real path.
        let dir = std::fs::canonicalize(&dir).unwrap();
        let git = |args: &[&str]| {
            Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap();
        };
        git(&["init", "-q"]);
        git(&["add", ".gitignore", "tracked.txt"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);

        let data = scan(&dir);
        // The ignored directory itself is tagged (not just its children).
        assert_eq!(
            data.statuses.get(&dir.join("node_modules")),
            Some(&GitStatus::Ignored),
            "ignored dir should be tagged directly; got {:?}",
            data.statuses
        );
        // Its contents are NOT listed individually (collapsed).
        assert!(!data
            .statuses
            .contains_key(&dir.join("node_modules/foo/index.js")));
        // Untracked files inside a non-ignored new dir are still per-file.
        assert_eq!(
            data.statuses.get(&dir.join("untracked_dir/new.txt")),
            Some(&GitStatus::Untracked)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rename_skips_orig() {
        let top = Path::new("/repo");
        let mut map = HashMap::new();
        // "R  new.txt\0old.txt\0 M other\0"
        let buf = b"R  new.txt\0old.txt\0 M other\0";
        parse_porcelain_z(buf, top, &mut map);
        assert_eq!(
            map.get(Path::new("/repo/new.txt")),
            Some(&GitStatus::Renamed)
        );
        assert_eq!(map.get(Path::new("/repo/other")), Some(&GitStatus::Dirty));
        assert!(!map.contains_key(Path::new("/repo/old.txt")));
    }
}
