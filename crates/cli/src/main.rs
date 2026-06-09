//! `otf-dbms` — the command-line playground.
//!
//! Will host the REPL, scenario runner, and concurrency playground (Phase 11).
//! For now it is a placeholder that confirms the workspace builds and links
//! against the public `otf-dbms` API.

fn main() {
    // Touch the public API so the binary genuinely depends on `otf-dbms`.
    let _category = otf_dbms::ErrorCategory::Validation;
    println!(
        "{} {} — scaffolding (Phase 1). The engine is not yet implemented.",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION"),
    );
}
