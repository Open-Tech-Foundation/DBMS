//! `otf-dbms` — the command-line tool.
//!
//! ```text
//! otf-dbms check   <file>   # run the full integrity check
//! otf-dbms inspect <file>   # print a structural summary
//! otf-dbms repl    <file>   # open (or create) and explore interactively
//! ```

mod repl;

use std::io::{BufRead, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, path] if cmd == "check" => run(check(path)),
        [cmd, path] if cmd == "inspect" => run(inspect(path)),
        [cmd, path] if cmd == "repl" => run(repl_cmd(path)),
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

/// The interactive shell: a prompt loop over [`repl::run_line`]. Opens the
/// database at `path`, creating it if the file is absent or empty.
#[cfg(unix)]
fn repl_cmd(path: &str) -> otf_dbms::Result<()> {
    let empty = std::fs::metadata(path)
        .map(|m| m.len() == 0)
        .unwrap_or(true);
    let db = if empty {
        otf_dbms::Database::create(path)?
    } else {
        otf_dbms::Database::open(path)?
    };
    println!(
        "otf-dbms {} — {path}\ntype \\help for commands, \\quit to leave",
        env!("CARGO_PKG_VERSION"),
    );
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut timing = false;
    loop {
        print!("otf> ");
        let _ = stdout.flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF (Ctrl-D)
            Ok(_) => {}
            Err(err) => {
                eprintln!("input error: {err}");
                break;
            }
        }
        match repl::run_line(&db, &line, &mut timing) {
            repl::Step::Quit => break,
            repl::Step::Print(text) => {
                if !text.is_empty() {
                    println!("{text}");
                }
            }
        }
    }
    println!("bye");
    Ok(())
}

#[cfg(not(unix))]
fn repl_cmd(_path: &str) -> otf_dbms::Result<()> {
    Err(otf_dbms::Error::Usage("the repl requires a unix target"))
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
        "{} {} — embedded database tools\n\n\
         usage:\n  \
         {0} check   <file>   run the full integrity check\n  \
         {0} inspect <file>   print a structural summary\n  \
         {0} repl    <file>   open (or create) and explore interactively",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION"),
    );
}
