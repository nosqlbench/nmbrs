// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The `check` subcommand: run a workload — or every workload under a directory
//! — and verify its output against the rules it declares (`#@` comments or a
//! `verify:` block). Exits non-zero on any failure, so it drops into CI. The
//! verification logic lives in [`nmbrs_workload::verify`], shared with the
//! example-walker test, so users check their own workloads exactly the way CI
//! checks the bundled examples.
//!
//! On a terminal it renders a live `active · pending · done · errors` status
//! line as the (concurrent) checks run, then prints a summary and the slowest
//! workloads by wall-clock.

use crate::cli_spec::{Category, Command, Handler, Level, ParsedCommand};
use nmbrs_workload::verify::{CheckProgress, CheckStatus, WorkloadTiming};
use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub fn spec() -> Command {
    Command {
        name: "check",
        help: "Run a workload and verify its output against the `#@` / `verify:`\n\
               rules it declares. Accepts a file, a directory (checks every\n\
               workload under it), or a bundled catalog name — the same\n\
               reference `nmbrs run` takes. Non-zero exit on failure.",
        category: Category::Workloads,
        level: Level::Workload,
        flags: Vec::new(),
        kv_params: crate::completion::WORKLOAD_KV,
        dynamic_options: None,
        positionals: vec![crate::cli_spec::Positional {
            name: "workload",
            help: "Workload file, directory, or bundled catalog name to verify \
                   (or `workload=<ref>`).",
            kind: crate::cli_spec::PositionalKind::ZeroOrOne,
            value: crate::completion::WORKLOAD_VALUE,
        }],
        subcommands: Vec::new(),
        handler: Some(Handler::Sync(handle)),
        raw_args: true,
        completion_override: None,
    }
}

fn handle(p: ParsedCommand) -> Result<(), String> {
    check_command(&p.raw)
}

fn check_command(args: &[String]) -> Result<(), String> {
    // Closed surface: `check` takes one target and nothing else. Before
    // this guard, any `--flag` fell into the positional search and became
    // the WORKLOAD NAME (`nmbrs check --strict` → "no such workload
    // '--strict'"), and unknown `key=` forms were silently dropped. The
    // session-lifecycle family stays legal — the startup purge hook
    // consumes it globally.
    let session_ok = |k: &str| {
        k == "session"
            || k.starts_with("session-")
            || k.starts_with("sessions-")
            || k == "session_path"
    };
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if let Some(flag) = a.strip_prefix("--") {
            let key = flag.split('=').next().unwrap_or(flag);
            if session_ok(key) {
                if !flag.contains('=') {
                    skip_next = true;
                }
                continue;
            }
            return Err(format!(
                "unknown option '--{key}'. `nmbrs check` takes one target:                  workload=<file|dir|name> or a bare <file|dir|name>."
            ));
        }
        if let Some((key, _)) = a.split_once('=')
            && key != "workload"
            && !session_ok(key)
        {
            return Err(format!(
                "unknown parameter '{key}='. `nmbrs check` takes one target:                  workload=<file|dir|name> or a bare <file|dir|name>."
            ));
        }
    }
    // Target from `workload=<ref>` or the first bare positional — a file, a
    // directory, or a bundled catalog name, resolved by the verifier exactly
    // the way `nmbrs run` resolves `workload=…`.
    let target = args
        .iter()
        .find_map(|a| a.strip_prefix("workload=").map(String::from))
        .or_else(|| {
            args.iter()
                .find(|a| !a.contains('=') && !a.starts_with('-'))
                .cloned()
        })
        .ok_or("usage: nmbrs check workload=<file|dir|name>  (or: nmbrs check <file|dir|name>)")?;

    let binary =
        std::env::current_exe().map_err(|e| format!("cannot locate the nmbrs binary: {e}"))?;
    let sandbox = std::env::temp_dir().join(format!("nmbrs-check-{}", std::process::id()));

    // Live progress: a TTY-only status line redrawn as each workload starts
    // and finishes. Worker threads in `verify_path` drive it concurrently, so
    // the view is atomic + write-serialized.
    let view = ProgressView::new();
    let sum =
        nmbrs_workload::verify::verify_target(&binary, &target, &sandbox, &|ev| view.handle(ev));
    view.clear();

    for s in &sum.skipped {
        println!("  ⃠  skip  {s}");
    }
    let total = sum.passed + sum.failures.len();
    let result = if sum.failures.is_empty() {
        let skip = if sum.skipped.is_empty() {
            String::new()
        } else {
            format!(", {} skipped", sum.skipped.len())
        };
        println!(
            "✓ {} check{} passed{skip}",
            sum.passed,
            if sum.passed == 1 { "" } else { "s" }
        );
        Ok(())
    } else {
        for f in &sum.failures {
            eprintln!("  ✗  {f}");
        }
        Err(format!("{} of {total} checks failed", sum.failures.len()))
    };

    print_timing_report(&sum.timings);
    result
}

/// Print the slowest workloads by wall-clock. Skipped for a single workload —
/// the one timing carries no comparison and the summary line says enough.
fn print_timing_report(timings: &[WorkloadTiming]) {
    if timings.len() < 2 {
        return;
    }
    let mut ranked: Vec<&WorkloadTiming> = timings.iter().collect();
    ranked.sort_by(|a, b| b.elapsed.cmp(&a.elapsed));
    let shown = ranked.len().min(10);
    let wall: Duration = timings.iter().map(|t| t.elapsed).sum();
    println!(
        "\ntop {shown} by time taken ({} workloads, {} total):",
        timings.len(),
        fmt_dur(wall),
    );
    for t in ranked.into_iter().take(10) {
        println!(
            "  {:>9}  {} {}",
            fmt_dur(t.elapsed),
            status_mark(t.status),
            t.label
        );
    }
}

fn status_mark(s: CheckStatus) -> char {
    match s {
        CheckStatus::Pass => '✓',
        CheckStatus::Skip => '⃠',
        CheckStatus::Fail => '✗',
    }
}

fn fmt_dur(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 1.0 {
        format!("{secs:.2}s")
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// Atomic, write-serialized backing for the live `nmbrs check` status line.
struct ProgressView {
    total: AtomicUsize,
    started: AtomicUsize,
    finished: AtomicUsize,
    errors: AtomicUsize,
    tty: bool,
    write: Mutex<()>,
}

impl ProgressView {
    fn new() -> Self {
        Self {
            total: AtomicUsize::new(0),
            started: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            errors: AtomicUsize::new(0),
            tty: std::io::stderr().is_terminal(),
            write: Mutex::new(()),
        }
    }

    fn handle(&self, ev: CheckProgress) {
        match ev {
            CheckProgress::Begin { total } => {
                self.total.store(total, Ordering::Relaxed);
            }
            CheckProgress::Started { .. } => {
                self.started.fetch_add(1, Ordering::Relaxed);
            }
            CheckProgress::Finished { status, .. } => {
                self.finished.fetch_add(1, Ordering::Relaxed);
                if status == CheckStatus::Fail {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.render();
    }

    fn render(&self) {
        if !self.tty {
            return;
        }
        let _g = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let total = self.total.load(Ordering::Relaxed);
        let started = self.started.load(Ordering::Relaxed);
        let finished = self.finished.load(Ordering::Relaxed);
        let errors = self.errors.load(Ordering::Relaxed);
        let active = started.saturating_sub(finished);
        let pending = total.saturating_sub(started);
        let mut err = std::io::stderr();
        let _ = write!(
            err,
            "\r\x1b[K⏳ {active} active · {pending} pending · {finished} done · {errors} errors",
        );
        let _ = err.flush();
    }

    /// Erase the status line so the summary starts on a clean row.
    fn clear(&self) {
        if !self.tty {
            return;
        }
        let _g = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut err = std::io::stderr();
        let _ = write!(err, "\r\x1b[K");
        let _ = err.flush();
    }
}
