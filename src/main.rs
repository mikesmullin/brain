//! brain — a knowledge-graph brain.
//!
//! v2: Rust, one shared PostgreSQL, a folder of Markdown as the source of
//! truth, an OWL-flavoured T-box in `schema.yaml`, and `crawl` as the default
//! way to ask a question.

mod abox;
mod bench;
mod cli;
mod commands;
mod config;
mod convert;
mod index;
mod llm;
mod mcp;
mod pg;
mod query;
mod server;
mod slug;
mod store;
mod tbox;
mod web;

use anyhow::Result;

fn show(s: &str) {
    println!("{s}");
}

fn main() {
    restore_sigpipe();
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // `brain web` runs the rustex1 dev server: synchronous, owns the process,
    // re-execs itself when a source file changes, and builds its own runtime
    // for the database work. It must therefore start OUTSIDE a runtime, which
    // is why this is a plain `fn main` with the runtime built by hand below.
    if is_web(&argv) {
        let (global, rest) = match cli::peel_global(argv) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
        config::set_use_override(global.use_alias.clone());
        if rest.iter().any(|a| a == "-h" || a == "--help") {
            if let Some(u) = cli::usage("web") {
                show(u);
            }
            std::process::exit(0);
        }
        std::process::exit(web::main(rest[1..].to_vec(), &global));
    }

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = match rt.block_on(run(argv)) {
        Ok(c) => c,
        Err(e) => {
            // A missing-required-arg error shows the command's full usage,
            // exactly as v1 did: the help IS the error message there.
            let msg = format!("{e}");
            if let Some(rest) = msg.strip_prefix("usage:") {
                let cmd = rest.trim();
                if let Some(u) = cli::usage(cmd) {
                    eprintln!("{u}");
                    std::process::exit(1);
                }
            }
            eprintln!("error: {msg}");
            for cause in e.chain().skip(1) {
                eprintln!("  caused by: {cause}");
            }
            1
        }
    };
    std::process::exit(code);
}

/// Die quietly when a reader goes away, like every other Unix tool.
///
/// Rust ignores `SIGPIPE` at startup, which turns a closed stdout into a write
/// error, which makes `println!` panic. `brain ls | head` is an ordinary thing
/// to type, and it should not print a backtrace note. `brain ls` streams
/// millions of rows, so this is not a corner case.
#[cfg(unix)]
fn restore_sigpipe() {
    // SAFETY: setting a signal disposition to the default before any threads
    // exist is the documented remedy, and SIG_DFL for SIGPIPE is terminate.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

/// Is this invocation `brain web`? Checked before the global flags are peeled,
/// so it has to skip over their values.
fn is_web(argv: &[String]) -> bool {
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--use" | "--db" => i += 2,
            a if a.starts_with("--use=") || a.starts_with("--db=") => i += 1,
            "web" => return true,
            _ => return false,
        }
    }
    false
}

async fn run(argv: Vec<String>) -> Result<i32> {
    let (global, argv) = cli::peel_global(argv)?;
    config::set_use_override(global.use_alias.clone());

    let Some(cmd) = argv.first().cloned() else {
        show(cli::HELP);
        return Ok(0);
    };
    let rest: Vec<String> = argv[1..].to_vec();

    match cmd.as_str() {
        "--help" | "-h" => {
            show(cli::HELP);
            return Ok(0);
        }
        "--version" | "-V" | "version" => {
            show(cli::VERSION);
            return Ok(0);
        }
        "help" => {
            return Ok(match rest.first() {
                None => {
                    show(cli::HELP);
                    0
                }
                Some(sub) => match cli::usage(sub) {
                    Some(u) => {
                        show(u);
                        0
                    }
                    None => {
                        eprintln!("unknown command: {sub}\n");
                        show(cli::HELP);
                        1
                    }
                },
            })
        }
        _ => {}
    }

    // Per-command help.
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        match cli::usage(&cmd) {
            Some(u) => show(u),
            None => show(&format!("no detailed help for '{cmd}'")),
        }
        return Ok(0);
    }

    // `brain "<question>"` — crawl is the default mode. A single word that is
    // not a command is still a typo, not a question, so the fallback needs
    // whitespace or a question mark before it will run.
    let (cmd, rest) = if cli::COMMANDS.contains(&cmd.as_str()) {
        (cmd, rest)
    } else if cmd.contains(char::is_whitespace) || cmd.ends_with('?') {
        let mut r = vec![cmd];
        r.extend(rest);
        ("crawl".to_string(), r)
    } else {
        eprintln!("unknown command: {cmd}\n");
        show(cli::HELP);
        return Ok(1);
    };

    // `init`, `use` and `pg` work without a resolvable brain; everything else
    // needs one.
    if !matches!(cmd.as_str(), "init" | "use" | "pg" | "convert") && !config::exists(global.db.as_deref()) {
        eprintln!("fatal: brain database not found.");
        eprintln!(
            "select a database with `brain use`, pass `--use <alias>` or `--db <path>`, \
or run this from a brain directory."
        );
        return Ok(1);
    }

    commands::dispatch(&cmd, &rest, &global).await
}
