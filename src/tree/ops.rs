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
/// moves into `~/.Trash` (macOS). NEVER falls back to a permanent remove: the
/// user asked for a recoverable operation, so when no trash mechanism exists
/// this fails and the caller reports it — a deliberate delete is a different
/// keybinding.
pub fn trash(path: &Path) -> io::Result<()> {
    trash_impl(path, which("trash"), std::env::var_os("HOME"))
}

fn trash_impl(path: &Path, has_cli: bool, home: Option<std::ffi::OsString>) -> io::Result<()> {
    if has_cli {
        let status = Command::new("trash").arg(path).status()?;
        if status.success() {
            return Ok(());
        }
        // A failing CLI is not fatal: the ~/.Trash rename below is an
        // equivalent mechanism. Only when BOTH are unavailable do we error.
    }
    if let Some(home) = home {
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
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no trash mechanism (install the `trash` CLI or create ~/.Trash); use delete for permanent removal",
    ))
}

/// Rename / move a path. Refuses to replace an existing destination — the
/// caller surfaces the error and the user picks a different name. (A bare
/// `fs::rename` silently destroys whatever lived at the destination.)
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    if fs::symlink_metadata(to).is_ok() {
        // On case-insensitive filesystems (macOS default) a case-only rename
        // of the SAME file ("foo" -> "Foo") also finds metadata at `to`;
        // canonicalizing both reveals whether it's genuinely the same inode
        // path, which fs::rename handles correctly.
        let same_file = match (fs::canonicalize(from), fs::canonicalize(to)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if !same_file {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", to.display()),
            ));
        }
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to)
}

/// Recursively copy `from` to `to`. Refuses a destination inside the source —
/// copying a directory into itself otherwise recurses over its own output,
/// nesting copies until PATH_MAX and flooding the disk.
pub fn copy(from: &Path, to: &Path) -> io::Result<()> {
    let from_real = fs::canonicalize(from)?;
    let to_real = canonicalize_lenient(to);
    if to_real.starts_with(&from_real) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot copy a directory into itself",
        ));
    }
    copy_inner(from, to)
}

fn copy_inner(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    // Recreate a symlink as a symlink. fs::symlink_metadata does not follow
    // links, so this branch is reached for a symlink whether it points at a
    // file or a directory; fs::copy would instead FOLLOW it — materializing a
    // full byte copy of the target (duplicating a symlinked 2GB asset) or
    // failing with EISDIR for a link to a directory.
    if meta.file_type().is_symlink() {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        {
            return std::os::unix::fs::symlink(&target, to);
        }
        #[cfg(not(unix))]
        {
            return fs::copy(from, to).map(|_| ());
        }
    }
    if meta.is_dir() {
        fs::create_dir_all(to)?;
        // Snapshot the listing before recursing: a live read_dir handle can
        // observe entries the copy itself creates (defense in depth on top of
        // the containment check above).
        let entries: Vec<_> = fs::read_dir(from)?.collect::<Result<_, _>>()?;
        for entry in entries {
            let child_to = to.join(entry.file_name());
            copy_inner(&entry.path(), &child_to)?;
        }
        Ok(())
    } else {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to).map(|_| ())
    }
}

/// Canonicalize a path that may not exist yet: resolve the deepest existing
/// ancestor and re-join the remaining components.
fn canonicalize_lenient(p: &Path) -> PathBuf {
    for anc in p.ancestors().skip(1) {
        if let Ok(real) = fs::canonicalize(anc) {
            if let Ok(rest) = p.strip_prefix(anc) {
                return real.join(rest);
            }
        }
    }
    p.to_path_buf()
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

    fn tmpdir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("treelix-ops-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::canonicalize(&d).unwrap()
    }

    #[test]
    fn copy_into_itself_is_rejected() {
        // Regression: copy(/d/src, /d/src/src) recursed over its own output,
        // nesting copies until PATH_MAX and flooding the disk.
        let d = tmpdir("copy-self");
        fs::create_dir(d.join("src")).unwrap();
        fs::write(d.join("src/a.txt"), b"x").unwrap();
        let err = copy(&d.join("src"), &d.join("src/src")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // Deeper descendant too.
        fs::create_dir(d.join("src/sub")).unwrap();
        let err = copy(&d.join("src"), &d.join("src/sub/src")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // A sibling destination still works.
        copy(&d.join("src"), &d.join("dst")).unwrap();
        assert!(d.join("dst/a.txt").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn copy_preserves_symlinks_instead_of_dereferencing() {
        // Regression: fs::copy followed the link — duplicating the target's
        // bytes for a file link, or failing EISDIR for a dir link. Copy must
        // recreate the link itself.
        let d = tmpdir("copy-symlink");
        fs::create_dir(d.join("realdir")).unwrap();
        fs::write(d.join("realdir/inner.txt"), b"data").unwrap();
        fs::write(d.join("target.txt"), b"payload").unwrap();
        std::os::unix::fs::symlink(d.join("target.txt"), d.join("link_file")).unwrap();
        std::os::unix::fs::symlink(d.join("realdir"), d.join("link_dir")).unwrap();
        // Copy a directory containing both kinds of symlink.
        fs::create_dir(d.join("bundle")).unwrap();
        fs::rename(d.join("link_file"), d.join("bundle/link_file")).unwrap();
        fs::rename(d.join("link_dir"), d.join("bundle/link_dir")).unwrap();
        copy(&d.join("bundle"), &d.join("bundle_copy")).unwrap();
        // Both copied entries are still symlinks, not materialized copies.
        assert!(fs::symlink_metadata(d.join("bundle_copy/link_file"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(fs::symlink_metadata(d.join("bundle_copy/link_dir"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(d.join("bundle_copy/link_file")).unwrap(),
            d.join("target.txt")
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rename_refuses_to_replace_existing() {
        // Regression: a bare fs::rename silently destroyed the destination.
        let d = tmpdir("rename-exists");
        fs::write(d.join("notes.md"), b"keep me").unwrap();
        fs::write(d.join("draft.md"), b"draft").unwrap();
        let err = rename(&d.join("draft.md"), &d.join("notes.md")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(d.join("notes.md")).unwrap(), b"keep me");
        // A dangling symlink at the destination also counts as occupied.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join("gone"), d.join("dangling")).unwrap();
            let err = rename(&d.join("draft.md"), &d.join("dangling")).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        }
        // Plain rename to a fresh name still works.
        rename(&d.join("draft.md"), &d.join("final.md")).unwrap();
        assert!(d.join("final.md").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn trash_never_permanently_removes() {
        // Regression: with no `trash` CLI and no ~/.Trash, trash() fell back
        // to fs::remove_dir_all while reporting success. It must error out
        // and leave the path untouched instead. Exercised via trash_impl so
        // no process-global env vars are mutated (tests run in parallel).
        let d = tmpdir("trash-fallback");
        fs::create_dir(d.join("precious")).unwrap();
        fs::write(d.join("precious/data.txt"), b"x").unwrap();
        // No CLI, HOME without a .Trash dir.
        let fakehome = d.join("fakehome");
        fs::create_dir(&fakehome).unwrap();
        let res = trash_impl(&d.join("precious"), false, Some(fakehome.into_os_string()));
        assert!(res.is_err(), "no trash mechanism must be an error");
        assert!(d.join("precious/data.txt").exists(), "nothing may be deleted");
        // No CLI, no HOME at all.
        let res = trash_impl(&d.join("precious"), false, None);
        assert!(res.is_err());
        assert!(d.join("precious/data.txt").exists());
        // With a real .Trash dir the rename path still works.
        let home2 = d.join("home2");
        fs::create_dir_all(home2.join(".Trash")).unwrap();
        trash_impl(&d.join("precious"), false, Some(home2.clone().into_os_string())).unwrap();
        assert!(home2.join(".Trash/precious/data.txt").exists(), "moved into trash");
        let _ = fs::remove_dir_all(&d);
    }

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
