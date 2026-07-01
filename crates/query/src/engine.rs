//! The top-level query entry point: the full journey a request takes
//! (`ARCHITECTURE.md` §4).
//!
//! - **Read** (`select`): validate → lower → plan → execute → shape a
//!   [`QueryResult`] (`columns` + `rows`).
//! - **Explain**: validate → plan → render the physical plan as `rows` of text.
//! - **Write** (`insert`/`update`/`delete`): validate → run in the writer →
//!   report `applied`/`affected`.
//! - **Transaction**: validate every op, then commit them as **one atomic
//!   batch** in the writer.
//!
//! [`execute_wire`] is the bytes-in/bytes-out form: it decodes a hardened
//! request, runs it, and encodes either a result or a typed error result
//! (`SPEC.md` §5.6/§9). The misuse-resistant public API arrives in Phase 10;
//! this is the engine seam it will wrap.

use common::{CategorizedError, IoBackend};
use proto::{
    decode_request, encode_error_result, encode_result, DecodeLimits, QueryResult, Request, Select,
};
use types::Value;

use catalog::Catalog;

use crate::lower::lower;
use crate::plan::{plan, render_plan};
use crate::stream::execute_page;
use crate::validate::validate;
use crate::write::{execute_write, recover_batch, write_spec};
use crate::QueryError;

/// Run a validated request against `cat`, returning the shaped result.
pub fn execute_query<B: IoBackend + 'static>(
    request: &Request,
    cat: &Catalog<B>,
) -> Result<QueryResult, QueryError> {
    let snap = cat.snapshot();
    // Every request is validated against the current schema first.
    validate(request, &snap)?;

    match request {
        Request::Select(select) => run_select(select, &snap),
        Request::Explain(select) => run_explain(select, &snap),
        Request::Insert(_) | Request::Update(_) | Request::Delete(_) => {
            let out = execute_write(request, cat)?;
            Ok(QueryResult {
                applied: out.applied,
                affected: Some(out.affected),
                ..QueryResult::default()
            })
        }
        Request::Transaction(ops) => run_transaction(ops, cat),
    }
}

/// Decode, run, and encode a request: bytes in, `SPEC.md` §5.6 result bytes
/// out. A failure becomes a typed `{ok:false, code, error}` result rather than
/// a Rust error, so the caller always gets a well-formed response.
pub fn execute_wire<B: IoBackend + 'static>(wire: &[u8], cat: &Catalog<B>) -> Vec<u8> {
    match run_wire(wire, cat) {
        Ok(bytes) => bytes,
        Err(err) => encode_error_result(err.category(), &err.to_string()),
    }
}

fn run_wire<B: IoBackend + 'static>(wire: &[u8], cat: &Catalog<B>) -> Result<Vec<u8>, QueryError> {
    let request = decode_request(wire, &DecodeLimits::default())?;
    let result = execute_query(&request, cat)?;
    Ok(encode_result(&result)?)
}

fn run_select<B: IoBackend>(
    select: &Select,
    snap: &catalog::CatSnapshot<B>,
) -> Result<QueryResult, QueryError> {
    let physical = plan(&lower(select)?, snap)?;
    // The streaming executor applies keyset pagination and returns a
    // continuation token when more rows remain.
    let page = execute_page(&physical, snap)?;
    Ok(QueryResult {
        columns: page.shape.cols.iter().map(|c| c.name.clone()).collect(),
        rows: page.rows,
        cursor: page.cursor,
        ..QueryResult::default()
    })
}

fn run_explain<B: IoBackend>(
    select: &Select,
    snap: &catalog::CatSnapshot<B>,
) -> Result<QueryResult, QueryError> {
    let physical = plan(&lower(select)?, snap)?;
    let text = render_plan(&physical);
    // Represent the plan as one text row per operator line.
    let rows = text
        .lines()
        .map(|line| vec![Value::Text(line.to_string())])
        .collect();
    Ok(QueryResult {
        columns: vec!["plan".to_string()],
        rows,
        ..QueryResult::default()
    })
}

fn run_transaction<B: IoBackend + 'static>(
    ops: &[Request],
    cat: &Catalog<B>,
) -> Result<QueryResult, QueryError> {
    let mut specs = Vec::with_capacity(ops.len());
    for op in ops {
        match write_spec(op) {
            Some(spec) => specs.push(spec),
            // Validation has already ruled out non-writes in a transaction,
            // but stay defensive.
            None => {
                return Err(QueryError::Exec(crate::ExecError::Unsupported {
                    feature: "non-write op in a transaction",
                }))
            }
        }
    }
    let counts = cat.write_batch(specs).map_err(recover_batch)?;
    Ok(QueryResult {
        applied: None,
        affected: Some(counts.iter().sum()),
        ..QueryResult::default()
    })
}
