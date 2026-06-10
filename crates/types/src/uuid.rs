//! UUIDv7 generation (RFC 9562) and the canonical string form.
//!
//! UUIDv7 is time-ordered — the default generator for key/indexed columns
//! (`SPEC.md` §3) precisely because near-sequential keys insert efficiently
//! into B+trees. The generator is driven by the injected [`Clock`] and
//! [`Rng`], so it is fully deterministic under simulation, and it is
//! **strictly monotonic within a run** even if the clock stalls or steps
//! backward (a counter in `rand_a` carries the order; on overflow the
//! timestamp is nudged forward).

use std::sync::{Arc, Mutex};

use common::{Clock, Rng};

use crate::{Result, TypeError};

/// Layout: 48-bit unix-ms timestamp | 4-bit version (7) | 12-bit `rand_a`
/// (used as a monotonicity counter) | 2-bit variant (0b10) | 62-bit `rand_b`.
///
/// Owns its (shared) host services so it can live as long as the engine —
/// the monotonicity state must span transactions, not one borrow.
pub struct UuidV7Gen {
    clock: Arc<dyn Clock>,
    rng: Arc<dyn Rng>,
    /// The last issued `(unix_ms, counter)` pair, strictly increasing.
    last: Mutex<(u64, u16)>,
}

impl UuidV7Gen {
    /// Create a generator over the injected host services.
    pub fn new(clock: Arc<dyn Clock>, rng: Arc<dyn Rng>) -> Self {
        UuidV7Gen {
            clock,
            rng,
            last: Mutex::new((0, 0)),
        }
    }

    /// Generate the next UUIDv7. Strictly greater (bytewise) than every UUID
    /// previously returned by this generator.
    pub fn next_uuid(&self) -> [u8; 16] {
        let now_ms = unix_ms(self.clock.now_micros());
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let (ms, counter) = if now_ms > last.0 {
            // Fresh millisecond: re-seed the counter randomly in the lower
            // half of its 12-bit space, leaving headroom to count within.
            (now_ms, (self.rng.next_u64() & 0x7FF) as u16)
        } else if last.1 < 0xFFF {
            // Same (or backward) clock: count within the last timestamp.
            (last.0, last.1 + 1)
        } else {
            // Counter exhausted: nudge the timestamp forward one ms.
            (last.0 + 1, (self.rng.next_u64() & 0x7FF) as u16)
        };
        *last = (ms, counter);
        drop(last);

        let rand_b = self.rng.next_u64();
        let mut uuid = [0u8; 16];
        // 48-bit big-endian timestamp.
        uuid[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
        // Version 7 in the top nibble, then the 12-bit counter.
        uuid[6] = 0x70 | ((counter >> 8) as u8 & 0x0F);
        uuid[7] = (counter & 0xFF) as u8;
        // Variant 0b10 over the top two bits, then 62 random bits.
        let tail = (rand_b & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
        uuid[8..].copy_from_slice(&tail.to_be_bytes());
        uuid
    }
}

/// Clamp epoch-microseconds to a non-negative 48-bit millisecond value.
fn unix_ms(micros: i64) -> u64 {
    let ms = micros.div_euclid(1000).max(0) as u64;
    ms & 0xFFFF_FFFF_FFFF
}

/// Format a UUID in its canonical lowercase hyphenated form
/// (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`).
///
/// # Examples
///
/// ```
/// use types::{uuid_from_str, uuid_to_string};
///
/// let s = "0190a0b1-c2d3-7e4f-8a9b-0c1d2e3f4a5b";
/// assert_eq!(uuid_to_string(&uuid_from_str(s).unwrap()), s);
/// ```
pub fn uuid_to_string(uuid: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(36);
    for (i, byte) in uuid.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }
    out
}

/// Parse a canonical hyphenated UUID string (case-insensitive on input;
/// output is always lowercase).
pub fn uuid_from_str(s: &str) -> Result<[u8; 16]> {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return Err(TypeError::BadUuid);
    }
    let mut uuid = [0u8; 16];
    let mut nibbles = bytes.iter().enumerate().filter_map(|(i, &b)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            if b == b'-' {
                None
            } else {
                Some(Err(TypeError::BadUuid))
            }
        } else {
            Some(hex_nibble(b))
        }
    });
    for slot in &mut uuid {
        let hi = nibbles.next().ok_or(TypeError::BadUuid)??;
        let lo = nibbles.next().ok_or(TypeError::BadUuid)??;
        *slot = (hi << 4) | lo;
    }
    Ok(uuid)
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(TypeError::BadUuid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_form_round_trips() {
        let uuid = *b"\x01\x90\xA0\xB1\xC2\xD3\x7E\x4F\x8A\x9B\x0C\x1D\x2E\x3F\x4A\x5B";
        let s = uuid_to_string(&uuid);
        assert_eq!(s, "0190a0b1-c2d3-7e4f-8a9b-0c1d2e3f4a5b");
        assert_eq!(uuid_from_str(&s).unwrap(), uuid);
        assert_eq!(uuid_from_str(&s.to_uppercase()).unwrap(), uuid);
    }

    #[test]
    fn bad_strings_are_rejected() {
        for s in [
            "",
            "0190a0b1c2d37e4f8a9b0c1d2e3f4a5b",
            "0190a0b1-c2d3-7e4f-8a9b-0c1d2e3f4a5",
            "0190a0b1-c2d3-7e4f-8a9b-0c1d2e3f4a5g",
            "0190a0b1+c2d3-7e4f-8a9b-0c1d2e3f4a5b",
        ] {
            assert!(uuid_from_str(s).is_err(), "{s:?} should be rejected");
        }
    }
}
