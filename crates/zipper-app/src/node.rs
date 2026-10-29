//! Tree nodes for the outline view. `NSOutlineView` keeps *unretained*
//! pointers to its items, so every node is an Objective-C object owned by
//! the document (roots) or its parent node (children) for as long as the
//! window lives.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_foundation::NSObject;

use archive_core::{Entry, EntryKind};

pub struct NodeData {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub packed: Option<u64>,
    pub modified: Option<SystemTime>,
    #[allow(dead_code)] // shown in the UI in Phase 3
    pub encrypted: bool,
    pub children: RefCell<Vec<Retained<Node>>>,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - Node does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "ZipperNode"]
    #[ivars = NodeData]
    pub struct Node;
);

impl Node {
    fn new(data: NodeData) -> Retained<Self> {
        let this = Self::alloc().set_ivars(data);
        unsafe { msg_send![super(this), init] }
    }

    pub fn data(&self) -> &NodeData {
        self.ivars()
    }

    pub fn child_count(&self) -> usize {
        self.data().children.borrow().len()
    }

    pub fn child(&self, i: usize) -> Option<Retained<Node>> {
        self.data().children.borrow().get(i).cloned()
    }

    /// Recursive total of file sizes under this node (None if any unknown).
    pub fn total_size(&self) -> Option<u64> {
        if !self.data().is_dir {
            return self.data().size;
        }
        let mut t = 0u64;
        for c in self.data().children.borrow().iter() {
            t += c.total_size()?;
        }
        Some(t)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Packed,
    Modified,
}

impl SortKey {
    pub fn from_identifier(id: &str) -> Option<Self> {
        match id {
            "name" => Some(SortKey::Name),
            "size" => Some(SortKey::Size),
            "packed" => Some(SortKey::Packed),
            "modified" => Some(SortKey::Modified),
            _ => None,
        }
    }
}

/// Build the tree from a flat entry list. Directories that only exist
/// implicitly (a `a/b/c.txt` entry without an `a/` or `a/b/` entry) are
/// created on the fly.
pub fn build_tree(entries: &[Entry]) -> Vec<Retained<Node>> {
    #[derive(Default)]
    struct Tmp {
        is_dir: bool,
        size: Option<u64>,
        packed: Option<u64>,
        modified: Option<SystemTime>,
        encrypted: bool,
        explicit: bool,
        children: BTreeMap<String, Tmp>,
    }

    let mut root = Tmp {
        is_dir: true,
        ..Default::default()
    };
    for e in entries {
        let comps: Vec<String> = e
            .path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if comps.is_empty() {
            continue;
        }
        let mut cur = &mut root;
        for (i, name) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let node = cur.children.entry(name.clone()).or_insert_with(|| Tmp {
                is_dir: !last || e.kind == EntryKind::Dir,
                ..Default::default()
            });
            if last {
                node.explicit = true;
                node.is_dir = e.kind == EntryKind::Dir;
                node.size = e.size;
                node.packed = e.packed_size;
                node.modified = e.modified;
                node.encrypted = e.encrypted;
            } else {
                node.is_dir = true;
            }
            cur = node;
        }
    }

    fn convert(name: &str, path: &Path, t: Tmp) -> Retained<Node> {
        let mut children: Vec<Retained<Node>> = t
            .children
            .into_iter()
            .map(|(n, c)| convert(&n, &path.join(&n), c))
            .collect();
        sort_nodes(&mut children, SortKey::Name, true);
        Node::new(NodeData {
            name: name.to_string(),
            path: path.to_path_buf(),
            is_dir: t.is_dir,
            size: t.size,
            packed: t.packed,
            modified: t.modified,
            encrypted: t.encrypted,
            children: RefCell::new(children),
        })
    }

    let mut roots: Vec<Retained<Node>> = root
        .children
        .into_iter()
        .map(|(n, c)| convert(&n, Path::new(&n), c))
        .collect();
    sort_nodes(&mut roots, SortKey::Name, true);
    roots
}

/// Sort a sibling list in place: folders first, then by key.
pub fn sort_nodes(nodes: &mut [Retained<Node>], key: SortKey, ascending: bool) {
    nodes.sort_by(|a, b| {
        let (da, db) = (a.data(), b.data());
        db.is_dir.cmp(&da.is_dir).then_with(|| {
            let ord = match key {
                SortKey::Name => natural_cmp(&da.name, &db.name),
                SortKey::Size => a.total_size().cmp(&b.total_size()),
                SortKey::Packed => da.packed.cmp(&db.packed),
                SortKey::Modified => da.modified.cmp(&db.modified),
            };
            let ord = if ascending { ord } else { ord.reverse() };
            ord.then_with(|| natural_cmp(&da.name, &db.name))
        })
    });
}

/// Recursively re-sort the whole tree.
pub fn sort_tree(roots: &mut [Retained<Node>], key: SortKey, ascending: bool) {
    sort_nodes(roots, key, ascending);
    for n in roots.iter() {
        let mut ch = n.data().children.borrow_mut();
        sort_tree(&mut ch, key, ascending);
    }
}

/// Case-insensitive, digit-aware comparison ("file2" < "file10").
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut ia, mut ib) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ia.peek().copied(), ib.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let mut na = 0u128;
                while let Some(c) = ia.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na = na
                        .saturating_mul(10)
                        .saturating_add(c as u128 - '0' as u128);
                    ia.next();
                }
                let mut nb = 0u128;
                while let Some(c) = ib.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb = nb
                        .saturating_mul(10)
                        .saturating_add(c as u128 - '0' as u128);
                    ib.next();
                }
                if na != nb {
                    return na.cmp(&nb);
                }
            }
            (Some(ca), Some(cb)) => {
                let (la, lb) = (
                    ca.to_lowercase().next().unwrap_or(ca),
                    cb.to_lowercase().next().unwrap_or(cb),
                );
                if la != lb {
                    return la.cmp(&lb);
                }
                ia.next();
                ib.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("file2", "file10"), Less);
        assert_eq!(natural_cmp("B", "a"), Greater);
        assert_eq!(natural_cmp("x", "x"), Equal);
    }
}
