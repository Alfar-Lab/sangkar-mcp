//! The `sangkar` binary: the entry point the installed command runs.
//!
//! Every module lives in the `sangkar_cli` library (`src/lib.rs`), and this binary is a shell over
//! it: it collects the process's own argv, environment, working directory and three streams and
//! hands them to [`sangkar_cli::dispatch`], which owns every decision. The module tree is declared in
//! the library and only there: a second declaration here would compile every module twice, so the
//! `store_lifecycle` code `tests/store_refresh.rs` exercises would not be the code this binary runs.
//!
//! This is also the one place in the crate that names the process's streams. Every module below it
//! takes an injected `Write`/`BufRead`, which is what keeps a stray line out of a protocol stream and
//! lets an integration test read the bytes a run would have printed; the handles are opened here,
//! once. They are deliberately **not locked**: one rendered envelope is a single `write_all`, so a
//! per-write lock gives every CLI answer the same non-interleaving guarantee, while a lock held for
//! the whole run would deadlock the MCP server — `rmcp`'s stdio transport reads and writes through
//! `tokio::io::stdin()`/`stdout()` on other threads, and those lock the same handles
//! (`std::io::Stdin`/`Stdout`'s locks are not re-entrant across threads). `sangkar_core::log` locks
//! stderr per line for the same reason: it logs from whichever thread the work happens on.

use std::io::{self, IsTerminal, Write as _};

use sangkar_cli::dispatch::{Invocation, dispatch};

fn main() {
    // `args_os` rather than `args`: a path that is not UTF-8 must not panic the process before it
    // has written a diagnostic. The lossy conversion is the same spelling the config resolver would
    // have read had the shell passed the string it was given.
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let stdin = io::stdin();
    // Whether the index gate may ask its question: a redirected stdin has nobody to answer it, and
    // the gate refuses rather than assuming a yes.
    let tty = stdin.is_terminal();
    // Buffered but **not locked**: `StdinLock` would hold the process's stdin mutex for the whole
    // run, which deadlocks the MCP server's own reads through `tokio::io::stdin()`. Nothing buffered
    // here is ever read by that path — the transport reads fd 0 itself — so the buffer is empty when
    // a session starts.
    let mut stdin = io::BufReader::new(stdin);
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();

    let code = dispatch(&mut Invocation {
        argv,
        env: &sangkar_core::config::process_env,
        cwd: sangkar_core::config::current_dir(),
        tty,
        stdin: &mut stdin,
        stdout: &mut stdout,
        stderr: &mut stderr,
    });

    // Flushed before the exit: `exit` does not run destructors, and a refusal that never reached
    // the terminal would be the worst of the two. (An MCP session has already flushed its own
    // frames — each one ends in a newline through a line-buffered handle — so this is the CLI's.)
    let _ = stdout.flush();
    let _ = stderr.flush();
    std::process::exit(code);
}
