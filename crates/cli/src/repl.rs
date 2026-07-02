//! A small, robust interactive shell over the public [`Database`] API.
//!
//! v1 has no text query language (requests are built as typed ASTs or sent as
//! MessagePack over the wire), so the REPL is an **inspection + scan** console:
//! meta-commands to list tables, show a schema line, count and scan rows, and
//! run the integrity/inspect tools. Every command is fail-proof — a bad command
//! or an engine error prints a message and returns to the prompt, never panics.

use std::time::Instant;

use otf_edb::{Database, IoBackend, Request, Select, Stage, TableRef, Value};

/// What the loop should do after a line.
pub enum Step {
    /// Print this text (may be multi-line or empty) and keep going.
    Print(String),
    /// Leave the REPL.
    Quit,
}

/// Handle one input line against `db`. Pure over I/O so it is unit-testable:
/// it returns the text to print rather than writing to stdout. `timing` is
/// toggled by `\timing` and, when on, appends an elapsed-time line.
pub fn run_line<B: IoBackend + 'static>(db: &Database<B>, line: &str, timing: &mut bool) -> Step {
    let line = line.trim();
    if line.is_empty() {
        return Step::Print(String::new());
    }
    let started = Instant::now();
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();

    let out = match cmd {
        "\\q" | "\\quit" | "\\exit" => return Step::Quit,
        "\\help" | "\\h" | "\\?" => Ok(help_text()),
        "\\timing" => {
            // Report the toggle itself untimed.
            *timing = !*timing;
            return Step::Print(format!("timing {}", if *timing { "on" } else { "off" }));
        }
        "\\tables" => tables(db),
        "\\schema" => match rest.as_slice() {
            [table] => schema(db, table),
            _ => Err("usage: \\schema <table>".to_string()),
        },
        "\\count" => match rest.as_slice() {
            [table] => count(db, table),
            _ => Err("usage: \\count <table>".to_string()),
        },
        "\\scan" => match rest.as_slice() {
            [table] => scan(db, table, 20),
            [table, limit] => match limit.parse::<u64>() {
                Ok(n) => scan(db, table, n),
                Err(_) => Err(format!("not a number: {limit}")),
            },
            _ => Err("usage: \\scan <table> [limit]".to_string()),
        },
        "\\inspect" => db
            .inspect()
            .map(|i| i.to_string())
            .map_err(|e| format!("[{:?}] {e}", e.category())),
        "\\check" => db
            .check()
            .map(|r| r.to_string())
            .map_err(|e| format!("[{:?}] {e}", e.category())),
        other => Err(format!("unknown command {other:?} — try \\help",)),
    };

    let mut text = match out {
        Ok(text) => text,
        Err(msg) => format!("error: {msg}"),
    };
    if *timing {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!(
            "({:.3} ms)",
            started.elapsed().as_secs_f64() * 1e3
        ));
    }
    Step::Print(text)
}

pub fn help_text() -> String {
    [
        "commands:",
        "  \\tables            list tables",
        "  \\schema <table>    show a table's columns, rows, and indexes",
        "  \\count  <table>    number of rows",
        "  \\scan   <table> [n]  print up to n rows (default 20)",
        "  \\inspect           storage + per-table summary",
        "  \\check             full integrity check",
        "  \\timing            toggle command timing",
        "  \\help              this help",
        "  \\quit              leave",
    ]
    .join("\n")
}

fn tables<B: IoBackend + 'static>(db: &Database<B>) -> Result<String, String> {
    let inspection = db.inspect().map_err(fmt_err)?;
    if inspection.tables.is_empty() {
        return Ok("(no tables)".to_string());
    }
    Ok(inspection
        .tables
        .iter()
        .map(|t| t.name.clone())
        .collect::<Vec<_>>()
        .join("\n"))
}

fn schema<B: IoBackend + 'static>(db: &Database<B>, table: &str) -> Result<String, String> {
    let inspection = db.inspect().map_err(fmt_err)?;
    let info = inspection
        .tables
        .iter()
        .find(|t| t.name == table)
        .ok_or_else(|| format!("no such table: {table}"))?;
    let indexes = if info.indexes.is_empty() {
        "(none)".to_string()
    } else {
        info.indexes.join(", ")
    };
    Ok(format!(
        "{}: {} column(s), {} row(s)\nindexes: {}",
        info.name, info.columns, info.rows, indexes
    ))
}

fn count<B: IoBackend + 'static>(db: &Database<B>, table: &str) -> Result<String, String> {
    let out = db.execute(&scan_request(table, None)).map_err(fmt_err)?;
    Ok(out.len().to_string())
}

fn scan<B: IoBackend + 'static>(
    db: &Database<B>,
    table: &str,
    limit: u64,
) -> Result<String, String> {
    let out = db
        .execute(&scan_request(table, Some(limit)))
        .map_err(fmt_err)?;
    Ok(render_table(&out))
}

/// A full-table scan, optionally capped by a top-level limit.
fn scan_request(table: &str, limit: Option<u64>) -> Request {
    let mut stages = vec![Stage::Scan(TableRef {
        table: table.to_string(),
        alias: None,
    })];
    if let Some(n) = limit {
        stages.push(Stage::Limit {
            limit: Some(n),
            offset: 0,
        });
    }
    Request::Select(Select::Pipeline(stages))
}

/// Render a response as an aligned text table.
fn render_table(out: &otf_edb::Response) -> String {
    let cols = out.columns();
    if cols.is_empty() {
        return "(no columns)".to_string();
    }
    let mut widths: Vec<usize> = cols.iter().map(String::len).collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row in out.rows() {
        let cells: Vec<String> = (0..cols.len())
            .map(|i| row.at(i).map(fmt_value).unwrap_or_default())
            .collect();
        for (i, cell) in cells.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
        rows.push(cells);
    }

    let mut out_text = String::new();
    let header: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
        .collect();
    out_text.push_str(&header.join("  "));
    out_text.push('\n');
    out_text.push_str(
        &widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  "),
    );
    for cells in &rows {
        out_text.push('\n');
        let line: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
            .collect();
        out_text.push_str(&line.join("  "));
    }
    out_text.push_str(&format!("\n({} row(s))", rows.len()));
    out_text
}

/// A compact, human-readable rendering of a value.
fn fmt_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::F64(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Timestamp(t) => t.to_string(),
        Value::Blob(b) => format!("<blob {} bytes>", b.len()),
        Value::Json(j) => String::from_utf8_lossy(j).into_owned(),
        Value::Uuid(u) => {
            let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
            format!(
                "{}-{}-{}-{}-{}",
                &h[0..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..32]
            )
        }
    }
}

fn fmt_err(e: otf_edb::Error) -> String {
    format!("[{:?}] {e}", e.category())
}

#[cfg(test)]
mod tests {
    use super::*;
    use otf_edb::{ColumnDef, Insert, TableDef, TypeKind};

    fn seeded() -> Database<otf_edb::MemoryBackend> {
        let db = Database::create_memory().unwrap();
        db.create_table(TableDef::new(
            "users",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("name", TypeKind::Text).not_null(),
            ],
            vec!["id"],
        ))
        .unwrap();
        for (id, name) in [(1, "Ada"), (2, "Grace")] {
            db.execute(&Request::Insert(Insert {
                table: "users".into(),
                rows: vec![vec![
                    ("id".into(), Value::I64(id)),
                    ("name".into(), Value::Text(name.into())),
                ]],
            }))
            .unwrap();
        }
        db
    }

    fn output(db: &Database<otf_edb::MemoryBackend>, line: &str) -> String {
        let mut timing = false;
        match run_line(db, line, &mut timing) {
            Step::Print(text) => text,
            Step::Quit => "<quit>".to_string(),
        }
    }

    #[test]
    fn quit_variants_signal_quit() {
        let db = seeded();
        for q in ["\\q", "\\quit", "\\exit"] {
            assert_eq!(output(&db, q), "<quit>");
        }
    }

    #[test]
    fn tables_and_count_and_scan() {
        let db = seeded();
        assert_eq!(output(&db, "\\tables"), "users");
        assert_eq!(output(&db, "\\count users"), "2");
        let scan = output(&db, "\\scan users");
        assert!(scan.contains("Ada") && scan.contains("Grace"));
        assert!(scan.contains("(2 row(s))"));
        // A limit caps the rows.
        assert!(output(&db, "\\scan users 1").contains("(1 row(s))"));
    }

    #[test]
    fn schema_reports_columns_and_indexes() {
        let db = seeded();
        let s = output(&db, "\\schema users");
        assert!(s.contains("users") && s.contains("2 column(s)") && s.contains("2 row(s)"));
    }

    #[test]
    fn errors_never_panic_and_stay_in_the_loop() {
        let db = seeded();
        assert!(output(&db, "\\count nope").starts_with("error:"));
        assert!(output(&db, "\\bogus").starts_with("error:"));
        assert!(output(&db, "\\scan").starts_with("error:"));
        assert_eq!(output(&db, ""), "");
    }

    #[test]
    fn timing_toggles_and_annotates() {
        let db = seeded();
        let mut timing = false;
        // Toggle on.
        match run_line(&db, "\\timing", &mut timing) {
            Step::Print(t) => assert_eq!(t, "timing on"),
            Step::Quit => panic!(),
        }
        assert!(timing);
        // A subsequent command carries an elapsed-time annotation.
        match run_line(&db, "\\count users", &mut timing) {
            Step::Print(t) => assert!(t.contains("ms)")),
            Step::Quit => panic!(),
        }
    }
}
