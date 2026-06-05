//! Tree operations: lazy load, sort (dirs first), flatten to visible rows with
//! indent-marker metadata, reveal-by-path, git application, and expand/collapse
//! state snapshot+restore (used to preserve UI state across reloads).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::git::{GitData, GitStatus};

use super::node::{Node, NodeKind};

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
    /// For each ancestor level, whether that ancestor was the last among its
    /// siblings (→ render blank) or not (→ render a vertical bar).
    pub ancestor_last: Vec<bool>,
    /// Whether this node itself is the last among its siblings.
    pub is_last: bool,
}

pub struct Tree {
    pub root: Node,
    pub show_hidden: bool,
    pub show_ignored: bool,
}

impl Tree {
    pub fn new(root_path: PathBuf) -> Self {
        let mut root = Node::new(root_path, NodeKind::Directory, false, None);
        root.expanded = true;
        let mut tree = Tree {
            root,
            show_hidden: false,
            show_ignored: false,
        };
        tree.load_children(&tree.root.path.clone());
        tree
    }

    /// Read children of the directory at `path` from disk, sorted, if not loaded.
    pub fn load_children(&mut self, path: &Path) {
        if let Some(node) = self.root.find_mut(path) {
            if node.is_dir() && !node.loaded {
                node.children = read_dir_sorted(&node.path);
                node.loaded = true;
            }
        }
    }

    pub fn toggle(&mut self, path: &Path) {
        let needs_load = matches!(self.root.find_mut(path), Some(n) if n.is_dir() && !n.loaded);
        if needs_load {
            self.load_children(path);
        }
        if let Some(node) = self.root.find_mut(path) {
            if node.is_dir() {
                node.expanded = !node.expanded;
            }
        }
    }

    pub fn expand(&mut self, path: &Path) {
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
            self.expand(&p);
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
        // Reset root to a fresh, unloaded directory, then re-expand.
        let root_path = self.root.path.clone();
        let mut root = Node::new(root_path, NodeKind::Directory, false, None);
        root.expanded = true;
        self.root = root;
        self.load_children(&self.root.path.clone());

        // Re-expand in path-depth order so parents load before children.
        let mut paths: Vec<&PathBuf> = expanded.iter().collect();
        paths.sort_by_key(|p| p.components().count());
        for p in paths {
            if p.exists() {
                self.expand(p);
            }
        }
    }

    /// Expand all ancestors of `target` so it becomes visible. Returns true if
    /// the target path exists within the tree.
    pub fn reveal(&mut self, target: &Path) -> bool {
        if !target.starts_with(&self.root.path) {
            return false;
        }
        // Expand each ancestor directory from root down to the target's parent.
        let mut cur = self.root.path.clone();
        if let Some(rel) = target.strip_prefix(&self.root.path).ok() {
            for comp in rel.components() {
                self.expand(&cur);
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
            // Directory: own explicit status (e.g. ignored) plus children's max.
            let mut best = statuses.get(&n.path).copied();
            for c in &mut n.children {
                if let Some(s) = walk(c, statuses) {
                    // Ignored does not propagate up over real changes.
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

    /// Flatten the visible tree into rows, honoring filter toggles.
    pub fn flatten(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        self.flatten_into(&self.root, 0, &mut Vec::new(), &mut rows);
        rows
    }

    fn flatten_into(
        &self,
        node: &Node,
        depth: usize,
        ancestor_last: &mut Vec<bool>,
        out: &mut Vec<Row>,
    ) {
        if depth > 0 {
            // Root itself is rendered as a header elsewhere; only push non-root.
            let has_children = node.is_dir() && (!node.loaded || !node.children.is_empty());
            out.push(Row {
                path: node.path.clone(),
                name: node.name.clone(),
                kind: node.kind,
                depth: depth - 1,
                expanded: node.expanded,
                has_children,
                executable: node.executable,
                git: node.git,
                link_to: node.link_to.clone(),
                ancestor_last: ancestor_last.clone(),
                is_last: false, // patched below
            });
        }

        if node.is_dir() && node.expanded {
            let visible: Vec<&Node> = node
                .children
                .iter()
                .filter(|c| self.is_visible(c))
                .collect();
            let last_idx = visible.len().saturating_sub(1);
            for (i, child) in visible.iter().enumerate() {
                let is_last = i == last_idx;
                let start = out.len();
                ancestor_last.push(is_last);
                self.flatten_into(child, depth + 1, ancestor_last, out);
                ancestor_last.pop();
                // Patch the child's own row is_last flag.
                if let Some(row) = out.get_mut(start) {
                    row.is_last = is_last;
                }
            }
        }
    }

    fn is_visible(&self, node: &Node) -> bool {
        if !self.show_hidden && node.is_hidden() {
            return false;
        }
        if !self.show_ignored && node.git == Some(GitStatus::Ignored) {
            return false;
        }
        true
    }
}

/// Read a directory and return its children as sorted nodes (dirs first, then
/// files, each case-insensitive by name). I/O errors yield an empty list.
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
            // metadata() follows the link; meta.is_dir() => points to a dir.
            (NodeKind::Symlink { to_dir: meta.is_dir() }, target)
        } else if meta.is_dir() {
            (NodeKind::Directory, None)
        } else {
            (NodeKind::File, None)
        };

        let executable = is_executable(&meta);
        nodes.push(Node::new(path, kind, executable, link_to));
    }
    sort_nodes(&mut nodes);
    nodes
}

fn sort_nodes(nodes: &mut [Node]) {
    nodes.sort_by(|a, b| {
        let a_dir = a.is_dir();
        let b_dir = b.is_dir();
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        }
    });
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
        let base = std::env::temp_dir().join(format!(
            "treelix-test-{}-{}",
            std::process::id(),
            label
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn dirs_sort_before_files() {
        let d = tmpdir("sort");
        fs::create_dir(d.join("zdir")).unwrap();
        fs::write(d.join("afile"), b"x").unwrap();
        let nodes = read_dir_sorted(&d);
        assert_eq!(nodes[0].name, "zdir");
        assert_eq!(nodes[1].name, "afile");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn flatten_and_reveal() {
        let d = tmpdir("reveal");
        fs::create_dir_all(d.join("sub/inner")).unwrap();
        fs::write(d.join("sub/inner/deep.txt"), b"x").unwrap();
        let mut tree = Tree::new(d.clone());
        // Initially only top-level visible.
        let rows = tree.flatten();
        assert!(rows.iter().any(|r| r.name == "sub"));
        assert!(!rows.iter().any(|r| r.name == "deep.txt"));

        let target = d.join("sub/inner/deep.txt");
        assert!(tree.reveal(&target));
        let rows = tree.flatten();
        assert!(rows.iter().any(|r| r.name == "deep.txt"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ignored_filter() {
        use crate::git::{GitData, GitStatus};
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

        assert!(!tree.flatten().iter().any(|r| r.name == "build"));
        assert!(tree.flatten().iter().any(|r| r.name == "keep.txt"));
        tree.show_ignored = true;
        assert!(tree.flatten().iter().any(|r| r.name == "build"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn hidden_filter() {
        let d = tmpdir("hidden");
        fs::write(d.join(".secret"), b"x").unwrap();
        fs::write(d.join("visible"), b"x").unwrap();
        let mut tree = Tree::new(d.clone());
        assert!(!tree.flatten().iter().any(|r| r.name == ".secret"));
        tree.show_hidden = true;
        assert!(tree.flatten().iter().any(|r| r.name == ".secret"));
        let _ = fs::remove_dir_all(&d);
    }
}
