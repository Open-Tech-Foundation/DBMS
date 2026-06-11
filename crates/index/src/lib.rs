//! `index` — secondary index mechanics.
//!
//! A secondary index is an ordinary copy-on-write B+tree whose keys are the
//! **order-preserving encoding of the indexed column values**; for non-unique
//! indexes the encoded primary key is appended so entries stay distinct
//! (`ARCHITECTURE.md` §3.6). Every entry's *value* is the base-tree key (the
//! encoded PK), so resolving an entry to its row is one base lookup.
//!
//! This crate owns the entry contract — composition, NULL semantics, probe
//! bounds — and thin maintenance ops over [`txn::WriteCtx`]. The catalog
//! layer decides *which* indexes exist and drives maintenance inside the same
//! write transaction as the base-row change; the executor (Phase 9) uses the
//! same bounds for index seeks.
//!
//! # NULL semantics
//!
//! - **Unique** indexes skip rows where any indexed column is NULL: NULLs
//!   never conflict (`DECISIONS.md` D17), so such rows simply have no entry.
//! - **Non-unique** indexes include them (NULLs sort first and stay
//!   queryable).

use common::{CategorizedError, ErrorCategory, IoBackend};
use pager::PageId;
use txn::WriteCtx;
use types::{encode_key, Value};

/// Errors raised by the index layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IndexError {
    /// An error from the underlying transaction layer.
    #[error(transparent)]
    Txn(#[from] txn::TxnError),
    /// An error from the type/encoding layer.
    #[error(transparent)]
    Type(#[from] types::TypeError),
}

impl CategorizedError for IndexError {
    fn category(&self) -> ErrorCategory {
        match self {
            IndexError::Txn(e) => e.category(),
            IndexError::Type(e) => e.category(),
        }
    }
}

/// Result alias for index operations.
pub type Result<T> = std::result::Result<T, IndexError>;

/// One index entry: `key` lives in the index tree, `value` is the encoded
/// primary key of the row it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The index-tree key (indexed columns, plus the PK suffix when
    /// non-unique).
    pub key: Vec<u8>,
    /// The base-tree key (encoded PK).
    pub value: Vec<u8>,
}

/// Build the entry a row contributes to one index, or `None` if the row has
/// no entry there (a unique index and a NULL indexed column).
///
/// `cols` are the indexed column values in index order; `pk_key` is the
/// row's encoded primary key.
///
/// # Examples
///
/// ```
/// use index::entry;
/// use types::{encode_key, Value};
///
/// let pk = encode_key(&[Value::I64(7)]).unwrap();
/// // Non-unique entries append the PK, so equal column values stay distinct.
/// let e = entry(&[Value::Text("ada".into())], &pk, false).unwrap().unwrap();
/// assert!(e.key.ends_with(&pk));
/// assert_eq!(e.value, pk);
///
/// // Unique entries are the bare column encoding…
/// let u = entry(&[Value::Text("ada".into())], &pk, true).unwrap().unwrap();
/// assert!(e.key.starts_with(&u.key));
/// // …and a NULL indexed column means no entry at all.
/// assert!(entry(&[Value::Null], &pk, true).unwrap().is_none());
/// ```
pub fn entry(cols: &[Value], pk_key: &[u8], unique: bool) -> Result<Option<Entry>> {
    if unique && cols.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(None);
    }
    let mut key = encode_key(cols)?;
    if !unique {
        key.extend_from_slice(pk_key);
    }
    Ok(Some(Entry {
        key,
        value: pk_key.to_vec(),
    }))
}

/// The `[lo, hi)` bounds covering every entry whose indexed columns equal
/// `cols` — an exact probe on a unique index, a prefix scan on a non-unique
/// one. `hi = None` means unbounded above (the prefix is all `0xFF`).
pub fn prefix_bounds(cols: &[Value]) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let lo = encode_key(cols)?;
    let hi = successor(lo.clone());
    Ok((lo, hi))
}

/// The smallest byte string greater than every string prefixed by `p`
/// (`None` when no such string exists — `p` is all `0xFF`).
fn successor(mut p: Vec<u8>) -> Option<Vec<u8>> {
    while let Some(last) = p.last_mut() {
        if *last == 0xFF {
            p.pop();
        } else {
            *last += 1;
            return Some(p);
        }
    }
    None
}

/// Insert `entry` into the index tree at `root`, returning its new root.
pub fn insert_entry<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    root: PageId,
    entry: &Entry,
) -> Result<PageId> {
    Ok(ctx.insert(root, &entry.key, &entry.value)?)
}

/// Remove `entry` from the index tree at `root`, returning its new root.
pub fn remove_entry<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    root: PageId,
    entry: &Entry,
) -> Result<PageId> {
    Ok(ctx.delete(root, &entry.key)?)
}

/// Probe a **unique** index for `cols`: the encoded PK of the row holding
/// those values, if any.
pub fn probe_unique<B: IoBackend>(
    ctx: &WriteCtx<'_, B>,
    root: PageId,
    cols: &[Value],
) -> Result<Option<Vec<u8>>> {
    let key = encode_key(cols)?;
    Ok(ctx.lookup(root, &key)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(n: i64) -> Vec<u8> {
        encode_key(&[Value::I64(n)]).unwrap()
    }

    #[test]
    fn non_unique_entries_with_equal_columns_stay_distinct() {
        let cols = [Value::Text("same".into())];
        let a = entry(&cols, &pk(1), false).unwrap().unwrap();
        let b = entry(&cols, &pk(2), false).unwrap().unwrap();
        assert_ne!(a.key, b.key);
        // Both fall inside the prefix bounds for the column value.
        let (lo, hi) = prefix_bounds(&cols).unwrap();
        let hi = hi.unwrap();
        assert!(lo <= a.key && a.key < hi);
        assert!(lo <= b.key && b.key < hi);
    }

    #[test]
    fn unique_entries_collide_on_equal_columns() {
        let cols = [Value::I64(42)];
        let a = entry(&cols, &pk(1), true).unwrap().unwrap();
        let b = entry(&cols, &pk(2), true).unwrap().unwrap();
        assert_eq!(a.key, b.key, "collision is what detects the violation");
        assert_ne!(a.value, b.value);
    }

    #[test]
    fn null_columns_skip_unique_but_not_plain_indexes() {
        let cols = [Value::Null, Value::I64(1)];
        assert!(entry(&cols, &pk(1), true).unwrap().is_none());
        assert!(entry(&cols, &pk(1), false).unwrap().is_some());
    }

    #[test]
    fn successor_handles_trailing_ff() {
        assert_eq!(successor(vec![0x01, 0x02]), Some(vec![0x01, 0x03]));
        assert_eq!(successor(vec![0x01, 0xFF]), Some(vec![0x02]));
        assert_eq!(successor(vec![0xFF, 0xFF]), None);
    }

    #[test]
    fn prefix_bounds_cover_exactly_the_prefix() {
        let (lo, hi) = prefix_bounds(&[Value::Text("ab".into())]).unwrap();
        let hi = hi.unwrap();
        let inside = entry(&[Value::Text("ab".into())], &pk(9), false)
            .unwrap()
            .unwrap();
        let outside = entry(&[Value::Text("ac".into())], &pk(9), false)
            .unwrap()
            .unwrap();
        assert!(lo <= inside.key && inside.key < hi);
        assert!(outside.key >= hi);
    }
}
