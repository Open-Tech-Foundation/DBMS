//! Integrity checking and the file inspector (Phase 10 tools).
//!
//! [`IntegrityReport`] is the result of a full, read-only cross-check: the
//! pager's structural invariants (meta slots, free-list) *and* every B+tree,
//! with each secondary index verified entry-for-entry against its base table.
//! A corrupted page fails its checksum on read and surfaces here as a
//! `Corruption`-category error.
//!
//! [`Inspection`] is a human-readable structural dump — storage statistics plus
//! per-table row and index counts — for the file-inspector tool.

use std::fmt;

use pager::PagerStats;

/// The outcome of a passing [`crate::Database::check`]: storage statistics and
/// how many tables were cross-checked. Failure returns a categorized error
/// instead (`Corruption` for a bad page/index).
#[derive(Debug, Clone)]
pub struct IntegrityReport {
    /// Pager-level statistics gathered during the check.
    pub stats: PagerStats,
    /// The number of tables whose trees and indexes were verified.
    pub tables_checked: usize,
}

impl fmt::Display for IntegrityReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "integrity ok: {} pages, {} free, {} tables checked (txn {})",
            self.stats.page_count, self.stats.free_ids, self.tables_checked, self.stats.txn_id
        )
    }
}

/// A structural snapshot of a database file: storage statistics and a summary
/// of each table. Produced by [`crate::Database::inspect`].
#[derive(Debug, Clone)]
pub struct Inspection {
    /// Pager-level statistics.
    pub stats: PagerStats,
    /// One entry per table, in name order.
    pub tables: Vec<TableInfo>,
}

/// A single table's structural summary.
#[derive(Debug, Clone)]
pub struct TableInfo {
    /// The table name.
    pub name: String,
    /// The number of columns.
    pub columns: usize,
    /// The number of live rows.
    pub rows: usize,
    /// The secondary index names.
    pub indexes: Vec<String>,
}

impl fmt::Display for Inspection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "database: {} pages, {} free, {} trunk(s), meta slot {}, txn {}",
            self.stats.page_count,
            self.stats.free_ids,
            self.stats.trunk_count,
            self.stats.active_slot,
            self.stats.txn_id,
        )?;
        if self.tables.is_empty() {
            writeln!(f, "  (no tables)")?;
        }
        for t in &self.tables {
            write!(
                f,
                "  {} — {} column(s), {} row(s)",
                t.name, t.columns, t.rows
            )?;
            if t.indexes.is_empty() {
                writeln!(f)?;
            } else {
                writeln!(f, ", indexes: {}", t.indexes.join(", "))?;
            }
        }
        Ok(())
    }
}
