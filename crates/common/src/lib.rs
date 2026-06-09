//! Cross-cutting foundations shared by every layer of the engine.
//!
//! This crate is deliberately tiny and sits **below** `pager` in the layering
//! (see `ARCHITECTURE.md` §2). It holds only genuinely shared abstractions:
//!
//! - the [`ErrorCategory`] taxonomy from `SPEC.md` §9 and the
//!   [`CategorizedError`] trait each crate's error enum implements, and
//! - the injectable host services [`Clock`], [`Rng`], and [`IoBackend`] (with
//!   real-file, in-memory, and fault-injecting backends) that make the lower
//!   layers testable and deterministically simulatable from day one.
//!
//! Domain newtypes (`PageId`, `TxnId`, `Value`, …) intentionally live in their
//! owning crates, not here — `common` is not a junk drawer.

mod clock;
mod error;
mod io;
mod rng;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::{CategorizedError, ErrorCategory};
pub use io::{FaultInjectingBackend, FaultPoint, IoBackend, IoError, IoResult, MemoryBackend};
pub use rng::{Rng, SeededRng};

#[cfg(unix)]
pub use io::RealFileBackend;
