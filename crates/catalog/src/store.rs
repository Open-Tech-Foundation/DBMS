//! Catalog-tree layout: which keys hold what.
//!
//! The published root **is** the catalog B+tree. Three kinds of entries,
//! keyed with the order-preserving key encoding so each kind forms one
//! contiguous, scannable band:
//!
//! | key | value | changes |
//! |---|---|---|
//! | `("tbl", name)`  | encoded [`TableDef`](crate::TableDef) | on DDL |
//! | `("root", name)` | the table's data-tree root (8 B LE)   | on every write |
//! | `("seq", name)`  | next auto-increment value (8 B LE)    | on auto-key inserts |

use pager::PageId;
use types::{encode_key, Value};

use crate::{CatalogCorruption, CatalogError, Result};

const TBL: &str = "tbl";
const ROOT: &str = "root";
const SEQ: &str = "seq";

fn entry_key(band: &str, table: &str) -> Result<Vec<u8>> {
    Ok(encode_key(&[
        Value::Text(band.to_string()),
        Value::Text(table.to_string()),
    ])?)
}

/// The key of a table's schema entry.
pub(crate) fn tbl_key(table: &str) -> Result<Vec<u8>> {
    entry_key(TBL, table)
}

/// The key of a table's data-root entry.
pub(crate) fn root_key(table: &str) -> Result<Vec<u8>> {
    entry_key(ROOT, table)
}

/// The key of a table's auto-increment sequence entry.
pub(crate) fn seq_key(table: &str) -> Result<Vec<u8>> {
    entry_key(SEQ, table)
}

/// The `[lo, hi)` range containing exactly the `("tbl", *)` band.
///
/// The encoded `"tbl"` component ends with the `0x00 0x00` terminator and
/// every two-component key extends it, so bumping the final terminator byte
/// to `0x01` is the tightest exclusive upper bound for the band.
pub(crate) fn tbl_band() -> Result<(Vec<u8>, Vec<u8>)> {
    let lo = encode_key(&[Value::Text(TBL.to_string())])?;
    let mut hi = lo.clone();
    if let Some(last) = hi.last_mut() {
        *last = 0x01;
    }
    Ok((lo, hi))
}

/// Encode a data-tree root for its `("root", name)` entry.
pub(crate) fn encode_root(root: PageId) -> Vec<u8> {
    root.get().to_le_bytes().to_vec()
}

/// Decode a data-tree root entry.
pub(crate) fn decode_root(bytes: &[u8]) -> Result<PageId> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| CatalogError::Corrupt(CatalogCorruption::BadValue))?;
    Ok(PageId::new(u64::from_le_bytes(arr)))
}

/// Encode an auto-increment sequence value.
pub(crate) fn encode_seq(next: i64) -> Vec<u8> {
    next.to_le_bytes().to_vec()
}

/// Decode an auto-increment sequence value.
pub(crate) fn decode_seq(bytes: &[u8]) -> Result<i64> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| CatalogError::Corrupt(CatalogCorruption::BadValue))?;
    Ok(i64::from_le_bytes(arr))
}
