//! Scalar expression evaluation (`SPEC.md` §5.2, §3, §8).
//!
//! A pure function from an [`Expr`] and a bound row to a [`Value`], shared by
//! the reference executor, the pull-based executor, and the write path. It is
//! the runtime companion to the validator: the validator proves an expression
//! is well-typed and resolvable, so evaluation can trust that shape and only
//! reports the failures that are inherently *data*-dependent — arithmetic
//! overflow and division by zero (checked, never wrapping or panicking —
//! `SPEC.md` §8) and unrepresentable casts.
//!
//! Semantics follow SQL **three-valued logic**: a `null` operand propagates to
//! `null` (not `false`), and only a definitively-`true` predicate keeps a row.
//! Comparisons use the engine's logical order, except that the two numeric
//! kinds compare by value (`5 > 1.5`), not by variant rank.

use std::cmp::Ordering;

use common::{CategorizedError, ErrorCategory};
use proto::{ArithOp, CmpOp, Expr};
use types::{TypeKind, Value};

/// How expression evaluation can fail on otherwise-valid input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EvalError {
    /// Checked integer arithmetic overflowed (`SPEC.md` §8).
    #[error("integer overflow in {op}")]
    Overflow {
        /// The operator that overflowed.
        op: &'static str,
    },
    /// Integer division or modulo by zero.
    #[error("division by zero")]
    DivByZero,
    /// A cast whose source value cannot be represented in the target kind.
    #[error("cannot cast {from} to {to}")]
    BadCast {
        /// The source value's rendered kind.
        from: String,
        /// The target kind.
        to: TypeKind,
    },
    /// A numeric cast whose value is out of the target's range (or NaN/inf to
    /// an integer).
    #[error("value out of range casting to {to}")]
    CastRange {
        /// The target kind.
        to: TypeKind,
    },
    /// A column reference the row does not bind. A validated plan never hits
    /// this; it guards the invariant for direct callers.
    #[error("unresolved column {column:?}")]
    UnresolvedColumn {
        /// The unbound column name.
        column: String,
    },
    /// An operand's runtime kind is incompatible with the operator. A
    /// validated plan never hits this.
    #[error("type error evaluating {op}")]
    TypeError {
        /// The operator being evaluated.
        op: &'static str,
    },
    /// An aggregate expression reached the scalar evaluator. Aggregates are
    /// computed by the aggregate operator, never here.
    #[error("aggregate expression cannot be evaluated as a scalar")]
    UnexpectedAggregate,
}

impl CategorizedError for EvalError {
    fn category(&self) -> ErrorCategory {
        // All are runtime failures of an otherwise-valid query on specific
        // data (`DECISIONS.md` D25): the query is invalid *for this row*.
        ErrorCategory::Validation
    }
}

type Result<T> = std::result::Result<T, EvalError>;

// --- row binding -------------------------------------------------------------

/// One bound column: its optional qualifier (table alias or name) and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundColumn {
    /// The qualifier a reference may carry.
    pub qualifier: Option<String>,
    /// The column name.
    pub name: String,
}

/// The ordered columns of the row an expression evaluates against, used to
/// resolve column references to positions. Built by the executor as it walks
/// the plan (mirroring the validator's row type, `DECISIONS.md` D24).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shape {
    /// The columns, in row order.
    pub cols: Vec<BoundColumn>,
}

impl Shape {
    /// An empty shape.
    pub fn new() -> Shape {
        Shape { cols: Vec::new() }
    }

    /// Append a column.
    pub fn push(&mut self, qualifier: Option<String>, name: impl Into<String>) {
        self.cols.push(BoundColumn {
            qualifier,
            name: name.into(),
        });
    }

    /// Concatenate two shapes (the two sides of a join).
    pub fn concat(mut self, mut other: Shape) -> Shape {
        self.cols.append(&mut other.cols);
        self
    }

    /// The position of the column a reference names, matching the validator's
    /// resolution: an unqualified name matches on name alone (the validator
    /// has already ruled out ambiguity), a qualified one matches both.
    pub fn resolve(&self, qualifier: Option<&str>, column: &str) -> Option<usize> {
        self.cols.iter().position(|c| {
            c.name == column
                && match qualifier {
                    Some(q) => c.qualifier.as_deref() == Some(q),
                    None => true,
                }
        })
    }
}

// --- evaluation --------------------------------------------------------------

/// Evaluate `expr` against `row` (whose columns are described by `shape`).
pub fn eval(expr: &Expr, row: &[Value], shape: &Shape) -> Result<Value> {
    match expr {
        Expr::Column { table, column } => {
            let idx = shape.resolve(table.as_deref(), column).ok_or_else(|| {
                EvalError::UnresolvedColumn {
                    column: column.clone(),
                }
            })?;
            Ok(row.get(idx).cloned().unwrap_or(Value::Null))
        }
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Cmp { op, lhs, rhs } => {
            let a = eval(lhs, row, shape)?;
            let b = eval(rhs, row, shape)?;
            Ok(eval_cmp(*op, &a, &b))
        }
        Expr::And(items) => eval_and(items, row, shape),
        Expr::Or(items) => eval_or(items, row, shape),
        Expr::Not(inner) => match eval(inner, row, shape)? {
            Value::Null => Ok(Value::Null),
            Value::Bool(b) => Ok(Value::Bool(!b)),
            _ => Err(EvalError::TypeError { op: "not" }),
        },
        Expr::Arith { op, lhs, rhs } => {
            let a = eval(lhs, row, shape)?;
            let b = eval(rhs, row, shape)?;
            eval_arith(*op, &a, &b)
        }
        Expr::IsNull(inner) => Ok(Value::Bool(matches!(eval(inner, row, shape)?, Value::Null))),
        Expr::IsNotNull(inner) => Ok(Value::Bool(!matches!(
            eval(inner, row, shape)?,
            Value::Null
        ))),
        Expr::Between { expr, lo, hi } => {
            let v = eval(expr, row, shape)?;
            let lo = eval(lo, row, shape)?;
            let hi = eval(hi, row, shape)?;
            // v BETWEEN lo AND hi ≡ v >= lo AND v <= hi (3VL).
            let ge = eval_cmp(CmpOp::Gte, &v, &lo);
            let le = eval_cmp(CmpOp::Lte, &v, &hi);
            Ok(and3(ge, le))
        }
        Expr::InList { expr, list } => {
            let v = eval(expr, row, shape)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            let mut saw_null = false;
            for item in list {
                match eval_cmp(CmpOp::Eq, &v, &eval(item, row, shape)?) {
                    Value::Bool(true) => return Ok(Value::Bool(true)),
                    Value::Null => saw_null = true,
                    _ => {}
                }
            }
            Ok(if saw_null {
                Value::Null
            } else {
                Value::Bool(false)
            })
        }
        Expr::Like {
            expr,
            pattern,
            case_insensitive,
        } => match eval(expr, row, shape)? {
            Value::Null => Ok(Value::Null),
            Value::Text(s) => Ok(Value::Bool(like_match(&s, pattern, *case_insensitive))),
            _ => Err(EvalError::TypeError { op: "like" }),
        },
        Expr::Coalesce(items) => {
            for item in items {
                let v = eval(item, row, shape)?;
                if !matches!(v, Value::Null) {
                    return Ok(v);
                }
            }
            Ok(Value::Null)
        }
        Expr::NullIf { lhs, rhs } => {
            let a = eval(lhs, row, shape)?;
            let b = eval(rhs, row, shape)?;
            // NULLIF(a, b) = NULL when a = b, else a.
            match eval_cmp(CmpOp::Eq, &a, &b) {
                Value::Bool(true) => Ok(Value::Null),
                _ => Ok(a),
            }
        }
        Expr::Cast { expr, to } => {
            let v = eval(expr, row, shape)?;
            eval_cast(&v, *to)
        }
        Expr::Agg { .. } => Err(EvalError::UnexpectedAggregate),
    }
}

/// Evaluate a predicate to a definite keep/drop decision: only a `true`
/// predicate keeps the row (`null`/`false` drop it, `SPEC.md` §3).
pub fn eval_predicate(expr: &Expr, row: &[Value], shape: &Shape) -> Result<bool> {
    Ok(matches!(eval(expr, row, shape)?, Value::Bool(true)))
}

fn eval_and(items: &[Expr], row: &[Value], shape: &Shape) -> Result<Value> {
    let mut acc = Value::Bool(true);
    for item in items {
        acc = and3(acc, eval(item, row, shape)?);
    }
    Ok(acc)
}

fn eval_or(items: &[Expr], row: &[Value], shape: &Shape) -> Result<Value> {
    let mut acc = Value::Bool(false);
    for item in items {
        acc = or3(acc, eval(item, row, shape)?);
    }
    Ok(acc)
}

/// Three-valued AND: `false` dominates, then `null`.
fn and3(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Bool(false), _) | (_, Value::Bool(false)) => Value::Bool(false),
        (Value::Bool(true), Value::Bool(true)) => Value::Bool(true),
        _ => Value::Null,
    }
}

/// Three-valued OR: `true` dominates, then `null`.
fn or3(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Bool(true), _) | (_, Value::Bool(true)) => Value::Bool(true),
        (Value::Bool(false), Value::Bool(false)) => Value::Bool(false),
        _ => Value::Null,
    }
}

/// A comparison under 3VL: a `null` operand yields `null`, otherwise the
/// operator's truth over the value ordering.
fn eval_cmp(op: CmpOp, a: &Value, b: &Value) -> Value {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Value::Null;
    }
    let ord = value_order(a, b);
    Value::Bool(match op {
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Ne => ord != Ordering::Equal,
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Lte => ord != Ordering::Greater,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Gte => ord != Ordering::Less,
    })
}

/// The ordering of two values for comparison and sorting: the two numeric
/// kinds compare by value (coerced through `f64`), everything else by the
/// engine's logical order (which places `null` first). Shared with the
/// executor's sort/min/max so ordering is defined in one place.
pub(crate) fn value_order(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::I64(x), Value::F64(y)) => f64_order(*x as f64, *y),
        (Value::F64(x), Value::I64(y)) => f64_order(*x, *y as f64),
        _ => a.logical_cmp(b),
    }
}

/// Order two `f64`s via the value model's float total order (NaN is greatest
/// and equals itself, `-0.0 < +0.0`) — consistent with `Value`'s ordering.
fn f64_order(x: f64, y: f64) -> Ordering {
    Value::F64(x).logical_cmp(&Value::F64(y))
}

fn eval_arith(op: ArithOp, a: &Value, b: &Value) -> Result<Value> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    match (a, b) {
        (Value::I64(x), Value::I64(y)) => int_arith(op, *x, *y),
        (Value::F64(x), Value::F64(y)) => Ok(Value::F64(float_arith(op, *x, *y))),
        (Value::I64(x), Value::F64(y)) => Ok(Value::F64(float_arith(op, *x as f64, *y))),
        (Value::F64(x), Value::I64(y)) => Ok(Value::F64(float_arith(op, *x, *y as f64))),
        _ => Err(EvalError::TypeError { op: op.name() }),
    }
}

/// Checked `i64` arithmetic: overflow and division/modulo by zero are typed
/// errors, never wraparound or panic (`SPEC.md` §8).
fn int_arith(op: ArithOp, x: i64, y: i64) -> Result<Value> {
    let out = match op {
        ArithOp::Add => x.checked_add(y),
        ArithOp::Sub => x.checked_sub(y),
        ArithOp::Mul => x.checked_mul(y),
        ArithOp::Div => {
            if y == 0 {
                return Err(EvalError::DivByZero);
            }
            x.checked_div(y)
        }
        ArithOp::Mod => {
            if y == 0 {
                return Err(EvalError::DivByZero);
            }
            x.checked_rem(y)
        }
    };
    out.map(Value::I64)
        .ok_or(EvalError::Overflow { op: op.name() })
}

/// `f64` arithmetic follows IEEE-754 (division by zero yields ±inf, not an
/// error); overflow saturates to inf rather than panicking.
fn float_arith(op: ArithOp, x: f64, y: f64) -> f64 {
    match op {
        ArithOp::Add => x + y,
        ArithOp::Sub => x - y,
        ArithOp::Mul => x * y,
        ArithOp::Div => x / y,
        ArithOp::Mod => x % y,
    }
}

/// SQL `LIKE`: `%` matches any run (including empty), `_` matches exactly one
/// character; matching is over Unicode scalar values. `case_insensitive`
/// lowercases both sides first (the `%`/`_` metacharacters are unaffected).
fn like_match(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    let (text, pattern) = if case_insensitive {
        (text.to_lowercase(), pattern.to_lowercase())
    } else {
        (text.to_string(), pattern.to_string())
    };
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();

    // Iterative wildcard match with backtracking on the last `%`.
    let (mut i, mut j) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut resume = 0usize;
    while i < t.len() {
        if j < p.len() && (p[j] == '_' || p[j] == t[i]) {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '%' {
            star = Some(j);
            resume = i;
            j += 1;
        } else if let Some(s) = star {
            j = s + 1;
            resume += 1;
            i = resume;
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '%' {
        j += 1;
    }
    j == p.len()
}

/// Cast a value to `to`. Supports the conversions v1 needs; anything else is a
/// typed `BadCast`. A `null` casts to `null` of any kind.
fn eval_cast(v: &Value, to: TypeKind) -> Result<Value> {
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    // Identity cast.
    if v.kind() == Some(to) {
        return Ok(v.clone());
    }
    match (to, v) {
        // Numeric conversions.
        (TypeKind::F64, Value::I64(n)) => Ok(Value::F64(*n as f64)),
        (TypeKind::I64, Value::F64(f)) => {
            if !f.is_finite() || *f < i64::MIN as f64 || *f >= 9_223_372_036_854_775_808.0 {
                return Err(EvalError::CastRange { to });
            }
            Ok(Value::I64(f.trunc() as i64))
        }
        (TypeKind::I64, Value::Bool(b)) => Ok(Value::I64(i64::from(*b))),
        (TypeKind::I64, Value::Timestamp(t)) => Ok(Value::I64(*t)),
        (TypeKind::Timestamp, Value::I64(n)) => Ok(Value::Timestamp(*n)),
        // Parsing from text.
        (TypeKind::I64, Value::Text(s)) => s
            .trim()
            .parse::<i64>()
            .map(Value::I64)
            .map_err(|_| EvalError::CastRange { to }),
        (TypeKind::F64, Value::Text(s)) => s
            .trim()
            .parse::<f64>()
            .map(Value::F64)
            .map_err(|_| EvalError::CastRange { to }),
        (TypeKind::Bool, Value::Text(s)) => match s.trim() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(EvalError::CastRange { to }),
        },
        // Rendering to text.
        (TypeKind::Text, other) => render_text(other).map(Value::Text),
        _ => Err(EvalError::BadCast {
            from: render_kind(v),
            to,
        }),
    }
}

fn render_text(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Bool(b) => b.to_string(),
        Value::I64(n) | Value::Timestamp(n) => n.to_string(),
        Value::F64(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Uuid(u) => types::uuid_to_string(u),
        // Blob/Json to text is ambiguous and unsupported.
        Value::Blob(_) | Value::Json(_) | Value::Null => {
            return Err(EvalError::BadCast {
                from: render_kind(v),
                to: TypeKind::Text,
            })
        }
    })
}

fn render_kind(v: &Value) -> String {
    match v.kind() {
        Some(k) => k.to_string(),
        None => "null".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(cols: &[(&str, &str)]) -> Shape {
        let mut s = Shape::new();
        for (q, n) in cols {
            let qualifier = if q.is_empty() {
                None
            } else {
                Some((*q).to_string())
            };
            s.push(qualifier, *n);
        }
        s
    }

    fn col(name: &str) -> Expr {
        Expr::Column {
            table: None,
            column: name.to_string(),
        }
    }

    fn lit(n: i64) -> Expr {
        Expr::Literal(Value::I64(n))
    }

    fn cmp(op: CmpOp, l: Expr, r: Expr) -> Expr {
        Expr::Cmp {
            op,
            lhs: Box::new(l),
            rhs: Box::new(r),
        }
    }

    fn arith(op: ArithOp, l: Expr, r: Expr) -> Expr {
        Expr::Arith {
            op,
            lhs: Box::new(l),
            rhs: Box::new(r),
        }
    }

    fn evl(e: &Expr) -> Value {
        eval(e, &[], &Shape::new()).unwrap()
    }

    #[test]
    fn resolves_columns_qualified_and_unqualified() {
        let s = shape(&[("u", "id"), ("o", "amount")]);
        let row = [Value::I64(7), Value::I64(42)];
        assert_eq!(eval(&col("amount"), &row, &s).unwrap(), Value::I64(42));
        let q = Expr::Column {
            table: Some("u".into()),
            column: "id".into(),
        };
        assert_eq!(eval(&q, &row, &s).unwrap(), Value::I64(7));
    }

    #[test]
    fn null_propagates_through_comparison() {
        assert_eq!(
            evl(&cmp(CmpOp::Eq, Expr::Literal(Value::Null), lit(1))),
            Value::Null
        );
    }

    #[test]
    fn mixed_numeric_comparison_by_value() {
        // 5 (i64) > 1.5 (f64) — by value, not by variant rank.
        assert_eq!(
            evl(&cmp(CmpOp::Gt, lit(5), Expr::Literal(Value::F64(1.5)))),
            Value::Bool(true)
        );
    }

    #[test]
    fn three_valued_and_or() {
        let t = Expr::Literal(Value::Bool(true));
        let f = Expr::Literal(Value::Bool(false));
        let n = Expr::Literal(Value::Null);
        assert_eq!(evl(&Expr::And(vec![t.clone(), n.clone()])), Value::Null);
        assert_eq!(
            evl(&Expr::And(vec![f.clone(), n.clone()])),
            Value::Bool(false)
        );
        assert_eq!(
            evl(&Expr::Or(vec![t.clone(), n.clone()])),
            Value::Bool(true)
        );
        assert_eq!(evl(&Expr::Or(vec![f, n])), Value::Null);
    }

    #[test]
    fn checked_integer_overflow_is_an_error() {
        let e = arith(ArithOp::Add, Expr::Literal(Value::I64(i64::MAX)), lit(1));
        assert_eq!(
            eval(&e, &[], &Shape::new()).unwrap_err(),
            EvalError::Overflow { op: "add" }
        );
    }

    #[test]
    fn division_by_zero_is_an_error() {
        let e = arith(ArithOp::Div, lit(1), lit(0));
        assert_eq!(
            eval(&e, &[], &Shape::new()).unwrap_err(),
            EvalError::DivByZero
        );
    }

    #[test]
    fn relative_arithmetic_over_a_row() {
        // balance - 50 with balance = 100.
        let s = shape(&[("", "balance")]);
        let row = [Value::I64(100)];
        let e = arith(ArithOp::Sub, col("balance"), lit(50));
        assert_eq!(eval(&e, &row, &s).unwrap(), Value::I64(50));
    }

    #[test]
    fn float_division_by_zero_is_inf_not_error() {
        let e = arith(
            ArithOp::Div,
            Expr::Literal(Value::F64(1.0)),
            Expr::Literal(Value::F64(0.0)),
        );
        assert_eq!(evl(&e), Value::F64(f64::INFINITY));
    }

    #[test]
    fn between_and_in_semantics() {
        let b = Expr::Between {
            expr: Box::new(lit(5)),
            lo: Box::new(lit(1)),
            hi: Box::new(lit(10)),
        };
        assert_eq!(evl(&b), Value::Bool(true));

        let in_list = Expr::InList {
            expr: Box::new(lit(3)),
            list: vec![lit(1), lit(2), lit(3)],
        };
        assert_eq!(evl(&in_list), Value::Bool(true));

        // Not present, with a null in the list → unknown.
        let in_null = Expr::InList {
            expr: Box::new(lit(9)),
            list: vec![lit(1), Expr::Literal(Value::Null)],
        };
        assert_eq!(evl(&in_null), Value::Null);
    }

    #[test]
    fn like_wildcards() {
        assert!(like_match("hello", "h%o", false));
        assert!(like_match("hello", "he__o", false));
        assert!(!like_match("hello", "h_o", false));
        assert!(like_match("HELLO", "hello", true));
        assert!(like_match("abc", "%", false));
        assert!(like_match("", "%", false));
        assert!(!like_match("abc", "", false));
    }

    #[test]
    fn coalesce_and_nullif() {
        let c = Expr::Coalesce(vec![Expr::Literal(Value::Null), lit(7), lit(8)]);
        assert_eq!(evl(&c), Value::I64(7));

        let n_eq = Expr::NullIf {
            lhs: Box::new(lit(5)),
            rhs: Box::new(lit(5)),
        };
        assert_eq!(evl(&n_eq), Value::Null);
        let n_ne = Expr::NullIf {
            lhs: Box::new(lit(5)),
            rhs: Box::new(lit(6)),
        };
        assert_eq!(evl(&n_ne), Value::I64(5));
    }

    #[test]
    fn casts() {
        let to_f = Expr::Cast {
            expr: Box::new(lit(3)),
            to: TypeKind::F64,
        };
        assert_eq!(evl(&to_f), Value::F64(3.0));

        let to_i = Expr::Cast {
            expr: Box::new(Expr::Literal(Value::F64(3.9))),
            to: TypeKind::I64,
        };
        assert_eq!(evl(&to_i), Value::I64(3)); // truncates toward zero

        let to_text = Expr::Cast {
            expr: Box::new(lit(42)),
            to: TypeKind::Text,
        };
        assert_eq!(evl(&to_text), Value::Text("42".into()));

        let parse = Expr::Cast {
            expr: Box::new(Expr::Literal(Value::Text(" 7 ".into()))),
            to: TypeKind::I64,
        };
        assert_eq!(evl(&parse), Value::I64(7));
    }

    #[test]
    fn cast_out_of_range_is_an_error() {
        let e = Expr::Cast {
            expr: Box::new(Expr::Literal(Value::F64(f64::NAN))),
            to: TypeKind::I64,
        };
        assert_eq!(
            eval(&e, &[], &Shape::new()).unwrap_err(),
            EvalError::CastRange { to: TypeKind::I64 }
        );
    }

    #[test]
    fn predicate_only_true_keeps() {
        let s = Shape::new();
        assert!(eval_predicate(&Expr::Literal(Value::Bool(true)), &[], &s).unwrap());
        assert!(!eval_predicate(&Expr::Literal(Value::Bool(false)), &[], &s).unwrap());
        assert!(!eval_predicate(&Expr::Literal(Value::Null), &[], &s).unwrap());
    }
}
