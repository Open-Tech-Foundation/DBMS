//! The query AST (`SPEC.md` §5): expressions, pipeline stages, the clause
//! surface, and DML — plus the strict `Doc → Ast` mapping (unknown nodes and
//! fields rejected) and the canonical `Ast → Doc` encoding.
//!
//! Decoding is **grammar enforcement only**: name resolution, type checking,
//! and the §6 safety rules belong to the validator (Phase 9). In particular
//! an update/delete without a selector decodes faithfully (`selector: None`)
//! so the validator can reject it with the right error.

use types::{TypeKind, Value};

use crate::wire::{decode_doc, encode_doc, DecodeLimits, Doc};
use crate::{ProtoError, Result, PROTOCOL_VERSION};

// --- expression nodes --------------------------------------------------------

/// A comparison operator (`eq ne lt lte gt gte`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Lte,
    /// `>`
    Gt,
    /// `>=`
    Gte,
}

impl CmpOp {
    /// The wire node name.
    pub const fn name(self) -> &'static str {
        match self {
            CmpOp::Eq => "eq",
            CmpOp::Ne => "ne",
            CmpOp::Lt => "lt",
            CmpOp::Lte => "lte",
            CmpOp::Gt => "gt",
            CmpOp::Gte => "gte",
        }
    }
}

/// An arithmetic operator (`add sub mul div mod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
}

impl ArithOp {
    /// The wire node name.
    pub const fn name(self) -> &'static str {
        match self {
            ArithOp::Add => "add",
            ArithOp::Sub => "sub",
            ArithOp::Mul => "mul",
            ArithOp::Div => "div",
            ArithOp::Mod => "mod",
        }
    }
}

/// An aggregate function (`count sum min max avg`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    /// `COUNT`
    Count,
    /// `SUM`
    Sum,
    /// `MIN`
    Min,
    /// `MAX`
    Max,
    /// `AVG`
    Avg,
}

impl AggFunc {
    /// The wire node name.
    pub const fn name(self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
            AggFunc::Avg => "avg",
        }
    }
}

/// One expression node (`SPEC.md` §5.2). Strictly data; column references are
/// explicit and any bare scalar is a literal.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// `{col:"name"}` or `{col:["alias","name"]}`.
    Column {
        /// The table alias (or name) for a qualified reference.
        table: Option<String>,
        /// The column name.
        column: String,
    },
    /// A bare scalar literal.
    Literal(Value),
    /// `{eq:[a,b]}` … `{gte:[a,b]}`.
    Cmp {
        /// The operator.
        op: CmpOp,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
    },
    /// `{and:[...]}` (non-empty).
    And(Vec<Expr>),
    /// `{or:[...]}` (non-empty).
    Or(Vec<Expr>),
    /// `{not:x}`.
    Not(Box<Expr>),
    /// `{add:[a,b]}` … `{mod:[a,b]}`.
    Arith {
        /// The operator.
        op: ArithOp,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
    },
    /// `{is_null:x}`.
    IsNull(Box<Expr>),
    /// `{is_not_null:x}`.
    IsNotNull(Box<Expr>),
    /// `{between:[x, lo, hi]}`.
    Between {
        /// The tested expression.
        expr: Box<Expr>,
        /// Lower bound (inclusive).
        lo: Box<Expr>,
        /// Upper bound (inclusive).
        hi: Box<Expr>,
    },
    /// `{in:[x, [v1, v2, ...]]}`.
    InList {
        /// The tested expression.
        expr: Box<Expr>,
        /// The candidate list.
        list: Vec<Expr>,
    },
    /// `{like:[x, "abc%"]}` / `{ilike:[x, "abc%"]}`.
    Like {
        /// The tested expression.
        expr: Box<Expr>,
        /// The pattern (`%` any run, `_` one char).
        pattern: String,
        /// `true` for `ilike`.
        case_insensitive: bool,
    },
    /// `{coalesce:[a,b,...]}` (non-empty).
    Coalesce(Vec<Expr>),
    /// `{nullif:[a,b]}`.
    NullIf {
        /// The tested expression.
        lhs: Box<Expr>,
        /// Null is produced when `lhs == rhs`.
        rhs: Box<Expr>,
    },
    /// `{cast:[x, "i64"]}`.
    Cast {
        /// The casted expression.
        expr: Box<Expr>,
        /// The target type.
        to: TypeKind,
    },
    /// `{sum:x}` `{count:x}` … — valid only where the grammar allows
    /// aggregates (a `group` stage, or a clause `select` list).
    Agg {
        /// The aggregate function.
        func: AggFunc,
        /// The aggregated expression (`{count:1}` counts rows).
        arg: Box<Expr>,
    },
}

// --- pipeline / clause nodes -------------------------------------------------

/// A table reference with an optional alias: `{table:"users", as:"u"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    /// The table name.
    pub table: String,
    /// The alias rows are referenced by, if any.
    pub alias: Option<String>,
}

/// A join type (`SPEC.md` §11: INNER, LEFT, CROSS in v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// `inner`.
    Inner,
    /// `left`.
    Left,
    /// `cross` (no `on` predicate).
    Cross,
}

impl JoinKind {
    /// The wire name.
    pub const fn name(self) -> &'static str {
        match self {
            JoinKind::Inner => "inner",
            JoinKind::Left => "left",
            JoinKind::Cross => "cross",
        }
    }
}

/// One join: `{type, table, on}`.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinSpec {
    /// The join type.
    pub kind: JoinKind,
    /// The joined table.
    pub table: TableRef,
    /// The join predicate; required for inner/left, absent for cross.
    pub on: Option<Expr>,
}

/// A sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// Ascending (the default).
    Asc,
    /// Descending.
    Desc,
}

impl Dir {
    /// The wire name.
    pub const fn name(self) -> &'static str {
        match self {
            Dir::Asc => "asc",
            Dir::Desc => "desc",
        }
    }
}

/// One sort key: `{expr, dir}`.
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    /// The sorted expression.
    pub expr: Expr,
    /// The direction (`asc` when omitted on the wire).
    pub dir: Dir,
}

/// One projection item: a bare expression, or `{as:[name, expr]}`.
#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    /// An unaliased expression (output name resolved by the validator).
    Expr(Expr),
    /// An aliased expression.
    Aliased {
        /// The output column name.
        name: String,
        /// The projected expression.
        expr: Expr,
    },
}

/// One pipeline stage (`SPEC.md` §5.3). Stage order is logical; the planner
/// may reorder safe stages.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    /// `{scan:{table, as}}` — the pipeline head.
    Scan(TableRef),
    /// `{match:<expr>}` — WHERE (or HAVING when placed after `group`).
    Match(Expr),
    /// `{join:{type, table, on}}`.
    Join(JoinSpec),
    /// `{group:{by:[...], agg:{name: <agg>, ...}}}`.
    Group {
        /// Grouping keys (empty = one global group).
        by: Vec<Expr>,
        /// Named aggregate outputs, in wire order.
        aggs: Vec<(String, Expr)>,
    },
    /// `{sort:[{expr, dir}, ...]}` — ORDER BY.
    Sort(Vec<SortKey>),
    /// `{project:[...]}` — the SELECT list.
    Project(Vec<Projection>),
    /// `{distinct:true}`.
    Distinct(bool),
    /// `{limit:n, offset:m}` (either may be omitted).
    Limit {
        /// Maximum rows, if capped.
        limit: Option<u64>,
        /// Rows skipped first.
        offset: u64,
    },
    /// `{cursor:<token>}` — resume keyset pagination.
    Cursor(Vec<u8>),
}

/// The clause surface (`SPEC.md` §5.4) — sugar for a fixed-order pipeline
/// (FROM → WHERE → GROUP → HAVING → PROJECT → ORDER → LIMIT).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClauseSelect {
    /// `from` — the source table.
    pub from: Option<TableRef>,
    /// `joins`.
    pub joins: Vec<JoinSpec>,
    /// `where`.
    pub where_: Option<Expr>,
    /// `group_by`.
    pub group_by: Vec<Expr>,
    /// `having`.
    pub having: Option<Expr>,
    /// `order_by`.
    pub order_by: Vec<SortKey>,
    /// `select` — the output list (`None` = all columns).
    pub select: Option<Vec<Projection>>,
    /// `distinct`.
    pub distinct: bool,
    /// `limit`.
    pub limit: Option<u64>,
    /// `offset`.
    pub offset: Option<u64>,
    /// `cursor` — a keyset continuation token.
    pub cursor: Option<Vec<u8>>,
}

/// A select in either surface form. Both lower to the same IR (in `query`).
#[derive(Debug, Clone, PartialEq)]
pub enum Select {
    /// The pipeline form: an ordered stage array.
    Pipeline(Vec<Stage>),
    /// The clause form: `from/where/group_by/having/order_by/select/limit`.
    Clause(Box<ClauseSelect>),
}

// --- DML ----------------------------------------------------------------------

/// The row selector of an update/delete: a `where` predicate or an explicit
/// `{all:true}`. Its *absence* is represented as `Option<Selector>::None` and
/// rejected by the validator (`SPEC.md` §6 rule 1).
#[derive(Debug, Clone, PartialEq)]
pub enum Selector {
    /// `where:<expr>`.
    Where(Expr),
    /// `all:true` — an explicit full-table write.
    All,
}

/// `{op:"insert", table, rows:[{col: value, ...}, ...]}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    /// The target table.
    pub table: String,
    /// The rows, each as named wire values in wire order.
    pub rows: Vec<Vec<(String, Value)>>,
}

/// `{op:"update", table, where/all, set, unconditional}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// The target table.
    pub table: String,
    /// The row selector; `None` decodes but fails validation (§6 rule 1).
    pub selector: Option<Selector>,
    /// The set list: column → expression (absolute or relative).
    pub set: Vec<(String, Expr)>,
    /// `unconditional:true` — an explicit blind set (free columns only, §6
    /// rule 2).
    pub unconditional: bool,
}

/// `{op:"delete", table, where/all}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    /// The target table.
    pub table: String,
    /// The row selector; `None` decodes but fails validation (§6 rule 1).
    pub selector: Option<Selector>,
}

/// One protocol request.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// `op:"select"` — either surface form.
    Select(Select),
    /// `op:"insert"`.
    Insert(Insert),
    /// `op:"update"`.
    Update(Update),
    /// `op:"delete"`.
    Delete(Delete),
    /// `op:"transaction"` — atomic multi-op (writes only).
    Transaction(Vec<Request>),
    /// `op:"explain"` — plan a select without executing it.
    Explain(Select),
}

// --- decoding ------------------------------------------------------------------

/// Decode one request from wire bytes under `limits`.
pub fn decode_request(bytes: &[u8], limits: &DecodeLimits) -> Result<Request> {
    request_from_doc(&decode_doc(bytes, limits)?)
}

/// Map an already-decoded [`Doc`] onto a [`Request`].
pub fn request_from_doc(doc: &Doc) -> Result<Request> {
    let map = as_map(doc, "request", "request")?;
    if let Some(v) = find(map, "v") {
        match v {
            Doc::Int(PROTOCOL_VERSION) => {}
            Doc::Int(found) => return Err(ProtoError::UnsupportedVersion { found: *found }),
            other => return Err(wrong("request", "v", "int", other)),
        }
    }
    let op = req_str(map, "request", "op")?;
    match op {
        "select" => Ok(Request::Select(select_from(map)?)),
        "insert" => Ok(Request::Insert(insert_from(map)?)),
        "update" => Ok(Request::Update(update_from(map)?)),
        "delete" => Ok(Request::Delete(delete_from(map)?)),
        "transaction" => transaction_from(map),
        "explain" => explain_from(map),
        other => Err(ProtoError::UnknownNode {
            context: "request",
            name: other.to_string(),
        }),
    }
}

type Map = [(String, Doc)];

fn find<'a>(map: &'a Map, key: &str) -> Option<&'a Doc> {
    map.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn as_map<'a>(doc: &'a Doc, node: &'static str, field: &'static str) -> Result<&'a Map> {
    match doc {
        Doc::Map(entries) => Ok(entries),
        other => Err(wrong(node, field, "map", other)),
    }
}

fn as_array<'a>(doc: &'a Doc, node: &'static str, field: &'static str) -> Result<&'a [Doc]> {
    match doc {
        Doc::Array(items) => Ok(items),
        other => Err(wrong(node, field, "array", other)),
    }
}

fn as_str<'a>(doc: &'a Doc, node: &'static str, field: &'static str) -> Result<&'a str> {
    match doc {
        Doc::Str(s) => Ok(s),
        other => Err(wrong(node, field, "string", other)),
    }
}

fn as_bool(doc: &Doc, node: &'static str, field: &'static str) -> Result<bool> {
    match doc {
        Doc::Bool(b) => Ok(*b),
        other => Err(wrong(node, field, "bool", other)),
    }
}

fn as_u64(doc: &Doc, node: &'static str, field: &'static str) -> Result<u64> {
    match doc {
        Doc::Int(n) => u64::try_from(*n).map_err(|_| ProtoError::InvalidValue {
            node,
            field,
            found: n.to_string(),
        }),
        other => Err(wrong(node, field, "non-negative int", other)),
    }
}

fn wrong(
    node: &'static str,
    field: &'static str,
    expected: &'static str,
    found: &Doc,
) -> ProtoError {
    ProtoError::WrongShape {
        node,
        field,
        expected,
        found: found.shape(),
    }
}

fn missing(node: &'static str, field: &'static str) -> ProtoError {
    ProtoError::MissingField { node, field }
}

fn req<'a>(map: &'a Map, node: &'static str, field: &'static str) -> Result<&'a Doc> {
    find(map, field).ok_or_else(|| missing(node, field))
}

fn req_str<'a>(map: &'a Map, node: &'static str, field: &'static str) -> Result<&'a str> {
    as_str(req(map, node, field)?, node, field)
}

/// Reject any key outside `allowed` (unknown-field hardening).
fn only(map: &Map, node: &'static str, allowed: &[&str]) -> Result<()> {
    for (key, _) in map {
        if !allowed.contains(&key.as_str()) {
            return Err(ProtoError::UnknownField {
                node,
                name: key.clone(),
            });
        }
    }
    Ok(())
}

const CLAUSE_KEYS: [&str; 14] = [
    "op", "v", "from", "joins", "where", "group_by", "having", "order_by", "select", "distinct",
    "limit", "offset", "cursor", "pipeline",
];

fn select_from(map: &Map) -> Result<Select> {
    only(map, "select", &CLAUSE_KEYS)?;
    if let Some(stages) = find(map, "pipeline") {
        // Pipeline form: no clause keys may ride along.
        only(map, "select", &["op", "v", "pipeline"])?;
        let items = as_array(stages, "select", "pipeline")?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(stage_from(item)?);
        }
        return Ok(Select::Pipeline(out));
    }
    let mut clause = ClauseSelect {
        from: Some(table_ref_from(req(map, "select", "from")?, "from")?),
        ..ClauseSelect::default()
    };
    if let Some(doc) = find(map, "joins") {
        for item in as_array(doc, "select", "joins")? {
            clause.joins.push(join_spec_from(item)?);
        }
    }
    if let Some(doc) = find(map, "where") {
        clause.where_ = Some(expr_from(doc)?);
    }
    if let Some(doc) = find(map, "group_by") {
        for item in as_array(doc, "select", "group_by")? {
            clause.group_by.push(expr_from(item)?);
        }
    }
    if let Some(doc) = find(map, "having") {
        clause.having = Some(expr_from(doc)?);
    }
    if let Some(doc) = find(map, "order_by") {
        clause.order_by = sort_keys_from(doc, "select", "order_by")?;
    }
    if let Some(doc) = find(map, "select") {
        let items = as_array(doc, "select", "select")?;
        let mut list = Vec::with_capacity(items.len());
        for item in items {
            list.push(projection_from(item)?);
        }
        clause.select = Some(list);
    }
    if let Some(doc) = find(map, "distinct") {
        clause.distinct = as_bool(doc, "select", "distinct")?;
    }
    if let Some(doc) = find(map, "limit") {
        clause.limit = Some(as_u64(doc, "select", "limit")?);
    }
    if let Some(doc) = find(map, "offset") {
        clause.offset = Some(as_u64(doc, "select", "offset")?);
    }
    if let Some(doc) = find(map, "cursor") {
        clause.cursor = Some(token_from(doc, "select", "cursor")?);
    }
    Ok(Select::Clause(Box::new(clause)))
}

fn token_from(doc: &Doc, node: &'static str, field: &'static str) -> Result<Vec<u8>> {
    match doc {
        Doc::Bin(bytes) => Ok(bytes.clone()),
        other => Err(wrong(node, field, "bin", other)),
    }
}

fn table_ref_from(doc: &Doc, field: &'static str) -> Result<TableRef> {
    let map = as_map(doc, "table ref", field)?;
    only(map, "table ref", &["table", "as"])?;
    Ok(TableRef {
        table: req_str(map, "table ref", "table")?.to_string(),
        alias: match find(map, "as") {
            Some(doc) => Some(as_str(doc, "table ref", "as")?.to_string()),
            None => None,
        },
    })
}

fn join_spec_from(doc: &Doc) -> Result<JoinSpec> {
    let map = as_map(doc, "join", "join")?;
    only(map, "join", &["type", "table", "on"])?;
    let kind = match req_str(map, "join", "type")? {
        "inner" => JoinKind::Inner,
        "left" => JoinKind::Left,
        "cross" => JoinKind::Cross,
        other => {
            return Err(ProtoError::InvalidValue {
                node: "join",
                field: "type",
                found: other.to_string(),
            })
        }
    };
    let table = table_ref_from(req(map, "join", "table")?, "table")?;
    let on = match find(map, "on") {
        Some(doc) => Some(expr_from(doc)?),
        None => None,
    };
    // Grammar-level shape: cross joins carry no predicate, the others must.
    match (kind, &on) {
        (JoinKind::Cross, Some(_)) => Err(ProtoError::UnknownField {
            node: "cross join",
            name: "on".to_string(),
        }),
        (JoinKind::Inner | JoinKind::Left, None) => Err(missing("join", "on")),
        _ => Ok(JoinSpec { kind, table, on }),
    }
}

fn sort_keys_from(doc: &Doc, node: &'static str, field: &'static str) -> Result<Vec<SortKey>> {
    let items = as_array(doc, node, field)?;
    let mut keys = Vec::with_capacity(items.len());
    for item in items {
        let map = as_map(item, "sort key", field)?;
        only(map, "sort key", &["expr", "dir"])?;
        let expr = expr_from(req(map, "sort key", "expr")?)?;
        let dir = match find(map, "dir") {
            None => Dir::Asc,
            Some(doc) => match as_str(doc, "sort key", "dir")? {
                "asc" => Dir::Asc,
                "desc" => Dir::Desc,
                other => {
                    return Err(ProtoError::InvalidValue {
                        node: "sort key",
                        field: "dir",
                        found: other.to_string(),
                    })
                }
            },
        };
        keys.push(SortKey { expr, dir });
    }
    Ok(keys)
}

fn projection_from(doc: &Doc) -> Result<Projection> {
    if let Doc::Map(entries) = doc {
        if entries.len() == 1 && entries[0].0 == "as" {
            let parts = as_array(&entries[0].1, "as", "as")?;
            let [name, expr] = parts else {
                return Err(wrong("as", "as", "[name, expr] pair", doc));
            };
            return Ok(Projection::Aliased {
                name: as_str(name, "as", "name")?.to_string(),
                expr: expr_from(expr)?,
            });
        }
    }
    Ok(Projection::Expr(expr_from(doc)?))
}

fn stage_from(doc: &Doc) -> Result<Stage> {
    let map = as_map(doc, "stage", "stage")?;
    // `limit`/`offset` form one stage with up to two keys; every other stage
    // is a single-key map.
    if map.iter().any(|(k, _)| k == "limit" || k == "offset") {
        only(map, "limit stage", &["limit", "offset"])?;
        let limit = match find(map, "limit") {
            Some(doc) => Some(as_u64(doc, "limit stage", "limit")?),
            None => None,
        };
        let offset = match find(map, "offset") {
            Some(doc) => as_u64(doc, "limit stage", "offset")?,
            None => 0,
        };
        return Ok(Stage::Limit { limit, offset });
    }
    let [(name, body)] = map else {
        return Err(wrong("stage", "stage", "single-key map", doc));
    };
    match name.as_str() {
        "scan" => Ok(Stage::Scan(table_ref_from(body, "scan")?)),
        "match" => Ok(Stage::Match(expr_from(body)?)),
        "join" => Ok(Stage::Join(join_spec_from(body)?)),
        "group" => {
            let group = as_map(body, "group", "group")?;
            only(group, "group", &["by", "agg"])?;
            let mut by = Vec::new();
            if let Some(doc) = find(group, "by") {
                for item in as_array(doc, "group", "by")? {
                    by.push(expr_from(item)?);
                }
            }
            let mut aggs = Vec::new();
            if let Some(doc) = find(group, "agg") {
                for (name, value) in as_map(doc, "group", "agg")? {
                    aggs.push((name.clone(), expr_from(value)?));
                }
            }
            Ok(Stage::Group { by, aggs })
        }
        "sort" => Ok(Stage::Sort(sort_keys_from(body, "sort", "sort")?)),
        "project" => {
            let items = as_array(body, "project", "project")?;
            let mut list = Vec::with_capacity(items.len());
            for item in items {
                list.push(projection_from(item)?);
            }
            Ok(Stage::Project(list))
        }
        "distinct" => Ok(Stage::Distinct(as_bool(body, "distinct", "distinct")?)),
        "cursor" => Ok(Stage::Cursor(token_from(body, "cursor", "cursor")?)),
        other => Err(ProtoError::UnknownNode {
            context: "stage",
            name: other.to_string(),
        }),
    }
}

/// Map a `Doc` onto an expression: scalars are literals, a single-key map is
/// an operator node, anything else is outside the grammar.
fn expr_from(doc: &Doc) -> Result<Expr> {
    let entries = match doc {
        Doc::Null => return Ok(Expr::Literal(Value::Null)),
        Doc::Bool(b) => return Ok(Expr::Literal(Value::Bool(*b))),
        Doc::Int(n) => return Ok(Expr::Literal(Value::I64(*n))),
        Doc::Float(f) => return Ok(Expr::Literal(Value::F64(*f))),
        Doc::Str(s) => return Ok(Expr::Literal(Value::Text(s.clone()))),
        Doc::Bin(b) => return Ok(Expr::Literal(Value::Blob(b.clone()))),
        Doc::Array(_) => return Err(wrong("expression", "expression", "scalar or node map", doc)),
        Doc::Map(entries) => entries,
    };
    let [(name, body)] = entries.as_slice() else {
        return Err(wrong("expression", "expression", "single-key map", doc));
    };
    let cmp = |op: CmpOp| -> Result<Expr> {
        let [lhs, rhs] = pair(body, "expression", op.name())?;
        Ok(Expr::Cmp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
    };
    let arith = |op: ArithOp| -> Result<Expr> {
        let [lhs, rhs] = pair(body, "expression", op.name())?;
        Ok(Expr::Arith {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
    };
    let agg = |func: AggFunc| -> Result<Expr> {
        Ok(Expr::Agg {
            func,
            arg: Box::new(expr_from(body)?),
        })
    };
    match name.as_str() {
        "col" => column_from(body),
        "eq" => cmp(CmpOp::Eq),
        "ne" => cmp(CmpOp::Ne),
        "lt" => cmp(CmpOp::Lt),
        "lte" => cmp(CmpOp::Lte),
        "gt" => cmp(CmpOp::Gt),
        "gte" => cmp(CmpOp::Gte),
        "and" => Ok(Expr::And(expr_list(body, "and")?)),
        "or" => Ok(Expr::Or(expr_list(body, "or")?)),
        "not" => Ok(Expr::Not(Box::new(expr_from(body)?))),
        "add" => arith(ArithOp::Add),
        "sub" => arith(ArithOp::Sub),
        "mul" => arith(ArithOp::Mul),
        "div" => arith(ArithOp::Div),
        "mod" => arith(ArithOp::Mod),
        "is_null" => Ok(Expr::IsNull(Box::new(expr_from(body)?))),
        "is_not_null" => Ok(Expr::IsNotNull(Box::new(expr_from(body)?))),
        "between" => {
            let items = as_array(body, "expression", "between")?;
            let [expr, lo, hi] = items else {
                return Err(wrong("expression", "between", "[x, lo, hi] triple", body));
            };
            Ok(Expr::Between {
                expr: Box::new(expr_from(expr)?),
                lo: Box::new(expr_from(lo)?),
                hi: Box::new(expr_from(hi)?),
            })
        }
        "in" => {
            let items = as_array(body, "expression", "in")?;
            let [expr, list] = items else {
                return Err(wrong("expression", "in", "[x, [values]] pair", body));
            };
            Ok(Expr::InList {
                expr: Box::new(expr_from(expr)?),
                list: expr_list(list, "in")?,
            })
        }
        "like" | "ilike" => {
            let items = as_array(body, "expression", "like")?;
            let [expr, pattern] = items else {
                return Err(wrong("expression", "like", "[x, pattern] pair", body));
            };
            Ok(Expr::Like {
                expr: Box::new(expr_from(expr)?),
                pattern: as_str(pattern, "expression", "pattern")?.to_string(),
                case_insensitive: name == "ilike",
            })
        }
        "coalesce" => Ok(Expr::Coalesce(expr_list(body, "coalesce")?)),
        "nullif" => {
            let [lhs, rhs] = pair(body, "expression", "nullif")?;
            Ok(Expr::NullIf {
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        }
        "cast" => {
            let items = as_array(body, "expression", "cast")?;
            let [expr, to] = items else {
                return Err(wrong("expression", "cast", "[x, type] pair", body));
            };
            let type_name = as_str(to, "expression", "cast type")?;
            let to = type_kind_from_name(type_name).ok_or_else(|| ProtoError::InvalidValue {
                node: "expression",
                field: "cast type",
                found: type_name.to_string(),
            })?;
            Ok(Expr::Cast {
                expr: Box::new(expr_from(expr)?),
                to,
            })
        }
        "count" => agg(AggFunc::Count),
        "sum" => agg(AggFunc::Sum),
        "min" => agg(AggFunc::Min),
        "max" => agg(AggFunc::Max),
        "avg" => agg(AggFunc::Avg),
        other => Err(ProtoError::UnknownNode {
            context: "expression",
            name: other.to_string(),
        }),
    }
}

fn column_from(body: &Doc) -> Result<Expr> {
    match body {
        Doc::Str(name) => Ok(Expr::Column {
            table: None,
            column: name.clone(),
        }),
        Doc::Array(items) => {
            let [table, column] = items.as_slice() else {
                return Err(wrong("expression", "col", "[table, column] pair", body));
            };
            Ok(Expr::Column {
                table: Some(as_str(table, "expression", "col table")?.to_string()),
                column: as_str(column, "expression", "col name")?.to_string(),
            })
        }
        other => Err(wrong(
            "expression",
            "col",
            "string or [table, column] pair",
            other,
        )),
    }
}

fn pair(body: &Doc, node: &'static str, field: &'static str) -> Result<[Expr; 2]> {
    let items = as_array(body, node, field)?;
    let [a, b] = items else {
        return Err(wrong(node, field, "[a, b] pair", body));
    };
    Ok([expr_from(a)?, expr_from(b)?])
}

fn expr_list(body: &Doc, field: &'static str) -> Result<Vec<Expr>> {
    let items = as_array(body, "expression", field)?;
    if items.is_empty() {
        return Err(ProtoError::InvalidValue {
            node: "expression",
            field,
            found: "[]".to_string(),
        });
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(expr_from(item)?);
    }
    Ok(out)
}

fn type_kind_from_name(name: &str) -> Option<TypeKind> {
    match name {
        "bool" => Some(TypeKind::Bool),
        "i64" => Some(TypeKind::I64),
        "f64" => Some(TypeKind::F64),
        "text" => Some(TypeKind::Text),
        "blob" => Some(TypeKind::Blob),
        "uuid" => Some(TypeKind::Uuid),
        "json" => Some(TypeKind::Json),
        "timestamp" => Some(TypeKind::Timestamp),
        _ => None,
    }
}

// --- DML decoding ---------------------------------------------------------------

fn insert_from(map: &Map) -> Result<Insert> {
    only(map, "insert", &["op", "v", "table", "rows"])?;
    let table = req_str(map, "insert", "table")?.to_string();
    let rows_doc = as_array(req(map, "insert", "rows")?, "insert", "rows")?;
    let mut rows = Vec::with_capacity(rows_doc.len());
    for row in rows_doc {
        let entries = as_map(row, "insert", "rows")?;
        let mut cols = Vec::with_capacity(entries.len());
        for (name, value) in entries {
            cols.push((name.clone(), row_value_from(value)));
        }
        rows.push(cols);
    }
    Ok(Insert { table, rows })
}

/// An insert-row value: scalars map to their `Value`, and any container is a
/// `json` document stored as its (re-encoded, canonical) MessagePack bytes.
fn row_value_from(doc: &Doc) -> Value {
    match doc {
        Doc::Null => Value::Null,
        Doc::Bool(b) => Value::Bool(*b),
        Doc::Int(n) => Value::I64(*n),
        Doc::Float(f) => Value::F64(*f),
        Doc::Str(s) => Value::Text(s.clone()),
        Doc::Bin(b) => Value::Blob(b.clone()),
        Doc::Array(_) | Doc::Map(_) => Value::Json(encode_doc(doc)),
    }
}

fn selector_from(map: &Map, node: &'static str) -> Result<Option<Selector>> {
    let where_ = find(map, "where");
    let all = find(map, "all");
    match (where_, all) {
        (Some(_), Some(_)) => Err(ProtoError::InvalidValue {
            node,
            field: "all",
            found: "set together with where".to_string(),
        }),
        (Some(doc), None) => Ok(Some(Selector::Where(expr_from(doc)?))),
        (None, Some(doc)) => {
            if as_bool(doc, node, "all")? {
                Ok(Some(Selector::All))
            } else {
                Err(ProtoError::InvalidValue {
                    node,
                    field: "all",
                    found: "false".to_string(),
                })
            }
        }
        (None, None) => Ok(None),
    }
}

fn update_from(map: &Map) -> Result<Update> {
    only(
        map,
        "update",
        &["op", "v", "table", "where", "all", "set", "unconditional"],
    )?;
    let table = req_str(map, "update", "table")?.to_string();
    let selector = selector_from(map, "update")?;
    let set_doc = as_map(req(map, "update", "set")?, "update", "set")?;
    let mut set = Vec::with_capacity(set_doc.len());
    for (name, value) in set_doc {
        set.push((name.clone(), expr_from(value)?));
    }
    let unconditional = match find(map, "unconditional") {
        Some(doc) => as_bool(doc, "update", "unconditional")?,
        None => false,
    };
    Ok(Update {
        table,
        selector,
        set,
        unconditional,
    })
}

fn delete_from(map: &Map) -> Result<Delete> {
    only(map, "delete", &["op", "v", "table", "where", "all"])?;
    Ok(Delete {
        table: req_str(map, "delete", "table")?.to_string(),
        selector: selector_from(map, "delete")?,
    })
}

fn transaction_from(map: &Map) -> Result<Request> {
    only(map, "transaction", &["op", "v", "ops"])?;
    let ops_doc = as_array(req(map, "transaction", "ops")?, "transaction", "ops")?;
    let mut ops = Vec::with_capacity(ops_doc.len());
    for op_doc in ops_doc {
        let op_map = as_map(op_doc, "transaction", "ops")?;
        // Only writes compose into a transaction (SPEC §5.5); also no `v`
        // field on inner ops — the envelope already carries it.
        match req_str(op_map, "transaction op", "op")? {
            "insert" => ops.push(Request::Insert(insert_from(op_map)?)),
            "update" => ops.push(Request::Update(update_from(op_map)?)),
            "delete" => ops.push(Request::Delete(delete_from(op_map)?)),
            other => {
                return Err(ProtoError::UnknownNode {
                    context: "transaction op",
                    name: other.to_string(),
                })
            }
        }
    }
    Ok(Request::Transaction(ops))
}

fn explain_from(map: &Map) -> Result<Request> {
    only(map, "explain", &["op", "v", "query"])?;
    let query = as_map(req(map, "explain", "query")?, "explain", "query")?;
    match req_str(query, "explain query", "op")? {
        "select" => Ok(Request::Explain(select_from(query)?)),
        other => Err(ProtoError::UnknownNode {
            context: "explain query",
            name: other.to_string(),
        }),
    }
}

// --- encoding --------------------------------------------------------------------

/// Encode a request in its canonical wire form (the inverse of
/// [`decode_request`]: decode ∘ encode is the identity on ASTs).
pub fn encode_request(request: &Request) -> Vec<u8> {
    encode_doc(&request_to_doc(request))
}

fn request_to_doc(request: &Request) -> Doc {
    let mut entries = vec![("v".to_string(), Doc::Int(PROTOCOL_VERSION))];
    match request {
        Request::Select(select) => {
            entries.push(("op".to_string(), Doc::Str("select".to_string())));
            entries.extend(select_entries(select));
        }
        Request::Insert(insert) => {
            entries.push(("op".to_string(), Doc::Str("insert".to_string())));
            entries.extend(insert_entries(insert));
        }
        Request::Update(update) => {
            entries.push(("op".to_string(), Doc::Str("update".to_string())));
            entries.extend(update_entries(update));
        }
        Request::Delete(delete) => {
            entries.push(("op".to_string(), Doc::Str("delete".to_string())));
            entries.extend(delete_entries(delete));
        }
        Request::Transaction(ops) => {
            entries.push(("op".to_string(), Doc::Str("transaction".to_string())));
            let inner: Vec<Doc> = ops
                .iter()
                .map(|op| {
                    // Inner ops carry no `v`; strip the envelope field.
                    match request_to_doc(op) {
                        Doc::Map(entries) => {
                            Doc::Map(entries.into_iter().filter(|(k, _)| k != "v").collect())
                        }
                        // request_to_doc always returns a map; preserved
                        // as-is rather than panicking under any refactor.
                        other => other,
                    }
                })
                .collect();
            entries.push(("ops".to_string(), Doc::Array(inner)));
        }
        Request::Explain(select) => {
            entries.push(("op".to_string(), Doc::Str("explain".to_string())));
            let mut query = vec![("op".to_string(), Doc::Str("select".to_string()))];
            query.extend(select_entries(select));
            entries.push(("query".to_string(), Doc::Map(query)));
        }
    }
    Doc::Map(entries)
}

fn select_entries(select: &Select) -> Vec<(String, Doc)> {
    match select {
        Select::Pipeline(stages) => vec![(
            "pipeline".to_string(),
            Doc::Array(stages.iter().map(stage_to_doc).collect()),
        )],
        Select::Clause(clause) => {
            let mut entries = Vec::new();
            if let Some(from) = &clause.from {
                entries.push(("from".to_string(), table_ref_to_doc(from)));
            }
            if !clause.joins.is_empty() {
                entries.push((
                    "joins".to_string(),
                    Doc::Array(clause.joins.iter().map(join_spec_to_doc).collect()),
                ));
            }
            if let Some(where_) = &clause.where_ {
                entries.push(("where".to_string(), expr_to_doc(where_)));
            }
            if !clause.group_by.is_empty() {
                entries.push((
                    "group_by".to_string(),
                    Doc::Array(clause.group_by.iter().map(expr_to_doc).collect()),
                ));
            }
            if let Some(having) = &clause.having {
                entries.push(("having".to_string(), expr_to_doc(having)));
            }
            if !clause.order_by.is_empty() {
                entries.push((
                    "order_by".to_string(),
                    Doc::Array(clause.order_by.iter().map(sort_key_to_doc).collect()),
                ));
            }
            if let Some(select) = &clause.select {
                entries.push((
                    "select".to_string(),
                    Doc::Array(select.iter().map(projection_to_doc).collect()),
                ));
            }
            if clause.distinct {
                entries.push(("distinct".to_string(), Doc::Bool(true)));
            }
            if let Some(limit) = clause.limit {
                entries.push(("limit".to_string(), Doc::Int(int_from_u64(limit))));
            }
            if let Some(offset) = clause.offset {
                entries.push(("offset".to_string(), Doc::Int(int_from_u64(offset))));
            }
            if let Some(cursor) = &clause.cursor {
                entries.push(("cursor".to_string(), Doc::Bin(cursor.clone())));
            }
            entries
        }
    }
}

/// Wire ints are i64; protocol counts saturate rather than wrap.
fn int_from_u64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn insert_entries(insert: &Insert) -> Vec<(String, Doc)> {
    let rows: Vec<Doc> = insert
        .rows
        .iter()
        .map(|row| {
            Doc::Map(
                row.iter()
                    .map(|(name, value)| (name.clone(), row_value_to_doc(value)))
                    .collect(),
            )
        })
        .collect();
    vec![
        ("table".to_string(), Doc::Str(insert.table.clone())),
        ("rows".to_string(), Doc::Array(rows)),
    ]
}

/// The wire form of an insert-row value. A `json` value is re-decoded into a
/// `Doc` so the canonical encoding nests it structurally (the bytes were
/// validated at construction; corrupt bytes degrade to a bin, never a panic).
fn row_value_to_doc(value: &Value) -> Doc {
    match value {
        Value::Null => Doc::Null,
        Value::Bool(b) => Doc::Bool(*b),
        Value::I64(n) => Doc::Int(*n),
        Value::F64(f) => Doc::Float(*f),
        Value::Text(s) => Doc::Str(s.clone()),
        Value::Blob(b) => Doc::Bin(b.clone()),
        Value::Json(bytes) => {
            decode_doc(bytes, &DecodeLimits::default()).unwrap_or(Doc::Bin(bytes.clone()))
        }
        Value::Uuid(u) => Doc::Str(types::uuid_to_string(u)),
        Value::Timestamp(t) => Doc::Int(*t),
    }
}

fn selector_entries(selector: &Option<Selector>) -> Vec<(String, Doc)> {
    match selector {
        Some(Selector::Where(expr)) => vec![("where".to_string(), expr_to_doc(expr))],
        Some(Selector::All) => vec![("all".to_string(), Doc::Bool(true))],
        None => Vec::new(),
    }
}

fn update_entries(update: &Update) -> Vec<(String, Doc)> {
    let mut entries = vec![("table".to_string(), Doc::Str(update.table.clone()))];
    entries.extend(selector_entries(&update.selector));
    entries.push((
        "set".to_string(),
        Doc::Map(
            update
                .set
                .iter()
                .map(|(name, expr)| (name.clone(), expr_to_doc(expr)))
                .collect(),
        ),
    ));
    if update.unconditional {
        entries.push(("unconditional".to_string(), Doc::Bool(true)));
    }
    entries
}

fn delete_entries(delete: &Delete) -> Vec<(String, Doc)> {
    let mut entries = vec![("table".to_string(), Doc::Str(delete.table.clone()))];
    entries.extend(selector_entries(&delete.selector));
    entries
}

fn table_ref_to_doc(table: &TableRef) -> Doc {
    let mut entries = vec![("table".to_string(), Doc::Str(table.table.clone()))];
    if let Some(alias) = &table.alias {
        entries.push(("as".to_string(), Doc::Str(alias.clone())));
    }
    Doc::Map(entries)
}

fn join_spec_to_doc(join: &JoinSpec) -> Doc {
    let mut entries = vec![
        ("type".to_string(), Doc::Str(join.kind.name().to_string())),
        ("table".to_string(), table_ref_to_doc(&join.table)),
    ];
    if let Some(on) = &join.on {
        entries.push(("on".to_string(), expr_to_doc(on)));
    }
    Doc::Map(entries)
}

fn sort_key_to_doc(key: &SortKey) -> Doc {
    Doc::Map(vec![
        ("expr".to_string(), expr_to_doc(&key.expr)),
        ("dir".to_string(), Doc::Str(key.dir.name().to_string())),
    ])
}

fn projection_to_doc(item: &Projection) -> Doc {
    match item {
        Projection::Expr(expr) => expr_to_doc(expr),
        Projection::Aliased { name, expr } => Doc::Map(vec![(
            "as".to_string(),
            Doc::Array(vec![Doc::Str(name.clone()), expr_to_doc(expr)]),
        )]),
    }
}

fn stage_to_doc(stage: &Stage) -> Doc {
    let single = |name: &str, body: Doc| Doc::Map(vec![(name.to_string(), body)]);
    match stage {
        Stage::Scan(table) => single("scan", table_ref_to_doc(table)),
        Stage::Match(expr) => single("match", expr_to_doc(expr)),
        Stage::Join(join) => single("join", join_spec_to_doc(join)),
        Stage::Group { by, aggs } => single(
            "group",
            Doc::Map(vec![
                (
                    "by".to_string(),
                    Doc::Array(by.iter().map(expr_to_doc).collect()),
                ),
                (
                    "agg".to_string(),
                    Doc::Map(
                        aggs.iter()
                            .map(|(name, expr)| (name.clone(), expr_to_doc(expr)))
                            .collect(),
                    ),
                ),
            ]),
        ),
        Stage::Sort(keys) => single(
            "sort",
            Doc::Array(keys.iter().map(sort_key_to_doc).collect()),
        ),
        Stage::Project(items) => single(
            "project",
            Doc::Array(items.iter().map(projection_to_doc).collect()),
        ),
        Stage::Distinct(on) => single("distinct", Doc::Bool(*on)),
        Stage::Limit { limit, offset } => {
            let mut entries = Vec::new();
            if let Some(limit) = limit {
                entries.push(("limit".to_string(), Doc::Int(int_from_u64(*limit))));
            }
            entries.push(("offset".to_string(), Doc::Int(int_from_u64(*offset))));
            Doc::Map(entries)
        }
        Stage::Cursor(token) => single("cursor", Doc::Bin(token.clone())),
    }
}

fn expr_to_doc(expr: &Expr) -> Doc {
    let single = |name: &str, body: Doc| Doc::Map(vec![(name.to_string(), body)]);
    let pair = |name: &str, a: &Expr, b: &Expr| {
        single(name, Doc::Array(vec![expr_to_doc(a), expr_to_doc(b)]))
    };
    match expr {
        Expr::Column { table, column } => single(
            "col",
            match table {
                Some(table) => Doc::Array(vec![Doc::Str(table.clone()), Doc::Str(column.clone())]),
                None => Doc::Str(column.clone()),
            },
        ),
        Expr::Literal(value) => row_value_to_doc(value),
        Expr::Cmp { op, lhs, rhs } => pair(op.name(), lhs, rhs),
        Expr::And(items) => single("and", Doc::Array(items.iter().map(expr_to_doc).collect())),
        Expr::Or(items) => single("or", Doc::Array(items.iter().map(expr_to_doc).collect())),
        Expr::Not(inner) => single("not", expr_to_doc(inner)),
        Expr::Arith { op, lhs, rhs } => pair(op.name(), lhs, rhs),
        Expr::IsNull(inner) => single("is_null", expr_to_doc(inner)),
        Expr::IsNotNull(inner) => single("is_not_null", expr_to_doc(inner)),
        Expr::Between { expr, lo, hi } => single(
            "between",
            Doc::Array(vec![expr_to_doc(expr), expr_to_doc(lo), expr_to_doc(hi)]),
        ),
        Expr::InList { expr, list } => single(
            "in",
            Doc::Array(vec![
                expr_to_doc(expr),
                Doc::Array(list.iter().map(expr_to_doc).collect()),
            ]),
        ),
        Expr::Like {
            expr,
            pattern,
            case_insensitive,
        } => single(
            if *case_insensitive { "ilike" } else { "like" },
            Doc::Array(vec![expr_to_doc(expr), Doc::Str(pattern.clone())]),
        ),
        Expr::Coalesce(items) => single(
            "coalesce",
            Doc::Array(items.iter().map(expr_to_doc).collect()),
        ),
        Expr::NullIf { lhs, rhs } => pair("nullif", lhs, rhs),
        Expr::Cast { expr, to } => single(
            "cast",
            Doc::Array(vec![expr_to_doc(expr), Doc::Str(to.as_str().to_string())]),
        ),
        Expr::Agg { func, arg } => single(func.name(), expr_to_doc(arg)),
    }
}
