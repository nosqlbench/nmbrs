// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Help text rendered from the [`Command`] spec. One source
//! of truth — adding a flag updates `nmbrs <cmd> --help`
//! automatically.

use super::*;

/// Render usage for a command (or one of its subcommands)
/// to stderr. `path` walks deeper into the tree:
/// `render_usage(&root, &["metrics", "list"])` prints help
/// for `nmbrs metrics list`.
pub fn render_usage(root: &Command, path: &[&str]) {
    let mut current = root;
    let mut full_path: Vec<&str> = vec![root.name];
    for seg in path {
        match current.subcommands.iter().find(|s| s.name == *seg) {
            Some(sub) => {
                current = sub;
                full_path.push(seg);
            }
            None => break,
        }
    }
    print_command(current, &full_path);
}

fn print_command(cmd: &Command, full_path: &[&str]) {
    eprintln!("{}", cmd.help);
    eprintln!();
    let invocation = full_path.join(" ");

    if !cmd.subcommands.is_empty() {
        eprintln!("USAGE:");
        eprintln!("  {invocation} <subcommand> [args...]");
        eprintln!();
        eprintln!("SUBCOMMANDS:");
        // Two-column layout: name padded to a stable width
        // computed across all siblings, then the one-line help.
        let name_width = cmd
            .subcommands
            .iter()
            .map(|s| s.name.len())
            .max()
            .unwrap_or(0);
        for sub in &cmd.subcommands {
            let one_line = sub.help.lines().next().unwrap_or("").trim_end();
            eprintln!("  {:<name_width$}  {}", sub.name, one_line);
        }
        eprintln!();
        eprintln!("GLOBAL OPTIONS:");
        eprintln!("  --help, -h     Show usage for the current command path");
        eprintln!("  --version, -V  Print the nmbrs version");
        eprintln!();
        eprintln!(
            "PHYSICAL THREAD POOLS (SRD-102, process-wide; CLI > env > core-count defaults):"
        );
        eprintln!("  --threads.timing=N          timing-pool thread count (default 1)");
        eprintln!("  --threads.io=N              io-pool thread count (default 2)");
        eprintln!("  --threads.workers=N         async worker threads (default cores - 1)");
        eprintln!(
            "  --threads.timing.sched=P    timing sched policy: rr|fifo|nice|none (default rr)"
        );
        eprintln!(
            "  --threads.timing.pin=C      timing core affinity: auto|off|<core> (default auto)"
        );
        eprintln!(
            "  (env equivalents: NMBRS_THREADS_TIMING, _IO, _WORKERS, _TIMING_SCHED, _TIMING_PIN)"
        );
        eprintln!();
        eprintln!("Run `{invocation} <subcommand> --help` for per-subcommand help.");
        return;
    }

    // Leaf: usage line summarises positionals + (any) flags.
    let pos_hint = positionals_hint(&cmd.positionals);
    let flags_hint = if cmd.flags.is_empty() { "" } else { " [flags]" };
    eprintln!("USAGE:");
    eprintln!("  {invocation}{pos_hint}{flags_hint}");
    eprintln!();

    if !cmd.positionals.is_empty() {
        eprintln!("POSITIONAL ARGUMENTS:");
        let name_width = cmd
            .positionals
            .iter()
            .map(|p| p.name.len() + 2)
            .max()
            .unwrap_or(0);
        for p in &cmd.positionals {
            let kind = match p.kind {
                PositionalKind::One => "required",
                PositionalKind::ZeroOrOne => "optional",
                PositionalKind::Many => "0+",
            };
            let label = format!("<{}>", p.name);
            eprintln!("  {label:<name_width$}  ({kind}) {}", p.help);
        }
        eprintln!();
    }

    if !cmd.flags.is_empty() {
        eprintln!("FLAGS:");
        // Compute label width across all flags so help columns
        // align. Label = `--long [aliases…] <value>?`.
        let labels: Vec<String> = cmd.flags.iter().map(flag_label).collect();
        let lw = labels.iter().map(|s| s.len()).max().unwrap_or(0);
        for (f, label) in cmd.flags.iter().zip(labels.iter()) {
            eprintln!("  {label:<lw$}  {}", f.help);
        }
        eprintln!();
    }
}

fn flag_label(f: &Flag) -> String {
    let mut s = f.long.to_string();
    if let Some(short) = f.short {
        s.push_str(&format!(", {short}"));
    }
    if !f.aliases.is_empty() {
        s.push_str(&format!(" ({})", f.aliases.join(", ")));
    }
    if matches!(f.arity, Arity::Value) {
        s.push_str(" <value>");
    }
    s
}

fn positionals_hint(ps: &[Positional]) -> String {
    if ps.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = ps
        .iter()
        .map(|p| match p.kind {
            PositionalKind::One => format!("<{}>", p.name),
            PositionalKind::ZeroOrOne => format!("[<{}>]", p.name),
            PositionalKind::Many => format!("[{}...]", p.name),
        })
        .collect();
    format!(" {}", parts.join(" "))
}
