// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Built-in CPU profiler for nmbrs workloads.
//!
//! Two modes:
//!
//! - **`profiler=flamegraph`** — Rust-native sampling via `pprof` crate.
//!   Fast, no external tools needed, but only captures Rust frames.
//!   Good for profiling Polydat resolution, scope composition, and other
//!   pure-Rust hot paths.
//!
//! - **`profiler=perf`** — Full-system profiling via Linux `perf record`.
//!   Captures all frames including C/C++ libraries (CQL driver, etc.).
//!   Requires `perf` and `inferno` tools installed. Produces an SVG
//!   flamegraph that shows the complete call graph.

use std::collections::HashMap;

/// A running profiler guard.
///
/// `finish()` may be called explicitly to stop profiling and
/// write output (the existing end-of-run path), but is also
/// driven implicitly by [`Drop`] so that early returns and
/// signal-driven shutdowns still flush the flamegraph. Idempotent —
/// calling `finish()` after Drop has already run is a no-op.
pub struct ProfileGuard {
    /// Wrapped in `Option` so `finish` can `take` ownership of
    /// the variants' owned fields (`pprof::ProfilerGuard`, etc.)
    /// while still leaving the struct in a valid state for Drop.
    mode: Option<ProfileMode>,
}

enum ProfileMode {
    #[cfg(feature = "flamegraph")]
    Pprof {
        guard: pprof::ProfilerGuard<'static>,
        output_path: String,
    },
    Perf {
        perf_data: String,
        output_path: String,
        /// Live `perf record` child. Held so we can signal it
        /// directly (SIGINT) rather than chasing the process by
        /// regex with `pkill -f`. Cleared on finish.
        child: Option<std::process::Child>,
        /// Reader thread that pumps perf's stderr line-by-line
        /// into our observer log stream. Joined on finish so the
        /// final lines (perf's "[ perf record: Captured and wrote …]"
        /// summary) appear before profiler output messages.
        stderr_pump: Option<std::thread::JoinHandle<()>>,
    },
}

impl ProfileGuard {
    /// Start profiling if `profiler=flamegraph` or `profiler=perf` is set.
    /// Output files go in `session_dir` if provided, otherwise current directory.
    /// Returns `None` if profiling is not requested or not available.
    pub fn maybe_start(
        params: &HashMap<String, String>,
        session_dir: Option<&std::path::Path>,
    ) -> Option<Self> {
        // Always announce the profiler decision at startup —
        // `(none)` and `off` are quietly handled, but the line
        // tells the operator the runner *saw* (or didn't see)
        // the profiler param. Avoids a silent-failure mode where
        // a typo or missing CLI arg leaves the user staring at
        // an empty session_dir wondering why no flamegraph
        // appeared.
        match params.get("profiler").map(|s| s.as_str()) {
            None => {
                // The default-off case carries no operator-
                // visible value at INFO. Demoted to Debug so
                // it's still discoverable by `--debug` /
                // session.log without dominating the startup
                // banner. The actionable hint (`profiler=perf`
                // / `profiler=flamegraph`) belongs in the
                // CLI help text, not every quiet startup.
                crate::observer::log(
                    crate::observer::LogLevel::Debug,
                    "profiler: off (pass `profiler=perf` or \
                     `profiler=flamegraph` to enable)",
                );
                None
            }
            Some("off") | Some("none") | Some("") => {
                // Explicit-off — the operator typed
                // `profiler=off`, so they already know.
                // Debug-level confirmation is enough.
                crate::observer::log(
                    crate::observer::LogLevel::Debug,
                    "profiler: off (explicitly disabled)",
                );
                None
            }
            Some("flamegraph") => Self::start_pprof(session_dir),
            Some("perf") => Self::start_perf(session_dir, params),
            Some(other) => {
                crate::observer::log(
                    crate::observer::LogLevel::Warn,
                    &format!(
                        "profiler: unknown mode '{other}'; \
                              expected 'perf', 'flamegraph', or 'off'"
                    ),
                );
                None
            }
        }
    }

    fn start_pprof(session_dir: Option<&std::path::Path>) -> Option<Self> {
        #[cfg(not(feature = "flamegraph"))]
        {
            let _ = session_dir;
            crate::observer::log(
                crate::observer::LogLevel::Warn,
                "profiler=flamegraph requested but the 'flamegraph' feature is not enabled. \
                 Rebuild with: cargo build --features flamegraph",
            );
            None
        }

        #[cfg(feature = "flamegraph")]
        {
            match pprof::ProfilerGuardBuilder::default()
                .frequency(997)
                .blocklist(&["libc", "libgcc", "pthread", "vdso"])
                .build()
            {
                Ok(guard) => {
                    let ts = timestamp();
                    let output_path = match session_dir {
                        Some(d) => d.join("flamegraph.svg").to_string_lossy().into_owned(),
                        None => format!("flamegraph-{ts}.svg"),
                    };
                    crate::observer::log(
                        crate::observer::LogLevel::Info,
                        &format!(
                            "profiler: pprof started (997 Hz, Rust frames only), output → {output_path} (rendered via inferno-flamegraph)"
                        ),
                    );
                    Some(Self {
                        mode: Some(ProfileMode::Pprof { guard, output_path }),
                    })
                }
                Err(e) => {
                    crate::observer::log(
                        crate::observer::LogLevel::Warn,
                        &format!("failed to start pprof profiler: {e}"),
                    );
                    crate::observer::log(
                        crate::observer::LogLevel::Warn,
                        "  (requires Linux perf_event_open; try: echo 1 > /proc/sys/kernel/perf_event_paranoid)",
                    );
                    None
                }
            }
        }
    }

    fn start_perf(
        session_dir: Option<&std::path::Path>,
        params: &HashMap<String, String>,
    ) -> Option<Self> {
        // Check that perf is available
        let perf_check = std::process::Command::new("perf").arg("version").output();
        if perf_check.is_err() || !perf_check.as_ref().unwrap().status.success() {
            crate::observer::log(
                crate::observer::LogLevel::Warn,
                "profiler=perf requested but `perf` is not available.",
            );
            crate::observer::log(
                crate::observer::LogLevel::Warn,
                "  Install with: sudo apt install linux-tools-$(uname -r)",
            );
            return None;
        }

        // Resolve the call-graph strategy. `fp` is fast and
        // requires the binary to have frame pointers (workspace
        // [profile.release] sets `force-frame-pointers = true`,
        // so the released `nmbrs` does). `dwarf` is the high-
        // fidelity option — captures inlined frames — but
        // post-processing routes through `addr2line`, which is
        // 5–10× slower per sample on debug-info-rich binaries.
        // `lbr` uses the CPU's Last Branch Record — lowest
        // overhead during capture, limited stack depth.
        let callgraph = params
            .get("profiler_callgraph")
            .map(|s| s.as_str())
            .unwrap_or("fp");
        let callgraph_args: &[&str] = match callgraph {
            "fp" => &["--call-graph", "fp"],
            "dwarf" => &["--call-graph", "dwarf,16384"],
            "lbr" => &["--call-graph", "lbr"],
            other => {
                crate::observer::log(
                    crate::observer::LogLevel::Warn,
                    &format!(
                        "profiler_callgraph='{other}' is not recognized; \
                         expected 'fp' (default), 'dwarf', or 'lbr'. \
                         Falling back to 'fp'."
                    ),
                );
                &["--call-graph", "fp"]
            }
        };

        let ts = timestamp();
        let (perf_data, output_path) = match session_dir {
            Some(d) => (
                d.join("perf.data").to_string_lossy().into_owned(),
                d.join("flamegraph-perf.svg").to_string_lossy().into_owned(),
            ),
            None => (
                format!("perf-{ts}.data"),
                format!("flamegraph-perf-{ts}.svg"),
            ),
        };
        let pid = std::process::id();

        // Spawn perf with stderr piped so its diagnostics flow
        // into our observer log stream rather than into a
        // separate file. Routine status lines (`[ perf record:
        // Captured ... ]`) become Info; anything that looks like
        // an error (`permission denied`, `paranoid`, `error:`,
        // `failed`) gets Warn so it's visible at default verbosity.
        let mut perf_args: Vec<&str> = vec!["record", "-g"];
        perf_args.extend_from_slice(callgraph_args);
        let pid_str = pid.to_string();
        perf_args.extend_from_slice(&["-F", "997", "-p", &pid_str, "-o", &perf_data]);
        let result = std::process::Command::new("perf")
            .args(&perf_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(e) => {
                crate::observer::log(
                    crate::observer::LogLevel::Warn,
                    &format!("profiler: failed to start perf record: {e}"),
                );
                return None;
            }
        };

        // Take the stderr pipe and spawn a pump thread.
        let stderr_pipe = child.stderr.take();
        let stderr_pump = stderr_pipe.and_then(|stderr| {
            std::thread::Builder::new()
                .name("profiler-perf-stderr".into())
                .spawn(move || {
                    use std::io::{BufRead, BufReader};
                    let reader = BufReader::new(stderr);
                    for line in reader.lines() {
                        let Ok(line) = line else { break };
                        let trimmed = line.trim_end();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let level = classify_perf_line(trimmed);
                        crate::observer::log(level, &format!("perf: {trimmed}"));
                    }
                })
                .ok()
        });

        // Verify perf actually came up. spawn() returns Ok the
        // moment fork+exec succeeds, but perf can immediately
        // exit if e.g. perf_event_paranoid blocks the attach.
        // Sleep briefly, then `try_wait` — if the child has
        // already terminated, the pump thread has read whatever
        // perf wrote to stderr and emitted it through the
        // observer log; we just need to surface a clear "no
        // flamegraph" message.
        std::thread::sleep(std::time::Duration::from_millis(200));
        if let Ok(Some(status)) = child.try_wait() {
            // Give the pump a moment to drain the rest of stderr.
            std::thread::sleep(std::time::Duration::from_millis(100));
            crate::observer::log(
                crate::observer::LogLevel::Warn,
                &format!(
                    "profiler: perf record exited immediately ({status}). \
                     No flamegraph will be produced — see the perf: \
                     lines above for the reason. Common causes: paranoid \
                     level too high (`sudo sysctl -w kernel.perf_event_paranoid=1`), \
                     or perf not built with -p PID support."
                ),
            );
            if let Some(h) = stderr_pump {
                let _ = h.join();
            }
            return None;
        }

        crate::observer::log(
            crate::observer::LogLevel::Info,
            &format!(
                "profiler: perf record attached (997 Hz, callgraph={callgraph}), \
                      output → {output_path}"
            ),
        );
        Some(Self {
            mode: Some(ProfileMode::Perf {
                perf_data,
                output_path,
                child: Some(child),
                stderr_pump,
            }),
        })
    }

    /// Stop profiling and write the flamegraph SVG. Idempotent —
    /// safe to call multiple times; the second call is a no-op.
    /// [`Drop`] also calls this, so a process killed mid-run via
    /// SIGINT (or a runner that returns early via `?`) still
    /// produces the flamegraph as long as Drop runs.
    pub fn finish(&mut self) {
        let Some(mode) = self.mode.take() else {
            return;
        };
        match mode {
            #[cfg(feature = "flamegraph")]
            ProfileMode::Pprof { guard, output_path } => match guard.report().build() {
                Ok(report) => {
                    let folded = fold_report(&report);
                    if folded.is_empty() {
                        crate::observer::log(
                            crate::observer::LogLevel::Warn,
                            "profiler: pprof collected no samples — nothing to render",
                        );
                    } else if inferno_flamegraph_available() {
                        render_folded(&folded, &output_path);
                    } else {
                        // No renderer on PATH: keep the folded stacks so the
                        // operator can render them by hand, and say how.
                        let folded_path =
                            output_path.trim_end_matches(".svg").to_string() + ".folded";
                        match std::fs::write(&folded_path, &folded) {
                            Ok(()) => crate::observer::log(
                                crate::observer::LogLevel::Info,
                                &format!(
                                    "profiler: `inferno-flamegraph` not found — wrote folded \
                                     stacks to {folded_path}; render with: inferno-flamegraph \
                                     < {folded_path} > {output_path}  (install: cargo install inferno)"
                                ),
                            ),
                            Err(e) => crate::observer::log(
                                crate::observer::LogLevel::Warn,
                                &format!("profiler: failed to write {folded_path}: {e}"),
                            ),
                        }
                    }
                }
                Err(e) => crate::observer::log(
                    crate::observer::LogLevel::Warn,
                    &format!("profiler: failed to build report: {e}"),
                ),
            },
            ProfileMode::Perf {
                perf_data,
                output_path,
                child,
                stderr_pump,
            } => {
                // Stop perf cleanly. We held the Child handle
                // expressly so we can signal *this* process by
                // PID — no `pkill -f` regex chase, no risk of
                // signalling an unrelated `perf record` the
                // operator started.
                if let Some(mut c) = child {
                    // `perf` only exists on Linux, so this arm is
                    // dead on Windows — but it must still compile
                    // there; substitute a hard kill for the
                    // graceful SIGINT.
                    #[cfg(not(unix))]
                    let killed = if c.kill().is_ok() { 0 } else { -1 };
                    #[cfg(unix)]
                    let killed = {
                        let pid = c.id() as i32;
                        unsafe { libc::kill(pid, libc::SIGINT) }
                    };
                    if killed == 0 {
                        // Wait for perf to flush perf.data and
                        // exit. `wait` is blocking; in finish()
                        // that's fine — we're already in
                        // teardown and want the data flushed
                        // before we read it.
                        match c.wait() {
                            Ok(_status) => {}
                            Err(e) => crate::observer::log(
                                crate::observer::LogLevel::Warn,
                                &format!("profiler: perf record wait failed: {e}"),
                            ),
                        }
                    } else {
                        // Errno is in the global; the child may
                        // have already exited (Ctrl-C in the
                        // terminal sent SIGINT to the whole
                        // process group, including perf).
                        let _ = c.wait();
                    }
                }
                // Join the stderr pump so any final perf output
                // (like `[ perf record: Captured and wrote …]`)
                // appears in the log before profiler progress
                // messages.
                if let Some(h) = stderr_pump {
                    let _ = h.join();
                }

                if !std::path::Path::new(&perf_data).exists() {
                    crate::observer::log(
                        crate::observer::LogLevel::Warn,
                        &format!(
                            "profiler: perf.data not found at {perf_data} \
                                  — perf record may have failed; check the \
                                  perf: log lines above"
                        ),
                    );
                    return;
                }

                // Try inferno pipeline: perf script | collapse | flamegraph
                let has_inferno = std::process::Command::new("inferno-collapse-perf")
                    .arg("--help")
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);

                if has_inferno {
                    let perf_data_size =
                        std::fs::metadata(&perf_data).map(|m| m.len()).unwrap_or(0);
                    let mb = perf_data_size as f64 / (1024.0 * 1024.0);
                    crate::observer::log(
                        crate::observer::LogLevel::Info,
                        &format!(
                            "profiler: post-processing {perf_data} ({mb:.1} MB) with \
                             inferno — perf script + collapse can take 30s–2min on \
                             dwarf call-graphs. Stages will log as they complete."
                        ),
                    );

                    // Stage 1: `perf script` reads perf.data and emits text.
                    // This is usually the longest single stage.
                    let t0 = std::time::Instant::now();
                    crate::observer::log(
                        crate::observer::LogLevel::Info,
                        "profiler: stage 1/3: running `perf script` (decoding perf.data)...",
                    );
                    let script = std::process::Command::new("perf")
                        .args(["script", "-i", &perf_data])
                        .output();

                    if let Ok(script_output) = script {
                        let script_lines =
                            script_output.stdout.iter().filter(|&&b| b == b'\n').count();
                        crate::observer::log(
                            crate::observer::LogLevel::Info,
                            &format!(
                                "profiler:   `perf script` done in {:.1}s ({script_lines} lines)",
                                t0.elapsed().as_secs_f64()
                            ),
                        );

                        // Stage 2: collapse — parses every line, builds the
                        // folded stack format. CPU-bound, single-threaded.
                        let t1 = std::time::Instant::now();
                        crate::observer::log(
                            crate::observer::LogLevel::Info,
                            "profiler: stage 2/3: running `inferno-collapse-perf`...",
                        );
                        let collapse = std::process::Command::new("inferno-collapse-perf")
                            .stdin(std::process::Stdio::piped())
                            .stdout(std::process::Stdio::piped())
                            .spawn();

                        if let Ok(mut collapse_proc) = collapse {
                            use std::io::Write;
                            if let Some(ref mut stdin) = collapse_proc.stdin {
                                let _ = stdin.write_all(&script_output.stdout);
                            }
                            if let Ok(collapsed) = collapse_proc.wait_with_output() {
                                crate::observer::log(
                                    crate::observer::LogLevel::Info,
                                    &format!(
                                        "profiler:   collapse done in {:.1}s",
                                        t1.elapsed().as_secs_f64()
                                    ),
                                );
                                render_folded(&collapsed.stdout, &output_path);
                            }
                        }
                    }
                } else {
                    // No inferno — leave perf.data for manual processing
                    crate::observer::log(
                        crate::observer::LogLevel::Info,
                        &format!("profiler: perf data saved to {perf_data}"),
                    );
                    crate::observer::log(
                        crate::observer::LogLevel::Info,
                        "  To generate flamegraph manually:",
                    );
                    crate::observer::log(
                        crate::observer::LogLevel::Info,
                        &format!(
                            "    perf script -i {perf_data} | inferno-collapse-perf | inferno-flamegraph > {output_path}"
                        ),
                    );
                    crate::observer::log(
                        crate::observer::LogLevel::Info,
                        "  Install inferno: cargo install inferno",
                    );
                }

                // Clean up perf.data (it can be large)
                // Keep it if inferno wasn't available (user needs it)
                if has_inferno {
                    let _ = std::fs::remove_file(&perf_data);
                }
            }
        }
    }
}

impl Drop for ProfileGuard {
    /// Catch-all flush: if `finish()` wasn't called explicitly
    /// (early return, SIGINT-triggered graceful shutdown, panic
    /// unwind), Drop runs the same code path so the flamegraph
    /// SVG still lands. Calling this *after* a successful
    /// `finish()` is a cheap no-op (`mode` is `None`).
    fn drop(&mut self) {
        self.finish();
    }
}

/// Whether the `inferno-flamegraph` renderer is on `PATH`. The perf
/// path probes `inferno-collapse-perf` instead, since it needs both.
#[cfg(feature = "flamegraph")]
fn inferno_flamegraph_available() -> bool {
    std::process::Command::new("inferno-flamegraph")
        .arg("--help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Fold a pprof report into inferno's collapsed-stack text: one line
/// per distinct stack, `thread;root;…;leaf count`, root first — the
/// same shape `inferno-collapse-perf` emits for the perf path, so both
/// profilers feed one renderer and one summarizer.
#[cfg(feature = "flamegraph")]
fn fold_report(report: &pprof::Report) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (frames, count) in &report.data {
        let mut line = frames.thread_name_or_id();
        line.push(';');
        for frame in frames.frames.iter().rev() {
            for symbol in frame.iter().rev() {
                let _ = write!(line, "{symbol};");
            }
        }
        line.pop();
        let _ = writeln!(line, " {count}");
        out.push_str(&line);
    }
    out.into_bytes()
}

/// Render folded stacks to the flamegraph SVG at `output_path` via
/// `inferno-flamegraph`, then write the companion top-N markdown
/// summary beside it (`.svg` → `.md`). Both artifacts are linked into
/// the session on success. Shared by the pprof and perf profilers.
fn render_folded(folded: &[u8], output_path: &str) {
    use std::io::Write as _;
    let t2 = std::time::Instant::now();
    crate::observer::log(
        crate::observer::LogLevel::Info,
        "profiler: rendering `inferno-flamegraph` SVG...",
    );
    let flamegraph = std::process::Command::new("inferno-flamegraph")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn();
    match flamegraph {
        Ok(mut fg_proc) => {
            if let Some(ref mut stdin) = fg_proc.stdin {
                let _ = stdin.write_all(folded);
            }
            match fg_proc.wait_with_output() {
                Ok(svg_output) if svg_output.status.success() => {
                    if let Err(e) = std::fs::write(output_path, &svg_output.stdout) {
                        crate::observer::log(
                            crate::observer::LogLevel::Warn,
                            &format!("profiler: failed to write {output_path}: {e}"),
                        );
                    } else {
                        let size_kb = svg_output.stdout.len() / 1024;
                        crate::observer::log(
                            crate::observer::LogLevel::Info,
                            &format!(
                                "profiler:   wrote {output_path} ({size_kb} KB) in {:.1}s",
                                t2.elapsed().as_secs_f64()
                            ),
                        );
                        // Convenience symlink only after the write
                        // succeeded — Session::new intentionally doesn't
                        // pre-create optional-artifact links so they never
                        // dangle when a run skips profiling.
                        if let Some(name) = std::path::Path::new(output_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                        {
                            crate::session::Session::link_artifact(name);
                        }
                    }
                }
                Ok(svg_output) => crate::observer::log(
                    crate::observer::LogLevel::Warn,
                    &format!(
                        "profiler: `inferno-flamegraph` exited with {}: {}",
                        svg_output.status,
                        String::from_utf8_lossy(&svg_output.stderr).trim()
                    ),
                ),
                Err(e) => crate::observer::log(
                    crate::observer::LogLevel::Warn,
                    &format!("profiler: `inferno-flamegraph` failed: {e}"),
                ),
            }
        }
        Err(e) => crate::observer::log(
            crate::observer::LogLevel::Warn,
            &format!("profiler: could not spawn `inferno-flamegraph`: {e}"),
        ),
    }

    // Markdown summary (top-N tables) — same folded input, parsed
    // in-process. Companion file alongside the SVG, swap .svg → .md.
    let md_path = output_path.trim_end_matches(".svg").to_string() + ".md";
    let companion = std::path::Path::new(output_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| output_path.to_string());
    let md = summarize_folded(folded, 20, &companion);
    match std::fs::write(&md_path, md.as_bytes()) {
        Ok(()) => {
            crate::observer::log(
                crate::observer::LogLevel::Info,
                &format!("profiler: wrote {md_path} (top-20 self/inclusive)"),
            );
            if let Some(name) = std::path::Path::new(&md_path)
                .file_name()
                .and_then(|n| n.to_str())
            {
                crate::session::Session::link_artifact(name);
            }
        }
        Err(e) => crate::observer::log(
            crate::observer::LogLevel::Warn,
            &format!("profiler: failed to write {md_path}: {e}"),
        ),
    }
}

/// Produce a markdown summary from inferno's "folded" stack
/// format (`frame1;frame2;...;leaf count` per line).
///
/// Two ranked tables are emitted:
///
/// - **Self time** — the sum of sample counts for each frame
///   *as the bottom of the stack*. This is "where the CPU was
///   when sampled," which is the most direct read of where time
///   was actually spent.
/// - **Inclusive time** — the sum of sample counts for stacks
///   *containing* each frame anywhere. This is "what callers
///   are dragging in cost," useful for spotting hot subsystems
///   that don't appear at the leaf because the leaf is in a
///   library you can't change.
///
/// `top_n` caps each table; the empirical sweet spot is 20.
/// `[unknown]` and bracket-quoted module names are kept (they
/// surface unresolved-symbol cliffs explicitly rather than
/// silently disappearing).
fn summarize_folded(folded: &[u8], top_n: usize, companion: &str) -> String {
    use std::collections::HashMap;
    use std::fmt::Write as _;

    let folded_str = match std::str::from_utf8(folded) {
        Ok(s) => s,
        Err(_) => return "(profiler: folded output was not valid UTF-8)\n".into(),
    };

    let mut self_counts: HashMap<&str, u64> = HashMap::new();
    let mut inclusive_counts: HashMap<&str, u64> = HashMap::new();
    let mut total_samples: u64 = 0;

    for line in folded_str.lines() {
        // Each folded line: `frame1;frame2;...;leaf count`. The
        // count is whitespace-separated from the stack and is the
        // last token; parse from the right.
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let Some(sp) = line.rfind(' ') else {
            continue;
        };
        let (stack, count_str) = (&line[..sp], &line[sp + 1..]);
        let Ok(count) = count_str.parse::<u64>() else {
            continue;
        };
        total_samples = total_samples.saturating_add(count);

        // Inclusive: every distinct frame in the stack (deduped).
        let mut seen: Vec<&str> = stack.split(';').collect();
        seen.sort();
        seen.dedup();
        for frame in &seen {
            *inclusive_counts.entry(*frame).or_insert(0) += count;
        }

        // Self: just the leaf.
        if let Some(leaf) = stack.split(';').next_back() {
            *self_counts.entry(leaf).or_insert(0) += count;
        }
    }

    if total_samples == 0 {
        return "(profiler: no samples in folded output)\n".into();
    }

    let mut self_ranked: Vec<(&str, u64)> = self_counts.into_iter().collect();
    self_ranked.sort_by(|a, b| b.1.cmp(&a.1));
    self_ranked.truncate(top_n);

    let mut incl_ranked: Vec<(&str, u64)> = inclusive_counts.into_iter().collect();
    incl_ranked.sort_by(|a, b| b.1.cmp(&a.1));
    incl_ranked.truncate(top_n);

    let mut out = String::new();
    out.push_str("# Profiler summary\n\n");
    out.push_str(&format!("- total samples: **{total_samples}**\n"));
    let _ = writeln!(out, "- companion artefact: `{companion}`");
    out.push_str("- self time = leaf-frame samples (where CPU was);\n");
    out.push_str("  inclusive time = stacks containing the frame anywhere\n\n");

    out.push_str(&format!("## Top {} by self time\n\n", self_ranked.len()));
    out.push_str("| % | samples | frame |\n");
    out.push_str("|--:|--------:|:------|\n");
    for (frame, count) in &self_ranked {
        let pct = 100.0 * (*count as f64) / (total_samples as f64);
        out.push_str(&format!(
            "| {pct:.2} | {count} | `{}` |\n",
            md_escape(frame)
        ));
    }
    out.push('\n');

    out.push_str(&format!(
        "## Top {} by inclusive time\n\n",
        incl_ranked.len()
    ));
    out.push_str("| % | samples | frame |\n");
    out.push_str("|--:|--------:|:------|\n");
    for (frame, count) in &incl_ranked {
        let pct = 100.0 * (*count as f64) / (total_samples as f64);
        out.push_str(&format!(
            "| {pct:.2} | {count} | `{}` |\n",
            md_escape(frame)
        ));
    }
    out.push('\n');

    out
}

/// Escape `|` so a frame name with a pipe character doesn't
/// break the markdown table.
fn md_escape(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Decide which observer log level a single perf-stderr line
/// deserves. Errors and permission failures get `Warn` so they
/// reach default-verbosity output; anything else (status banners
/// like `[ perf record: Woken up N times … ]`) is `Info`.
fn classify_perf_line(line: &str) -> crate::observer::LogLevel {
    let lc = line.to_ascii_lowercase();
    let warn_markers = [
        "error",
        "fail",
        "denied",
        "no permission",
        "paranoid",
        "cannot",
        "unable to",
        "not found",
        "warning",
    ];
    if warn_markers.iter().any(|m| lc.contains(m)) {
        crate::observer::LogLevel::Warn
    } else {
        crate::observer::LogLevel::Info
    }
}

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_folded_basic() {
        let folded = b"\
            a;b;c 10\n\
            a;b;d 20\n\
            a;e 30\n\
            f 5\n";
        let md = summarize_folded(folded, 10, "flamegraph.svg");
        assert!(md.contains("total samples: **65**"));
        // Self time leaders: e=30, d=20, c=10, f=5
        assert!(md.contains("`e`"), "self table should rank `e`: {md}");
        assert!(md.contains("`d`"));
        assert!(md.contains("`c`"));
        assert!(md.contains("`f`"));
        // Inclusive time leader: `a` (10+20+30 = 60).
        let incl_section = md.split("## Top").nth(2).expect("two sections");
        let a_line = incl_section
            .lines()
            .find(|l| l.contains("`a`"))
            .expect("`a` row");
        assert!(
            a_line.contains("60"),
            "`a` inclusive count should be 60: {a_line}"
        );
    }

    #[test]
    fn summarize_folded_handles_empty() {
        assert!(summarize_folded(b"", 10, "flamegraph.svg").contains("no samples"));
    }

    #[test]
    fn summarize_folded_escapes_pipes_in_frame_names() {
        let folded = b"frame|with|pipes 7\n";
        let md = summarize_folded(folded, 5, "flamegraph.svg");
        assert!(md.contains("frame\\|with\\|pipes"));
    }
}
