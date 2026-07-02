//! A lazy range cursor over a B+tree snapshot.
//!
//! CoW trees keep no leaf sibling pointers (they would have to be copied on
//! every edit), so iteration walks a root-to-leaf path held on a stack. The
//! cursor reads the tree at a fixed `root`, giving a stable view for its whole
//! life regardless of concurrent writers installing new roots.

use common::IoBackend;
use pager::{PageId, Pager, HEADER_SIZE};

use crate::node::Node;
use crate::Result;

/// Iteration direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Ascending key order.
    Forward,
    /// Descending key order.
    Backward,
}

/// A key/value pair yielded by a [`Cursor`].
pub type Item = (Vec<u8>, Vec<u8>);

/// One level of the descent path.
struct Frame {
    node: Node,
    /// Internal: the child index this frame descends through. Leaf: the next
    /// entry index a forward step would yield (a backward step yields `idx-1`).
    idx: usize,
}

/// A lazy, bounded iterator over `[lo, hi)` of a tree.
///
/// Advance it with [`next_entry`](Cursor::next_entry), which returns `Ok(None)`
/// at the end. Both bounds are optional; `lo` is inclusive and `hi` is exclusive.
pub struct Cursor<'p, B: IoBackend> {
    pager: &'p Pager<B>,
    dir: Direction,
    lo: Option<Vec<u8>>,
    hi: Option<Vec<u8>>,
    path: Vec<Frame>,
}

impl<'p, B: IoBackend> Cursor<'p, B> {
    pub(crate) fn open(
        pager: &'p Pager<B>,
        root: PageId,
        dir: Direction,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Cursor<'p, B>> {
        let mut cursor = Cursor {
            pager,
            dir,
            lo: lo.map(<[u8]>::to_vec),
            hi: hi.map(<[u8]>::to_vec),
            path: Vec::new(),
        };
        // Both directions seek to "the first entry ≥ the start bound": for
        // forward that bound is `lo` (and we step forward from it); for backward
        // it is `hi` (and we step backward into the range from it). An absent
        // bound is −∞ for forward (the start) but +∞ for backward (the end).
        let bound = match dir {
            Direction::Forward => cursor.lo.clone(),
            Direction::Backward => cursor.hi.clone(),
        };
        match (dir, bound) {
            (Direction::Backward, None) => cursor.descend_rightmost(root)?,
            (_, b) => cursor.descend_lower_bound(root, b.as_deref())?,
        }
        Ok(cursor)
    }

    /// Yield the next pair, or `Ok(None)` when the range is exhausted. Fallible
    /// (a page read may fail), so this is not an [`Iterator`].
    pub fn next_entry(&mut self) -> Result<Option<Item>> {
        match self.dir {
            Direction::Forward => self.next_forward(),
            Direction::Backward => self.next_backward(),
        }
    }

    /// Collect the remaining pairs into a vector (convenience for callers/tests).
    pub fn collect_all(mut self) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        while let Some(item) = self.next_entry()? {
            out.push(item);
        }
        Ok(out)
    }

    /// Count the remaining entries in the range, holding only one at a time —
    /// O(1) memory, for callers that need a count but not the rows.
    pub fn count(mut self) -> Result<u64> {
        let mut n = 0;
        while self.next_entry()?.is_some() {
            n += 1;
        }
        Ok(n)
    }

    fn read(&self, id: PageId) -> Result<Node> {
        let frame = self.pager.read_page(id)?;
        Node::decode(&frame[HEADER_SIZE..], id)
    }

    /// Build a path positioned at the first entry whose key is ≥ `bound`
    /// (or the leaf's end if none here — a caller step then crosses leaves).
    fn descend_lower_bound(&mut self, root: PageId, bound: Option<&[u8]>) -> Result<()> {
        let mut id = root;
        loop {
            let node = self.read(id)?;
            match &node {
                Node::Internal { keys, children } => {
                    let ci = bound.map_or(0, |b| keys.partition_point(|k| k.as_slice() <= b));
                    let next = children[ci];
                    self.path.push(Frame { node, idx: ci });
                    id = next;
                }
                Node::Leaf { keys, .. } => {
                    let start = bound.map_or(0, |b| keys.partition_point(|k| k.as_slice() < b));
                    self.path.push(Frame { node, idx: start });
                    return Ok(());
                }
            }
        }
    }

    /// Descend to the leftmost leaf of `id`, positioned at its first entry.
    fn descend_leftmost(&mut self, id: PageId) -> Result<()> {
        let mut id = id;
        loop {
            let node = self.read(id)?;
            match &node {
                Node::Internal { children, .. } => {
                    let next = children[0];
                    self.path.push(Frame { node, idx: 0 });
                    id = next;
                }
                Node::Leaf { .. } => {
                    self.path.push(Frame { node, idx: 0 });
                    return Ok(());
                }
            }
        }
    }

    /// Descend to the rightmost leaf of `id`, positioned just past its last
    /// entry (so a backward step yields that last entry).
    fn descend_rightmost(&mut self, id: PageId) -> Result<()> {
        let mut id = id;
        loop {
            let node = self.read(id)?;
            match &node {
                Node::Internal { children, .. } => {
                    let last = children.len() - 1;
                    let next = children[last];
                    self.path.push(Frame { node, idx: last });
                    id = next;
                }
                Node::Leaf { keys, .. } => {
                    let end = keys.len();
                    self.path.push(Frame { node, idx: end });
                    return Ok(());
                }
            }
        }
    }

    fn next_forward(&mut self) -> Result<Option<Item>> {
        loop {
            let Some(frame) = self.path.last() else {
                return Ok(None);
            };
            if let Node::Leaf { keys, vals } = &frame.node {
                let idx = frame.idx;
                if idx < keys.len() {
                    if self
                        .hi
                        .as_deref()
                        .is_some_and(|hi| keys[idx].as_slice() >= hi)
                    {
                        self.path.clear();
                        return Ok(None);
                    }
                    let item = (keys[idx].clone(), vals[idx].clone());
                    if let Some(top) = self.path.last_mut() {
                        top.idx += 1;
                    }
                    return Ok(Some(item));
                }
            }
            // Leaf exhausted: drop it and advance the parent to the next child.
            self.path.pop();
            if !self.advance_to_next_subtree()? {
                return Ok(None);
            }
        }
    }

    fn next_backward(&mut self) -> Result<Option<Item>> {
        loop {
            let Some(frame) = self.path.last() else {
                return Ok(None);
            };
            if let Node::Leaf { keys, vals } = &frame.node {
                if frame.idx > 0 {
                    let idx = frame.idx - 1;
                    if self
                        .lo
                        .as_deref()
                        .is_some_and(|lo| keys[idx].as_slice() < lo)
                    {
                        self.path.clear();
                        return Ok(None);
                    }
                    let item = (keys[idx].clone(), vals[idx].clone());
                    if let Some(top) = self.path.last_mut() {
                        top.idx -= 1;
                    }
                    return Ok(Some(item));
                }
            }
            // Start of this leaf: drop it and retreat to the previous child.
            self.path.pop();
            if !self.retreat_to_prev_subtree()? {
                return Ok(None);
            }
        }
    }

    /// After popping an exhausted leaf, move the path to the leftmost leaf of the
    /// next sibling subtree. Returns `false` if there is none.
    fn advance_to_next_subtree(&mut self) -> Result<bool> {
        while let Some(frame) = self.path.last_mut() {
            if let Node::Internal { children, .. } = &frame.node {
                frame.idx += 1;
                if frame.idx < children.len() {
                    let child = children[frame.idx];
                    self.descend_leftmost(child)?;
                    return Ok(true);
                }
            }
            self.path.pop();
        }
        Ok(false)
    }

    /// After popping a leaf at its start, move the path to the rightmost leaf of
    /// the previous sibling subtree. Returns `false` if there is none.
    fn retreat_to_prev_subtree(&mut self) -> Result<bool> {
        while let Some(frame) = self.path.last_mut() {
            if let Node::Internal { .. } = &frame.node {
                if frame.idx > 0 {
                    frame.idx -= 1;
                    let child = match &frame.node {
                        Node::Internal { children, .. } => children[frame.idx],
                        Node::Leaf { .. } => return Ok(false),
                    };
                    self.descend_rightmost(child)?;
                    return Ok(true);
                }
            }
            self.path.pop();
        }
        Ok(false)
    }
}
