//! Binary persistence of [`TableDef`]s in the system catalog.
//!
//! A versioned, hand-rolled format in the codebase's house style: length
//! prefixes everywhere, a bounds-checked reader, and typed
//! [`CatalogCorruption`] errors — stored bytes never panic the decoder.
//! Embedded literals (defaults, CHECK literals) reuse the row encoding.

use types::{decode_row, encode_row, TypeKind, Value};

use crate::schema::{
    CheckExpr, CmpOp, ColumnDef, DefaultSpec, IndexDef, TableDef, UpdatePolicy, MAX_CHECK_DEPTH,
};
use crate::{CatalogCorruption, CatalogError, Result};

/// Version 2 added the secondary-index section (Phase 7); version-1 records
/// (no indexes) still decode.
const VERSION: u8 = 2;
const VERSION_NO_INDEXES: u8 = 1;

const FLAG_NULLABLE: u8 = 1 << 0;
const FLAG_UNIQUE: u8 = 1 << 1;
const FLAG_AUTO_INCREMENT: u8 = 1 << 2;
const FLAG_ROWVERSION: u8 = 1 << 3;
const FLAG_ON_UPDATE_NOW: u8 = 1 << 4;
const FLAG_GUARDED: u8 = 1 << 5;

const DEFAULT_NONE: u8 = 0;
const DEFAULT_VALUE: u8 = 1;
const DEFAULT_NOW: u8 = 2;
const DEFAULT_UUID_V7: u8 = 3;

const CHECK_CMP: u8 = 1;
const CHECK_IS_NULL: u8 = 2;
const CHECK_IS_NOT_NULL: u8 = 3;
const CHECK_AND: u8 = 4;
const CHECK_OR: u8 = 5;
const CHECK_NOT: u8 = 6;

/// Encode a table definition for storage in the catalog tree.
pub(crate) fn encode_table(def: &TableDef) -> Result<Vec<u8>> {
    let mut out = vec![VERSION];
    push_str(&mut out, &def.name)?;
    push_count(&mut out, def.columns.len())?;
    for col in &def.columns {
        push_str(&mut out, &col.name)?;
        out.push(kind_code(col.kind));
        let mut flags = 0u8;
        if col.nullable {
            flags |= FLAG_NULLABLE;
        }
        if col.unique {
            flags |= FLAG_UNIQUE;
        }
        if col.auto_increment {
            flags |= FLAG_AUTO_INCREMENT;
        }
        if col.rowversion {
            flags |= FLAG_ROWVERSION;
        }
        if col.on_update_now {
            flags |= FLAG_ON_UPDATE_NOW;
        }
        if col.update == UpdatePolicy::Guarded {
            flags |= FLAG_GUARDED;
        }
        out.push(flags);
        match &col.default {
            None => out.push(DEFAULT_NONE),
            Some(DefaultSpec::Value(v)) => {
                out.push(DEFAULT_VALUE);
                push_value(&mut out, v)?;
            }
            Some(DefaultSpec::Now) => out.push(DEFAULT_NOW),
            Some(DefaultSpec::UuidV7) => out.push(DEFAULT_UUID_V7),
        }
    }
    push_count(&mut out, def.pk.len())?;
    for name in &def.pk {
        push_str(&mut out, name)?;
    }
    push_count(&mut out, def.checks.len())?;
    for check in &def.checks {
        push_check(&mut out, check)?;
    }
    push_count(&mut out, def.indexes.len())?;
    for index in &def.indexes {
        push_str(&mut out, &index.name)?;
        out.push(u8::from(index.unique));
        push_count(&mut out, index.columns.len())?;
        for col in &index.columns {
            push_str(&mut out, col)?;
        }
    }
    Ok(out)
}

/// Decode a stored table definition.
pub(crate) fn decode_table(bytes: &[u8]) -> Result<TableDef> {
    let mut r = Reader { rest: bytes };
    let version = r.byte()?;
    if version != VERSION && version != VERSION_NO_INDEXES {
        return Err(corrupt(CatalogCorruption::BadVersion { version }));
    }
    let name = r.string()?;
    let column_count = r.count()?;
    let mut columns = Vec::with_capacity(column_count.min(256));
    for _ in 0..column_count {
        let name = r.string()?;
        let kind = kind_from(r.byte()?)?;
        let flags = r.byte()?;
        let default = match r.byte()? {
            DEFAULT_NONE => None,
            DEFAULT_VALUE => Some(DefaultSpec::Value(r.value()?)),
            DEFAULT_NOW => Some(DefaultSpec::Now),
            DEFAULT_UUID_V7 => Some(DefaultSpec::UuidV7),
            tag => return Err(corrupt(CatalogCorruption::BadTag { tag })),
        };
        columns.push(ColumnDef {
            name,
            kind,
            nullable: flags & FLAG_NULLABLE != 0,
            unique: flags & FLAG_UNIQUE != 0,
            default,
            auto_increment: flags & FLAG_AUTO_INCREMENT != 0,
            rowversion: flags & FLAG_ROWVERSION != 0,
            on_update_now: flags & FLAG_ON_UPDATE_NOW != 0,
            update: if flags & FLAG_GUARDED != 0 {
                UpdatePolicy::Guarded
            } else {
                UpdatePolicy::Free
            },
        });
    }
    let pk_count = r.count()?;
    let mut pk = Vec::with_capacity(pk_count.min(16));
    for _ in 0..pk_count {
        pk.push(r.string()?);
    }
    let check_count = r.count()?;
    let mut checks = Vec::with_capacity(check_count.min(16));
    for _ in 0..check_count {
        checks.push(r.check(0)?);
    }
    let mut indexes = Vec::new();
    if version >= VERSION {
        let index_count = r.count()?;
        indexes.reserve(index_count.min(64));
        for _ in 0..index_count {
            let name = r.string()?;
            let unique = match r.byte()? {
                0 => false,
                1 => true,
                tag => return Err(corrupt(CatalogCorruption::BadTag { tag })),
            };
            let col_count = r.count()?;
            let mut cols = Vec::with_capacity(col_count.min(16));
            for _ in 0..col_count {
                cols.push(r.string()?);
            }
            indexes.push(IndexDef {
                name,
                columns: cols,
                unique,
            });
        }
    }
    if !r.rest.is_empty() {
        return Err(corrupt(CatalogCorruption::TrailingBytes));
    }
    Ok(TableDef {
        name,
        columns,
        pk,
        checks,
        indexes,
    })
}

fn kind_code(kind: TypeKind) -> u8 {
    match kind {
        TypeKind::Bool => 1,
        TypeKind::I64 => 2,
        TypeKind::F64 => 3,
        TypeKind::Text => 4,
        TypeKind::Blob => 5,
        TypeKind::Uuid => 6,
        TypeKind::Json => 7,
        TypeKind::Timestamp => 8,
    }
}

fn kind_from(code: u8) -> Result<TypeKind> {
    Ok(match code {
        1 => TypeKind::Bool,
        2 => TypeKind::I64,
        3 => TypeKind::F64,
        4 => TypeKind::Text,
        5 => TypeKind::Blob,
        6 => TypeKind::Uuid,
        7 => TypeKind::Json,
        8 => TypeKind::Timestamp,
        tag => return Err(corrupt(CatalogCorruption::BadTag { tag })),
    })
}

fn push_count(out: &mut Vec<u8>, n: usize) -> Result<()> {
    let n = u16::try_from(n).map_err(|_| CatalogError::InvalidSchema {
        reason: "definition is too large to encode".to_string(),
    })?;
    out.extend_from_slice(&n.to_le_bytes());
    Ok(())
}

fn push_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    push_count(out, s.len())?;
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Embed one literal via the row encoding (robust, already round-tripping).
fn push_value(out: &mut Vec<u8>, value: &Value) -> Result<()> {
    let bytes = encode_row(std::slice::from_ref(value))?;
    push_count(out, bytes.len())?;
    out.extend_from_slice(&bytes);
    Ok(())
}

fn push_check(out: &mut Vec<u8>, check: &CheckExpr) -> Result<()> {
    match check {
        CheckExpr::Cmp { col, op, value } => {
            out.push(CHECK_CMP);
            push_str(out, col)?;
            out.push(match op {
                CmpOp::Eq => 1,
                CmpOp::Ne => 2,
                CmpOp::Lt => 3,
                CmpOp::Lte => 4,
                CmpOp::Gt => 5,
                CmpOp::Gte => 6,
            });
            push_value(out, value)?;
        }
        CheckExpr::IsNull { col } => {
            out.push(CHECK_IS_NULL);
            push_str(out, col)?;
        }
        CheckExpr::IsNotNull { col } => {
            out.push(CHECK_IS_NOT_NULL);
            push_str(out, col)?;
        }
        CheckExpr::And(items) | CheckExpr::Or(items) => {
            out.push(if matches!(check, CheckExpr::And(_)) {
                CHECK_AND
            } else {
                CHECK_OR
            });
            push_count(out, items.len())?;
            for item in items {
                push_check(out, item)?;
            }
        }
        CheckExpr::Not(inner) => {
            out.push(CHECK_NOT);
            push_check(out, inner)?;
        }
    }
    Ok(())
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.rest.len() < n {
            return Err(corrupt(CatalogCorruption::Truncated));
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn byte(&mut self) -> Result<u8> {
        let (&b, tail) = self
            .rest
            .split_first()
            .ok_or_else(|| corrupt(CatalogCorruption::Truncated))?;
        self.rest = tail;
        Ok(b)
    }

    fn count(&mut self) -> Result<usize> {
        let mut n = [0u8; 2];
        n.copy_from_slice(self.take(2)?);
        Ok(usize::from(u16::from_le_bytes(n)))
    }

    fn string(&mut self) -> Result<String> {
        let len = self.count()?;
        let bytes = self.take(len)?.to_vec();
        String::from_utf8(bytes).map_err(|_| corrupt(CatalogCorruption::InvalidUtf8))
    }

    fn value(&mut self) -> Result<Value> {
        let len = self.count()?;
        let bytes = self.take(len)?;
        let mut row = decode_row(bytes).map_err(|_| corrupt(CatalogCorruption::BadValue))?;
        if row.len() != 1 {
            return Err(corrupt(CatalogCorruption::BadValue));
        }
        Ok(row.swap_remove(0))
    }

    fn check(&mut self, depth: usize) -> Result<CheckExpr> {
        if depth > MAX_CHECK_DEPTH {
            return Err(corrupt(CatalogCorruption::DepthExceeded));
        }
        Ok(match self.byte()? {
            CHECK_CMP => {
                let col = self.string()?;
                let op = match self.byte()? {
                    1 => CmpOp::Eq,
                    2 => CmpOp::Ne,
                    3 => CmpOp::Lt,
                    4 => CmpOp::Lte,
                    5 => CmpOp::Gt,
                    6 => CmpOp::Gte,
                    tag => return Err(corrupt(CatalogCorruption::BadTag { tag })),
                };
                let value = self.value()?;
                CheckExpr::Cmp { col, op, value }
            }
            CHECK_IS_NULL => CheckExpr::IsNull {
                col: self.string()?,
            },
            CHECK_IS_NOT_NULL => CheckExpr::IsNotNull {
                col: self.string()?,
            },
            tag @ (CHECK_AND | CHECK_OR) => {
                let count = self.count()?;
                let mut items = Vec::with_capacity(count.min(16));
                for _ in 0..count {
                    items.push(self.check(depth + 1)?);
                }
                if tag == CHECK_AND {
                    CheckExpr::And(items)
                } else {
                    CheckExpr::Or(items)
                }
            }
            CHECK_NOT => CheckExpr::Not(Box::new(self.check(depth + 1)?)),
            tag => return Err(corrupt(CatalogCorruption::BadTag { tag })),
        })
    }
}

fn corrupt(kind: CatalogCorruption) -> CatalogError {
    CatalogError::Corrupt(kind)
}
