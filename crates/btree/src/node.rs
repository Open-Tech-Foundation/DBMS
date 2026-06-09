//! In-memory B+tree nodes and their on-page byte encoding.
//!
//! A node lives in the payload region of one `Data` page. Because the tree is
//! copy-on-write, a modification never edits a page in place: it decodes a node,
//! produces a new in-memory [`Node`], and writes it to a freshly allocated page.
//! So the encoding favours simplicity (decode-whole / encode-whole) over
//! in-place slot surgery.
//!
//! Payload layout (little-endian), where `p` is the page payload:
//! ```text
//! leaf:      p[0]=0  p[1]=0  p[2..4]=count   then count×(klen u16, key, vlen u16, val)
//! internal:  p[0]=1  p[1]=0  p[2..4]=count   p[4..12]=children[0]
//!            then count×(klen u16, key, child u64)   (children = count+1 total)
//! ```

use pager::{CorruptionKind, PageId, PAGE_PAYLOAD_SIZE};

use crate::{BTreeError, Corruption, Result};

const KIND_LEAF: u8 = 0;
const KIND_INTERNAL: u8 = 1;

/// Header bytes before a leaf's entries: kind, reserved, count.
const HDR_LEAF: usize = 4;
/// Header bytes before an internal node's separators: leaf header + leftmost child.
const HDR_INTERNAL: usize = 12;

/// Usable payload bytes for a node (the page payload region).
pub const CAP: usize = PAGE_PAYLOAD_SIZE;

/// A node is underfull (and must be rebalanced unless it is the root) when it
/// uses less than this many bytes.
pub const MIN_FILL: usize = CAP / 4;

/// The largest a single encoded entry/separator cell may be. Capping each cell
/// at half the (header-adjusted) capacity guarantees any two cells share a page,
/// so a split always yields two non-empty halves that each fit. Larger payloads
/// would need overflow pages, which v1 does not provide.
pub const MAX_CELL: usize = (CAP - HDR_INTERNAL) / 2;

/// A decoded B+tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A leaf: parallel, key-sorted `keys`/`vals`.
    Leaf {
        /// Keys, strictly ascending.
        keys: Vec<Vec<u8>>,
        /// Values, positionally paired with `keys`.
        vals: Vec<Vec<u8>>,
    },
    /// An internal node: `children.len() == keys.len() + 1`. `keys[i]` is the
    /// smallest key reachable through `children[i + 1]`.
    Internal {
        /// Separator keys, strictly ascending.
        keys: Vec<Vec<u8>>,
        /// Child page ids; one more than `keys`.
        children: Vec<PageId>,
    },
}

impl Node {
    /// An empty leaf (the shape of a freshly created tree's root).
    pub fn empty_leaf() -> Node {
        Node::Leaf {
            keys: Vec::new(),
            vals: Vec::new(),
        }
    }

    /// Number of keys held (kv pairs for a leaf, separators for an internal).
    pub fn key_count(&self) -> usize {
        match self {
            Node::Leaf { keys, .. } | Node::Internal { keys, .. } => keys.len(),
        }
    }

    fn header_len(&self) -> usize {
        match self {
            Node::Leaf { .. } => HDR_LEAF,
            Node::Internal { .. } => HDR_INTERNAL,
        }
    }

    /// The size of entry/separator `i`'s encoded cell.
    fn cell_len(&self, i: usize) -> usize {
        match self {
            Node::Leaf { keys, vals } => 2 + keys[i].len() + 2 + vals[i].len(),
            Node::Internal { keys, .. } => 2 + keys[i].len() + 8,
        }
    }

    /// Total encoded byte length.
    pub fn encoded_len(&self) -> usize {
        let mut n = self.header_len();
        for i in 0..self.key_count() {
            n += self.cell_len(i);
        }
        n
    }

    /// Whether this node would not fit in a page.
    pub fn is_overfull(&self) -> bool {
        self.encoded_len() > CAP
    }

    /// Whether this node is below the minimum fill (callers exempt the root).
    pub fn is_underfull(&self) -> bool {
        self.encoded_len() < MIN_FILL
    }

    /// Encode this node into its on-page payload bytes (length ≤ [`CAP`]).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        match self {
            Node::Leaf { keys, vals } => {
                out.push(KIND_LEAF);
                out.push(0);
                out.extend_from_slice(&(keys.len() as u16).to_le_bytes());
                for (k, v) in keys.iter().zip(vals) {
                    out.extend_from_slice(&(k.len() as u16).to_le_bytes());
                    out.extend_from_slice(k);
                    out.extend_from_slice(&(v.len() as u16).to_le_bytes());
                    out.extend_from_slice(v);
                }
            }
            Node::Internal { keys, children } => {
                out.push(KIND_INTERNAL);
                out.push(0);
                out.extend_from_slice(&(keys.len() as u16).to_le_bytes());
                out.extend_from_slice(&children[0].get().to_le_bytes());
                for (k, c) in keys.iter().zip(&children[1..]) {
                    out.extend_from_slice(&(k.len() as u16).to_le_bytes());
                    out.extend_from_slice(k);
                    out.extend_from_slice(&c.get().to_le_bytes());
                }
            }
        }
        out
    }

    /// Decode a node from a page payload, rejecting any malformed bytes with a
    /// typed [`Corruption`] error. Never panics on hostile input.
    pub fn decode(payload: &[u8], page: PageId) -> Result<Node> {
        let mut r = Reader::new(payload, page);
        let kind = r.u8()?;
        let _reserved = r.u8()?;
        let count = r.u16()? as usize;
        match kind {
            KIND_LEAF => {
                let mut keys = Vec::with_capacity(count);
                let mut vals = Vec::with_capacity(count);
                for _ in 0..count {
                    keys.push(r.bytes_u16()?.to_vec());
                    vals.push(r.bytes_u16()?.to_vec());
                }
                ascending(&keys, page)?;
                Ok(Node::Leaf { keys, vals })
            }
            KIND_INTERNAL => {
                let mut children = Vec::with_capacity(count + 1);
                children.push(child_id(r.u64()?, page)?);
                let mut keys = Vec::with_capacity(count);
                for _ in 0..count {
                    keys.push(r.bytes_u16()?.to_vec());
                    children.push(child_id(r.u64()?, page)?);
                }
                ascending(&keys, page)?;
                Ok(Node::Internal { keys, children })
            }
            other => Err(BTreeError::Corruption(Corruption::UnknownNodeKind {
                page: page.get(),
                byte: other,
            })),
        }
    }

    /// Look up `key` in a leaf, returning its value if present. Returns `None`
    /// for an internal node.
    pub fn leaf_get(&self, key: &[u8]) -> Option<&[u8]> {
        match self {
            Node::Leaf { keys, vals } => match keys.binary_search_by(|k| k.as_slice().cmp(key)) {
                Ok(i) => Some(&vals[i]),
                Err(_) => None,
            },
            Node::Internal { .. } => None,
        }
    }

    /// Route a search key to the child page that may contain it (`None` for a
    /// leaf node).
    pub fn route(&self, key: &[u8]) -> Option<PageId> {
        match self {
            Node::Internal { keys, children } => {
                Some(children[keys.partition_point(|k| k.as_slice() <= key)])
            }
            Node::Leaf { .. } => None,
        }
    }

    /// Choose a split index that keeps both halves within a page. Returns the
    /// number of cells (`m`) that stay on the left; for an internal node the
    /// cell at `m` is the one promoted upward. Both sides are non-empty.
    fn split_at(&self) -> usize {
        let hdr = self.header_len();
        let internal = matches!(self, Node::Internal { .. });
        let n = self.key_count();
        let total: usize = (0..n).map(|i| self.cell_len(i)).sum();

        // Pick the balanced split that leaves both sides ≤ CAP. The MAX_CELL
        // invariant guarantees such a split exists with 1 ≤ m ≤ n-1.
        let mut best = 1usize;
        let mut best_skew = usize::MAX;
        let mut left = 0usize;
        for m in 1..n {
            left += self.cell_len(m - 1);
            // The internal promotion key (cell m) belongs to neither side.
            let promoted = if internal { self.cell_len(m) } else { 0 };
            let right = total - left - promoted;
            if hdr + left <= CAP && hdr + right <= CAP {
                let skew = left.abs_diff(right);
                if skew < best_skew {
                    best_skew = skew;
                    best = m;
                }
            }
        }
        best
    }

    /// Split an overfull node into `(left, promoted_key, right)`.
    pub fn split(&self) -> (Node, Vec<u8>, Node) {
        let m = self.split_at();
        match self {
            Node::Leaf { keys, vals } => {
                let left = Node::Leaf {
                    keys: keys[..m].to_vec(),
                    vals: vals[..m].to_vec(),
                };
                let right = Node::Leaf {
                    keys: keys[m..].to_vec(),
                    vals: vals[m..].to_vec(),
                };
                // Copy the right half's first key up as the separator.
                (left, keys[m].clone(), right)
            }
            Node::Internal { keys, children } => {
                let left = Node::Internal {
                    keys: keys[..m].to_vec(),
                    children: children[..=m].to_vec(),
                };
                let right = Node::Internal {
                    keys: keys[m + 1..].to_vec(),
                    children: children[m + 1..].to_vec(),
                };
                // The middle key moves up and out of both halves.
                (left, keys[m].clone(), right)
            }
        }
    }

    /// Merge two adjacent siblings. For internal nodes `separator` is the parent
    /// key between them (it descends into the merged node); leaves ignore it.
    pub fn merge(left: &Node, separator: &[u8], right: &Node) -> Node {
        match (left, right) {
            (Node::Leaf { keys: lk, vals: lv }, Node::Leaf { keys: rk, vals: rv }) => {
                let mut keys = lk.clone();
                keys.extend_from_slice(rk);
                let mut vals = lv.clone();
                vals.extend_from_slice(rv);
                Node::Leaf { keys, vals }
            }
            (
                Node::Internal {
                    keys: lk,
                    children: lc,
                },
                Node::Internal {
                    keys: rk,
                    children: rc,
                },
            ) => {
                let mut keys = lk.clone();
                keys.push(separator.to_vec());
                keys.extend_from_slice(rk);
                let mut children = lc.clone();
                children.extend_from_slice(rc);
                Node::Internal { keys, children }
            }
            // Siblings are always the same kind in a well-formed tree.
            _ => left.clone(),
        }
    }
}

/// Validate that an encoded child page id is in the legal range.
fn child_id(raw: u64, page: PageId) -> Result<PageId> {
    if raw < pager::FIRST_DATA_PAGE {
        return Err(BTreeError::Corruption(Corruption::BadChild {
            page: page.get(),
            child: raw,
        }));
    }
    Ok(PageId::new(raw))
}

/// Verify keys are strictly ascending (rejects unsorted/duplicate on-disk data).
fn ascending(keys: &[Vec<u8>], page: PageId) -> Result<()> {
    for w in keys.windows(2) {
        if w[0] >= w[1] {
            return Err(BTreeError::Corruption(Corruption::KeysNotSorted {
                page: page.get(),
            }));
        }
    }
    Ok(())
}

/// A bounds-checked reader over a page payload. Every accessor fails with a
/// typed corruption error rather than panicking on a short or hostile buffer.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    page: u64,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8], page: PageId) -> Self {
        Reader {
            buf,
            pos: 0,
            page: page.get(),
        }
    }

    fn truncated(&self) -> BTreeError {
        // Reuse the pager's truncated-page corruption kind for short node bytes.
        BTreeError::Corruption(Corruption::Node(CorruptionKind::TruncatedPage {
            page: self.page,
        }))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| self.truncated())?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| self.truncated())?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    fn bytes_u16(&mut self) -> Result<&'a [u8]> {
        let len = self.u16()? as usize;
        self.take(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(pairs: &[(&[u8], &[u8])]) -> Node {
        Node::Leaf {
            keys: pairs.iter().map(|(k, _)| k.to_vec()).collect(),
            vals: pairs.iter().map(|(_, v)| v.to_vec()).collect(),
        }
    }

    #[test]
    fn leaf_round_trips() {
        let n = leaf(&[(b"a", b"1"), (b"bb", b"22"), (b"ccc", b"333")]);
        let bytes = n.encode();
        assert_eq!(Node::decode(&bytes, PageId::new(2)).unwrap(), n);
    }

    #[test]
    fn internal_round_trips() {
        let n = Node::Internal {
            keys: vec![b"m".to_vec(), b"t".to_vec()],
            children: vec![PageId::new(2), PageId::new(3), PageId::new(4)],
        };
        let bytes = n.encode();
        assert_eq!(Node::decode(&bytes, PageId::new(5)).unwrap(), n);
    }

    #[test]
    fn routing_picks_the_covering_child() {
        let n = Node::Internal {
            keys: vec![b"m".to_vec(), b"t".to_vec()],
            children: vec![PageId::new(2), PageId::new(3), PageId::new(4)],
        };
        assert_eq!(n.route(b"a").unwrap(), PageId::new(2));
        assert_eq!(n.route(b"m").unwrap(), PageId::new(3));
        assert_eq!(n.route(b"s").unwrap(), PageId::new(3));
        assert_eq!(n.route(b"t").unwrap(), PageId::new(4));
        assert_eq!(n.route(b"z").unwrap(), PageId::new(4));
    }

    #[test]
    fn truncated_node_is_rejected_not_panicked() {
        let n = leaf(&[(b"a", b"1")]);
        let mut bytes = n.encode();
        bytes.truncate(bytes.len() - 1); // drop the last value byte
        assert!(matches!(
            Node::decode(&bytes, PageId::new(2)),
            Err(BTreeError::Corruption(_))
        ));
    }

    #[test]
    fn unsorted_leaf_is_rejected() {
        // Hand-encode a leaf with descending keys.
        let bad = Node::Leaf {
            keys: vec![b"b".to_vec(), b"a".to_vec()],
            vals: vec![b"1".to_vec(), b"2".to_vec()],
        };
        let bytes = bad.encode();
        assert!(matches!(
            Node::decode(&bytes, PageId::new(2)),
            Err(BTreeError::Corruption(Corruption::KeysNotSorted { .. }))
        ));
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let mut bytes = leaf(&[(b"a", b"1")]).encode();
        bytes[0] = 9;
        assert!(matches!(
            Node::decode(&bytes, PageId::new(2)),
            Err(BTreeError::Corruption(Corruption::UnknownNodeKind {
                byte: 9,
                ..
            }))
        ));
    }

    #[test]
    fn leaf_split_halves_and_separates() {
        // Build a leaf big enough to overflow, then split.
        let big = vec![b'x'; 600];
        let mut keys = Vec::new();
        let mut vals = Vec::new();
        for i in 0..10u8 {
            keys.push(vec![i]);
            vals.push(big.clone());
        }
        let n = Node::Leaf { keys, vals };
        assert!(n.is_overfull());
        let (l, sep, r) = n.split();
        assert!(!l.is_overfull() && !r.is_overfull());
        assert!(l.key_count() >= 1 && r.key_count() >= 1);
        // Separator is the right half's first key.
        if let Node::Leaf { keys, .. } = &r {
            assert_eq!(sep, keys[0]);
        }
    }
}
