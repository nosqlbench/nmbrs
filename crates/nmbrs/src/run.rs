// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The `run` subcommand: workload execution with optional TUI.
//!
//! nmbrs registers its adapters (stdout, http, testkit, plotter,
//! cql/scylla, optionally cassandra-cpp) via inventory at link
//! time. This module decides between two execution paths:
//!
//! - **TUI mode** — stderr is a TTY, no `dryrun=`, the adapter
//!   doesn't claim raw terminal. Builds an
//!   [`nmbrs_tui::observer::TuiObserver`], runs via
//!   [`nmbrs_runtime::runner::run_with_observer`], prints a
//!   post-teardown summary, and exits 2 if any pre-mapped
//!   phases were skipped.
//! - **Plain mode** — stderr is not a TTY, or `tui=off`, or a
//!   raw-terminal adapter (e.g. `plotter`). Falls through to
//!   `runner::run` with the default stderr observer.
//!
//! `tui=on|off` overrides auto-detection.

// Link adapter crates for inventory registration.
extern crate nmbrs_adapter_http;
extern crate nmbrs_adapter_plotter;
extern crate nmbrs_adapter_stdout;
extern crate nmbrs_adapter_testkit;
// CQL adapter — `default-features = false` in Cargo.toml; nmbrs's
// own engine-* features forward into it. The always-on `common`
// module registers `adapter=cql`; the engine modules contribute
// `DriverImpl`s selected at runtime via `cqldriver=`.
extern crate nmbrs_adapter_cql;

// SRD-86 — link the optimizer plugin crate so its `runtime`-feature inventory
// bridge (one `OptimizerRegistration` per optimizer) is included in the
// binary and discovered by the core contract's registry / `nmbrs describe
// optimizers`. Same force-link mechanism as the adapters above.
extern crate nmbrs_optimizers;

// SRD-86 §"The metric-reader surface" — force-link nmbrs-metricsql so its
// `polydat-nodes`-feature inventory registration (`metricsql`,
// `metricsql_scalar`, `metricsql_vector`, `metricsql_window`) is included.
// The binary uses the engine (the `metrics query` CLI) but not these node
// symbols directly, so pull them in explicitly like the plugins above.
extern crate nmbrs_metricsql;

use std::sync::Arc;

use nmbrs_metrics::cadence::Cadences;
use nmbrs_tui::observer::{TuiObserver, print_post_run_summary, unreached_phase_exit_code};
use nmbrs_tui::run_state_actor::{RunStateCmd, spawn_run_state_actor};
use nmbrs_tui::state::RunState;

/// Resolve a log-level floor (SRD-41) from the effective params (workload
/// defaults overlaid by the CLI) under the given key aliases, falling back to
/// an environment variable, then to the caller's default. Precedence:
/// CLI > workload `params:` > env > default. Returns `None` when nothing is
/// set so the caller supplies the built-in default.
fn resolve_log_level(
    params: &std::collections::HashMap<String, String>,
    keys: &[&str],
    env_var: &str,
) -> Option<nmbrs_runtime::observer::LogLevel> {
    keys.iter()
        .find_map(|k| params.get(*k))
        .and_then(|s| nmbrs_runtime::runner::parse_log_level(s))
        .or_else(|| {
            std::env::var(env_var)
                .ok()
                .and_then(|s| nmbrs_runtime::runner::parse_log_level(&s))
        })
}

pub async fn run_command(args: &[String]) {
    // Parse only `key=value` and workload-file args for mode
    // detection. Skip the `run` subcommand token itself.
    let param_args: Vec<String> = args
        .iter()
        .filter(|a| a.contains('=') || a.ends_with(".yaml") || a.ends_with(".yml"))
        .cloned()
        .collect();
    let params = nmbrs_runtime::runner::parse_params(&param_args);
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    // Every `dryrun=...` value is a first-class dryrun. They split
    // into two operational shapes that ask different things of the
    // surface:
    //
    //   - **cycle-running**: `cycle`, `full`, and the adapter-
    //     substitution modes `emit` / `silent` / `json`. The walker
    //     runs the full per-cycle pipeline (bind-point eval, kernel
    //     pulls, dataset reads, wrapper stack) and short-circuits
    //     only the outbound adapter call. These runs *need* the
    //     LogOnlySink so the inline `phase_status` line ticks and
    //     the operator can see preparation work happening.
    //
    //   - **early-exit display modes**: `phase`, `op`, `controls`.
    //     The walker exits before running any cycles, dumps a
    //     one-shot structural report to stdout, and is done. The
    //     LogOnlySink's managed bottom region would only interleave
    //     with that dump and break piping (`| less`, redirect into
    //     another tool).
    //
    // `dryrun=labels` and `dryrun=wiring` are *output filter*
    // sub-flags that ride on whichever execution depth was selected;
    // they don't drive this decision themselves.
    let dryrun_runs_cycles = params
        .get("dryrun")
        .map(|s| {
            let cfg = nmbrs_runtime::runner::DiagnosticConfig::parse(s);
            cfg.depth >= nmbrs_runtime::runner::ExecDepth::Cycle
        })
        .unwrap_or(false);
    let dryrun_is_early_exit = params.contains_key("dryrun") && !dryrun_runs_cycles;

    // Adapters that need raw terminal output (e.g. plotter)
    // override TUI detection — checked at startup before any
    // adapter is constructed.
    let has_workload = params.contains_key("workload")
        || param_args
            .iter()
            .any(|a| (a.ends_with(".yaml") || a.ends_with(".yml")) && !a.contains('='));
    // SRD-41/87 console-ownership: a console-owning adapter (plotter,
    // stdout-to-terminal) may be declared in the WORKLOAD's `params:` block,
    // not just on the CLI. The run's EFFECTIVE params — the workload's
    // declared `params:` (extends-merged) overlaid by CLI args (CLI wins) —
    // are the single consolidated set the runner also uses for session
    // services; computing them here lets the display preference see the
    // adapter and its shaping keys (e.g. stdout's `filename`) up front.
    let effective_params = nmbrs_runtime::runner::effective_params(&param_args);
    // CLI adapter wins; else the workload's declared adapter.
    let resolved_adapter: Option<String> = params
        .get("adapter")
        .or(params.get("driver"))
        .or_else(|| {
            effective_params
                .get("adapter")
                .or(effective_params.get("driver"))
        })
        .cloned();
    let adapter_name = resolved_adapter
        .clone()
        .unwrap_or_else(|| "stdout".to_string());
    // Console-ownership (the adapter writes its own output to the terminal, so
    // the dashboard must yield) is a property of a console-owning adapter
    // (stdout-to-terminal, plotter) declared on the CLI *or* in the workload,
    // or the implicit `stdout` default of an inline-op run. A workload run
    // with NO declared adapter is NOT console-owning — it gets the normal
    // dashboard (the `None if has_workload` arm); without that a bare
    // `workload=…` would default to `stdout`, reserve the console, and
    // suppress the ENTIRE run display.
    let adapter_pref = match resolved_adapter.as_deref() {
        Some(a) => nmbrs_runtime::adapter::adapter_display_preference(a, &effective_params),
        None if has_workload => nmbrs_runtime::adapter::DisplayPreference::Auto,
        None => nmbrs_runtime::adapter::adapter_display_preference("stdout", &effective_params),
    };

    // A console-owning adapter (stdout-to-terminal, plotter) on an
    // INTERACTIVE terminal owns the screen: stdout and stderr both land
    // on it, so any diagnostic on stderr interleaves with the adapter's
    // own output. In that case the console is reserved for the adapter
    // and ALL nmbrs system signals — the in-run sink stream AND the
    // post-run summary — go to `session.log` only. Non-TTY (pipes/CI)
    // keeps diagnostics on stderr (a separate stream); `dryrun=` always
    // shows since its output IS the requested result.
    let silent_console = adapter_pref == nmbrs_runtime::adapter::DisplayPreference::Off
        && is_tty
        && !params.contains_key("dryrun");
    // SRD-87: install the op-output channel for this run's context. A
    // console-owning adapter (`silent_console`) and a piped run both own a raw
    // stdout surface; an interactive dashboard routes op output through the
    // live display so it composites without the raw-mode staircase. Replaces
    // the prior `console_reserved_for_adapter` global flag that `op_output`
    // consulted inline — the selected impl now *is* the routing decision.
    nmbrs_runtime::output_channel::install(nmbrs_runtime::output_channel::select(
        silent_console,
        is_tty,
    ));

    // Three-mode lattice. Default is `terminal` for interactive
    // sessions: line-mode rendering driven by the snapshot stream
    // (see `nmbrs_tui::log_only_sink`). `on` is explicit opt-in
    // for the full raw-mode TUI. `off` strips the sink entirely
    // — used when an adapter needs unfettered terminal access
    // (plotter, anything writing cursor controls of its own) or
    // when the operator wants bare stderr output for piping/CI.
    //
    // Adapter override: if the adapter declares
    // `DisplayPreference::Off`, the mode collapses to `off`
    // regardless of what the user asked for, with a log line
    // explaining the override.
    let user_tui = params.get("tui").map(|s| s.as_str());
    let tui_mode: &str = if adapter_pref == nmbrs_runtime::adapter::DisplayPreference::Off {
        if let Some(req) = user_tui
            && req != "off"
        {
            nmbrs_runtime::diag!(
                nmbrs_runtime::observer::LogLevel::Warn,
                "display: adapter '{adapter_name}' writes its own output to the \
                 terminal — forcing tui=off (overriding tui={req}) so the dashboard \
                 doesn't overwrite it; run detail is still captured in the log"
            );
        }
        "off"
    } else if let Some(req) = user_tui {
        req
    } else if is_tty && !dryrun_is_early_exit {
        // Cycle-running dryruns (`cycle`, `full`, `emit`,
        // `silent`, `json`) take this branch — they need the
        // LogOnlySink so the inline `phase_status` line is
        // actually rendered. Early-exit display dryruns
        // (`phase`, `op`, `controls`) fall through to `off`.
        "terminal"
    } else if !dryrun_is_early_exit {
        // Stderr is NOT a tty (piped / redirected) but this
        // is still a cycle-running session. Emit periodic
        // status snapshots via the FormattedLineSink so a
        // long-running workload tailing its log shows forward
        // progress, not just per-event log lines. No cursor
        // positioning, no escapes — append-only.
        "formatted"
    } else {
        "off"
    };

    // Register `watch=` phase-end triggers before the actor
    // spawn so any registration warnings land in the
    // expected place in the log stream. The trigger worker
    // is process-global and lazy-spawned on the first
    // registration; nothing to clean up here.
    if let Some(watch_spec) = params.get("watch") {
        let specs = crate::watch_trigger::split_watch_param(watch_spec);
        let _ids = crate::watch_trigger::register_watch_triggers(&specs);
    }

    // Spawn the RunState actor + inspector socket *before* the
    // tui-mode branch. The actor was originally TUI-only and the
    // inspector got tied to it by accident — but the high-value
    // inspector commands (`controls`/`set`/`metrics`/`metric`/
    // `:pin`) walk the live component tree via
    // `runtime_context::session_root_handle()` and don't need a
    // populated RunState. Lifting the spawn above the branch lets
    // `nmbrs attach` reach a tui=off run for those commands too.
    // Legacy commands (`meta`/`phases`/`active`/`latency`/`tree`/
    // `log`) read the RunState; in tui=off mode the actor is
    // unpopulated and they return empty/defaults. The TUI thread
    // is still NOT spawned here — the observer starts it lazily
    // on the first `phase_starting` event so pre-phase failures
    // leave the terminal untouched.
    let (run_state, run_state_join) = spawn_run_state_actor(RunState::new(
        params.get("workload").map(|s| s.as_str()).unwrap_or("?"),
        params
            .get("scenario")
            .map(|s| s.as_str())
            .unwrap_or("default"),
        params
            .get("adapter")
            .or(params.get("driver"))
            .map(|s| s.as_str())
            .unwrap_or(&adapter_name),
    ));
    run_state.send(RunStateCmd::SetMeta {
        profiler: Some(
            params
                .get("profiler")
                .cloned()
                .unwrap_or_else(|| "off".into()),
        ),
        limit: Some(
            params
                .get("limit")
                .cloned()
                .unwrap_or_else(|| "none".into()),
        ),
    });

    // Capture the current tokio runtime handle so the inspector
    // server thread (a sync OS thread, not a tokio worker) can
    // dispatch async control writes via `handle.block_on(...)`
    // when an inspector client issues `set <name> <value>`. The
    // block_on runs on the per-connection thread, never on a
    // runtime worker, so no executor starvation. Bind failures
    // (read-only fs, socket name collision) don't abort the run;
    // the inspector just stays disabled with a warning.
    let runtime_handle = tokio::runtime::Handle::try_current().ok();
    // The inspector socket is the out-of-band endpoint for
    // `nmbrs attach`; the in-process run (TUI, observer, RunState) never
    // reads it. Most runs are never attached to, so it's OFF by default
    // — a per-run socket plus an announcement line is just noise. Opt in
    // with `inspector=on` when you intend to attach to a long workload.
    let inspector_enabled = params
        .get("inspector")
        .map(|v| matches!(v.as_str(), "on" | "true" | "1"))
        .unwrap_or(false);
    let _inspector_join = if inspector_enabled {
        match nmbrs_tui::inspector_server::spawn(run_state.clone(), runtime_handle.clone()) {
            Ok((_path, join)) => Some(join),
            Err(e) => {
                nmbrs_runtime::diag!(
                    nmbrs_runtime::observer::LogLevel::Warn,
                    "inspector endpoint disabled: {e}"
                );
                None
            }
        }
    } else {
        None
    };

    if tui_mode != "on" {
        // Two non-`on` modes: `terminal` runs a `LogOnlySink`
        // against the snapshot stream; `off` skips the sink
        // entirely (no rendering layer between the observer's
        // direct stderr writes and the user's terminal — used
        // by adapters that own the terminal themselves, like
        // plotter, and for piped/CI output).
        //
        // Both share the `LogOnlyObserver` since the observer's
        // job is just "send commands to the actor"; whether
        // anything renders from those commands is the sink's
        // call. The `sink_active` flag coordinates the handoff:
        // if no sink is up, the observer writes stderr
        // synchronously (legacy behaviour); when the sink
        // claims rendering, the observer suppresses its writes.
        let stripped: &[String] = match args.first().map(|s| s.as_str()) {
            Some("run") => &args[1..],
            _ => args,
        };
        let cli_params = nmbrs_runtime::runner::parse_params(stripped);
        // dryrun=phase walks the scenario tree purely to dump the
        // plan; the per-phase construction trace ("=== phase: X ===",
        // "phase 'X' (...): N op templates …", "phase 'X' complete")
        // is signal during a real run but pure noise when the user
        // just wants the post-run plan view. Default loglevel up to
        // Warn so the construction Info chatter falls below the
        // stderr threshold; explicit `loglevel=info` still wins.
        let dryrun_phase_default = cli_params
            .get("dryrun")
            .map(|s| {
                s.split(',')
                    .any(|f| f.trim() == "phase" || f.trim() == "controls")
            })
            .unwrap_or(false);
        // A console-owning adapter (stdout to the terminal, plotter)
        // forced tui=off because it writes its own output there. Raise
        // the stderr floor to Warn so the Info-level run-detail
        // (session/metrics banners, phase walk, shutdown notices) stays
        // in the session log only and doesn't bury the adapter's output.
        let default_min_level = if dryrun_phase_default {
            nmbrs_runtime::observer::LogLevel::Warn
        } else {
            nmbrs_runtime::observer::LogLevel::Info
        };
        // Two independent log-level knobs:
        //
        //   loglevel=         — display threshold (stderr).
        //                       Default Info; debug+ noisy
        //                       on console.
        //   loglevel-retain=  — file-sink threshold
        //                       (session.log). Default Debug
        //                       so the file captures
        //                       everything for post-mortem.
        //
        // Aliases: `loglevel-display=` is accepted for
        // symmetry with `loglevel-retain=`; both map to the
        // stderr threshold.
        //
        // Resolution precedence (closest wins): CLI > workload `params:` >
        // `NMBRS_LOG_*` env > built-in default. `effective_params` already
        // carries the workload's declared params overlaid by the CLI (see the
        // adapter-detection peek above), so a `params: { loglevel: warn }`
        // workload runs quiet by default while a CLI `loglevel=info` overrides.
        let stderr_min_level = resolve_log_level(
            &effective_params,
            &["loglevel", "loglevel-display", "loglevel_display"],
            "NMBRS_LOG_DISPLAY_LEVEL",
        )
        .unwrap_or(default_min_level);
        let retain_min_level = resolve_log_level(
            &effective_params,
            &["loglevel-retain", "loglevel_retain"],
            "NMBRS_LOG_RETAIN_LEVEL",
        )
        .unwrap_or(nmbrs_runtime::observer::LogLevel::Debug);
        nmbrs_runtime::observer::set_retain_level(retain_min_level);
        nmbrs_runtime::observer::set_display_level(stderr_min_level);
        // Same cadence parsing the `tui=on` path uses, so the
        // metrics scheduler plans the same windows whether the
        // observer eventually drives a LogOnlySink or a TuiSink.
        let cadences = cli_params
            .get("latency-cadences")
            .or_else(|| cli_params.get("latency_cadences"))
            .and_then(|s| match nmbrs_metrics::cadence::Cadences::parse(s) {
                Ok(c) => Some(c),
                Err(e) => {
                    nmbrs_runtime::diag!(
                        nmbrs_runtime::observer::LogLevel::Warn,
                        "latency-cadences='{s}': {e} — using defaults"
                    );
                    None
                }
            })
            .unwrap_or_else(nmbrs_metrics::cadence::Cadences::defaults);
        let observer_concrete =
            nmbrs_tui::log_only_observer::LogOnlyObserver::new(run_state.clone(), cadences)
                .with_min_level(stderr_min_level);
        let observer_concrete = if silent_console {
            observer_concrete.reserve_console_for_adapter()
        } else {
            observer_concrete
        };
        let observer_arc = std::sync::Arc::new(observer_concrete);
        let observer: std::sync::Arc<dyn nmbrs_runtime::observer::RunObserver> =
            observer_arc.clone();

        // `tui=terminal`: hand off to the SinkSupervisor. The
        // supervisor owns the active sink (`LogOnlySink`
        // initially) plus the `KeyWatcher`, and swaps to
        // `TuiSink` on Ctrl-T (and back on Ctrl-T or `q`
        // inside the TUI). Tears everything down cleanly when
        // the runner future completes via the supervisor's
        // own shutdown handle.
        //
        // `tui=off`: no supervisor, no sink, no keystroke
        // watcher. The observer's `sink_active` stays false;
        // every log line goes straight to stderr through the
        // synchronous `eprintln!` path. Adapters needing
        // exclusive terminal access (plotter) end up here via
        // the adapter-override above.
        let supervisor = if tui_mode == "terminal" {
            Some(nmbrs_tui::sink_supervisor::SinkSupervisor::spawn(
                observer_arc.clone(),
                run_state.clone(),
                runtime_handle.clone(),
            ))
        } else {
            None
        };

        // `tui=formatted`: append-only status snapshots
        // alongside the observer's synchronous log stream.
        // No supervisor (no key handling, no swap-to-tui), no
        // cursor positioning — just a periodic snapshot line
        // per `STATUS_CADENCE`. Started directly here because
        // the supervisor's machinery is overkill for the
        // single-sink, no-watcher case.
        let formatted_handle = if tui_mode == "formatted" {
            use nmbrs_tui::display_sink::{DisplayInputs, DisplaySink};
            // The sink OWNS the piped surface (sink_active): per-event
            // lines and status snapshots all come from its thread, so a
            // backpressured pipe can never block a workload thread.
            let sink = nmbrs_tui::formatted_line_sink::FormattedLineSink::new(
                observer_arc.min_level(),
                observer_arc.sink_active_flag(),
            );
            let handle = Box::new(sink).start(DisplayInputs {
                state: run_state.clone(),
                frame_rx: None,
                metrics_query: None,
            });
            Some(handle)
        } else {
            None
        };

        // `tui=off` with a real (cycle-running) workload — console-owning
        // adapters (plotter etc.). Per the always-a-sink invariant the
        // WORST CASE surface is log-only: claim `sink_active` so the
        // observer's synchronous stderr writes are suppressed for the
        // whole run (entries still land in the ring, the inspector, and
        // session.log via the async file sink) and no workload thread
        // can ever block on the adapter-owned terminal. Early-exit
        // dryruns keep the synchronous path: they run no workload
        // (nothing to throttle) and their console diagnostics matter.
        let off_claimed = if tui_mode == "off" && !dryrun_is_early_exit {
            observer_arc
                .sink_active_flag()
                .store(true, std::sync::atomic::Ordering::Release);
            true
        } else {
            false
        };

        let run_result = nmbrs_runtime::runner::run_with_observer(args, observer).await;

        if off_claimed {
            // Post-run: release so shutdown stragglers print normally.
            observer_arc
                .sink_active_flag()
                .store(false, std::sync::atomic::Ordering::Release);
        }

        if let Some(s) = supervisor {
            // Two-step teardown so the terminal is **fully
            // restored** before any post-run output fires:
            //
            //   1. Brief grace period (150 ms) so the active
            //      sink can drain the final log lines —
            //      `run_finished` enqueues `all phases
            //      complete` via `observer::log`, which lands
            //      in the actor; the LogOnlySink's 50 ms
            //      poller picks it up.
            //   2. `supervisor.shutdown()` joins the active
            //      sink and the KeyWatcher; the watcher's
            //      drop disables raw mode and the active
            //      TuiSink (if up) leaves the alt-screen.
            //
            // After step 2 returns, the terminal is in its
            // pre-run discipline (cooked mode, no alt-screen,
            // mouse capture off). Anything that writes
            // directly to stderr/stdout before step 2 is a
            // bug — observer-routed `crate::diag!()` calls
            // are the only legal in-run output channel.
            std::thread::sleep(std::time::Duration::from_millis(150));
            s.shutdown();
        }

        // Tear down the formatted-line sink (if up) before
        // post-run reports fire so its final cadence-driven
        // line doesn't interleave with the report banner.
        if let Some(h) = formatted_handle {
            h.shutdown();
        }

        // From here down the terminal is back in cooked mode
        // (or we never claimed it — `tui=off` path). Post-run
        // reports / errors are safe to print.
        print_post_run_reports(args, &run_state, &run_result, silent_console);

        if let Err(e) = run_result {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        if let Some(code) = unreached_phase_exit_code(&run_state) {
            std::process::exit(code);
        }
        // Keep the actor join + run_state alive until the run
        // returns so the inspector socket stays serviceable for
        // the duration. Drop on return.
        let _ = run_state_join;
        let _ = run_state;
        return;
    }

    // Suppress C++ CQL driver chatter when the TUI owns the
    // screen. Only relevant when the cassandra-cpp engine is
    // built in; the scylla engine uses `tracing` so its log
    // levels are controlled via env (RUST_LOG / SCYLLA_LOG).
    #[cfg(feature = "engine-cassandra-cpp")]
    cassandra_cpp::set_level(cassandra_cpp::LogLevel::ERROR);

    // Parse user-declared latency cadences. Defaults if
    // omitted; bad values fall back to defaults with a warning.
    let cadences = params
        .get("latency-cadences")
        .or_else(|| params.get("latency_cadences"))
        .and_then(|s| match Cadences::parse(s) {
            Ok(c) => Some(c),
            Err(e) => {
                nmbrs_runtime::diag!(
                    nmbrs_runtime::observer::LogLevel::Warn,
                    "latency-cadences='{s}': {e} — using defaults"
                );
                None
            }
        })
        .unwrap_or_else(Cadences::defaults);

    // Match what runner.rs::run does for tui=off: parse
    // `loglevel=` and apply it as the stderr-fallback severity
    // filter. The TUI's in-app log panel filters separately
    // (own LOD knobs); this only controls what reaches stderr
    // before the TUI claims the terminal and after it tears
    // down (`q` mid-run).
    // Display + retain levels: same dual-knob shape and precedence as the
    // tui=off / log-only path above (CLI > workload `params:` > `NMBRS_LOG_*`
    // env > default). `loglevel=` → display; `loglevel-retain=` → file sink.
    let stderr_min_level = resolve_log_level(
        &effective_params,
        &["loglevel", "loglevel-display", "loglevel_display"],
        "NMBRS_LOG_DISPLAY_LEVEL",
    )
    .unwrap_or(nmbrs_runtime::observer::LogLevel::Info);
    let retain_min_level = resolve_log_level(
        &effective_params,
        &["loglevel-retain", "loglevel_retain"],
        "NMBRS_LOG_RETAIN_LEVEL",
    )
    .unwrap_or(nmbrs_runtime::observer::LogLevel::Debug);
    nmbrs_runtime::observer::set_retain_level(retain_min_level);
    nmbrs_runtime::observer::set_display_level(stderr_min_level);
    let observer =
        Arc::new(TuiObserver::new(run_state.clone(), cadences).with_min_level(stderr_min_level));

    // Run with the TUI observer. The TUI thread is spawned
    // lazily on the first phase_starting event.
    let run_result = nmbrs_runtime::runner::run_with_observer(args, observer.clone()).await;

    // Wait for the TUI to tear down the alternate screen before
    // any further stderr / stdout writes.
    observer.shutdown();

    // From here down the terminal is back in cooked mode.
    // Shared with the `tui=terminal` / `tui=off` path above.
    // tui=on never overlaps a console-owning adapter, so the summary
    // always prints here (silent_console is false on this path).
    print_post_run_reports(args, &run_state, &run_result, silent_console);

    if let Err(ref e) = run_result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // Catch pre-mapped phases that were never visited.
    if let Some(code) = unreached_phase_exit_code(&run_state) {
        std::process::exit(code);
    }

    // The RunState actor thread is detached; the global
    // observer (set via `set_global_observer` inside the
    // runner) keeps a sender alive for the process lifetime,
    // which is fine — the actor exits with the process. We
    // still hold the JoinHandle so a future sandboxed test
    // build could opt to join it; runtime nmbrs just lets it
    // ride.
    drop(run_state_join);
}

/// Print summary files + the post-run summary line. Called
/// after the active display sink (LogOnlySink / TuiSink) has
/// been torn down — the contract is that the terminal is back
/// in cooked mode by the time this runs, so direct stdout /
/// stderr writes don't compete with raw-mode output. Shared
/// between `tui=terminal` (`tui=off` adapter override goes
/// through here too) and `tui=on`.
///
/// Markdown summaries are echoed verbatim; non-Markdown formats
/// are listed by path so the user knows where to find them.
/// `_summary.*` files in `logs/latest` are scanned; the runner
/// has deferred their stdout output until now in TUI mode (the
/// alternate screen would have buffered and discarded any
/// inline writes).
fn print_post_run_reports(
    args: &[String],
    run_state: &nmbrs_tui::run_state_actor::RunStateHandle,
    run_result: &Result<(), String>,
    silent_console: bool,
) {
    // Queue barrier before reading the snapshot: a fast walk (a
    // dryrun completes 256 phases in ~1s) can outrun the actor's
    // command queue, and a stale snapshot here miscounted phases
    // nondeterministically (reported "N not run" + a bogus exit
    // code 2 while the log said "all phases complete"). Ordered-
    // inbox ack guarantees every lifecycle event is applied;
    // best-effort on timeout.
    let _ = run_state.drain_barrier(std::time::Duration::from_secs(5));
    // Resolve the *active* session dir for this run. When the
    // user passed `--session-path`, `--logs-dir`, or
    // `--session-name`, the session lives there — NOT under
    // `logs/latest`, which still points at whatever the
    // previous run left behind. Falling back to `logs/latest`
    // when no override is set preserves the historical
    // bare-CLI behavior.
    // A DRY-RUN is resume-inert: it deliberately does not claim `latest`
    // (`args_request_dryrun` / SRD-44), so `latest` still points at the previous
    // REAL session. Falling back to it here made every post-run artifact of a
    // dry-run land in that unrelated session — scenario_tree.txt written into it,
    // its summary.md rewritten, and `render_session_reports` / `auto_inject_details`
    // reading ITS metrics.db. Verified by running a dry-run and watching a
    // finished session's files change. The CLI never learns the dry-run's own
    // directory (the runner does not report it back), so the honest answer is
    // `None`: skip post-run artifacts rather than write them somewhere wrong.
    let session_dir: Option<std::path::PathBuf> =
        match nmbrs_runtime::session::read_session_dir(args) {
            Some(explicit) => Some(explicit),
            None if nmbrs_runtime::session::args_request_dryrun(args) => None,
            None => Some(nmbrs_runtime::session::latest_session_dir()),
        };

    // SRD-46 auto-render: when the workload completed without
    // being aborted by the error handler, render every report
    // item the runner persisted into the session db — plots,
    // tables, text sections, and `file` targets — through the
    // same pipeline as `nmbrs report all`. This has to land here
    // because plot_metrics lives in this crate (cross-crate from
    // nmbrs-runtime). Non-exiting: one report's failure must not
    // flip a successful run's exit code; no-data (the normal
    // dryrun/short-run condition) is a warning.
    if run_result.is_ok() {
        if let Some(dir) = session_dir.as_deref() {
            match crate::report_cmd::render_session_reports(&dir.join("metrics.db")) {
                Ok(_) => {}
                Err(failures) => {
                    for f in failures {
                        nmbrs_runtime::diag!(
                            nmbrs_runtime::observer::LogLevel::Error,
                            "post-run report: {f}"
                        );
                    }
                }
            }
            auto_inject_details(dir);
        }
    }

    // Same rule as above: with no directory this run owns, there are no summaries
    // of ITS to echo — and scanning the previous session's would print that run's.
    if let Some(scan_dir) = session_dir.as_deref()
        && let Ok(entries) = std::fs::read_dir(scan_dir)
    {
        let mut summary_paths: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.contains("_summary."))
                    .unwrap_or(false)
            })
            .collect();
        summary_paths.sort();
        // SRD-46: an artifact in the session dir is the
        // `sessiondir` destination and NOTHING more. Echoing every
        // `_summary.md` here is what used to put a report on
        // stdout that no workload asked for — and, because those
        // tables carry wall-clock values, made a run's stdout
        // differ from itself run to run. Markdown bodies now reach
        // stdout only via the deferred file below, which the
        // runtime writes when (and only when) routing said stdout.
        // Non-markdown artifacts are still announced by path, on
        // the log stream, so the operator knows they exist.
        for path in &summary_paths {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "md" {
                nmbrs_runtime::diag!(
                    nmbrs_runtime::observer::LogLevel::Info,
                    "summary ({ext}): {}",
                    path.display()
                );
            }
        }
    }

    // Flush stdout-routed summary output the runner deferred
    // because a TUI owned the terminal at the time.
    if let Some(dir) = session_dir.as_deref() {
        let deferred = dir.join(nmbrs_runtime::runner::DEFERRED_STDOUT_FILE);
        if let Ok(rendered) = std::fs::read_to_string(&deferred)
            && !rendered.is_empty()
        {
            print!("{rendered}");
            let _ = std::fs::remove_file(&deferred);
        }
    }
    // The post-run summary is an nmbrs system signal, not adapter
    // output — when a console-owning adapter holds an interactive
    // terminal it goes to `session.log` only (the phase outcomes are
    // already there); the console stays the adapter's.
    if !silent_console {
        print_post_run_summary(run_state, run_result, session_dir.as_deref());
    }
}

/// SRD-46 Details auto-injection: walk every output markdown
/// file in the session directory (default `summary.md` plus
/// every named file referenced by `report.<name>` items'
/// `target` line) and prepend a session-context section.
///
/// Source data: `session_metadata` rows the runner persists at
/// end-of-run (`session`, `start_time`, `end_time`,
/// `phase_count`, `scenario_count`, `workload_file`,
/// `adapter`).
fn auto_inject_details(session_dir: &std::path::Path) {
    let db_path = session_dir.join("metrics.db");
    if !db_path.exists() {
        return;
    }
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Per-execution metadata for the latest execution (falls back to
    // legacy session_metadata, which still holds the invariant
    // `session` key).
    let read_meta = |key: &str| -> Option<String> {
        nmbrs_metrics::reporters::sqlite::latest_execution_metadata_value(&conn, key)
    };

    let session_id = read_meta("session").unwrap_or_else(|| "?".into());
    let workload = read_meta("workload_file")
        .or_else(|| read_meta("workload"))
        .unwrap_or_else(|| "(inline)".into());
    let scenario = read_meta("scenario").unwrap_or_else(|| "?".into());
    let adapter = read_meta("adapter").unwrap_or_else(|| "?".into());
    let phase_count = read_meta("phase_count").unwrap_or_else(|| "?".into());
    let scenario_count = read_meta("scenario_count").unwrap_or_else(|| "?".into());
    let start_time = read_meta("start_time").and_then(|s| s.parse::<u64>().ok());
    let end_time = read_meta("end_time").and_then(|s| s.parse::<u64>().ok());
    let duration = match (start_time, end_time) {
        (Some(s), Some(e)) if e >= s => format_duration(e - s),
        _ => "?".to_string(),
    };
    let started = start_time
        .map(format_unix_seconds)
        .unwrap_or_else(|| "?".to_string());
    let ended = end_time
        .map(format_unix_seconds)
        .unwrap_or_else(|| "?".to_string());

    let body = format!(
        "| Field | Value |\n\
         | --- | --- |\n\
         | Session | `{session_id}` |\n\
         | Workload | `{workload}` |\n\
         | Scenario | `{scenario}` |\n\
         | Adapter | `{adapter}` |\n\
         | Started | {started} |\n\
         | Ended | {ended} |\n\
         | Duration | {duration} |\n\
         | Phases | {phase_count} |\n\
         | Scenarios | {scenario_count} |\n",
    );

    // Collect every distinct target file referenced by any
    // persisted report item, plus the default summary.md.
    let mut files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    files.insert("summary.md".into());
    for (_k, value) in
        nmbrs_metrics::reporters::sqlite::latest_execution_metadata_like(&conn, "report.%")
    {
        for line in value.lines() {
            if let Some(rest) = line.strip_prefix("target ") {
                files.insert(rest.trim().to_string());
            }
        }
    }

    for f in &files {
        let path = session_dir.join(f);
        if let Err(e) =
            crate::report::write_named_section_first(&path, "run_details", "Run Details", &body)
        {
            nmbrs_runtime::diag!(
                nmbrs_runtime::observer::LogLevel::Warn,
                "details auto-inject failed on '{}': {e}",
                path.display()
            );
        }
    }
}

/// `123` → `2m 3s` / `7261` → `2h 1m 1s`.
fn format_duration(seconds: u64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// UNIX seconds → ISO-ish `YYYY-MM-DD HH:MM:SS UTC`.
fn format_unix_seconds(secs: u64) -> String {
    // Cheap formatter — avoids pulling in chrono just for this.
    // Days since 1970-01-01 → calendar (proleptic Gregorian).
    let days = secs / 86400;
    let rem = secs % 86400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;
    let (year, month, day) = days_to_ymd(days as i64);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02} UTC")
}

fn days_to_ymd(mut days: i64) -> (i32, u32, u32) {
    // Howard Hinnant's algorithm.
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (year, m as u32, d as u32)
}

// ── cli_spec entry ─────────────────────────────────────────

use crate::cli_spec::{
    Arity, Category, Command, Flag, Handler, Level, ParsedCommand, ValueProvider,
};

/// `nmbrs run` — workload execution. The argument grammar
/// is too rich for the generic walker (workload `key=value`
/// params, scenario auto-promotion, adapter passthrough), so
/// the spec advertises the well-known flag surface for
/// completion + help and the handler delegates to the
/// existing async parser.
pub fn spec() -> Command {
    Command {
        name: "run",
        help: "Execute a workload. Accepts `workload=<file>`,\n\
               `key=value` workload params, `scenario=<name>`,\n\
               and adapter flags.",
        category: Category::Workloads,
        level: Level::Workload,
        flags: standard_run_flags(),
        kv_params: crate::completion::RUN_KV_PARAMS,
        dynamic_options: Some(crate::completion::workload_dynamic_params),
        positionals: Vec::new(),
        subcommands: Vec::new(),
        handler: Some(Handler::Async(run_handler)),
        raw_args: true,
        completion_override: None,
    }
}

fn run_handler(
    p: ParsedCommand,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>>>> {
    Box::pin(async move {
        // Re-prepend the matched command path's last segment
        // ("run") because the legacy parser expects argv[0] ==
        // "run". `path` here is `["nmbrs", "run"]`; strip the
        // first ("nmbrs") and pass the rest plus the raw tail.
        let mut argv: Vec<String> = vec!["run".into()];
        argv.extend(p.raw.iter().cloned());
        run_command(&argv).await;
        Ok(())
    })
}

/// SRD-77 — exported so `nmbrs refine` declares the same flag
/// surface as `nmbrs run`. The two verbs share workload-level
/// arg shapes; only the runtime semantic (skip prior completed
/// phases, bump `exec_id`) differs.
pub fn standard_run_flags() -> Vec<Flag> {
    vec![
        Flag {
            long: "--strict",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Strict workload-param validation.",
            repeatable: false,
        },
        Flag {
            long: "--no-prompt",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Don't prompt; assume non-interactive.",
            repeatable: false,
        },
        Flag {
            long: "--resume-latest",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Resume the most recent compatible session.",
            repeatable: false,
        },
        Flag {
            long: "--force-retry-failed",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Retry a previously failed phase on resume.",
            repeatable: false,
        },
        Flag {
            long: "--session",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: crate::completion::SESSION_NAME_VALUE,
            help: "SRD-04 session umbrella (path or name).",
            repeatable: false,
        },
        Flag {
            long: "--session-name",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Override session name.",
            repeatable: false,
        },
        Flag {
            long: "--session-path",
            short: None,
            aliases: &["--session-dir"],
            arity: Arity::Value,
            value: ValueProvider::Path,
            help: "Override session directory. Env: NMBRS_SESSION_PATH \
                   (legacy SESSION_DIRECTORY still honoured).",
            repeatable: false,
        },
        Flag {
            long: "--session-reuse",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::Custom(crate::completion::session_reuse_values),
            help: "Reuse policy for the chosen session.",
            repeatable: false,
        },
        // The `--sessions-*` spellings are aliases: the user guide documented only
        // those, so declaring them is what stops a documented invocation from being
        // parsed as an unknown flag and ignored.
        Flag {
            long: "--session-keep",
            short: None,
            aliases: &["--sessions-max"],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "How many sessions to keep (0 disables count-based purge). \
                   Env: NMBRS_SESSION_KEEP.",
            repeatable: false,
        },
        Flag {
            long: "--session-shelflife",
            short: None,
            aliases: &["--sessions-shelflife"],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Age past which sessions are purged, e.g. 2w (0 disables). \
                   Env: NMBRS_SESSION_SHELFLIFE.",
            repeatable: false,
        },
        Flag {
            long: "--resume",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::Path,
            help: "Resume from a specific session.",
            repeatable: false,
        },
        Flag {
            long: "--polydat-lib",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::Path,
            help: "GK library path override.",
            repeatable: false,
        },
        Flag {
            long: "--readout",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Readout-binding override.",
            repeatable: true,
        },
        // Declared Bool, not Value, even though `--metrics-log=<path>` is
        // accepted: the runner only reads the `=` spelling, so advertising a
        // space-separated value would suggest `--metrics-log <path>`, where the
        // path is instead taken as the scenario positional and the log lands at
        // its default location. Bool advertises only what works.
        Flag {
            long: "--metrics-log",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Also write metrics to one JSONL file for outside observers \
                   (`=<path>`; bare ⇒ <session>/metrics.jsonl). The session db \
                   is written regardless.",
            repeatable: false,
        },
        Flag {
            long: "--per-instance-metrics",
            short: None,
            aliases: &[],
            arity: Arity::Bool,
            value: ValueProvider::None,
            help: "Also write one file per (metric, label-tuple).",
            repeatable: false,
        },
        Flag {
            long: "--report-openmetrics-to",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::None,
            help: "Push metrics to an OpenMetrics/Prometheus endpoint URL.",
            repeatable: false,
        },
        Flag {
            long: "--kernel-opt",
            short: None,
            aliases: &[],
            arity: Arity::Value,
            value: ValueProvider::Custom(crate::completion::static_kernel_opt),
            help: "Polydat kernel optimisation level (release|diagnostic).",
            repeatable: false,
        },
    ]
}

#[cfg(test)]
mod flag_declaration_tests {
    use super::*;

    /// Source files whose argv reads must be reflected in a command spec.
    /// Scanned as text at test time — the point is to catch a flag that the
    /// runtime honours but no spec declares, which is invisible to `--help`
    /// and to completion and therefore looks unsupported.
    const SCANNED: &[&str] = &[
        "/../nmbrs-runtime/src/runner.rs",
        "/../nmbrs-runtime/src/session.rs",
    ];

    /// Long flags read by the scanned files that legitimately belong to a
    /// command other than `run`, or to no command at all. Each needs a reason.
    fn exempt(flag: &str) -> bool {
        matches!(
            flag,
            // Declared on the report/plot commands' own completion nodes.
            "--db" | "--body" | "--body-file" | "--csv-also" | "--report"
            // Internal: rewritten into argv by `plot`/`replay`, never typed.
            | "--wrap-order" | "--wrap-default-order"
            // Injected into argv by `refine_command` (refine.rs) to mark the
            // run as a refinement; not a user-facing flag.
            | "--refine"
        )
    }

    fn declared() -> Vec<&'static str> {
        let mut names = Vec::new();
        for f in standard_run_flags() {
            names.push(f.long);
            names.extend(f.aliases.iter().copied());
        }
        names
    }

    #[test]
    fn argv_flags_read_by_the_runtime_are_declared_in_the_spec() {
        let declared = declared();
        let mut undeclared: Vec<(String, String)> = Vec::new();

        for rel in SCANNED {
            let path = format!("{}{}", env!("CARGO_MANIFEST_DIR"), rel);
            let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("scan {path}: {e}"));

            for (n, line) in src.lines().enumerate() {
                let code = line.trim_start();
                // Skip doc/line comments: a flag NAMED in prose is not a read.
                if code.starts_with("//") {
                    continue;
                }
                // Only lines that actually test argv against a literal.
                let is_read = code.contains("resolve_flag(")
                    || code.contains("strip_prefix(\"--")
                    || code.contains("== \"--")
                    || code.contains("starts_with(\"--");
                if !is_read {
                    continue;
                }

                for tok in code.split("\"--").skip(1) {
                    let name: String = tok
                        .chars()
                        .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                        .collect();
                    if name.is_empty() {
                        continue;
                    }
                    let flag = format!("--{name}");
                    if exempt(&flag) || declared.contains(&flag.as_str()) {
                        continue;
                    }
                    undeclared.push((
                        flag,
                        format!("{}:{}", rel.trim_start_matches("/../"), n + 1),
                    ));
                }
            }
        }

        assert!(
            undeclared.is_empty(),
            "flags read from argv but not declared in a command spec — they \
             will not appear in --help or completion. Declare them in \
             standard_run_flags(), or add an `exempt()` reason: {undeclared:?}"
        );
    }

    /// Aliases must be DECLARED, not just parsed: the walker validates against
    /// the spec, and `--help` / completion read from it. A flag the runtime
    /// honours but the spec omits reads as unsupported.
    #[test]
    fn documented_aliases_are_declared_in_the_spec() {
        let declared = declared();
        for alias in ["--sessions-max", "--sessions-shelflife", "--session-dir"] {
            assert!(
                declared.contains(&alias),
                "{alias} is accepted by the parser and named in the user guide, \
                 so it must appear in the spec"
            );
        }
    }

    #[test]
    fn metrics_sink_flags_are_discoverable() {
        // Both were honoured by the runner while absent from the spec, which is
        // exactly how `--per-instance-metrics` stayed undocumented.
        let declared = declared();
        for flag in ["--metrics-log", "--per-instance-metrics"] {
            assert!(declared.contains(&flag), "{flag} must be declared");
        }
    }
}
