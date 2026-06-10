//! `btree` — copy-on-write B+tree.
//!
//! The ordered map underlying tables and indexes: point lookup, forward and
//! backward range scans, insert/delete with node split/merge, and root-handle
//! semantics — a mutation copies the touched path to a **new root** and leaves
//! the old root (and every page it reaches) untouched, so a reader holding an
//! earlier root keeps seeing a consistent snapshot.
//!
//! The tree never frees pages itself. [`insert`](BTree::insert) and
//! [`delete`](BTree::delete) return the superseded page ids in
//! [`Edit::freed`]; the `txn` layer reclaims them once no live snapshot needs
//! them. Keys are compared bytewise — a provisional, order-preserving encoding
//! that `types` finalizes in Phase 5.
//!
//! ```
//! use btree::BTree;
//! use common::MemoryBackend;
//! use pager::Pager;
//!
//! let pager = Pager::create(MemoryBackend::new()).unwrap();
//! let tree = BTree::new(&pager);
//! let root = tree.create().unwrap();
//!
//! let edit = tree.insert(root, b"k", b"v").unwrap();
//! assert_eq!(tree.lookup(edit.new_root, b"k").unwrap().as_deref(), Some(&b"v"[..]));
//! ```

mod cursor;
mod node;
mod tree;

use common::{CategorizedError, ErrorCategory};
use pager::CorruptionKind;

pub use cursor::{Cursor, Direction};
pub use tree::{check_entry, BTree, Edit, TreeStats};

/// Errors raised by the B+tree.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BTreeError {
    /// An error from the underlying pager.
    #[error(transparent)]
    Pager(#[from] pager::PagerError),
    /// On-disk node bytes or tree structure failed an integrity check.
    #[error("btree corruption: {0}")]
    Corruption(Corruption),
    /// A single key/value entry is too large to fit a node (v1 has no overflow
    /// pages). The caller must use a smaller value.
    #[error("entry of {entry} bytes exceeds the {max}-byte node-cell limit")]
    EntryTooLarge {
        /// The encoded cell size that was rejected.
        entry: usize,
        /// The maximum cell size a node can hold.
        max: usize,
    },
}

impl CategorizedError for BTreeError {
    fn category(&self) -> ErrorCategory {
        match self {
            BTreeError::Pager(e) => e.category(),
            BTreeError::Corruption(_) => ErrorCategory::Corruption,
            BTreeError::EntryTooLarge { .. } => ErrorCategory::ResourceLimit,
        }
    }
}

/// The specific way B+tree node bytes or structure were found to be invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Corruption {
    /// A page's raw bytes failed a pager-level check while decoding a node.
    Node(CorruptionKind),
    /// A node's kind byte was neither leaf nor internal.
    UnknownNodeKind {
        /// The page the node was read from.
        page: u64,
        /// The unrecognized kind byte.
        byte: u8,
    },
    /// A node's keys were not strictly ascending.
    KeysNotSorted {
        /// The offending page.
        page: u64,
    },
    /// An internal node referenced a child page outside the legal range.
    BadChild {
        /// The internal page.
        page: u64,
        /// The illegal child id.
        child: u64,
    },
    /// Leaves were not all at the same depth.
    UnevenLeafDepth,
    /// A separator key disagreed with the key range of the child it guards.
    SeparatorMismatch {
        /// The internal page whose separator was wrong.
        page: u64,
    },
    /// A non-root node held fewer bytes than the minimum fill.
    Underfull {
        /// The underfull page.
        page: u64,
    },
    /// An internal node had fewer than two children.
    EmptyInternal {
        /// The offending page.
        page: u64,
    },
}

impl std::fmt::Display for Corruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Corruption::Node(k) => write!(f, "{k}"),
            Corruption::UnknownNodeKind { page, byte } => {
                write!(f, "unknown node kind {byte} on page {page}")
            }
            Corruption::KeysNotSorted { page } => write!(f, "unsorted keys on page {page}"),
            Corruption::BadChild { page, child } => {
                write!(f, "page {page} references out-of-range child {child}")
            }
            Corruption::UnevenLeafDepth => write!(f, "leaves at differing depths"),
            Corruption::SeparatorMismatch { page } => {
                write!(f, "separator/child-range mismatch on page {page}")
            }
            Corruption::Underfull { page } => write!(f, "non-root page {page} is underfull"),
            Corruption::EmptyInternal { page } => {
                write!(f, "internal page {page} has fewer than two children")
            }
        }
    }
}

/// Result alias for B+tree operations.
pub type Result<T> = std::result::Result<T, BTreeError>;
