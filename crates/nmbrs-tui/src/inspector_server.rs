// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Out-of-band introspection endpoint.
//!
//! Spawned at run start (TUI mode and tui=off both eligible)
//! on a dedicated OS thread — never as a tokio task. Listens
//! on a Unix domain socket under
//! `${XDG_RUNTIME_DIR:-/tmp}/nmbrs-<pid>.sock`. The endpoint
//! reads via [`crate::run_state_actor::RunStateHandle::load`]
//! (a single atomic op), so it stays responsive even when
//! tokio is wedged or the executor's drain loop is stuck.
//!
//! See SRD-02 §"Display and Diagnostic Decoupling" — this is
//! the "poke and prod the machine while it's stuck" surface
//! that section calls out as a first-class deliverable.
//!
//! ## Wire protocol
//!
//! Each connection handles exactly one request, then closes
//! (HTTP/1.0-style — stateless, easy to reason about). Client
//! writes a single line ending in `\n`; server replies with
//! formatted text terminated by `\n` then closes the socket.
//!
//! ## Commands
//!
//! - `commands` — newline-separated list of command names.
//!   Used by the inspector REPL for tab autocomplete.
//! - `help` — human-readable help with one line per command.
//! - `meta` — workload, scenario, adapter, profiler, limit.
//! - `pid` — server process id (as a sanity check).
//! - `snapshot` — multi-section dump combining `meta`,
//!   `phases`, `latency`, and recent `log` entries.
//! - `phases` — full phase list with status markers.
//! - `active` — currently-running phases with live counters.
//! - `latency` — current percentile readings.
//! - `tree` — scenario tree as indented text.
//! - `log [N]` — last N log entries (default 20, max 200).
//! - `controls` — list every dynamic control declared in the
//!   live component tree.
//! - `set <name> <value> [source=<id>]` — write a dynamic
//!   control via the session root walk-up (SRD 23).
//! - `metrics` — list every `(family, label_set)` pair currently
//!   present in the live component tree.
//! - `metric <selector>` — read current values for matching
//!   metric instance(s) using a Prometheus-style selector.

use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
// Windows has supported AF_UNIX sockets since Windows 10 1803,
// but std doesn't expose them there — `uds_windows` mirrors the
// std API surface we use (bind/incoming/connect/timeouts).
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::thread;
#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};

use crate::run_state_actor::RunStateHandle;
use crate::state::{EntryKind, LogSeverity, PhaseStatus, RunState};

use nmbrs_metrics::component::{self, Component};
use nmbrs_metrics::controls::{ControlOrigin, SetError};
use nmbrs_metrics::labels::Labels;
use nmbrs_metrics::selector::Selector;
use nmbrs_metrics::snapshot::MetricValue;

/// Compute the socket path for the current process. Honors
/// `XDG_RUNTIME_DIR` when set (per the freedesktop runtime
/// convention) and falls back to `/tmp` otherwise. The pid is
/// part of the filename so multiple concurrent nmbrs runs don't
/// collide and the inspector can disambiguate them.
pub fn socket_path_for(pid: u32) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(windows) {
                // No `/tmp` on Windows — use the real temp dir.
                // Must stay in agreement with the REPL's
                // `discover_sockets` scan dir.
                std::env::temp_dir()
            } else {
                PathBuf::from("/tmp")
            }
        });
    dir.join(format!("nmbrs-{pid}.sock"))
}

/// Spawn the inspector server thread. Returns the socket path
/// it bound to (so the runner can log it) and a `JoinHandle`
/// that the caller may keep or drop. The thread closes
/// gracefully when its `RunStateHandle` clone is the only
/// remaining sender — i.e. when the run has fully torn down.
///
/// `runtime` is an optional [`tokio::runtime::Handle`] captured
/// at spawn time. When present, async control writes (`set
/// <name> <value>`) run via `runtime.block_on(...)` on the
/// per-connection thread; the inspector itself stays sync.
/// When `None` (e.g. test harnesses without a tokio runtime),
/// the `set` command rejects with `ERR no_runtime` and reads
/// keep working.
///
/// On bind failure the function returns `Err` and the run
/// continues without an inspector (this is a diagnostic
/// affordance, not a critical path).
pub fn spawn(
    state: RunStateHandle,
    runtime: Option<tokio::runtime::Handle>,
) -> std::io::Result<(PathBuf, thread::JoinHandle<()>)> {
    let path = socket_path_for(std::process::id());
    spawn_at(path, state, runtime)
}

/// Variant of [`spawn`] that binds a caller-chosen path.
/// Useful for tests (unique tempfile paths so parallel tests
/// don't collide) and any future deployment where the runtime
/// directory convention isn't appropriate.
pub fn spawn_at(
    path: PathBuf,
    state: RunStateHandle,
    runtime: Option<tokio::runtime::Handle>,
) -> std::io::Result<(PathBuf, thread::JoinHandle<()>)> {
    // Best-effort cleanup of any stale socket from a previous
    // run that crashed without removing its file. A live socket
    // bound by another process won't unlink — that just makes
    // our bind fail below, which is correct.
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path)?;
    // Non-blocking is unnecessary here: each accept blocks until
    // a client connects, and per-connection work runs on a
    // freshly-spawned thread. The listener thread itself is
    // dedicated.

    let path_for_thread = path.clone();
    let handle = thread::Builder::new()
        .name("inspector-server".into())
        .spawn(move || {
            run_accept_loop(listener, state, runtime, path_for_thread);
        })?;

    Ok((path, handle))
}

fn run_accept_loop(
    listener: UnixListener,
    state: RunStateHandle,
    runtime: Option<tokio::runtime::Handle>,
    path: PathBuf,
) {
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let state_for_conn = state.clone();
                let runtime_for_conn = runtime.clone();
                // One short-lived OS thread per connection.
                // Connections are bounded by client count
                // (always 1-2 inspector REPLs in practice);
                // spinning up a fresh thread keeps the
                // accept-loop dispatching even if a handler
                // hangs on a slow write.
                let _ = thread::Builder::new()
                    .name("inspector-conn".into())
                    .spawn(move || {
                        let _ =
                            handle_connection(stream, &state_for_conn, runtime_for_conn.as_ref());
                    });
            }
            Err(_) => {
                // Listener errored — typically only if the
                // socket file was removed externally. Bail
                // out of the accept loop; the runner can
                // notice via the JoinHandle.
                break;
            }
        }
    }
    // Best-effort cleanup of the socket file on shutdown.
    let _ = std::fs::remove_file(&path);
}

fn handle_connection(
    stream: UnixStream,
    state: &RunStateHandle,
    runtime: Option<&tokio::runtime::Handle>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    if reader.read_line(&mut request)? == 0 {
        return Ok(()); // empty connection
    }
    let response = dispatch(request.trim(), state, runtime);
    let mut stream = stream;
    stream.write_all(response.as_bytes())?;
    if !response.ends_with('\n') {
        stream.write_all(b"\n")?;
    }
    stream.flush()?;
    // Caller closes stream by drop.
    Ok(())
}

/// Inspector command dispatch surface. Public to the crate so
/// the in-process nb-shell prompt (`crate::prompt_state`) can
/// invoke the same commands the unix-socket inspector accepts,
/// without re-implementing the parser.
pub(crate) fn dispatch(
    line: &str,
    state: &RunStateHandle,
    runtime: Option<&tokio::runtime::Handle>,
) -> String {
    // Split on first whitespace: command + tail (used by `log N`).
    let mut parts = line.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    let tail = parts.next().unwrap_or("").trim();

    match cmd.as_str() {
        "" => "ERR empty command — try `help`".into(),
        "commands" => COMMAND_NAMES.join("\n"),
        "help" => render_help(),
        "pid" => format!("{}", std::process::id()),
        "meta" => render_meta(&state.load()),
        "snapshot" => render_snapshot(&state.load()),
        "phases" => render_phases(&state.load()),
        "active" => render_active(&state.load()),
        "latency" => render_latency(&state.load()),
        "tree" => render_tree(&state.load()),
        "log" => {
            let n: usize = tail.parse().ok().unwrap_or(20).min(200);
            render_log(&state.load(), n)
        }
        "controls" => render_controls(),
        "set" => render_set(tail, runtime),
        "metrics" => render_metrics(),
        "metric" => render_metric(tail),
        "readout" => render_readout(&state.load(), tail),
        other => format!("ERR unknown command '{other}' — try `help`"),
    }
}

/// Canonical command names. `pub` so both the in-process
/// nb-shell prompt (`crate::prompt_state`) and the nmbrs CLI's
/// `attach --command` completion power Tab against the same
/// set the unix-socket inspector accepts.
pub const COMMAND_NAMES: &[&str] = &[
    "commands", "help", "meta", "pid", "snapshot", "phases", "active", "latency", "tree", "log",
    "controls", "set", "metrics", "metric", "readout",
];

fn render_help() -> String {
    let mut s = String::from("nmbrs inspector — out-of-band introspection\n\n");
    let lines: &[(&str, &str)] = &[
        (
            "commands",
            "list command names (one per line) — used for autocomplete",
        ),
        ("help", "this message"),
        ("meta", "workload, scenario, adapter, profiler, limit"),
        ("pid", "server process id"),
        (
            "snapshot",
            "combined dump: meta + phases + latency + recent log",
        ),
        ("phases", "all phases with status markers"),
        ("active", "currently-running phases with live counters"),
        ("latency", "current latency percentiles (ns)"),
        ("tree", "scenario tree as indented text"),
        ("log [N]", "last N log entries (default 20, max 200)"),
        (
            "controls",
            "list every dynamic control in the live component tree",
        ),
        (
            "set <name> <value> [source=<id>]",
            "write a dynamic control (default source=inspector)",
        ),
        (
            "metrics",
            "list every (family, labels) pair in the live tree",
        ),
        (
            "metric <selector>",
            "read matching metric instance(s) — Prometheus-style selector",
        ),
        (
            "readout [name]",
            "render the SRD-63 readout of the given name (default `phase_status`) \
                       against the latest active phase; ANSI-stripped plain text",
        ),
    ];
    for (name, descr) in lines {
        s.push_str(&format!("  {:<36} {}\n", name, descr));
    }
    s
}

fn render_meta(state: &RunState) -> String {
    let mut s = String::new();
    s.push_str(&format!("workload : {}\n", state.workload_file));
    s.push_str(&format!("scenario : {}\n", state.scenario_name));
    s.push_str(&format!("adapter  : {}\n", state.adapter));
    s.push_str(&format!("profiler : {}\n", state.profiler));
    s.push_str(&format!("limit    : {}\n", state.limit));
    s.push_str(&format!("elapsed  : {:.2}s\n", state.elapsed_secs()));
    s.push_str(&format!("finished : {}\n", state.finished));
    if let Some(sink) = nmbrs_runtime::log_sink::global() {
        let dropped = sink.dropped_count();
        if dropped > 0 {
            s.push_str(&format!("log lines dropped: {dropped} (sink overflow)\n"));
        }
    }
    s
}

fn render_phases(state: &RunState) -> String {
    if state.phases.is_empty() {
        return "(no phases)".into();
    }
    let mut s = String::new();
    for p in &state.phases {
        let indent = " ".repeat(p.depth);
        let marker = match &p.status {
            PhaseStatus::Pending => "[  ]",
            PhaseStatus::Running => "[..]",
            PhaseStatus::Completed => "[ok]",
            PhaseStatus::Failed(_) => "[!!]",
        };
        let kind = match p.kind {
            EntryKind::Phase => "phase",
            EntryKind::Scope => "scope",
            EntryKind::Root => "root",
        };
        let labels = if p.labels.is_empty() {
            String::new()
        } else {
            format!(" ({})", p.labels)
        };
        let dur = p
            .duration_secs
            .map(|d| format!(" {d:.2}s"))
            .unwrap_or_default();
        s.push_str(&format!(
            "{indent}{marker} {kind:5} {}{labels}{dur}\n",
            p.name
        ));
    }
    s
}

fn render_active(state: &RunState) -> String {
    if state.active_phases.is_empty() {
        return "(no active phases)".into();
    }
    let mut entries: Vec<_> = state.active_phases.values().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.labels.cmp(&b.labels)));

    let mut s = String::new();
    for a in entries {
        let labels = if a.labels.is_empty() {
            String::new()
        } else {
            format!(" ({})", a.labels)
        };
        s.push_str(&format!("phase: {}{labels}\n", a.name));
        s.push_str(&format!(
            "  cursor       : {} ({}/{})\n",
            a.cursor_name, a.ops_finished, a.cursor_extent
        ));
        s.push_str(&format!("  fibers       : {}\n", a.fibers));
        s.push_str(&format!("  ops/sec      : {:.1}\n", a.ops_per_sec));
        s.push_str(&format!("  ops_started  : {}\n", a.ops_started));
        s.push_str(&format!("  ops_finished : {}\n", a.ops_finished));
        s.push_str(&format!("  ops_ok       : {}\n", a.ops_ok));
        s.push_str(&format!("  errors       : {}\n", a.errors));
        s.push_str(&format!("  retries      : {}\n", a.retries));
        if !a.adapter_counters.is_empty() {
            s.push_str("  adapter:\n");
            for (n, t, r) in &a.adapter_counters {
                s.push_str(&format!("    {n:<14} total={t} rate={r:.1}/s\n"));
            }
        }
        if !a.relevancy.is_empty() {
            s.push_str("  relevancy:\n");
            for (n, w, t, c, l) in &a.relevancy {
                s.push_str(&format!(
                    "    {n:<14} window={w:.4} total={t:.4} count={c} window_len={l}\n"
                ));
            }
        }
    }
    s
}

fn render_latency(state: &RunState) -> String {
    let mut s = String::from("current latency (ns):\n");
    s.push_str(&format!("  min   : {}\n", state.min_nanos));
    s.push_str(&format!("  p50   : {}\n", state.p50_nanos));
    s.push_str(&format!("  p90   : {}\n", state.p90_nanos));
    s.push_str(&format!("  p99   : {}\n", state.p99_nanos));
    s.push_str(&format!("  p999  : {}\n", state.p999_nanos));
    s.push_str(&format!("  max   : {}\n", state.max_nanos));
    s
}

fn render_tree(state: &RunState) -> String {
    if state.phases.is_empty() {
        return "(no scenario tree)".into();
    }
    // Same shape as `phases` for now — the tree is already a
    // DFS-flattened list with `depth`. Distinguishes scope
    // headers visually so the indent structure reads cleanly.
    let mut s = String::new();
    for p in &state.phases {
        let indent = " ".repeat(p.depth);
        match p.kind {
            EntryKind::Scope => {
                s.push_str(&format!("{indent}· {}\n", p.labels));
            }
            EntryKind::Phase => {
                let labels = if p.labels.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", p.labels)
                };
                let status = match &p.status {
                    PhaseStatus::Pending => "○",
                    PhaseStatus::Running => "▶",
                    PhaseStatus::Completed => "✓",
                    PhaseStatus::Failed(_) => "✗",
                };
                s.push_str(&format!("{indent}{status} {}{labels}\n", p.name));
            }
            EntryKind::Root => {}
        }
    }
    s
}

fn render_log(state: &RunState, n: usize) -> String {
    if state.log_messages.is_empty() {
        return "(no log entries)".into();
    }
    let start = state.log_messages.len().saturating_sub(n);
    let mut s = String::new();
    for entry in &state.log_messages[start..] {
        let tag = match entry.severity {
            LogSeverity::Debug => "DBG",
            LogSeverity::Info => "INF",
            LogSeverity::Warn => "WRN",
            LogSeverity::Error => "ERR",
        };
        s.push_str(&format!("{tag} {}\n", entry.message));
    }
    s
}

fn render_snapshot(state: &RunState) -> String {
    let mut s = String::new();
    s.push_str("=== meta ===\n");
    s.push_str(&render_meta(state));
    s.push_str("\n=== phases ===\n");
    s.push_str(&render_phases(state));
    s.push_str("\n=== active ===\n");
    s.push_str(&render_active(state));
    s.push_str("\n=== latency ===\n");
    s.push_str(&render_latency(state));
    s.push_str("\n=== recent log (last 20) ===\n");
    s.push_str(&render_log(state, 20));
    s
}

// ─── Controls (SRD 23) ───────────────────────────────────────

/// One row in the `controls` listing. Matches the structure of
/// `nmbrs-web::routes::list_controls` but rendered as text.
fn render_controls() -> String {
    let Some(root) = nmbrs_runtime::polydat_nodes::runtime_context::session_root_handle() else {
        return "(no session)".into();
    };
    let mut rows: Vec<String> = Vec::new();
    for comp in component::find(&root, &Selector::new()) {
        let Ok(guard) = comp.read() else {
            continue;
        };
        let path = labels_to_path(guard.effective_labels());
        for ctl in guard.controls().list() {
            let scope = match ctl.branch_scope() {
                nmbrs_metrics::controls::BranchScope::Local => "local",
                nmbrs_metrics::controls::BranchScope::Subtree => "subtree",
            };
            let final_marker = match ctl.final_scope() {
                Some(s) => format!("final@{s}"),
                None => "-".to_string(),
            };
            rows.push(format!(
                "{path} | {name} | {ty} | {value} | rev={rev} | scope={scope} | {final_marker}",
                path = if path.is_empty() {
                    "<root>"
                } else {
                    path.as_str()
                },
                name = ctl.name(),
                ty = ctl.value_type_name(),
                value = ctl.value_string(),
                rev = ctl.rev(),
            ));
        }
    }
    if rows.is_empty() {
        return "(no controls declared)".into();
    }
    rows.sort();
    rows.join("\n")
}

fn labels_to_path(labels: &Labels) -> String {
    labels
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// `set <name> <value> [source=<id>]`. Mirrors the web route
/// `POST /api/control/{name}`. The server thread is sync; we
/// drive the async `erased.set_f64(...)` via the captured tokio
/// runtime handle, blocking the per-connection thread (NOT the
/// async runtime worker pool) until the applier completes.
fn render_set(tail: &str, runtime: Option<&tokio::runtime::Handle>) -> String {
    let mut tokens = tail.split_whitespace();
    let Some(name) = tokens.next() else {
        return "ERR parse: missing name. Usage: set <name> <value> [source=<id>]".into();
    };
    let Some(value_str) = tokens.next() else {
        return "ERR parse: missing value. Usage: set <name> <value> [source=<id>]".into();
    };
    let value: f64 = match value_str.parse() {
        Ok(v) => v,
        Err(_) => return format!("ERR parse: value '{value_str}' is not a number"),
    };
    let mut source = "inspector".to_string();
    for extra in tokens {
        if let Some(s) = extra.strip_prefix("source=") {
            source = s.to_string();
        } else {
            return format!("ERR parse: unexpected token '{extra}'");
        }
    }

    let Some(rt) = runtime else {
        return "ERR no_runtime: inspector server has no tokio runtime handle; \
                control writes are unavailable"
            .into();
    };
    let Some(root) = nmbrs_runtime::polydat_nodes::runtime_context::session_root_handle() else {
        return format!("ERR no_session: no active session; cannot resolve control '{name}'");
    };

    let erased = {
        let Ok(guard) = root.read() else {
            return "ERR session_poisoned: session root RwLock is poisoned".to_string();
        };
        match guard.find_control_erased_up(name) {
            Some(e) => e,
            None => {
                return format!("ERR not_found: no control named '{name}' via walk-up");
            }
        }
    };

    let origin = ControlOrigin::Api { source };
    let name_owned = name.to_string();
    // block_on runs the future on the per-connection thread — we
    // are NOT inside the tokio runtime, so this is safe (no
    // worker-thread starvation). The future itself runs on the
    // tokio runtime via the captured handle.
    match rt.block_on(async move { erased.set_f64(value, origin).await }) {
        Ok(rev) => format!("OK {name_owned} rev={rev} value={value}"),
        Err(e) => match &e {
            SetError::ValidationFailed(m) => format!("ERR validation_failed: {m}"),
            SetError::ApplyFailed(fs) => {
                let joined = fs
                    .iter()
                    .map(|f| format!("#{}: {}", f.applier_index, f.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("ERR apply_failed: {joined}")
            }
            SetError::FinalViolation { scope } => {
                format!("ERR final_violation: control is final at scope '{scope}'")
            }
        },
    }
}

// ─── Metrics enumeration / query ─────────────────────────────

/// `(family_name, label_set)` tuple used for the `metrics`
/// listing. Walks the live tree, collecting every metric instance
/// the live instrument sets currently expose plus every reified
/// control gauge.
struct MetricInstance {
    family: String,
    labels: Labels,
    value: MetricValue,
}

/// Walk the component tree and return one [`MetricInstance`] per
/// `(family, label_set)` pair currently present. Reads via the
/// non-draining `capture_current` so the inspector never disturbs
/// the cadence reporter's accumulators.
///
/// This is the canonical "list every (family, labels) pair across
/// the live component tree" helper for diagnostic surfaces. The
/// scheduler's own walk uses `capture_tree(_current)` and merges
/// into snapshots; we just flatten the same trees into a value
/// list.
fn walk_live_metrics(root: &Arc<StdRwLock<Component>>) -> Vec<MetricInstance> {
    let mut out = Vec::new();
    for (_, set) in component::capture_tree_current(root) {
        for family in set.families() {
            for metric in family.metrics() {
                let Some(point) = metric.point() else {
                    continue;
                };
                out.push(MetricInstance {
                    family: family.name().to_string(),
                    labels: metric.labels().clone(),
                    value: point.value().clone(),
                });
            }
        }
    }
    out
}

fn render_metrics() -> String {
    let Some(root) = nmbrs_runtime::polydat_nodes::runtime_context::session_root_handle() else {
        return "(no session)".into();
    };
    let instances = walk_live_metrics(&root);
    if instances.is_empty() {
        return "(no metrics)".into();
    }
    let mut rows: Vec<String> = instances
        .iter()
        .map(|m| format!("{}{}", m.family, m.labels.to_prometheus()))
        .collect();
    rows.sort();
    rows.dedup();
    rows.join("\n")
}

/// Parsed selector from `metric <selector>`. Family `None` means
/// the wildcard `*`; otherwise the family name must match exactly.
struct MetricSelector {
    family: Option<String>,
    label_eq: Vec<(String, String)>,
}

impl MetricSelector {
    fn parse(input: &str) -> Result<Self, String> {
        let s = input.trim();
        if s.is_empty() {
            return Err("missing selector. Usage: metric <family>{k=v,...}".into());
        }
        let (family_part, body) = match s.find('{') {
            Some(idx) => {
                let (f, rest) = s.split_at(idx);
                if !rest.ends_with('}') {
                    return Err("selector body must be wrapped in {...}".into());
                }
                (f, &rest[1..rest.len() - 1])
            }
            None => (s, ""),
        };
        let family = match family_part.trim() {
            "" => return Err("missing family. Usage: metric <family>{k=v,...}".into()),
            "*" => None,
            other => {
                if !other
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
                {
                    return Err(format!("invalid family '{other}'"));
                }
                Some(other.to_string())
            }
        };
        let mut label_eq: Vec<(String, String)> = Vec::new();
        for piece in body.split(',') {
            let p = piece.trim();
            if p.is_empty() {
                continue;
            }
            let Some((k, v)) = p.split_once('=') else {
                return Err(format!("expected k=v in selector body, got '{p}'"));
            };
            let k = k.trim();
            let v = v.trim();
            if k.is_empty() || v.is_empty() {
                return Err(format!("empty key or value in '{p}'"));
            }
            label_eq.push((k.to_string(), v.to_string()));
        }
        Ok(Self { family, label_eq })
    }

    fn matches(&self, family: &str, labels: &Labels) -> bool {
        if let Some(ref name) = self.family
            && name != family
        {
            return false;
        }
        for (k, v) in &self.label_eq {
            if labels.get(k) != Some(v.as_str()) {
                return false;
            }
        }
        true
    }
}

fn render_metric(tail: &str) -> String {
    let sel = match MetricSelector::parse(tail) {
        Ok(s) => s,
        Err(e) => return format!("ERR parse: {e}"),
    };
    let Some(root) = nmbrs_runtime::polydat_nodes::runtime_context::session_root_handle() else {
        return "(no session)".into();
    };
    let mut hits: Vec<&MetricInstance> = Vec::new();
    let instances = walk_live_metrics(&root);
    for inst in &instances {
        if sel.matches(&inst.family, &inst.labels) {
            hits.push(inst);
        }
    }

    let header = format!(
        "{}{}",
        sel.family.as_deref().unwrap_or("*"),
        if sel.label_eq.is_empty() {
            String::new()
        } else {
            let inner = sel
                .label_eq
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        },
    );
    if hits.is_empty() {
        return format!("{header}\n  (no matches)");
    }

    let mut out = String::new();
    out.push_str(&header);
    out.push('\n');
    // Sort matches for stable output.
    hits.sort_by(|a, b| {
        a.family
            .cmp(&b.family)
            .then_with(|| a.labels.to_prometheus().cmp(&b.labels.to_prometheus()))
    });
    for inst in &hits {
        let line = format_metric_line(inst);
        out.push_str("  ");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Render one `MetricInstance` as a single line under its
/// selector header. Latency/time metrics get ms-scaled
/// percentiles; other histograms keep raw nanos. Counters and
/// gauges show their current value.
fn format_metric_line(inst: &MetricInstance) -> String {
    let labels = inst.labels.to_prometheus();
    let prefix = format!("{}{}", inst.family, labels);
    match &inst.value {
        MetricValue::Counter(c) => format!("{prefix} = {} (counter)", c.cumulative),
        MetricValue::Gauge(g) => format!("{prefix} = {} (gauge)", g.value),
        MetricValue::Histogram(h) => {
            let ms_mode = is_latency_family(&inst.family);
            let scale = |v: u64| {
                if ms_mode {
                    v as f64 / 1_000_000.0
                } else {
                    v as f64
                }
            };
            let unit = if ms_mode { "ms" } else { "raw" };
            let p50 = h.reservoir.value_at_quantile(0.50);
            let p99 = h.reservoir.value_at_quantile(0.99);
            let max = h.reservoir.max();
            let mean = h.reservoir.mean();
            let mean_scaled = if ms_mode { mean / 1_000_000.0 } else { mean };
            format!(
                "{prefix} count={} mean={:.3}{unit} p50={:.3}{unit} p99={:.3}{unit} max={:.3}{unit}",
                h.count,
                mean_scaled,
                scale(p50),
                scale(p99),
                scale(max),
            )
        }
        MetricValue::BucketedHistogram(h) => {
            format!(
                "{prefix} count={} buckets={} (histogram)",
                h.count,
                h.buckets.len()
            )
        }
        MetricValue::Info(_) => format!("{prefix} = 1 (info)"),
        MetricValue::StateSet(s) => {
            let active: Vec<&str> = s
                .states
                .iter()
                .filter(|(_, a)| *a)
                .map(|(n, _)| n.as_str())
                .collect();
            format!(
                "{prefix} states={} active=[{}]",
                s.states.len(),
                active.join(",")
            )
        }
    }
}

/// Heuristic: family names containing `time` or `latency` are
/// treated as nanosecond timers and rendered in ms. Anything else
/// keeps raw units.
fn is_latency_family(family: &str) -> bool {
    let f = family.to_ascii_lowercase();
    f.contains("time") || f.contains("latency")
}

/// Render an SRD-63 readout against the latest active phase
/// (or, if none is running, the most recently completed
/// phase). Default readout is `phase_status`. ANSI escapes
/// are stripped — the inspector socket is plain text.
///
/// Inspector is a read-only out-of-band surface; this
/// command is the canonical way for tooling / scripts to
/// scrape the same status the TUI shows, without parsing
/// terminal output.
fn render_readout(state: &RunState, tail: &str) -> String {
    use nmbrs_runtime::readouts as ro;
    use ro::ReadoutContext;

    let name = if tail.is_empty() {
        "phase_status"
    } else {
        tail.trim()
    };
    let Some(handle) = ro::Registry::lookup(name) else {
        let known = ro::Registry::all_names().join(", ");
        return format!("ERR unknown readout '{name}' — known: {known}\n");
    };

    // Pick a context: prefer the first active phase; else
    // the most recently completed phase.
    let active = state.first_active().cloned();
    let phase = state
        .phases
        .iter()
        .rev()
        .find(|p| {
            p.kind == crate::state::EntryKind::Phase
                && (matches!(p.status, crate::state::PhaseStatus::Running)
                    || active.is_some()
                    || matches!(p.status, crate::state::PhaseStatus::Completed))
        })
        .cloned();
    let Some(phase) = phase else {
        return format!("(no phase to render `{name}` against)\n");
    };

    let ctx_box: Box<dyn ReadoutContext> = if let Some(a) = active {
        Box::new(crate::readout_panel::PhaseRowContext::live(&phase, &a, 0))
    } else {
        Box::new(crate::readout_panel::PhaseRowContext::terminal(&phase))
    };

    // Fire the readout directly (no binder — the inspector
    // wants the raw render, not the user's TUI-side focus
    // overrides).
    let mut sink_buf = String::new();
    {
        let mut buf = ro::buf::StringBuf::new(&mut sink_buf);
        handle.render(
            &*ctx_box,
            ro::Lod::Labeled,
            ro::ContentMode::Value,
            &ro::ReadoutOptions::new(),
            &mut buf,
        );
    }
    let plain = ro::snapshot::strip_ansi(&sink_buf);
    if plain.is_empty() {
        format!("(readout `{name}` produced no output for current phase state)\n")
    } else if plain.ends_with('\n') {
        plain
    } else {
        format!("{plain}\n")
    }
}

// Silence unused-import linting from the always-held Arc when
// the file is read in isolation.
#[allow(dead_code)]
fn _arc_kept_alive(_: Arc<RunState>) {}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_selector_parse_simple_family_only() {
        let s = MetricSelector::parse("cycles").unwrap();
        assert_eq!(s.family.as_deref(), Some("cycles"));
        assert!(s.label_eq.is_empty());
    }

    #[test]
    fn metric_selector_parse_with_labels() {
        let s = MetricSelector::parse("cycles_servicetime{phase=pvs_query,table=fknn}").unwrap();
        assert_eq!(s.family.as_deref(), Some("cycles_servicetime"));
        assert_eq!(s.label_eq.len(), 2);
        assert_eq!(
            s.label_eq[0],
            ("phase".to_string(), "pvs_query".to_string())
        );
        assert_eq!(s.label_eq[1], ("table".to_string(), "fknn".to_string()));
    }

    #[test]
    fn metric_selector_parse_wildcard_family() {
        let s = MetricSelector::parse("*{phase=load}").unwrap();
        assert!(s.family.is_none());
        assert_eq!(s.label_eq.len(), 1);
    }

    #[test]
    fn metric_selector_parse_rejects_empty() {
        assert!(MetricSelector::parse("").is_err());
        assert!(MetricSelector::parse("{}").is_err());
        assert!(MetricSelector::parse("foo{bad}").is_err());
        assert!(MetricSelector::parse("foo{=v}").is_err());
        assert!(MetricSelector::parse("foo{k=}").is_err());
    }

    #[test]
    fn metric_selector_match_family_and_labels() {
        let s = MetricSelector::parse("cycles{phase=load}").unwrap();
        let mut l = Labels::empty();
        l = l.with("phase", "load").with("k", "10");
        assert!(s.matches("cycles", &l));
        assert!(!s.matches("ops", &l));
        let other = Labels::of("phase", "verify");
        assert!(!s.matches("cycles", &other));
    }

    #[test]
    fn metric_selector_wildcard_matches_any_family() {
        let s = MetricSelector::parse("*{phase=load}").unwrap();
        let l = Labels::of("phase", "load");
        assert!(s.matches("anything", &l));
        assert!(s.matches("cycles", &l));
    }

    #[test]
    fn is_latency_family_heuristic() {
        assert!(is_latency_family("cycles_servicetime"));
        assert!(is_latency_family("op_latency"));
        assert!(is_latency_family("response_time"));
        assert!(!is_latency_family("cycles"));
        assert!(!is_latency_family("errors"));
    }

    /// Push 8d: the `readout` command rejects an unknown
    /// readout name with an ERR + the list of registered
    /// names.
    #[test]
    fn readout_unknown_name_returns_err_with_known_list() {
        let state = RunState::new("test.yaml", "fake", "stdout");
        let out = render_readout(&state, "not_a_real_readout");
        assert!(
            out.starts_with("ERR unknown readout"),
            "unexpected output: {out}"
        );
        assert!(
            out.contains("phase_status"),
            "should list known names: {out}"
        );
    }

    /// With no phase to render against, the command returns
    /// a placeholder line instead of empty output. Avoids
    /// the inspector returning a blank reply that scripts
    /// can't distinguish from a transport error.
    #[test]
    fn readout_with_no_phase_returns_placeholder() {
        let state = RunState::new("test.yaml", "fake", "stdout");
        let out = render_readout(&state, "phase_outcome");
        assert!(out.contains("no phase"), "expected placeholder, got: {out}");
    }
}
