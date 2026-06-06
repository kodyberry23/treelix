//! Filesystem operations behind the file-management keybindings.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Create a file or directory. A path ending in `/` (handled by the caller via
/// `is_dir`) creates a directory; parent directories are created as needed.
pub fn create(path: &Path, is_dir: bool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if is_dir {
        fs::create_dir_all(path)
    } else {
        if path.exists() {
            return Ok(());
        }
        fs::File::create(path).map(|_| ())
    }
}

/// Permanently remove a file or directory (recursive).
pub fn remove(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Send a path to the trash. Prefers the `trash` CLI if present; otherwise
/// moves into `~/.Trash` (macOS). Falls back to a permanent remove only if no
/// trash location can be determined.
pub fn trash(path: &Path) -> io::Result<()> {
    if which("trash") {
        let status = Command::new("trash").arg(path).status()?;
        if status.success() {
            return Ok(());
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let trash_dir = PathBuf::from(home).join(".Trash");
        if trash_dir.is_dir() {
            let name = path.file_name().unwrap_or_default();
            let mut dest = trash_dir.join(name);
            // Avoid clobbering an existing trashed item with the same name.
            let mut n = 1;
            while dest.exists() {
                let stem = path.file_name().unwrap_or_default().to_string_lossy();
                dest = trash_dir.join(format!("{stem} {n}"));
                n += 1;
            }
            return fs::rename(path, &dest);
        }
    }
    remove(path)
}

/// Rename / move a path.
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to)
}

/// Recursively copy `from` to `to`.
pub fn copy(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let child_to = to.join(entry.file_name());
            copy(&entry.path(), &child_to)?;
        }
        Ok(())
    } else {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to).map(|_| ())
    }
}

/// Compute a non-colliding destination path inside `dest_dir` for `src`.
pub fn paste_target(dest_dir: &Path, src: &Path) -> PathBuf {
    let name = src
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut candidate = dest_dir.join(&name);
    if !candidate.exists() {
        return candidate;
    }
    // Append "_copy", then "_copy_N".
    let (stem, ext) = split_name(&name);
    for n in 1..10_000 {
        let suffix = if n == 1 {
            "_copy".to_string()
        } else {
            format!("_copy_{n}")
        };
        let new_name = if ext.is_empty() {
            format!("{stem}{suffix}")
        } else {
            format!("{stem}{suffix}.{ext}")
        };
        candidate = dest_dir.join(new_name);
        if !candidate.exists() {
            break;
        }
    }
    candidate
}

fn split_name(name: &str) -> (String, String) {
    match name.rsplit_once('.') {
        // Treat dotfiles (".bashrc") as having no extension.
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), ext.to_string()),
        _ => (name.to_string(), String::new()),
    }
}

fn which(cmd: &str) -> bool {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if dir.join(cmd).is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_name_handles_dotfiles() {
        assert_eq!(split_name("foo.txt"), ("foo".into(), "txt".into()));
        assert_eq!(split_name(".bashrc"), (".bashrc".into(), "".into()));
        assert_eq!(split_name("Makefile"), ("Makefile".into(), "".into()));
    }

    #[test]
    fn paste_target_avoids_collision() {
        let d = std::env::temp_dir().join(format!("treelix-paste-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let src = d.join("a.txt");
        fs::write(&src, b"x").unwrap();
        // No collision: same name.
        let t1 = paste_target(&d.join("other"), &src);
        assert_eq!(t1.file_name().unwrap(), "a.txt");
        // Collision in same dir.
        let t2 = paste_target(&d, &src);
        assert_eq!(t2.file_name().unwrap(), "a_copy.txt");
        let _ = fs::remove_dir_all(&d);
    }
}
