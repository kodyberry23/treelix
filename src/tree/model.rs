//! Tree operations: lazy load, sort, flatten to visible rows with indent-marker
//! metadata, reveal-by-path, git application, group-empty collapsing, filtering,
//! and expand/collapse state snapshot+restore.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::git::{GitData, GitStatus};

use super::node::{Node, NodeKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    Modified,
    Extension,
    FileType,
}

impl SortMode {
    pub fn parse(s: &str) -> SortMode {
        match s.to_lowercase().as_str() {
            "modified" | "mtime" => SortMode::Modified,
            "extension" | "ext" => SortMode::Extension,
            "filetype" | "type" => SortMode::FileType,
            _ => SortMode::Name,
        }
    }
}

/// Per-render view options: filters, sorting, grouping.
pub struct ViewOptions<'a> {
    pub show_hidden: bool,
    pub show_ignored: bool,
    /// Show only git-changed (dirty) nodes.
    pub git_clean: bool,
    pub group_empty: bool,
    pub sort: SortMode,
    pub files_first: bool,
    /// Custom exclude patterns (substring match), applied when `custom_active`.
    pub exclude: &'a [String],
    pub custom_active: bool,
    /// Each node must be present in every set here to be visible (used for live
    /// filter, no_bookmark, no_buffer — app precomputes sets-with-ancestors).
    pub restricts: &'a [&'a HashSet<PathBuf>],
}

impl Default for ViewOptions<'_> {
    fn default() -> Self {
        ViewOptions {
            show_hidden: false,
            show_ignored: false,
            git_clean: false,
            group_empty: false,
            sort: SortMode::Name,
            files_first: false,
            exclude: &[],
            custom_active: false,
            restricts: &[],
        }
    }
}

/// A flattened, render-ready snapshot of one visible line.
#[derive(Debug, Clone)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub kind: NodeKind,
    pub depth: usize,
    pub expanded: bool,
    pub has_children: bool,
    pub executable: bool,
    pub git: Option<GitStatus>,
    pub link_to: Option<PathBuf>,
    /// For group-empty rows, the deepest directory in the chain (target for
    /// cd/create); `None` when not a grouped row.
    pub group_target: Option<PathBuf>,
    /// For each level (including this node, as the last element), whether the
    /// node at that level is the last among its siblings; drives indent glyphs.
    pub ancestor_last: Vec<bool>,
}

impl Row {
    /// Directory to act in for create/cd (deepest of a grouped chain).
    pub fn dir_target(&self) -> &Path {
        self.group_target.as_deref().unwrap_or(&self.path)
    }
}

pub struct Tree {
    pub root: Node,
    pub show_hidden: bool,
    pub show_ignored: bool,
    pub group_empty: bool,
}

impl Tree {
    pub fn new(root_path: PathBuf) -> Self {
        let mut root = Node::new(root_path, NodeKind::Directory, false, None);
        root.expanded = true;
        let mut tree = Tree {
            root,
            show_hidden: false,
            show_ignored: false,
            group_empty: false,
        };
        tree.load_children(&tree.root.path.clone());
        tree
    }

    /// Read children of the directory at `path` from disk, if not loaded.
    pub fn load_children(&mut self, path: &Path) {
        if let Some(node) = self.root.find_mut(path) {
            if node.is_dir() && !node.loaded {
                // Stat BEFORE reading: a change racing the read then looks
                // stale on the next expand instead of being missed.
                let mtime = dir_mtime(&node.path);
                // A read failure (permission denied, transient EMFILE) must
                // NOT mark the dir loaded-and-empty: that would freeze it as a
                // phantom-empty directory for the session (the mtime gate sees
                // ctime-only permission changes as Fresh and never retries).
                // Leave it unloaded so the next expand tries again.
                if let Some(children) = try_read_dir_sorted(&node.path) {
                    node.children = children;
                    node.loaded_mtime = mtime;
                    node.loaded = true;
                }
            }
        }
    }

    pub fn toggle(&mut self, path: &Path) {
        let expanded = matches!(self.root.find_mut(path), Some(n) if n.is_dir() && n.expanded);
        if expanded {
            self.collapse(path);
        } else {
            self.expand(path);
        }
    }

    pub fn expand(&mut self, path: &Path) {
        self.do_expand(path);
        // Group-empty: chain-expand through sole-child directories so the whole
        // chain renders as one line. This MUST follow the sole VISIBLE child
        // (honoring show_hidden / show_ignored), exactly as group_chain does at
        // render time — following the sole RAW child instead would either stop
        // early (a hidden sibling makes raw count > 1, leaving the visible sole
        // child unloaded and rendering as an empty expanded row) or descend
        // into an ignored node_modules the view then hides (a needless 100k
        // read).
        if self.group_empty {
            let mut cur = path.to_path_buf();
            loop {
                let next = self.sole_visible_child_dir(&cur);
                match next {
                    Some(child) => {
                        self.do_expand(&child);
                        cur = child;
                    }
                    None => break,
                }
            }
        }
    }

    /// The path of `dir`'s only visible child when that child is a directory
    /// and it is the sole visible entry — the structural test group_chain uses
    /// to collapse a chain into one row. Visibility here is the persistent
    /// toggles only (hidden dotfiles, git-ignored); the live-filter restricts
    /// don't change the on-disk chain structure.
    fn sole_visible_child_dir(&self, dir: &Path) -> Option<PathBuf> {
        let node = self.root.find(dir)?;
        if !node.is_dir() {
            return None;
        }
        let mut visible = node.children.iter().filter(|c| self.chain_visible(c));
        let first = visible.next()?;
        // Never chain-descend into a high-churn directory (node_modules,
        // target, ...). Its git-ignored status isn't applied yet at this point
        // (children were just loaded), so the name is the only reliable signal;
        // descending would read the whole tree the view immediately hides.
        if visible.next().is_none()
            && first.is_dir()
            && !crate::watcher::is_high_churn_name(&first.name)
        {
            Some(first.path.clone())
        } else {
            None
        }
    }

    /// Structural visibility for chain-expansion: excludes hidden dotfiles
    /// (unless show_hidden) and git-ignored entries (unless show_ignored).
    fn chain_visible(&self, node: &Node) -> bool {
        if !self.show_hidden && node.is_hidden() {
            return false;
        }
        if !self.show_ignored && node.git == Some(GitStatus::Ignored) {
            return false;
        }
        true
    }

    fn do_expand(&mut self, path: &Path) {
        // Loaded directories are re-synced with disk on expand when stale (the
        // merge in refresh_dir preserves child expansion state and cached
        // subtrees). Trusting the cache unconditionally would make any
        // filesystem-watcher gap permanent: a change missed while the
        // directory sat collapsed would never surface, because nothing else
        // re-reads a loaded dir. Staleness is decided by comparing the dir's
        // current mtime against the one observed at load — one stat, so the
        // common unchanged case stays O(1) instead of a full re-read per
        // toggle (which stalls on huge or network directories).
        enum State {
            Unloaded,
            Fresh,
            Stale,
        }
        let state = match self.root.find_mut(path) {
            Some(n) if n.is_dir() => {
                if !n.loaded {
                    Some(State::Unloaded)
                } else if n.loaded_mtime.is_some() && dir_mtime(&n.path) == n.loaded_mtime {
                    Some(State::Fresh)
                } else {
                    Some(State::Stale)
                }
            }
            _ => None,
        };
        match state {
            Some(State::Unloaded) => self.load_children(path),
            Some(State::Stale) => {
                self.refresh_dir(path);
            }
            Some(State::Fresh) | None => {}
        }
        if let Some(node) = self.root.find_mut(path) {
            if node.is_dir() {
                node.expanded = true;
            }
        }
    }

    pub fn collapse(&mut self, path: &Path) {
        if let Some(node) = self.root.find_mut(path) {
            node.expanded = false;
        }
    }

    /// Recursively expand every directory (bounded by what's on disk).
    pub fn expand_all(&mut self) {
        let mut stack = vec![self.root.path.clone()];
        while let Some(p) = stack.pop() {
            self.do_expand(&p);
            if let Some(node) = self.root.find_mut(&p) {
                for c in &node.children {
                    if c.is_dir() {
                        stack.push(c.path.clone());
                    }
                }
            }
        }
    }

    pub fn collapse_all(&mut self) {
        fn walk(n: &mut Node, is_root: bool) {
            if !is_root {
                n.expanded = false;
            }
            for c in &mut n.children {
                walk(c, false);
            }
        }
        walk(&mut self.root, true);
    }

    /// Re-root the tree at `path` (cd into). Preserves filter toggles.
    pub fn set_root(&mut self, path: PathBuf) {
        let mut root = Node::new(path, NodeKind::Directory, false, None);
        root.expanded = true;
        self.root = root;
        self.load_children(&self.root.path.clone());
    }

    /// Collect absolute paths of all currently-expanded directories.
    pub fn collect_expanded(&self) -> HashSet<PathBuf> {
        let mut set = HashSet::new();
        fn walk(n: &Node, set: &mut HashSet<PathBuf>) {
            if n.is_dir() && n.expanded {
                set.insert(n.path.clone());
                for c in &n.children {
                    walk(c, set);
                }
            }
        }
        walk(&self.root, &mut set);
        set
    }

    /// Rebuild children from disk for the root and every still-existing
    /// previously-expanded directory. Cheap: only reads expanded dirs.
    pub fn reload_preserving(&mut self, expanded: &HashSet<PathBuf>) {
        let root_path = self.root.path.clone();
        let mut root = Node::new(root_path, NodeKind::Directory, false, None);
        root.expanded = true;
        self.root = root;
        self.load_children(&self.root.path.clone());

        let mut paths: Vec<&PathBuf> = expanded.iter().collect();
        paths.sort_by_key(|p| p.components().count());
        for p in paths {
            if p.exists() {
                self.do_expand(p);
            }
        }
    }

    /// Re-scan a single loaded directory in place, preserving the expansion
    /// state and cached subtrees of children that still exist. Returns true if
    /// the directory was present and loaded (and thus refreshed).
    ///
    /// This is the targeted-reload primitive for filesystem-watcher events: only
    /// the directory whose contents changed is re-read, instead of re-reading
    /// every expanded directory (which is what a full rebuild does). Mirrors
    /// nvim-tree's per-directory `reload`.
    pub fn refresh_dir(&mut self, dir: &Path) -> bool {
        let Some(node) = self.root.find_mut(dir) else {
            return false;
        };
        if !node.is_dir() || !node.loaded {
            return false;
        }
        // A vanished or unreadable directory must not wipe the cached listing
        // to "empty" — keep what we had; the parent's own refresh prunes the
        // node once the deletion event lands.
        let mtime = dir_mtime(&node.path);
        let Some(fresh) = try_read_dir_sorted(&node.path) else {
            return false;
        };
        node.loaded_mtime = mtime;
        // Index existing children so survivors keep their expansion/subtree.
        let mut old: HashMap<PathBuf, Node> =
            node.children.drain(..).map(|c| (c.path.clone(), c)).collect();
        let mut merged = Vec::with_capacity(fresh.len());
        for f in fresh {
            match old.remove(&f.path) {
                // Same path, SAME kind and (for symlinks) same target: keep the
                // existing node so its expansion state and loaded children
                // survive; refresh metadata. A real dir replaced by a
                // symlink-to-dir, or a re-pointed symlink, both satisfy the old
                // `is_dir() && is_dir()` test but must NOT keep the stale kind,
                // arrow, or cached subtree — fall through to the fresh node.
                Some(mut existing)
                    if existing.is_dir()
                        && f.is_dir()
                        && existing.kind == f.kind
                        && existing.link_to == f.link_to =>
                {
                    existing.executable = f.executable;
                    existing.mtime = f.mtime;
                    existing.len = f.len;
                    merged.push(existing);
                }
                // New entry, kind/target changed, removed-and-recreated as a
                // different kind, or a file whose size/mtime we want refreshed:
                // take the fresh node (empty subtree, reloaded lazily).
                _ => merged.push(f),
            }
        }
        node.children = merged;
        node.loaded = true;
        true
    }

    /// Expand all ancestors of `target` so it becomes visible.
    pub fn reveal(&mut self, target: &Path) -> bool {
        if !target.starts_with(&self.root.path) {
            return false;
        }
        let mut cur = self.root.path.clone();
        if let Ok(rel) = target.strip_prefix(&self.root.path) {
            for comp in rel.components() {
                self.do_expand(&cur);
                cur = cur.join(comp);
                if cur == *target {
                    break;
                }
            }
        }
        target.exists()
    }

    /// Apply git statuses to file nodes and propagate to directories.
    pub fn apply_git(&mut self, data: &GitData) {
        fn walk(n: &mut Node, statuses: &HashMap<PathBuf, GitStatus>) -> Option<GitStatus> {
            if !n.is_dir() {
                n.git = statuses.get(&n.path).copied();
                return n.git;
            }
            let mut best = statuses.get(&n.path).copied();
            for c in &mut n.children {
                if let Some(s) = walk(c, statuses) {
                    if s != GitStatus::Ignored {
                        best = Some(best.map_or(s, |b| b.max(s)));
                    }
                    // Never let a child's Ignored status change the parent's; a
                    // directory is Ignored only if git reported its OWN path as
                    // Ignored (captured in `best` from statuses.get(&n.path) above).
                }
            }
            n.git = best;
            best
        }
        walk(&mut self.root, &data.statuses);
    }

    /// Flatten the visible tree into rows.
    pub fn flatten(&self, opts: &ViewOptions) -> Vec<Row> {
        let mut out = Vec::new();
        if self.root.expanded {
            let vis = self.visible_sorted(&self.root, opts);
            let last = vis.len().saturating_sub(1);
            let mut anc = Vec::new();
            for (i, c) in vis.iter().enumerate() {
                anc.push(i == last);
                self.emit(c, &mut anc, opts, &mut out);
                anc.pop();
            }
        }
        out
    }

    fn emit(
        &self,
        node: &Node,
        ancestor_last: &mut Vec<bool>,
        opts: &ViewOptions,
        out: &mut Vec<Row>,
    ) {
        let (name, deepest) = self.group_chain(node, opts);
        let depth = ancestor_last.len().saturating_sub(1);
        let has_children = deepest.is_dir()
            && (!deepest.loaded || deepest.children.iter().any(|c| self.is_visible(c, opts)));
        let group_target = if deepest.path != node.path {
            Some(deepest.path.clone())
        } else {
            None
        };

        out.push(Row {
            path: node.path.clone(),
            name,
            kind: node.kind,
            depth,
            expanded: node.expanded,
            has_children,
            executable: node.executable,
            git: node.git,
            link_to: node.link_to.clone(),
            group_target,
            ancestor_last: ancestor_last.clone(),
        });

        if node.expanded && deepest.is_dir() {
            let vis = self.visible_sorted(deepest, opts);
            let last = vis.len().saturating_sub(1);
            for (i, c) in vis.iter().enumerate() {
                ancestor_last.push(i == last);
                self.emit(c, ancestor_last, opts, out);
                ancestor_last.pop();
            }
        }
    }

    /// Follow a chain of sole-child directories, returning the joined display
    /// name and the deepest directory.
    fn group_chain<'t>(&'t self, node: &'t Node, opts: &ViewOptions) -> (String, &'t Node) {
        let mut name = node.name.clone();
        let mut cur = node;
        while opts.group_empty && cur.expanded {
            let vis = self.visible_sorted(cur, opts);
            if vis.len() == 1 && vis[0].is_dir() {
                cur = vis[0];
                name = format!("{name}/{}", cur.name);
            } else {
                break;
            }
        }
        (name, cur)
    }

    fn visible_sorted<'t>(&'t self, dir: &'t Node, opts: &ViewOptions) -> Vec<&'t Node> {
        let mut v: Vec<&Node> = dir
            .children
            .iter()
            .filter(|c| self.is_visible(c, opts))
            .collect();
        sort_refs(&mut v, opts.sort, opts.files_first);
        v
    }

    fn is_visible(&self, node: &Node, opts: &ViewOptions) -> bool {
        if !opts.show_hidden && node.is_hidden() {
            return false;
        }
        if !opts.show_ignored && node.git == Some(GitStatus::Ignored) {
            return false;
        }
        if opts.git_clean && (node.git.is_none() || node.git == Some(GitStatus::Ignored)) {
            return false;
        }
        if opts.custom_active && opts.exclude.iter().any(|p| node.name.contains(p.as_str())) {
            return false;
        }
        for set in opts.restricts {
            if !set.contains(&node.path) {
                return false;
            }
        }
        true
    }

}

fn node_cmp(a: &Node, b: &Node, mode: SortMode, files_first: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ad = a.is_dir();
    let bd = b.is_dir();
    if ad != bd {
        return if files_first {
            if ad {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        } else if ad {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    let by_name = || a.name.to_lowercase().cmp(&b.name.to_lowercase());
    match mode {
        SortMode::Name => by_name(),
        SortMode::Modified => b.mtime.cmp(&a.mtime).then_with(by_name),
        SortMode::Extension | SortMode::FileType => {
            ext_of(&a.name).cmp(&ext_of(&b.name)).then_with(by_name)
        }
    }
}

fn sort_refs(v: &mut [&Node], mode: SortMode, files_first: bool) {
    v.sort_by(|a, b| node_cmp(a, b, mode, files_first));
}

fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

/// The directory's own mtime, used as the staleness signal for cached listings.
fn dir_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    fs::metadata(dir).and_then(|m| m.modified()).ok()
}

/// Read a directory and return children as nodes (dirs first, name-sorted).
/// Returns `None` when the directory is unreadable, distinct from an empty
/// `Some(vec![])` — the caller must not cache "unreadable" as "empty".
fn try_read_dir_sorted(dir: &Path) -> Option<Vec<Node>> {
    let mut nodes = Vec::new();
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        // `DirEntry::metadata()` is a single `fstatat(AT_SYMLINK_NOFOLLOW)` (an
        // lstat — it does NOT follow symlinks), so one syscall yields type, size,
        // mtime and permission bits for the entry itself. We derive the kind from
        // it rather than calling `file_type()` separately: that avoids the
        // double-stat that `file_type()` would incur on filesystems returning
        // `DT_UNKNOWN`. Only symlinks need an extra stat (to learn if the target
        // is a directory). This is the std minimum: 1 stat/entry, 2 for symlinks.
        let Ok(meta) = entry.metadata() else { continue };
        let ft = meta.file_type();

        let (kind, link_to) = if ft.is_symlink() {
            let target = fs::read_link(&path).ok();
            let to_dir = fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
            (NodeKind::Symlink { to_dir }, target)
        } else if ft.is_dir() {
            (NodeKind::Directory, None)
        } else {
            (NodeKind::File, None)
        };

        let executable = is_executable(&meta);
        let mut node = Node::new(path, kind, executable, link_to);
        node.len = if ft.is_file() { meta.len() } else { 0 };
        node.mtime = meta.modified().ok();
        nodes.push(node);
    }
    nodes.sort_by(|a, b| node_cmp(a, b, SortMode::Name, false));
    Some(nodes)
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(label: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("treelix-test-{}-{}", std::process::id(), label));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn dirs_sort_before_files() {
        let d = tmpdir("sort");
        fs::create_dir(d.join("zdir")).unwrap();
        fs::write(d.join("afile"), b"x").unwrap();
        let tree = Tree::new(d.clone());
        let rows = tree.flatten(&ViewOptions::default());
        assert_eq!(rows[0].name, "zdir");
        assert_eq!(rows[1].name, "afile");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn flatten_and_reveal() {
        let d = tmpdir("reveal");
        fs::create_dir_all(d.join("sub/inner")).unwrap();
        fs::write(d.join("sub/inner/deep.txt"), b"x").unwrap();
        let mut tree = Tree::new(d.clone());
        let opts = ViewOptions::default();
        let rows = tree.flatten(&opts);
        assert!(rows.iter().any(|r| r.name == "sub"));
        assert!(!rows.iter().any(|r| r.name == "deep.txt"));

        let target = d.join("sub/inner/deep.txt");
        assert!(tree.reveal(&target));
        let rows = tree.flatten(&opts);
        assert!(rows.iter().any(|r| r.name == "deep.txt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ignored_filter() {
        let d = tmpdir("ignored");
        fs::create_dir(d.join("build")).unwrap();
        fs::write(d.join("keep.txt"), b"x").unwrap();
        let mut tree = Tree::new(d.clone());

        let mut statuses = HashMap::new();
        statuses.insert(d.join("build"), GitStatus::Ignored);
        let data = GitData {
            toplevel: Some(d.clone()),
            statuses,
        };
        tree.apply_git(&data);

        let mut opts = ViewOptions::default();
        assert!(!tree.flatten(&opts).iter().any(|r| r.name == "build"));
        assert!(tree.flatten(&opts).iter().any(|r| r.name == "keep.txt"));
        opts.show_ignored = true;
        assert!(tree.flatten(&opts).iter().any(|r| r.name == "build"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ignored_child_does_not_hide_parent_after_expand() {
        // Regression: a non-ignored directory whose ONLY git-status-bearing
        // descendants are ignored leaf dirs (e.g. a package containing only
        // committed-clean files plus node_modules/ and dist/) must NOT be
        // marked or hidden as ignored. Only the leaf dirs that git reports as
        // ignored by their OWN path stay hidden. This must hold even after the
        // package's children are lazily loaded via expand (which is what made
        // packages/ retroactively vanish before the fix).
        let d = tmpdir("ignored-child");
        fs::create_dir_all(d.join("packages/pkg-a/node_modules")).unwrap();
        fs::create_dir_all(d.join("packages/pkg-a/dist")).unwrap();
        fs::write(d.join("packages/pkg-a/node_modules/index.js"), b"x").unwrap();
        fs::write(d.join("packages/pkg-a/dist/out.js"), b"x").unwrap();
        fs::write(d.join("packages/pkg-a/package.json"), b"{}").unwrap();
        // Canonicalize: on macOS /tmp is a symlink to /private/tmp.
        let d = fs::canonicalize(&d).unwrap();

        let mut tree = Tree::new(d.clone());
        // Force the ignored grandchildren to be materialized as tree nodes,
        // exactly as an interactive expand of pkg-a would.
        tree.expand(&d.join("packages"));
        tree.expand(&d.join("packages/pkg-a"));

        // Git reports ONLY the leaf ignored dirs by their own path (this is
        // what `--ignored=matching` produces); never packages/ or pkg-a/.
        let mut statuses = HashMap::new();
        statuses.insert(d.join("packages/pkg-a/node_modules"), GitStatus::Ignored);
        statuses.insert(d.join("packages/pkg-a/dist"), GitStatus::Ignored);
        let data = GitData {
            toplevel: Some(d.clone()),
            statuses,
        };
        tree.apply_git(&data);

        // The non-ignored parents must keep git == None (not propagated Ignored).
        let pkg_a_git = tree.root.find_mut(&d.join("packages/pkg-a")).unwrap().git;
        assert_eq!(pkg_a_git, None, "pkg-a must not inherit child Ignored status");
        let packages_git = tree.root.find_mut(&d.join("packages")).unwrap().git;
        assert_eq!(packages_git, None, "packages/ must not inherit child Ignored status");
        // The leaf dir git reported as ignored keeps its own status.
        let nm_git = tree
            .root
            .find_mut(&d.join("packages/pkg-a/node_modules"))
            .unwrap()
            .git;
        assert_eq!(nm_git, Some(GitStatus::Ignored));

        // With show_ignored off: parents visible, ignored leaves hidden.
        let opts = ViewOptions::default();
        let rows = tree.flatten(&opts);
        let names: Vec<&String> = rows.iter().map(|r| &r.name).collect();
        assert!(names.iter().any(|n| *n == "packages"), "packages/ stays visible");
        assert!(names.iter().any(|n| *n == "pkg-a"), "pkg-a stays visible");
        assert!(!names.iter().any(|n| *n == "node_modules"), "node_modules hidden");
        assert!(!names.iter().any(|n| *n == "dist"), "dist hidden");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn hidden_filter() {
        let d = tmpdir("hidden");
        fs::write(d.join(".secret"), b"x").unwrap();
        fs::write(d.join("visible"), b"x").unwrap();
        let tree = Tree::new(d.clone());
        let mut opts = ViewOptions::default();
        assert!(!tree.flatten(&opts).iter().any(|r| r.name == ".secret"));
        opts.show_hidden = true;
        assert!(tree.flatten(&opts).iter().any(|r| r.name == ".secret"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn refresh_dir_merges_and_preserves_expansion() {
        let d = tmpdir("refresh");
        fs::create_dir_all(d.join("sub/inner")).unwrap();
        fs::write(d.join("sub/inner/deep.txt"), b"x").unwrap();
        fs::write(d.join("old.txt"), b"x").unwrap();
        let mut tree = Tree::new(d.clone());
        // Expand sub/inner so it has a loaded subtree to preserve.
        tree.expand(&d.join("sub"));
        tree.expand(&d.join("sub/inner"));
        assert!(tree.collect_expanded().contains(&d.join("sub/inner")));

        // Mutate the root dir on disk: add a new file, remove an old one.
        fs::write(d.join("new.txt"), b"x").unwrap();
        fs::remove_file(d.join("old.txt")).unwrap();

        // Targeted refresh of the root only.
        assert!(tree.refresh_dir(&d));

        let rows = tree.flatten(&ViewOptions::default());
        let names: Vec<&String> = rows.iter().map(|r| &r.name).collect();
        assert!(names.iter().any(|n| *n == "new.txt"), "new file should appear");
        assert!(!names.iter().any(|n| *n == "old.txt"), "removed file should be gone");
        // The previously-expanded sub/inner subtree must survive the merge.
        assert!(
            tree.collect_expanded().contains(&d.join("sub/inner")),
            "expansion state of untouched subtree should be preserved"
        );
        assert!(names.iter().any(|n| *n == "deep.txt"), "deep file still visible");

        // Refreshing an unloaded/absent dir is a no-op returning false.
        assert!(!tree.refresh_dir(&d.join("does-not-exist")));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn expand_rereads_loaded_dir_from_disk() {
        // Regression: a directory's children were read once (loaded=true) and
        // never again on expand, so any change the filesystem watcher missed
        // (event overflow, ignored-component filtering, races) stayed invisible
        // forever — "the dir shows up but not the files in it". Expand must
        // re-sync with disk while preserving child expansion state.
        let d = tmpdir("expand-reread");
        fs::create_dir_all(d.join("sub/inner")).unwrap();
        fs::write(d.join("sub/old.txt"), b"x").unwrap();
        let mut tree = Tree::new(d.clone());
        tree.expand(&d.join("sub"));
        tree.expand(&d.join("sub/inner"));
        tree.collapse(&d.join("sub"));

        // Mutate sub/ on disk with NO watcher notification.
        fs::write(d.join("sub/new.txt"), b"y").unwrap();
        fs::remove_file(d.join("sub/old.txt")).unwrap();

        tree.expand(&d.join("sub"));
        let rows = tree.flatten(&ViewOptions::default());
        let names: Vec<&String> = rows.iter().map(|r| &r.name).collect();
        assert!(names.iter().any(|n| *n == "new.txt"), "missed file appears on expand");
        assert!(!names.iter().any(|n| *n == "old.txt"), "missed deletion applied on expand");
        // inner survived the merge with its expansion state intact.
        assert!(
            tree.collect_expanded().contains(&d.join("sub/inner")),
            "child expansion state preserved across the re-read"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn refresh_dir_keeps_children_when_dir_unreadable() {
        // A directory deleted (or made unreadable) between the watcher event
        // and the re-read must not have its cached listing wiped to "empty" —
        // the parent's own refresh prunes the node once the deletion lands.
        let d = tmpdir("refresh-vanish");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/kept.txt"), b"x").unwrap();
        let mut tree = Tree::new(d.clone());
        tree.expand(&d.join("sub"));
        fs::remove_dir_all(d.join("sub")).unwrap();

        assert!(!tree.refresh_dir(&d.join("sub")), "unreadable dir: no refresh");
        let sub = tree.root.find_mut(&d.join("sub")).unwrap();
        assert!(
            sub.children.iter().any(|c| c.name == "kept.txt"),
            "cached listing preserved until the parent refresh prunes the node"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn group_empty_chain() {
        let d = tmpdir("group");
        fs::create_dir_all(d.join("a/b/c")).unwrap();
        fs::write(d.join("a/b/c/file.txt"), b"x").unwrap();
        let mut tree = Tree::new(d.clone());
        tree.group_empty = true;
        tree.expand(&d.join("a"));
        let opts = ViewOptions {
            group_empty: true,
            ..Default::default()
        };
        let rows = tree.flatten(&opts);
        // The a→b→c chain collapses into one row.
        assert!(
            rows.iter().any(|r| r.name == "a/b/c"),
            "rows: {:?}",
            rows.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert!(rows.iter().any(|r| r.name == "file.txt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn group_empty_chain_follows_visible_sole_child_past_hidden_sibling() {
        // Regression: the chain-expand used RAW child count, so a hidden
        // sibling (.DS_Store) beside the sole visible dir stopped the chain,
        // leaving that dir unloaded — group_chain then followed the visible
        // sole child at render time and produced an expanded row with NO
        // contents, hiding the file underneath.
        let d = tmpdir("group-hidden");
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::write(d.join("a/.DS_Store"), b"x").unwrap(); // hidden sibling of b
        fs::write(d.join("a/b/file.txt"), b"y").unwrap();
        let mut tree = Tree::new(d.clone());
        tree.group_empty = true;
        tree.expand(&d.join("a"));
        let opts = ViewOptions {
            group_empty: true,
            ..Default::default()
        };
        let rows = tree.flatten(&opts);
        let names: Vec<&String> = rows.iter().map(|r| &r.name).collect();
        assert!(
            names.iter().any(|n| *n == "a/b"),
            "a and its sole visible child b group into one row: {names:?}"
        );
        assert!(
            names.iter().any(|n| *n == "file.txt"),
            "the file under the grouped chain is reachable: {names:?}"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn group_empty_does_not_descend_into_high_churn_sole_child() {
        // A dir whose only child is node_modules must NOT chain-expand into it
        // (that read the whole ignored tree the view hides). The high-churn
        // NAME is the signal — git-ignored status isn't applied to freshly
        // loaded children at chain-expand time.
        let d = tmpdir("group-churn");
        fs::create_dir_all(d.join("pkg/node_modules/dep")).unwrap();
        fs::write(d.join("pkg/node_modules/dep/index.js"), b"x").unwrap();
        let mut tree = Tree::new(d.clone());
        tree.group_empty = true;
        tree.expand(&d.join("pkg"));
        let nm = tree.root.find(&d.join("pkg/node_modules")).unwrap();
        assert!(
            !nm.loaded,
            "high-churn sole child must not be chain-expanded/loaded"
        );
        let _ = fs::remove_dir_all(&d);
    }
}
