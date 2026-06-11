//! On-disk page frame: fixed 4 KiB, a self-describing header, and a CRC32C.
//!
//! Layout (little-endian):
//! ```text
//! offset  size  field
//!  0      4     crc32c of bytes [4..PAGE_SIZE]
//!  4      1     page type
//!  5      1     flags (reserved, 0)
//!  6      2     reserved (0)
//!  8      8     page id (self-identifying; detects misdirected writes)
//! 16      ...   payload (PAGE_PAYLOAD_SIZE bytes)
//! ```

use crate::{corrupt, CorruptionKind, PageId, PagerError, Result};

/// The fixed page size in bytes.
pub const PAGE_SIZE: usize = 4096;

/// Bytes of header preceding the payload of every page.
pub const HEADER_SIZE: usize = 16;

/// Usable payload bytes per page.
pub const PAGE_PAYLOAD_SIZE: usize = PAGE_SIZE - HEADER_SIZE;

/// A full in-memory page image.
pub type Frame = [u8; PAGE_SIZE];

const CRC_OFF: usize = 0;
const TYPE_OFF: usize = 4;
const FLAGS_OFF: usize = 5;
const ID_OFF: usize = 8;

/// The kind of data a page holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    /// A double-buffered meta page (slots 0 and 1).
    Meta = 1,
    /// A free-list trunk page.
    Freelist = 2,
    /// A generic data page (used by higher layers, e.g. the B+tree).
    Data = 3,
}

impl PageType {
    fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(PageType::Meta),
            2 => Some(PageType::Freelist),
            3 => Some(PageType::Data),
            _ => None,
        }
    }
}

/// Allocate a zeroed page frame on the heap.
pub fn zeroed() -> Box<Frame> {
    Box::new([0u8; PAGE_SIZE])
}

pub(crate) fn read_u32(frame: &Frame, at: usize) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&frame[at..at + 4]);
    u32::from_le_bytes(buf)
}

pub(crate) fn read_u64(frame: &Frame, at: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&frame[at..at + 8]);
    u64::from_le_bytes(buf)
}

pub(crate) fn write_u32(frame: &mut Frame, at: usize, value: u32) {
    frame[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn write_u64(frame: &mut Frame, at: usize, value: u64) {
    frame[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// Interpret a raw buffer as a page frame, returning `Corruption` if it is not
/// exactly [`PAGE_SIZE`] bytes. Never panics on hostile input.
pub fn as_frame(bytes: &[u8], page: PageId) -> Result<&Frame> {
    bytes
        .try_into()
        .map_err(|_| corrupt(CorruptionKind::TruncatedPage { page: page.get() }))
}

/// Read the page type from a frame.
pub fn page_type(frame: &Frame) -> Result<PageType> {
    PageType::from_u8(frame[TYPE_OFF]).ok_or_else(|| {
        corrupt(CorruptionKind::UnknownPageType {
            byte: frame[TYPE_OFF],
        })
    })
}

/// Read the self-identifying page id from a frame.
pub fn page_id(frame: &Frame) -> u64 {
    read_u64(frame, ID_OFF)
}

/// Verify a raw buffer is a structurally sound page for `expected`: correct
/// length, matching checksum, and matching self-id. Returns the typed `Frame`.
pub fn verify(bytes: &[u8], expected: PageId) -> Result<&Frame> {
    let frame = as_frame(bytes, expected)?;
    let stored = read_u32(frame, CRC_OFF);
    let computed = common::crc32c(&frame[TYPE_OFF..]);
    if stored != computed {
        return Err(corrupt(CorruptionKind::Checksum {
            page: expected.get(),
        }));
    }
    let id = page_id(frame);
    if id != expected.get() {
        return Err(corrupt(CorruptionKind::PageIdMismatch {
            expected: expected.get(),
            found: id,
        }));
    }
    Ok(frame)
}

/// Stamp a frame's header (type, flags, id) and recompute its checksum. Call
/// this last, after the payload is fully written.
pub fn finalize(frame: &mut Frame, page_type: PageType, id: PageId) {
    frame[TYPE_OFF] = page_type as u8;
    frame[FLAGS_OFF] = 0;
    frame[6] = 0;
    frame[7] = 0;
    write_u64(frame, ID_OFF, id.get());
    let crc = common::crc32c(&frame[TYPE_OFF..]);
    write_u32(frame, CRC_OFF, crc);
}

/// Write `payload` into a frame's payload region, returning
/// [`PagerError::PayloadTooLarge`] if it does not fit.
pub fn set_payload(frame: &mut Frame, payload: &[u8]) -> Result<()> {
    if payload.len() > PAGE_PAYLOAD_SIZE {
        return Err(PagerError::PayloadTooLarge {
            len: payload.len(),
            max: PAGE_PAYLOAD_SIZE,
        });
    }
    frame[HEADER_SIZE..HEADER_SIZE + payload.len()].copy_from_slice(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalize_then_verify_round_trips() {
        let mut frame = zeroed();
        set_payload(&mut frame, b"hello payload").unwrap();
        finalize(&mut frame, PageType::Data, PageId::new(7));
        let verified = verify(&frame[..], PageId::new(7)).unwrap();
        assert_eq!(page_type(verified).unwrap(), PageType::Data);
        assert_eq!(page_id(verified), 7);
        assert_eq!(&verified[HEADER_SIZE..HEADER_SIZE + 13], b"hello payload");
    }

    #[test]
    fn corrupted_checksum_is_detected() {
        let mut frame = zeroed();
        finalize(&mut frame, PageType::Data, PageId::new(3));
        frame[100] ^= 0xFF; // flip a payload byte
        let err = verify(&frame[..], PageId::new(3)).unwrap_err();
        assert!(matches!(
            err,
            PagerError::Corruption(CorruptionKind::Checksum { page: 3 })
        ));
    }

    #[test]
    fn misdirected_write_is_detected() {
        let mut frame = zeroed();
        finalize(&mut frame, PageType::Data, PageId::new(5));
        let err = verify(&frame[..], PageId::new(6)).unwrap_err();
        assert!(matches!(
            err,
            PagerError::Corruption(CorruptionKind::PageIdMismatch {
                expected: 6,
                found: 5
            })
        ));
    }

    #[test]
    fn wrong_length_is_rejected_not_panicked() {
        let short = vec![0u8; 100];
        let err = verify(&short, PageId::new(2)).unwrap_err();
        assert!(matches!(
            err,
            PagerError::Corruption(CorruptionKind::TruncatedPage { page: 2 })
        ));
    }

    #[test]
    fn unknown_page_type_is_rejected() {
        let mut frame = zeroed();
        finalize(&mut frame, PageType::Data, PageId::new(2));
        frame[TYPE_OFF] = 99;
        // Re-checksum so the type byte is what fails, not the CRC.
        let crc = common::crc32c(&frame[TYPE_OFF..]);
        write_u32(&mut frame, CRC_OFF, crc);
        let verified = verify(&frame[..], PageId::new(2)).unwrap();
        assert!(matches!(
            page_type(verified).unwrap_err(),
            PagerError::Corruption(CorruptionKind::UnknownPageType { byte: 99 })
        ));
    }
}
