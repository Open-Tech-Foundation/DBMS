//! Free-page list: a linked chain of trunk pages holding free page ids.
//!
//! A trunk page's payload is `next: u64` (the next trunk, 0 = end), `count: u32`
//! (ids stored here), then `count` little-endian `u64` ids.
//!
//! This is only the **durable serialization** of the free set: the authoritative
//! set lives in memory in the pager, and the chain is rebuilt into fresh,
//! crash-safe pages at each commit (see `pager::Pager::rebuild_free_list`) rather
//! than mutated in place — an in-place trunk write flushed before the meta swap
//! would corrupt the last committed meta's free-list on a crash.

use crate::page::{self, Frame, HEADER_SIZE};

const NEXT_OFF: usize = HEADER_SIZE; // 16
const COUNT_OFF: usize = 24;
const IDS_OFF: usize = 28;

/// Maximum number of free ids a single trunk page can hold.
pub(crate) const CAPACITY: u32 = ((page::PAGE_SIZE - IDS_OFF) / 8) as u32;

pub(crate) fn next(frame: &Frame) -> u64 {
    page::read_u64(frame, NEXT_OFF)
}

pub(crate) fn set_next(frame: &mut Frame, value: u64) {
    page::write_u64(frame, NEXT_OFF, value);
}

pub(crate) fn count(frame: &Frame) -> u32 {
    page::read_u32(frame, COUNT_OFF)
}

pub(crate) fn set_count(frame: &mut Frame, value: u32) {
    page::write_u32(frame, COUNT_OFF, value);
}

pub(crate) fn id_at(frame: &Frame, index: u32) -> u64 {
    page::read_u64(frame, IDS_OFF + index as usize * 8)
}

pub(crate) fn set_id_at(frame: &mut Frame, index: u32, value: u64) {
    page::write_u64(frame, IDS_OFF + index as usize * 8, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_fits_within_the_page() {
        assert!(IDS_OFF + CAPACITY as usize * 8 <= page::PAGE_SIZE);
        assert_eq!(CAPACITY, 508);
    }

    #[test]
    fn trunk_fields_round_trip() {
        let mut frame = page::zeroed();
        set_next(&mut frame, 12);
        set_count(&mut frame, 2);
        set_id_at(&mut frame, 0, 100);
        set_id_at(&mut frame, 1, 200);
        assert_eq!(next(&frame), 12);
        assert_eq!(count(&frame), 2);
        assert_eq!(id_at(&frame, 0), 100);
        assert_eq!(id_at(&frame, 1), 200);
    }
}
