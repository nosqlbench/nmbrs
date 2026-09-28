// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs attach` — connect to a running nmbrs's OOB
//! introspection socket. Three modes, in priority order:
//!
//! 1. **One-shot** (`-c <cmd>` / `--command <cmd>`, repeatable):
//!    run the supplied command(s) against the socket, print
//!    each response to stdout, and exit. Designed for
//!    scripting and pipelines.
//! 2. **Line-mode REPL** (`tui=off`): a plain stdin/stdout
//!    REPL with no terminal control sequences, suitable for
//!    pipes, automation, and other programs that want to
//!    drive nmbrs synchronously.
//! 3. **TUI REPL** (default when stdin is a TTY): the
//!    ratatui-based interactive shell with autocomplete,
//!    history, and scrollback.
//!
//! Discovery: scans the runtime directory
//! (`${XDG_RUNTIME_DIR:-/tmp}`) for `nmbrs-<pid>.sock` files.
//! With exactly one match, attaches automatically. With none,
//! prints a clear error. With several, lists them and lets the
//! user pass `--pid <N>` or `--socket <path>` to disambiguate.
//!
//! See `nmbrs_tui::inspector_server` for the server side and
//! SRD-02 §"Display and Diagnostic Decoupling" for the
//! architectural rationale.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

use nmbrs_tui::inspector_repl::{discover_sockets, query, run_repl};

pub fn inspector_command(args: &[String]) {
    let opts = parse_args(args);
    let target = match resolve_socket(&opts) {
        Some(path) => path,
        None => std::process::exit(1),
    };

    // Probe the socket before entering any mode. The socket
    // file might be stale (server crashed without cleanup) or
    // the server might have died between discovery and our
    // attach. We'd rather report that here, with a clear
    // message, than drop the user into a TUI that immediately
    // shows connection errors. `phases` is a cheap read-only
    // query every server answers.
    if let Err(e) = query(&target, "phases") {
        eprintln!(
            "nmbrs attach: cannot reach inspector at {}: {e}",
            target.display()
        );
        eprintln!();
        eprintln!("The socket exists but no nmbrs is listening on it");
        eprintln!("(stale file from a previous run, or the server died");
        eprintln!("after the socket was discovered). Try:");
        eprintln!("  rm {}", target.display());
        eprintln!("  ls $XDG_RUNTIME_DIR/nmbrs-*.sock 2>/dev/null");
        std::process::exit(1);
    }

    // Mode 1: one-shot — run each `-c <cmd>` in order, print
    // responses, exit. Exit status reflects whether any
    // command came back as `ERR ...` or hit a connection
    // error. Stdout is the canonical output channel; stderr
    // carries connection / discovery problems only.
    if !opts.commands.is_empty() {
        let mut any_err = false;
        for cmd in &opts.commands {
            match query(&target, cmd) {
                Ok(resp) => {
                    print!("{resp}");
                    if !resp.ends_with('\n') {
                        println!();
                    }
                    if resp.starts_with("ERR ") {
                        any_err = true;
                    }
                }
                Err(e) => {
                    eprintln!("inspector: {cmd}: {e}");
                    any_err = true;
                }
            }
        }
        std::process::exit(if any_err { 1 } else { 0 });
    }

    // Mode 2: line-mode REPL — `tui=off`, or auto-detected
    // when stdin is not a TTY (pipe, here-doc, file
    // redirection). No raw mode, no alternate screen, no
    // crossterm dependency on the input path: just
    // `read_line` / `println!`. Exits cleanly on EOF.
    let stdin_is_tty = io::stdin().is_terminal();
    let force_tui = opts.tui == Some(true);
    let force_no_tui = opts.tui == Some(false);
    let line_mode = force_no_tui || (!force_tui && !stdin_is_tty);
    if line_mode {
        run_line_repl(&target);
        return;
    }

    // Mode 3: TUI REPL.
    if let Err(e) = run_repl(target) {
        eprintln!("inspector: {e}");
        std::process::exit(1);
    }
}

/// Stateless line REPL. Reads commands from stdin one line at
/// a time, dispatches each to the inspector socket, writes the
/// response to stdout. EOF (Ctrl+D, end of pipe) ends the
/// session cleanly. Suitable for `echo phases | nmbrs
/// --inspector tui=off` or any other consumer that talks line
/// protocol.
fn run_line_repl(socket: &std::path::Path) {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let prompt_to_tty = std::io::stderr().is_terminal();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if matches!(trimmed, ":q" | ":quit" | "quit" | "exit") {
            break;
        }
        match query(socket, trimmed) {
            Ok(resp) => {
                let _ = stdout.write_all(resp.as_bytes());
                if !resp.ends_with('\n') {
                    let _ = stdout.write_all(b"\n");
                }
                // Visual separator between responses so
                // consumers doing `cat | inspector tui=off`
                // can split records easily.
                let _ = stdout.write_all(b"\n");
                let _ = stdout.flush();
            }
            Err(e) => {
                if prompt_to_tty {
                    eprintln!("inspector: {e}");
                } else {
                    eprintln!("ERR connection: {e}");
                }
            }
        }
    }
}

fn resolve_socket(opts: &InspectorOpts) -> Option<PathBuf> {
    if let Some(p) = opts.socket.clone() {
        return Some(p);
    }
    let mut sockets = discover_sockets();
    if let Some(pid) = opts.pid_filter {
        sockets.retain(|s| s.pid == pid);
    }
    match sockets.len() {
        0 => {
            eprintln!("nmbrs attach: no running nmbrs sockets found.");
            eprintln!();
            eprintln!("nmbrs publishes its inspector socket while `nmbrs run` is");
            eprintln!("executing a workload. Searched:");
            eprintln!(
                "  {}/nmbrs-<pid>.sock",
                std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string())
            );
            eprintln!();
            eprintln!("Try: --socket /path/to/nmbrs-<pid>.sock");
            None
        }
        1 => Some(sockets[0].path.clone()),
        _ => {
            eprintln!("nmbrs attach: more than one running nmbrs found.");
            eprintln!();
            for s in &sockets {
                eprintln!("  pid {:>6}  {}", s.pid, s.path.display());
            }
            eprintln!();
            eprintln!("Choose one:");
            eprintln!("  nmbrs attach --pid <N>");
            eprintln!("  nmbrs attach --socket <path>");
            None
        }
    }
}

#[derive(Default)]
struct InspectorOpts {
    pid_filter: Option<u32>,
    socket: Option<PathBuf>,
    /// User-forced TUI/line preference. `Some(true)` means
    /// `tui=on` was set; `Some(false)` means `tui=off` was set
    /// (or `--no-tui`); `None` means auto-detect from stdin.
    tui: Option<bool>,
    /// One-shot commands from `-c <cmd>` / `--command <cmd>`
    /// arguments, in the order given. Empty = enter REPL.
    commands: Vec<String>,
}

fn parse_args(args: &[String]) -> InspectorOpts {
    let mut opts = InspectorOpts::default();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--pid" => {
                if let Some(v) = iter.next() {
                    opts.pid_filter = v.parse::<u32>().ok();
                }
            }
            "--socket" => {
                if let Some(v) = iter.next() {
                    opts.socket = Some(PathBuf::from(v));
                }
            }
            "-c" | "--command" => {
                if let Some(v) = iter.next() {
                    opts.commands.push(v.clone());
                }
            }
            "--no-tui" => {
                opts.tui = Some(false);
            }
            other => {
                if let Some(v) = other.strip_prefix("tui=") {
                    opts.tui = Some(matches!(v, "on" | "true" | "1" | "yes"));
                } else if let Some(v) = other.strip_prefix("--pid=") {
                    opts.pid_filter = v.parse::<u32>().ok();
                } else if let Some(v) = other.strip_prefix("--socket=") {
                    opts.socket = Some(PathBuf::from(v));
                } else if let Some(v) = other.strip_prefix("--command=") {
                    opts.commands.push(v.to_string());
                }
            }
        }
    }
    opts
}

// ── cli_spec entry ─────────────────────────────────────────

use crate::cli_spec::{
    Arity, Category, Command, Flag, Handler, Level, ParsedCommand, ValueProvider,
};

/// `nmbrs attach` — connect to a running nmbrs's OOB
/// introspection socket. Walker-parsed: single flat flag set,
/// no subcommands.
pub fn spec() -> Command {
    Command {
        name: "attach",
        help: "Connect to a running nmbrs's introspection socket.",
        category: Category::Shell,
        level: Level::Workload,
        flags: vec![
            Flag {
                long: "--pid",
                short: None,
                aliases: &[],
                arity: Arity::Value,
                value: ValueProvider::Custom(crate::completion::pid_provider),
                help: "Filter by PID (resolves <runtime-dir>/nmbrs-<pid>.sock).",
                repeatable: false,
            },
            Flag {
                long: "--socket",
                short: None,
                aliases: &[],
                arity: Arity::Value,
                value: ValueProvider::Custom(crate::completion::socket_path_provider),
                help: "Direct path to the inspector socket.",
                repeatable: false,
            },
            Flag {
                long: "--command",
                short: Some("-c"),
                aliases: &[],
                arity: Arity::Value,
                value: ValueProvider::Custom(crate::completion::inspector_command_provider),
                help: "One-shot command(s); repeat for multiple.",
                repeatable: true,
            },
            Flag {
                long: "--no-tui",
                short: None,
                aliases: &[],
                arity: Arity::Bool,
                value: ValueProvider::None,
                help: "Disable TUI mode.",
                repeatable: false,
            },
            Flag {
                long: "--tui",
                short: None,
                aliases: &[],
                arity: Arity::Value,
                value: ValueProvider::Custom(static_tui),
                help: "tui=on|off override.",
                repeatable: false,
            },
        ],
        kv_params: &[],
        dynamic_options: None,
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: false,
        completion_override: None,
    }
}

fn static_tui(p: &str, _: &[&str]) -> Vec<String> {
    ["on", "off"]
        .iter()
        .filter(|s| s.starts_with(p))
        .map(|s| s.to_string())
        .collect()
}

fn handle(p: ParsedCommand) -> Result<(), String> {
    // Re-build legacy argv shape so the existing parser/handler
    // can run unchanged. The spec validated the flag set
    // upstream; this just translates ParsedCommand → argv.
    let mut argv: Vec<String> = Vec::new();
    if let Some(v) = p.flag("--pid") {
        argv.push("--pid".into());
        argv.push(v.into());
    }
    if let Some(v) = p.flag("--socket") {
        argv.push("--socket".into());
        argv.push(v.into());
    }
    for c in p.flag_all("--command") {
        argv.push("--command".into());
        argv.push(c.clone());
    }
    if p.bool("--no-tui") {
        argv.push("--no-tui".into());
    }
    if let Some(v) = p.flag("--tui") {
        argv.push(format!("tui={v}"));
    }
    inspector_command(&argv);
    Ok(())
}
