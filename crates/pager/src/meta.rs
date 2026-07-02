//! The double-buffered meta page (`SPEC.md` §10).
//!
//! Pages 0 and 1 each hold a meta image. A commit writes the *inactive* slot
//! and fsyncs, then promotes it; recovery adopts the checksum-valid slot with
//! the highest committed transaction id.

use crate::page::{self, Frame, HEADER_SIZE};
use crate::{corrupt, CorruptionKind, PageId, PagerError, Result};

/// File-format magic: identifies an OTF EDB file and its format generation.
pub(crate) const MAGIC: [u8; 8] = *b"OTF-EDB\x01";

/// The format version this build reads and writes.
pub(crate) const FORMAT_VERSION: u32 = 1;

const MAGIC_OFF: usize = HEADER_SIZE; // 16
const VERSION_OFF: usize = 24;
const PAGE_SIZE_OFF: usize = 28;
const TXN_OFF: usize = 32;
const PAGE_COUNT_OFF: usize = 40;
const FREE_HEAD_OFF: usize = 48;
const FREE_LEN_OFF: usize = 56;
const CATALOG_OFF: usize = 64;

/// The committed root state recorded in a meta page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    /// Format version of the file.
    pub format_version: u32,
    /// Page size in bytes (must equal [`page::PAGE_SIZE`]).
    pub page_size: u32,
    /// Monotonic committed transaction id.
    pub txn_id: u64,
    /// Total number of pages the file accounts for (allocation high-water mark).
    pub page_count: u64,
    /// Head of the free-list trunk chain (`None` if empty).
    pub freelist_head: Option<PageId>,
    /// Number of free page ids stored across the trunk chain.
    pub freelist_len: u64,
    /// Root of the system catalog (`None` until the catalog exists).
    pub catalog_root: Option<PageId>,
}

impl Meta {
    /// The meta of a freshly created, empty database: pages 0 and 1 reserved.
    pub(crate) fn initial() -> Self {
        Meta {
            format_version: FORMAT_VERSION,
            page_size: page::PAGE_SIZE as u32,
            txn_id: 0,
            page_count: crate::FIRST_DATA_PAGE,
            freelist_head: None,
            freelist_len: 0,
            catalog_root: None,
        }
    }

    /// Encode this meta into a freshly zeroed frame's payload. The caller must
    /// still [`page::finalize`] the frame (which stamps the checksum).
    pub(crate) fn encode(&self, frame: &mut Frame) {
        frame[MAGIC_OFF..MAGIC_OFF + 8].copy_from_slice(&MAGIC);
        page::write_u32(frame, VERSION_OFF, self.format_version);
        page::write_u32(frame, PAGE_SIZE_OFF, self.page_size);
        page::write_u64(frame, TXN_OFF, self.txn_id);
        page::write_u64(frame, PAGE_COUNT_OFF, self.page_count);
        page::write_u64(frame, FREE_HEAD_OFF, opt_to_u64(self.freelist_head));
        page::write_u64(frame, FREE_LEN_OFF, self.freelist_len);
        page::write_u64(frame, CATALOG_OFF, opt_to_u64(self.catalog_root));
    }

    /// Decode and validate a meta page read from `slot` (0 or 1).
    pub(crate) fn decode(frame: &Frame, slot: PageId) -> Result<Self> {
        if frame[MAGIC_OFF..MAGIC_OFF + 8] != MAGIC {
            return Err(corrupt(CorruptionKind::BadMagic));
        }
        let format_version = page::read_u32(frame, VERSION_OFF);
        if format_version != FORMAT_VERSION {
            return Err(PagerError::UnsupportedVersion {
                found: format_version,
                supported: FORMAT_VERSION,
            });
        }
        let page_size = page::read_u32(frame, PAGE_SIZE_OFF);
        if page_size as usize != page::PAGE_SIZE {
            return Err(corrupt(CorruptionKind::BadPageSize { found: page_size }));
        }
        let meta = Meta {
            format_version,
            page_size,
            txn_id: page::read_u64(frame, TXN_OFF),
            page_count: page::read_u64(frame, PAGE_COUNT_OFF),
            freelist_head: u64_to_opt(page::read_u64(frame, FREE_HEAD_OFF)),
            freelist_len: page::read_u64(frame, FREE_LEN_OFF),
            catalog_root: u64_to_opt(page::read_u64(frame, CATALOG_OFF)),
        };
        // A page_count below the two reserved meta slots is structurally impossible.
        if meta.page_count < crate::FIRST_DATA_PAGE {
            return Err(corrupt(CorruptionKind::BadMeta { slot: slot.get() }));
        }
        Ok(meta)
    }
}

fn opt_to_u64(id: Option<PageId>) -> u64 {
    id.map_or(0, PageId::get)
}

fn u64_to_opt(value: u64) -> Option<PageId> {
    if value == 0 {
        None
    } else {
        Some(PageId::new(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{finalize, verify, PageType};

    fn sample() -> Meta {
        Meta {
            format_version: FORMAT_VERSION,
            page_size: page::PAGE_SIZE as u32,
            txn_id: 42,
            page_count: 17,
            freelist_head: Some(PageId::new(9)),
            freelist_len: 3,
            catalog_root: Some(PageId::new(5)),
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        let meta = sample();
        let mut frame = page::zeroed();
        meta.encode(&mut frame);
        finalize(&mut frame, PageType::Meta, PageId::new(0));
        let frame = verify(&frame[..], PageId::new(0)).unwrap();
        assert_eq!(Meta::decode(frame, PageId::new(0)).unwrap(), meta);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut frame = page::zeroed();
        sample().encode(&mut frame);
        frame[MAGIC_OFF] ^= 0xFF;
        finalize(&mut frame, PageType::Meta, PageId::new(0));
        let frame = verify(&frame[..], PageId::new(0)).unwrap();
        assert!(matches!(
            Meta::decode(frame, PageId::new(0)).unwrap_err(),
            PagerError::Corruption(CorruptionKind::BadMagic)
        ));
    }

    #[test]
    fn unsupported_version_is_reported() {
        let mut meta = sample();
        meta.format_version = 999;
        let mut frame = page::zeroed();
        meta.encode(&mut frame);
        finalize(&mut frame, PageType::Meta, PageId::new(0));
        let frame = verify(&frame[..], PageId::new(0)).unwrap();
        assert!(matches!(
            Meta::decode(frame, PageId::new(0)).unwrap_err(),
            PagerError::UnsupportedVersion {
                found: 999,
                supported: 1
            }
        ));
    }
}
