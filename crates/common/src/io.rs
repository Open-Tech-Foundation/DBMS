//! Injectable block I/O.
//!
//! Every byte the engine persists flows through [`IoBackend`], a small
//! offset-addressed read/write/sync abstraction. Three implementations ship:
//!
//! - [`RealFileBackend`] — a real file (unix; positional `pread`/`pwrite`),
//! - [`MemoryBackend`] — an in-memory buffer for fast, isolated tests, and
//! - [`FaultInjectingBackend`] — wraps another backend and fails a chosen
//!   operation occurrence, for crash/durability testing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, RwLock};

use crate::{CategorizedError, ErrorCategory};

/// Result alias for backend operations.
pub type IoResult<T> = Result<T, IoError>;

/// A failure from an [`IoBackend`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IoError {
    /// An error reported by the underlying backing store.
    #[error("backend I/O failure: {0}")]
    Backend(#[from] std::io::Error),
    /// A deliberately injected fault (testing only).
    #[error("injected fault at {point} (occurrence {occurrence})")]
    Injected {
        /// Which operation was tripped.
        point: FaultPoint,
        /// The 1-based occurrence count at which it tripped.
        occurrence: u64,
    },
    /// An access that would read or address past the end of the store.
    #[error(
        "access past end of backing store: offset={offset}, requested={requested}, size={size}"
    )]
    OutOfBounds {
        /// Starting byte offset of the access.
        offset: u64,
        /// Number of bytes requested.
        requested: u64,
        /// Current size of the backing store.
        size: u64,
    },
}

impl CategorizedError for IoError {
    fn category(&self) -> ErrorCategory {
        ErrorCategory::Io
    }
}

/// An offset-addressed block store. Implementations must be safe to share
/// across threads (the engine reads concurrently from many threads).
///
/// Reads and writes are positional and do not maintain a cursor.
pub trait IoBackend: Send + Sync {
    /// Fill `buf` with bytes starting at `offset`. Errors if the range is not
    /// fully backed.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()>;

    /// Write `data` starting at `offset`, growing the store if needed.
    fn write_at(&self, offset: u64, data: &[u8]) -> IoResult<()>;

    /// Flush all prior writes durably to the backing store.
    fn sync(&self) -> IoResult<()>;

    /// The current size of the store in bytes.
    fn len(&self) -> IoResult<u64>;

    /// Whether the store is empty.
    fn is_empty(&self) -> IoResult<bool> {
        Ok(self.len()? == 0)
    }

    /// Resize the store to exactly `len` bytes (truncating or zero-extending).
    fn truncate(&self, len: u64) -> IoResult<()>;
}

/// An in-memory [`IoBackend`] backed by a growable byte buffer.
///
/// # Examples
///
/// ```
/// use common::{IoBackend, MemoryBackend};
///
/// let store = MemoryBackend::new();
/// store.write_at(0, b"hello").unwrap();
/// let mut buf = [0u8; 5];
/// store.read_at(0, &mut buf).unwrap();
/// assert_eq!(&buf, b"hello");
/// ```
#[derive(Debug, Default)]
pub struct MemoryBackend {
    data: RwLock<Vec<u8>>,
}

impl MemoryBackend {
    /// Create an empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an in-memory store pre-populated with `bytes`.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            data: RwLock::new(bytes),
        }
    }

    /// Take a copy of the current contents.
    pub fn snapshot(&self) -> Vec<u8> {
        self.data
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl IoBackend for MemoryBackend {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
        let data = self.data.read().unwrap_or_else(PoisonError::into_inner);
        let requested = buf.len() as u64;
        let size = data.len() as u64;
        let end = offset.checked_add(requested).ok_or(IoError::OutOfBounds {
            offset,
            requested,
            size,
        })?;
        if end > size {
            return Err(IoError::OutOfBounds {
                offset,
                requested,
                size,
            });
        }
        let start = offset as usize;
        buf.copy_from_slice(&data[start..start + buf.len()]);
        Ok(())
    }

    fn write_at(&self, offset: u64, src: &[u8]) -> IoResult<()> {
        let mut data = self.data.write().unwrap_or_else(PoisonError::into_inner);
        let requested = src.len() as u64;
        let end = offset.checked_add(requested).ok_or(IoError::OutOfBounds {
            offset,
            requested,
            size: data.len() as u64,
        })?;
        let end = usize::try_from(end).map_err(|_| IoError::OutOfBounds {
            offset,
            requested,
            size: data.len() as u64,
        })?;
        if data.len() < end {
            data.resize(end, 0);
        }
        let start = offset as usize;
        data[start..end].copy_from_slice(src);
        Ok(())
    }

    fn sync(&self) -> IoResult<()> {
        Ok(())
    }

    fn len(&self) -> IoResult<u64> {
        Ok(self
            .data
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len() as u64)
    }

    fn truncate(&self, len: u64) -> IoResult<()> {
        let new_len = usize::try_from(len).map_err(|_| IoError::OutOfBounds {
            offset: 0,
            requested: len,
            size: 0,
        })?;
        self.data
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .resize(new_len, 0);
        Ok(())
    }
}

/// The operation an injected fault targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    /// A [`IoBackend::read_at`] call.
    Read,
    /// A [`IoBackend::write_at`] call.
    Write,
    /// A [`IoBackend::sync`] call.
    Sync,
}

impl std::fmt::Display for FaultPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            FaultPoint::Read => "read",
            FaultPoint::Write => "write",
            FaultPoint::Sync => "sync",
        };
        f.write_str(s)
    }
}

/// Wraps an [`IoBackend`] and fails a chosen operation occurrence, simulating
/// a crash or hardware fault at a precise point for durability testing.
///
/// Arm a single trip point with [`arm`](Self::arm); the Nth matching operation
/// (1-based) then returns [`IoError::Injected`]. Operations of each kind are
/// counted independently.
///
/// # Examples
///
/// ```
/// use common::{FaultInjectingBackend, FaultPoint, IoBackend, MemoryBackend};
///
/// let store = FaultInjectingBackend::new(MemoryBackend::new());
/// store.arm(FaultPoint::Sync, 1); // fail the first sync
/// store.write_at(0, b"data").unwrap();
/// assert!(store.sync().is_err());
/// assert!(store.sync().is_ok()); // second sync succeeds
/// ```
#[derive(Debug)]
pub struct FaultInjectingBackend<B: IoBackend> {
    inner: B,
    armed: Mutex<Option<(FaultPoint, u64)>>,
    reads: AtomicU64,
    writes: AtomicU64,
    syncs: AtomicU64,
}

impl<B: IoBackend> FaultInjectingBackend<B> {
    /// Wrap `inner` with no fault armed.
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            armed: Mutex::new(None),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
        }
    }

    /// Arm a fault on the `occurrence`-th (1-based) operation of `point`.
    /// Replaces any previously armed trip point.
    pub fn arm(&self, point: FaultPoint, occurrence: u64) {
        *self.armed.lock().unwrap_or_else(PoisonError::into_inner) = Some((point, occurrence));
    }

    /// Clear any armed fault.
    pub fn disarm(&self) {
        *self.armed.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Reset all per-operation occurrence counters to zero, so a subsequent
    /// [`arm`](Self::arm) targets operations relative to this point. Useful for
    /// faulting a specific operation within a later phase of a workload.
    pub fn reset_counters(&self) {
        self.reads.store(0, Ordering::Relaxed);
        self.writes.store(0, Ordering::Relaxed);
        self.syncs.store(0, Ordering::Relaxed);
    }

    /// Consume the wrapper and return the inner backend.
    pub fn into_inner(self) -> B {
        self.inner
    }

    fn counter(&self, point: FaultPoint) -> &AtomicU64 {
        match point {
            FaultPoint::Read => &self.reads,
            FaultPoint::Write => &self.writes,
            FaultPoint::Sync => &self.syncs,
        }
    }

    fn check(&self, point: FaultPoint) -> IoResult<()> {
        let occurrence = self.counter(point).fetch_add(1, Ordering::Relaxed) + 1;
        let armed = *self.armed.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((armed_point, armed_occ)) = armed {
            if armed_point == point && armed_occ == occurrence {
                return Err(IoError::Injected { point, occurrence });
            }
        }
        Ok(())
    }
}

impl<B: IoBackend> IoBackend for FaultInjectingBackend<B> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
        self.check(FaultPoint::Read)?;
        self.inner.read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> IoResult<()> {
        self.check(FaultPoint::Write)?;
        self.inner.write_at(offset, data)
    }

    fn sync(&self) -> IoResult<()> {
        self.check(FaultPoint::Sync)?;
        self.inner.sync()
    }

    fn len(&self) -> IoResult<u64> {
        self.inner.len()
    }

    fn truncate(&self, len: u64) -> IoResult<()> {
        self.inner.truncate(len)
    }
}

/// Sharing an [`IoBackend`] behind an [`Arc`](std::sync::Arc) keeps it an
/// `IoBackend`. This lets a test retain a handle (e.g. to arm faults) while a
/// `Pager` owns another clone of the same backend.
impl<B: IoBackend + ?Sized> IoBackend for std::sync::Arc<B> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
        (**self).read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> IoResult<()> {
        (**self).write_at(offset, data)
    }

    fn sync(&self) -> IoResult<()> {
        (**self).sync()
    }

    fn len(&self) -> IoResult<u64> {
        (**self).len()
    }

    fn truncate(&self, len: u64) -> IoResult<()> {
        (**self).truncate(len)
    }
}

/// A real-file [`IoBackend`] using positional reads/writes (`pread`/`pwrite`),
/// so concurrent reads need no shared cursor.
#[cfg(unix)]
#[derive(Debug)]
pub struct RealFileBackend {
    file: std::fs::File,
}

#[cfg(unix)]
impl RealFileBackend {
    /// Open (creating if absent) the file at `path` for read/write.
    pub fn open(path: impl AsRef<std::path::Path>) -> IoResult<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Self { file })
    }
}

#[cfg(unix)]
impl IoBackend for RealFileBackend {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset)?;
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> IoResult<()> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(data, offset)?;
        Ok(())
    }

    fn sync(&self) -> IoResult<()> {
        self.file.sync_all()?;
        Ok(())
    }

    fn len(&self) -> IoResult<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn truncate(&self, len: u64) -> IoResult<()> {
        self.file.set_len(len)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(store: &dyn IoBackend) {
        assert!(store.is_empty().unwrap());
        store.write_at(0, b"hello world").unwrap();
        store.sync().unwrap();
        assert_eq!(store.len().unwrap(), 11);

        let mut buf = [0u8; 5];
        store.read_at(6, &mut buf).unwrap();
        assert_eq!(&buf, b"world");

        // Sparse write grows the store and zero-fills the gap.
        store.write_at(20, b"!").unwrap();
        assert_eq!(store.len().unwrap(), 21);
        let mut gap = [0xFFu8; 4];
        store.read_at(12, &mut gap).unwrap();
        assert_eq!(gap, [0, 0, 0, 0]);

        store.truncate(5).unwrap();
        assert_eq!(store.len().unwrap(), 5);
    }

    #[test]
    fn memory_backend_round_trip() {
        let store = MemoryBackend::new();
        round_trip(&store);
    }

    #[test]
    fn memory_backend_reads_past_end_are_rejected() {
        let store = MemoryBackend::new();
        store.write_at(0, b"abc").unwrap();
        let mut buf = [0u8; 4];
        let err = store.read_at(0, &mut buf).unwrap_err();
        assert_eq!(err.category(), ErrorCategory::Io);
        assert!(matches!(err, IoError::OutOfBounds { .. }));
    }

    #[test]
    fn memory_backend_from_bytes_and_snapshot() {
        let store = MemoryBackend::from_bytes(vec![1, 2, 3]);
        assert_eq!(store.snapshot(), vec![1, 2, 3]);
    }

    #[test]
    fn fault_injection_trips_once_at_the_chosen_occurrence() {
        let store = FaultInjectingBackend::new(MemoryBackend::new());
        store.arm(FaultPoint::Write, 2);

        store.write_at(0, b"a").unwrap(); // 1st write ok
        let err = store.write_at(1, b"b").unwrap_err(); // 2nd write trips
        assert!(matches!(
            err,
            IoError::Injected {
                point: FaultPoint::Write,
                occurrence: 2
            }
        ));
        store.write_at(2, b"c").unwrap(); // 3rd write ok again
    }

    #[test]
    fn fault_injection_can_be_disarmed() {
        let store = FaultInjectingBackend::new(MemoryBackend::new());
        store.arm(FaultPoint::Sync, 1);
        store.disarm();
        store.sync().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn real_file_backend_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "common-io-rt-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let store = RealFileBackend::open(&path).unwrap();
        round_trip(&store);
        drop(store);
        let _ = std::fs::remove_file(&path);
    }
}
