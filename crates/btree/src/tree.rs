//! The copy-on-write B+tree over a [`Pager`].
//!
//! Every mutation descends to the affected leaf, then rebuilds the touched path
//! from the leaf up into **freshly allocated pages**, returning the new root.
//! The old pages are reported in [`Edit::freed`] but left intact, so any root
//! captured earlier remains a valid, immutable snapshot.

use common::IoBackend;
use pager::{PageId, PageType, Pager, HEADER_SIZE};

use crate::cursor::{Cursor, Direction};
use crate::node::{Node, MAX_CELL};
use crate::{BTreeError, Corruption, Result};

/// A copy-on-write B+tree rooted at a caller-held page id.
///
/// The tree is a thin set of operations over the pager; it stores no mutable
/// state of its own, so several `BTree` handles over the same pager are fine.
pub struct BTree<'p, B: IoBackend> {
    pager: &'p Pager<B>,
}

/// The result of a mutation: the new root to install, plus the pages the
/// mutation superseded (for the `txn` layer to reclaim once no snapshot needs
/// them). The B+tree itself never frees a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The root to read from after this mutation.
    pub new_root: PageId,
    /// Pages no longer reachable from `new_root` (old copied-path nodes).
    pub freed: Vec<PageId>,
}

/// A structural summary of a tree, returned by [`BTree::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeStats {
    /// Number of levels (a single-leaf tree has height 1).
    pub height: usize,
    /// Number of key/value entries (leaf entries).
    pub entries: u64,
    /// Number of pages (nodes) in the tree.
    pub nodes: u64,
}

/// Outcome of a recursive insert into a subtree.
enum Ins {
    /// Subtree rebuilt under a single new root page, no split.
    Updated(PageId),
    /// Subtree split; `(left, separator, right)` to install in the parent.
    Split(PageId, Vec<u8>, PageId),
}

/// Outcome of a recursive delete from a subtree.
enum Del {
    /// The key was absent; nothing changed and nothing was copied.
    Unchanged,
    /// The subtree was rebuilt under `id`; `underfull` asks the parent to
    /// rebalance it against a sibling.
    Done {
        /// New subtree root page.
        id: PageId,
        /// Whether `id` fell below the minimum fill.
        underfull: bool,
    },
}

impl<'p, B: IoBackend> BTree<'p, B> {
    /// Borrow a pager to operate on.
    pub fn new(pager: &'p Pager<B>) -> Self {
        BTree { pager }
    }

    /// Create an empty tree, returning its root page id.
    pub fn create(&self) -> Result<PageId> {
        self.write_node(&Node::empty_leaf())
    }

    /// Look up `key`, returning its value if present.
    pub fn lookup(&self, root: PageId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut id = root;
        loop {
            let node = self.read_node(id)?;
            match node.route(key) {
                Some(child) => id = child,
                None => return Ok(node.leaf_get(key).map(<[u8]>::to_vec)),
            }
        }
    }

    /// Insert or replace `key` → `value`, returning the new root and freed pages.
    pub fn insert(&self, root: PageId, key: &[u8], value: &[u8]) -> Result<Edit> {
        // Reject entries that could never fit a node (as a leaf cell now, or as
        // an internal separator later). v1 has no overflow pages.
        let need = (4 + key.len() + value.len()).max(10 + key.len());
        if need > MAX_CELL {
            return Err(BTreeError::EntryTooLarge {
                entry: need,
                max: MAX_CELL,
            });
        }

        let mut freed = Vec::new();
        let new_root = match self.insert_rec(root, key, value, &mut freed)? {
            Ins::Updated(id) => id,
            Ins::Split(left, sep, right) => {
                // The root split: grow the tree by one level.
                let node = Node::Internal {
                    keys: vec![sep],
                    children: vec![left, right],
                };
                self.write_node(&node)?
            }
        };
        Ok(Edit { new_root, freed })
    }

    /// Delete `key` if present, returning the new root and freed pages. Deleting
    /// an absent key leaves the root unchanged with no freed pages.
    pub fn delete(&self, root: PageId, key: &[u8]) -> Result<Edit> {
        let mut freed = Vec::new();
        match self.delete_rec(root, key, &mut freed)? {
            Del::Unchanged => Ok(Edit {
                new_root: root,
                freed: Vec::new(),
            }),
            Del::Done { id, .. } => {
                // If the root became an internal node with a single child, that
                // child becomes the new root (the tree shrinks by a level).
                let node = self.read_node(id)?;
                if let Node::Internal { children, .. } = &node {
                    if children.len() == 1 {
                        let new_root = children[0];
                        freed.push(id);
                        return Ok(Edit { new_root, freed });
                    }
                }
                Ok(Edit {
                    new_root: id,
                    freed,
                })
            }
        }
    }

    /// Open a forward cursor over `[lo, hi)` (each bound optional/unbounded).
    pub fn range(
        &self,
        root: PageId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Cursor<'p, B>> {
        Cursor::open(self.pager, root, Direction::Forward, lo, hi)
    }

    /// Open a cursor over `[lo, hi)` in the given direction.
    pub fn range_dir(
        &self,
        root: PageId,
        dir: Direction,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Cursor<'p, B>> {
        Cursor::open(self.pager, root, dir, lo, hi)
    }

    /// Walk the whole tree and prove its structural invariants: balanced depth,
    /// ordered keys consistent with separators, and non-empty non-root nodes.
    pub fn validate(&self, root: PageId) -> Result<TreeStats> {
        let mut nodes = 0u64;
        let (depth, entries) = self.validate_rec(root, None, None, true, &mut nodes)?;
        Ok(TreeStats {
            height: depth,
            entries,
            nodes,
        })
    }

    // --- internals -------------------------------------------------------

    fn read_node(&self, id: PageId) -> Result<Node> {
        let frame = self.pager.read_page(id)?;
        Node::decode(&frame[HEADER_SIZE..], id)
    }

    fn write_node(&self, node: &Node) -> Result<PageId> {
        let id = self.pager.alloc()?;
        self.pager.write_page(id, PageType::Data, &node.encode())?;
        Ok(id)
    }

    /// Write a (possibly overfull) node, splitting it into the parent-facing
    /// [`Ins`] result.
    fn write_balanced(&self, node: Node) -> Result<Ins> {
        if node.is_overfull() {
            let (left, sep, right) = node.split();
            let lid = self.write_node(&left)?;
            let rid = self.write_node(&right)?;
            Ok(Ins::Split(lid, sep, rid))
        } else {
            Ok(Ins::Updated(self.write_node(&node)?))
        }
    }

    fn insert_rec(
        &self,
        node_id: PageId,
        key: &[u8],
        value: &[u8],
        freed: &mut Vec<PageId>,
    ) -> Result<Ins> {
        let node = self.read_node(node_id)?;
        freed.push(node_id);
        match node {
            Node::Leaf { mut keys, mut vals } => {
                match keys.binary_search_by(|k| k.as_slice().cmp(key)) {
                    Ok(i) => vals[i] = value.to_vec(),
                    Err(i) => {
                        keys.insert(i, key.to_vec());
                        vals.insert(i, value.to_vec());
                    }
                }
                self.write_balanced(Node::Leaf { keys, vals })
            }
            Node::Internal {
                mut keys,
                mut children,
            } => {
                let ci = route_index(&keys, key);
                match self.insert_rec(children[ci], key, value, freed)? {
                    Ins::Updated(c) => children[ci] = c,
                    Ins::Split(left, sep, right) => {
                        children[ci] = left;
                        keys.insert(ci, sep);
                        children.insert(ci + 1, right);
                    }
                }
                self.write_balanced(Node::Internal { keys, children })
            }
        }
    }

    fn delete_rec(&self, node_id: PageId, key: &[u8], freed: &mut Vec<PageId>) -> Result<Del> {
        let node = self.read_node(node_id)?;
        match node {
            Node::Leaf { mut keys, mut vals } => {
                match keys.binary_search_by(|k| k.as_slice().cmp(key)) {
                    Err(_) => Ok(Del::Unchanged),
                    Ok(i) => {
                        freed.push(node_id);
                        keys.remove(i);
                        vals.remove(i);
                        let leaf = Node::Leaf { keys, vals };
                        let underfull = leaf.is_underfull();
                        let id = self.write_node(&leaf)?;
                        Ok(Del::Done { id, underfull })
                    }
                }
            }
            Node::Internal {
                mut keys,
                mut children,
            } => {
                let ci = route_index(&keys, key);
                match self.delete_rec(children[ci], key, freed)? {
                    Del::Unchanged => Ok(Del::Unchanged),
                    Del::Done {
                        id,
                        underfull: child_underfull,
                    } => {
                        freed.push(node_id);
                        children[ci] = id;
                        if child_underfull {
                            self.rebalance(&mut keys, &mut children, ci, freed)?;
                        }
                        let internal = Node::Internal { keys, children };
                        let underfull = internal.is_underfull();
                        let id = self.write_node(&internal)?;
                        Ok(Del::Done { id, underfull })
                    }
                }
            }
        }
    }

    /// Fix an underfull child at index `ci` by merging it with an adjacent
    /// sibling, splitting the merge back into two balanced nodes if it overflows
    /// (a rotation). Mutates the parent's `keys`/`children` in place.
    fn rebalance(
        &self,
        keys: &mut Vec<Vec<u8>>,
        children: &mut Vec<PageId>,
        ci: usize,
        freed: &mut Vec<PageId>,
    ) -> Result<()> {
        // Adjacent pair to combine: prefer the left sibling.
        let (l_idx, r_idx, sep_idx) = if ci > 0 {
            (ci - 1, ci, ci - 1)
        } else {
            (ci, ci + 1, ci)
        };
        let left = self.read_node(children[l_idx])?;
        let right = self.read_node(children[r_idx])?;
        let merged = Node::merge(&left, &keys[sep_idx], &right);
        freed.push(children[l_idx]);
        freed.push(children[r_idx]);

        if merged.is_overfull() {
            // Too big to combine: redistribute by splitting the merge, sending a
            // fresh separator back up.
            let (l2, new_sep, r2) = merged.split();
            children[l_idx] = self.write_node(&l2)?;
            children[r_idx] = self.write_node(&r2)?;
            keys[sep_idx] = new_sep;
        } else {
            // They fit in one node: the parent loses a separator and a child.
            children[l_idx] = self.write_node(&merged)?;
            children.remove(r_idx);
            keys.remove(sep_idx);
        }
        Ok(())
    }

    fn validate_rec(
        &self,
        id: PageId,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
        is_root: bool,
        nodes: &mut u64,
    ) -> Result<(usize, u64)> {
        *nodes += 1;
        let node = self.read_node(id)?;

        // Every key must fall inside the bounds inherited from the separators.
        let in_bounds = |k: &[u8]| lower.is_none_or(|lo| k >= lo) && upper.is_none_or(|hi| k < hi);

        match node {
            Node::Leaf { keys, vals } => {
                if !is_root && keys.is_empty() {
                    return Err(BTreeError::Corruption(Corruption::Underfull {
                        page: id.get(),
                    }));
                }
                if keys.len() != vals.len() {
                    return Err(BTreeError::Corruption(Corruption::KeysNotSorted {
                        page: id.get(),
                    }));
                }
                for k in &keys {
                    if !in_bounds(k) {
                        return Err(BTreeError::Corruption(Corruption::SeparatorMismatch {
                            page: id.get(),
                        }));
                    }
                }
                Ok((1, keys.len() as u64))
            }
            Node::Internal { keys, children } => {
                if children.len() < 2 || children.len() != keys.len() + 1 {
                    return Err(BTreeError::Corruption(Corruption::EmptyInternal {
                        page: id.get(),
                    }));
                }
                for k in &keys {
                    if !in_bounds(k) {
                        return Err(BTreeError::Corruption(Corruption::SeparatorMismatch {
                            page: id.get(),
                        }));
                    }
                }
                let mut entries = 0u64;
                let mut child_depth: Option<usize> = None;
                for (i, &child) in children.iter().enumerate() {
                    let lo = if i == 0 {
                        lower
                    } else {
                        Some(keys[i - 1].as_slice())
                    };
                    let hi = if i == keys.len() {
                        upper
                    } else {
                        Some(keys[i].as_slice())
                    };
                    let (d, e) = self.validate_rec(child, lo, hi, false, nodes)?;
                    entries += e;
                    match child_depth {
                        None => child_depth = Some(d),
                        Some(prev) if prev != d => {
                            return Err(BTreeError::Corruption(Corruption::UnevenLeafDepth));
                        }
                        _ => {}
                    }
                }
                Ok((child_depth.unwrap_or(0) + 1, entries))
            }
        }
    }
}

/// The index of the child covering `key` for an internal node with these
/// separator `keys`.
fn route_index(keys: &[Vec<u8>], key: &[u8]) -> usize {
    keys.partition_point(|k| k.as_slice() <= key)
}
