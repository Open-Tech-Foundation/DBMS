//! `otf-dbms` — the command-line tool.
//!
//! Phase 10 ships two read-only file tools over the public API:
//!
//! ```text
//! otf-dbms check   <file>   # run the full integrity check
//! otf-dbms inspect <file>   # print a structural summary
//! ```
//!
//! The REPL, scenario runner, and concurrency playground follow in Phase 11.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, path] if cmd == "check" => run(check(path)),
        [cmd, path] if cmd == "inspect" => run(inspect(path)),
        _ => {
            usage();
            ExitCode::FAILURE
        }
    }
}

fn run(result: otf_dbms::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error [{:?}]: {err}", err.category());
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
fn check(path: &str) -> otf_dbms::Result<()> {
    let db = otf_dbms::Database::open(path)?;
    let report = db.check()?;
    println!("{report}");
    Ok(())
}

#[cfg(unix)]
fn inspect(path: &str) -> otf_dbms::Result<()> {
    let db = otf_dbms::Database::open(path)?;
    let report = db.inspect()?;
    print!("{report}");
    Ok(())
}

#[cfg(not(unix))]
fn check(_path: &str) -> otf_dbms::Result<()> {
    Err(otf_dbms::Error::Usage("file tools require a unix target"))
}

#[cfg(not(unix))]
fn inspect(_path: &str) -> otf_dbms::Result<()> {
    Err(otf_dbms::Error::Usage("file tools require a unix target"))
}

fn usage() {
    eprintln!(
        "{} {} — embedded database file tools\n\n\
         usage:\n  \
         {0} check   <file>   run the full integrity check\n  \
         {0} inspect <file>   print a structural summary",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION"),
    );
}
