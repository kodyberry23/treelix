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
                node.children = read_dir_sorted(&node.path);
                node.loaded = true;
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
        // chain renders as one line.
        if self.group_empty {
            let mut cur = path.to_path_buf();
            loop {
                let next = match self.root.find_mut(&cur) {
                    Some(n) if n.is_dir() && n.children.len() == 1 && n.children[0].is_dir() => {
                        Some(n.children[0].path.clone())
                    }
                    _ => None,
                };
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

    fn do_expand(&mut self, path: &Path) {
        let needs_load = matches!(self.root.find_mut(path), Some(n) if n.is_dir() && !n.loaded);
        if needs_load {
            self.load_children(path);
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
                    } else if best.is_none() {
                        best = Some(s);
                    }
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

    fn emit(&self, node: &Node, ancestor_last: &mut Vec<bool>, opts: &ViewOptions, out: &mut Vec<Row>) {
        let (name, deepest) = self.group_chain(node, opts);
        let depth = ancestor_last.len().saturating_sub(1);
        let has_children =
            deepest.is_dir() && (!deepest.loaded || deepest.children.iter().any(|c| self.is_visible(c, opts)));
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
        let mut v: Vec<&Node> = dir.children.iter().filter(|c| self.is_visible(c, opts)).collect();
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

    /// All paths (files and dirs) in the loaded tree, for live-filter matching.
    pub fn all_paths(&self) -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        fn walk(n: &Node, out: &mut Vec<(PathBuf, String)>) {
            for c in &n.children {
                out.push((c.path.clone(), c.name.clone()));
                walk(c, out);
            }
        }
        walk(&self.root, &mut out);
        out
    }
}

fn sort_refs(v: &mut [&Node], mode: SortMode, files_first: bool) {
    use std::cmp::Ordering;
    v.sort_by(|a, b| {
        let ad = a.is_dir();
        let bd = b.is_dir();
        if ad != bd {
            return if files_first {
                if ad { Ordering::Greater } else { Ordering::Less }
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
    });
}

fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

/// Read a directory and return children as nodes (dirs first, name-sorted).
pub fn read_dir_sorted(dir: &Path) -> Vec<Node> {
    let mut nodes = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return nodes;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let symlink_meta = fs::symlink_metadata(&path).ok();
        let is_symlink = symlink_meta.as_ref().map_or(false, |m| m.is_symlink());

        let (kind, link_to) = if is_symlink {
            let target = fs::read_link(&path).ok();
            (NodeKind::Symlink { to_dir: meta.is_dir() }, target)
        } else if meta.is_dir() {
            (NodeKind::Directory, None)
        } else {
            (NodeKind::File, None)
        };

        let executable = is_executable(&meta);
        let mut node = Node::new(path, kind, executable, link_to);
        node.len = if meta.is_file() { meta.len() } else { 0 };
        node.mtime = meta.modified().ok();
        nodes.push(node);
    }
    let mut refs: Vec<&Node> = nodes.iter().collect();
    sort_refs(&mut refs, SortMode::Name, false);
    let order: Vec<PathBuf> = refs.iter().map(|n| n.path.clone()).collect();
    nodes.sort_by_key(|n| order.iter().position(|p| p == &n.path).unwrap_or(0));
    nodes
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
        let base = std::env::temp_dir().join(format!("treelix-test-{}-{}", std::process::id(), label));
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
        let data = GitData { toplevel: Some(d.clone()), statuses };
        tree.apply_git(&data);

        let mut opts = ViewOptions::default();
        assert!(!tree.flatten(&opts).iter().any(|r| r.name == "build"));
        assert!(tree.flatten(&opts).iter().any(|r| r.name == "keep.txt"));
        opts.show_ignored = true;
        assert!(tree.flatten(&opts).iter().any(|r| r.name == "build"));
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
    fn group_empty_chain() {
        let d = tmpdir("group");
        fs::create_dir_all(d.join("a/b/c")).unwrap();
        fs::write(d.join("a/b/c/file.txt"), b"x").unwrap();
        let mut tree = Tree::new(d.clone());
        tree.group_empty = true;
        tree.expand(&d.join("a"));
        let opts = ViewOptions { group_empty: true, ..Default::default() };
        let rows = tree.flatten(&opts);
        // The a→b→c chain collapses into one row.
        assert!(rows.iter().any(|r| r.name == "a/b/c"), "rows: {:?}", rows.iter().map(|r| &r.name).collect::<Vec<_>>());
        assert!(rows.iter().any(|r| r.name == "file.txt"));
        let _ = fs::remove_dir_all(&d);
    }
}
