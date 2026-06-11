//! Phase 8 exit criteria, part 2: **fuzzer clean on adversarial input** —
//! the decoder never panics, over-allocates, or hangs; every outcome is
//! `Ok` or a typed error. Seeded and deterministic (`DECISIONS.md` D4): a
//! failure reproduces from the printed seed/iteration.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use common::{Rng, SeededRng};
use proto::{decode_doc, decode_request, encode_request, DecodeLimits, Request};

const RANDOM_INPUTS: u64 = 100_000;
const MUTATED_INPUTS: u64 = 100_000;

/// A small corpus of valid wire messages to mutate (taken from the
/// round-trip suite's territory: both surfaces, DML, a transaction).
fn corpus() -> Vec<Vec<u8>> {
    use proto::{
        ClauseSelect, CmpOp, Delete, Dir, Expr, Insert, Select, Selector, SortKey, Stage, TableRef,
        Update,
    };
    use types::Value;
    let col = |name: &str| Expr::Column {
        table: None,
        column: name.to_string(),
    };
    let requests = [
        Request::Select(Select::Pipeline(vec![
            Stage::Scan(TableRef {
                table: "users".to_string(),
                alias: Some("u".to_string()),
            }),
            Stage::Match(Expr::Cmp {
                op: CmpOp::Gt,
                lhs: Box::new(col("age")),
                rhs: Box::new(Expr::Literal(Value::I64(18))),
            }),
            Stage::Sort(vec![SortKey {
                expr: col("age"),
                dir: Dir::Desc,
            }]),
            Stage::Limit {
                limit: Some(10),
                offset: 0,
            },
        ])),
        Request::Select(Select::Clause(Box::new(ClauseSelect {
            from: Some(TableRef {
                table: "t".to_string(),
                alias: None,
            }),
            where_: Some(Expr::IsNull(Box::new(col("x")))),
            limit: Some(5),
            ..ClauseSelect::default()
        }))),
        Request::Insert(Insert {
            table: "users".to_string(),
            rows: vec![vec![
                ("name".to_string(), Value::Text("Ada".to_string())),
                ("age".to_string(), Value::I64(36)),
            ]],
        }),
        Request::Transaction(vec![Request::Update(Update {
            table: "accounts".to_string(),
            selector: Some(Selector::Where(Expr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(col("id")),
                rhs: Box::new(Expr::Literal(Value::I64(1))),
            })),
            set: vec![("balance".to_string(), Expr::Literal(Value::I64(0)))],
            unconditional: false,
        })]),
        Request::Delete(Delete {
            table: "sessions".to_string(),
            selector: Some(Selector::All),
        }),
    ];
    requests.iter().map(encode_request).collect()
}

/// Decoding must return, not panic — and a decoded request must re-encode
/// and decode back to itself (no "parses but unrepresentable" states).
fn probe(bytes: &[u8], limits: &DecodeLimits, what: &str) {
    if let Ok(request) = decode_request(bytes, limits) {
        let canonical = encode_request(&request);
        let again = decode_request(&canonical, limits)
            .unwrap_or_else(|e| panic!("{what}: canonical re-decode failed: {e}"));
        assert_eq!(again, request, "{what}: canonical round-trip diverged");
    }
}

#[test]
fn random_bytes_never_panic_the_decoder() {
    let rng = SeededRng::new(0xF0221E5);
    let limits = DecodeLimits::default();
    let tight = DecodeLimits {
        max_bytes: 256,
        max_depth: 8,
        max_nodes: 64,
    };
    for i in 0..RANDOM_INPUTS {
        let len = (rng.next_u64() % 96) as usize;
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            bytes.push((rng.next_u64() & 0xFF) as u8);
        }
        probe(&bytes, &limits, &format!("random #{i}"));
        // The doc layer alone, under tight limits too.
        let _ = decode_doc(&bytes, &tight);
        let _ = proto::decode_cursor_token(&bytes);
    }
}

#[test]
fn mutated_valid_messages_never_panic_the_decoder() {
    let rng = SeededRng::new(0xBADC_0FFE);
    let limits = DecodeLimits::default();
    let corpus = corpus();
    for i in 0..MUTATED_INPUTS {
        let base = &corpus[(rng.next_u64() as usize) % corpus.len()];
        let mut bytes = base.clone();
        match rng.next_u64() % 4 {
            // Bit flips.
            0 => {
                for _ in 0..=(rng.next_u64() % 8) {
                    let pos = (rng.next_u64() as usize) % bytes.len();
                    bytes[pos] ^= 1 << (rng.next_u64() % 8);
                }
            }
            // Truncation.
            1 => {
                let keep = (rng.next_u64() as usize) % (bytes.len() + 1);
                bytes.truncate(keep);
            }
            // Random byte overwrite.
            2 => {
                let pos = (rng.next_u64() as usize) % bytes.len();
                bytes[pos] = (rng.next_u64() & 0xFF) as u8;
            }
            // Splice a chunk of another corpus entry into the middle.
            _ => {
                let other = &corpus[(rng.next_u64() as usize) % corpus.len()];
                let at = (rng.next_u64() as usize) % bytes.len();
                let take = (rng.next_u64() as usize) % other.len();
                let tail = bytes.split_off(at);
                bytes.extend_from_slice(&other[..take]);
                bytes.extend_from_slice(&tail);
            }
        }
        probe(&bytes, &limits, &format!("mutation #{i}"));
    }
}

#[test]
fn hostile_container_shapes_are_handled() {
    // Hand-built nasties beyond what random mutation tends to find.
    let limits = DecodeLimits::default();
    let mut nasties: Vec<Vec<u8>> = Vec::new();
    // Deep nesting exactly at, just under, and far over the limit.
    for depth in [63usize, 64, 65, 1000, 100_000] {
        let mut bytes = vec![0x91; depth];
        bytes.push(0xC0);
        nasties.push(bytes);
    }
    // Wide flat array right at the node budget.
    nasties.push({
        let mut bytes = vec![0xDD];
        bytes.extend_from_slice(&100_000u32.to_be_bytes());
        bytes.extend(std::iter::repeat_n(0xC0, 100_000));
        bytes
    });
    // Containers claiming far more items than the message could hold.
    for head in [0xDCu8, 0xDE] {
        nasties.push(vec![head, 0xFF, 0xFF, 0xC0]);
    }
    for head in [0xDDu8, 0xDF] {
        let mut bytes = vec![head];
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        nasties.push(bytes);
    }
    // str32/bin32 claiming u32::MAX bytes.
    for head in [0xDBu8, 0xC6] {
        let mut bytes = vec![head];
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        bytes.push(b'x');
        nasties.push(bytes);
    }
    for (i, bytes) in nasties.iter().enumerate() {
        let _ = decode_doc(bytes, &limits);
        probe(bytes, &limits, &format!("nasty #{i}"));
    }
}
