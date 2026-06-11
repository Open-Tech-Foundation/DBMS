//! `pager` — fixed-size paged file I/O.
//!
//! Owns the on-disk page format and CRC32C checksums, a byte-budgeted LRU page
//! cache, the double-buffered meta pages, and the free-page allocator. Sits
//! just above [`common`] (the injectable [`IoBackend`](common::IoBackend)) and
//! provides the durable, atomically-committing page store the B+tree builds on.
//!
//! ```
//! use common::MemoryBackend;
//! use pager::{PageId, Pager, PageType};
//!
//! let pager = Pager::create(MemoryBackend::new()).unwrap();
//! let id = pager.alloc().unwrap();
//! pager.write_page(id, PageType::Data, b"row data").unwrap();
//! pager.commit().unwrap();
//!
//! let frame = pager.read_page(id).unwrap();
//! assert_eq!(&frame[16..24], b"row data");
//! ```

mod cache;
mod freelist;
mod meta;
mod page;
mod pager;

use common::{CategorizedError, ErrorCategory};

pub use common::crc32c;
pub use meta::Meta;
pub use page::{Frame, PageType, HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE};
pub use pager::{Pager, PagerStats, DEFAULT_CACHE_BYTES};

/// Pages 0 and 1 are the reserved double-buffered meta slots; data pages start
/// here.
pub const FIRST_DATA_PAGE: u64 = 2;

/// A page identifier (an index into the file, in page-size units).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId(u64);

impl PageId {
    /// Wrap a raw page number.
    pub const fn new(raw: u64) -> Self {
        PageId(raw)
    }

    /// The raw page number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for PageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "page#{}", self.0)
    }
}

/// Result alias for pager operations.
pub type Result<T> = std::result::Result<T, PagerError>;

/// Errors raised by the pager.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PagerError {
    /// An underlying I/O failure from the backend.
    #[error(transparent)]
    Io(#[from] common::IoError),
    /// On-disk data failed an integrity check; it is never served as data.
    #[error("corruption: {0}")]
    Corruption(CorruptionKind),
    /// The file was written by an unsupported format version.
    #[error("unsupported format version {found} (this build supports {supported})")]
    UnsupportedVersion {
        /// The version found in the file.
        found: u32,
        /// The version this build supports.
        supported: u32,
    },
    /// A page id outside the currently allocated range was referenced.
    #[error("page {id} is out of range (page_count={page_count})")]
    PageOutOfRange {
        /// The offending page id.
        id: u64,
        /// The current allocation high-water mark.
        page_count: u64,
    },
    /// A payload larger than a page's usable space was supplied.
    #[error("payload of {len} bytes exceeds the {max}-byte page payload")]
    PayloadTooLarge {
        /// The supplied payload length.
        len: usize,
        /// The maximum payload size.
        max: usize,
    },
}

impl CategorizedError for PagerError {
    fn category(&self) -> ErrorCategory {
        match self {
            PagerError::Io(e) => e.category(),
            PagerError::Corruption(_) | PagerError::UnsupportedVersion { .. } => {
                ErrorCategory::Corruption
            }
            PagerError::PageOutOfRange { .. } => ErrorCategory::NotFound,
            PagerError::PayloadTooLarge { .. } => ErrorCategory::Validation,
        }
    }
}

/// The specific way a page or structure was found to be corrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorruptionKind {
    /// A buffer was not exactly one page in length.
    TruncatedPage {
        /// The page being read.
        page: u64,
    },
    /// A page's stored checksum did not match its contents.
    Checksum {
        /// The page that failed.
        page: u64,
    },
    /// A page's self-id did not match where it was read from.
    PageIdMismatch {
        /// The id expected from the read offset.
        expected: u64,
        /// The id stored in the page.
        found: u64,
    },
    /// A page's type byte was not a known value.
    UnknownPageType {
        /// The unrecognized byte.
        byte: u8,
    },
    /// A meta page lacked the format magic.
    BadMagic,
    /// A meta page declared a page size this build does not use.
    BadPageSize {
        /// The page size found.
        found: u32,
    },
    /// A meta slot was structurally invalid.
    BadMeta {
        /// The slot (0 or 1).
        slot: u64,
    },
    /// Neither meta slot was valid on open.
    NoValidMeta,
    /// A free-list entry referenced a page outside the valid range.
    FreelistOutOfRange {
        /// The offending id.
        id: u64,
    },
    /// The free-list trunk chain contained a cycle.
    FreelistCycle {
        /// The id revisited.
        id: u64,
    },
    /// A free-list trunk page was not of type `Freelist`.
    FreelistBadType {
        /// The trunk id.
        id: u64,
    },
    /// A free-list trunk declared more entries than fit in a page.
    FreelistBadCount {
        /// The trunk id.
        id: u64,
    },
    /// The same page id appeared twice in the free list.
    FreelistDuplicate {
        /// The duplicated id.
        id: u64,
    },
    /// The walked free-list length disagreed with the meta record.
    FreelistLenMismatch {
        /// Ids counted by walking the chain.
        counted: u64,
        /// The length recorded in the meta page.
        recorded: u64,
    },
}

impl std::fmt::Display for CorruptionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CorruptionKind::TruncatedPage { page } => write!(f, "truncated page {page}"),
            CorruptionKind::Checksum { page } => write!(f, "checksum mismatch on page {page}"),
            CorruptionKind::PageIdMismatch { expected, found } => {
                write!(f, "page id mismatch: expected {expected}, found {found}")
            }
            CorruptionKind::UnknownPageType { byte } => write!(f, "unknown page type byte {byte}"),
            CorruptionKind::BadMagic => write!(f, "bad file magic"),
            CorruptionKind::BadPageSize { found } => write!(f, "unexpected page size {found}"),
            CorruptionKind::BadMeta { slot } => write!(f, "invalid meta slot {slot}"),
            CorruptionKind::NoValidMeta => write!(f, "no valid meta page"),
            CorruptionKind::FreelistOutOfRange { id } => {
                write!(f, "free-list entry {id} out of range")
            }
            CorruptionKind::FreelistCycle { id } => write!(f, "free-list cycle at {id}"),
            CorruptionKind::FreelistBadType { id } => {
                write!(f, "free-list page {id} has wrong type")
            }
            CorruptionKind::FreelistBadCount { id } => {
                write!(f, "free-list trunk {id} has an invalid count")
            }
            CorruptionKind::FreelistDuplicate { id } => write!(f, "duplicate free-list entry {id}"),
            CorruptionKind::FreelistLenMismatch { counted, recorded } => {
                write!(
                    f,
                    "free-list length mismatch: counted {counted}, recorded {recorded}"
                )
            }
        }
    }
}

pub(crate) fn corrupt(kind: CorruptionKind) -> PagerError {
    PagerError::Corruption(kind)
}
