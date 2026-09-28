// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs metrics query <expr>` — evaluate a metricsql
//! expression against the active session's `metrics.db`.
//!
//! Wraps the `nmbrs_metrics::queryapi::sqlite::SqliteDataSource`
//! adapter (SRD-47 §"What this push enables next") plus the
//! batch evaluator. End-to-end: `parse -> compile-shape ->
//! evaluate -> format`. Two modes:
//!
//! - **Instant** (default): start_ms == end_ms. One sample
//!   per result series. Anchor defaults to `MAX(timestamp_ms)`
//!   from the database — i.e. "latest data we have".
//! - **Range** (`--lookback <duration>`): from
//!   `anchor - lookback` to `anchor`, stepped by `--step`.
//!   Multi-sample series, one row per step.
//!
//! Output format matches Prometheus' `promtool` flat form:
//!
//! ```text
//! <labels> @ <timestamp> = <value>
//! ```
//!
//! one row per (series, sample). Labels are
//! `key="value",key="value"`, sorted, with `__name__`
//! always first when present.

use std::path::PathBuf;

use nmbrs_metrics::queryapi::sqlite::SqliteDataSource;
use nmbrs_metricsql::eval::{
    DataSourceError, EvalContext, Matcher, MetricAccess, Series, Vector, evaluate,
};
use nmbrs_metricsql::runtime::{
    ContinuousQueryRuntime, QueryHandle, RegisterError, RegisterOptions, SampleFeed, WindowPolicy,
};

pub fn query(args: &[String]) {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("nmbrs metrics query: {e}");
            print_usage();
            std::process::exit(2);
        }
    };
    if !parsed.db_path.exists() {
        eprintln!(
            "nmbrs metrics query: db not found at '{}'",
            parsed.db_path.display()
        );
        std::process::exit(2);
    }

    // Open the data source first; we re-use its connection
    // for the anchor-timestamp lookup so we read a coherent
    // view of the database.
    let ds = match SqliteDataSource::open(&parsed.db_path) {
        // SRD-77 — coalesce across executions by default: each series
        // comes from the newest execution that produced it.
        Ok(ds) => ds.with_execution_selection(
            nmbrs_metrics::queryapi::sqlite::ExecutionSelection::LatestPerInstance,
        ),
        Err(e) => {
            eprintln!("nmbrs metrics query: open db: {e}");
            std::process::exit(2);
        }
    };

    // Anchor: explicit `--at` wins, else use the latest
    // sample timestamp in the db. Falling back to wall-clock
    // `now()` would silently produce empty results when the
    // db is older than the run — surface that as "no data".
    let anchor_ms = match parsed.anchor_ms {
        Some(ts) => ts,
        None => match latest_sample_ts(&parsed.db_path) {
            Ok(Some(ts)) => ts,
            Ok(None) => {
                eprintln!("nmbrs metrics query: db has no samples");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("nmbrs metrics query: probe latest sample: {e}");
                std::process::exit(2);
            }
        },
    };
    let step_ms = parsed.step_ms;

    // `--range <dur|all>` applies the normative metricsql `[<dur>]`
    // range-vector suffix to the (selector) query — the evaluator handles
    // `[...]` natively. `all` sizes the window to the whole test span
    // (anchor − earliest sample) so every series across the run is
    // returned; otherwise the literal duration is appended verbatim. This
    // is the discoverable answer to "show me the metric over a window /
    // the whole run" without hand-typing the `[…]` selector.
    let range_suffix: Option<String> = parsed.range.as_ref().map(|range| {
        if range.eq_ignore_ascii_case("all") {
            let earliest = match earliest_sample_ts(&parsed.db_path) {
                Ok(Some(ts)) => ts,
                _ => anchor_ms,
            };
            let span = (anchor_ms - earliest).max(1);
            format!("[{span}ms]")
        } else {
            if let Err(e) = parse_duration_ms(range) {
                eprintln!(
                    "nmbrs metrics query: --range: {e} \
                    (expected a duration like 1m / 30s, or `all`)"
                );
                std::process::exit(2);
            }
            format!("[{range}]")
        }
    });

    // Two query modes:
    //
    // - **Instant** (`--lookback 0`, the default): use
    //   PromQL's stale-tolerance semantic via
    //   `EvalContext::lookback_ms`. The selector path
    //   fetches `[T - delta, T]` and keeps the latest
    //   sample per series, projected to T. `delta` is the
    //   `--stale-window` flag (default 5 min).
    //
    // - **Range** (`--lookback > 0`): explicit range query
    //   from `T - lookback` to `T`. Samples returned as-is;
    //   `--all-samples` controls whether all are emitted or
    //   just the latest per series.
    let (ctx_start, ctx_end, instant_lookback) = if parsed.lookback_ms == 0 {
        (anchor_ms, anchor_ms, Some(parsed.stale_window_ms))
    } else {
        (anchor_ms - parsed.lookback_ms, anchor_ms, None)
    };
    // SRD-77 — the DataSource coalesces across executions
    // (per-instance-latest); no implicit single-`exec_id` injection.
    // An explicit `exec_id="N"` the operator typed survives as a
    // normal label matcher.
    let ctx = EvalContext {
        data: &ds,
        start_ms: ctx_start,
        end_ms: ctx_end,
        step_ms,
        lookback_ms: instant_lookback,
        // CLI-level instant/range query — `@ start()/end()`
        // resolve to the original bounds.
        query_start_ms: Some(ctx_start),
        query_end_ms: Some(ctx_end),
    };
    // Evaluate each positional independently and merge the results: a
    // single quoted expression is one eval; multiple bare family names
    // (`attempt_total result_total …`) each resolve as a selector and
    // their series union into one sorted output — "show me these
    // families". `--range` (if given) applies to each.
    let mut all_series: Vec<Series> = Vec::new();
    for raw in &parsed.queries {
        let q = match &range_suffix {
            Some(s) => format!("{raw}{s}"),
            None => raw.clone(),
        };
        let expr = match nmbrs_metricsql::parse(&q) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("nmbrs metrics query: parse {q:?}: {e}");
                std::process::exit(2);
            }
        };
        match evaluate(&ctx, &expr) {
            Ok(series) => all_series.extend(series),
            Err(e) => {
                eprintln!("nmbrs metrics query: evaluate {q:?}: {e}");
                std::process::exit(2);
            }
        }
    }
    emit(&all_series, parsed.latest_only);
}

#[derive(Debug)]
struct ParsedArgs {
    db_path: PathBuf,
    /// One or more positionals. A single quoted metricsql expression
    /// (`'sum(x) by (y)'`) is one element evaluated as-is; multiple bare
    /// family names (`attempt_total result_total …`) are each evaluated as
    /// a selector and the results merged — "show me these families".
    queries: Vec<String>,
    anchor_ms: Option<i64>,
    lookback_ms: i64,
    step_ms: i64,
    /// Stale-tolerance window for instant queries. Mirrors
    /// PromQL's lookback delta. Only consulted when
    /// `lookback_ms == 0` (instant mode).
    stale_window_ms: i64,
    /// When `true`, emit only the most recent sample per
    /// output series (matches what `instant query` users
    /// want: "latest value per metric"). When `false`, emit
    /// every sample the evaluator produced — useful for
    /// time-range debugging.
    latest_only: bool,
    /// `--range <dur|all>`: apply the normative metricsql `[<dur>]`
    /// range-vector suffix to the (selector) query. `all` expands to the
    /// whole test span. `None` leaves the query untouched. Autocompletes
    /// to `all` + the session's actual reporting cadences (distinct
    /// `interval_ms` in the db).
    range: Option<String>,
}

/// The db to read: an explicit `--db`, else the session named on the line, else
/// the latest session.
///
/// The middle step was missing. Both parsers below SWALLOWED `--session*` and its
/// value and then fell back to the latest session, working only because the
/// startup hook repointed the `sessions/latest` symlink at the named session —
/// which made a read command mutate global state and only worked for sessions
/// under `sessions/`. Resolving locally works for any path and mutates nothing.
fn resolve_db(db_flag: Option<PathBuf>, args: &[String]) -> PathBuf {
    db_flag
        .or_else(|| nmbrs_runtime::session::read_session_dir(args).map(|d| d.join("metrics.db")))
        .unwrap_or_else(nmbrs_runtime::session::latest_metrics_db)
}

fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut db_path: Option<PathBuf> = None;
    let mut queries: Vec<String> = Vec::new();
    let mut anchor_ms: Option<i64> = None;
    // Default to instant query (lookback 0). Caller can opt
    // into range mode with `--lookback <duration>`.
    let mut lookback_ms: i64 = 0;
    let mut step_ms: i64 = 60_000; // 1 minute default for range mode
    // PromQL stale-tolerance window for instant queries —
    // 5 minutes matches Prometheus' default; absorbs the
    // counter/summary timestamp skew within a single
    // reporter cadence.
    let mut stale_window_ms: i64 = 300_000;
    // Default "latest sample per series" matches the user
    // intent for ad-hoc instant queries. `--all-samples`
    // flips it for range debugging.
    let mut latest_only = true;
    let mut range: Option<String> = None;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--db" => {
                db_path = iter.next().map(PathBuf::from);
            }
            other if other.starts_with("--db=") => {
                db_path = Some(PathBuf::from(&other[5..]));
            }
            "--at" => {
                let v = iter.next().ok_or("--at requires a millisecond timestamp")?;
                anchor_ms = Some(v.parse::<i64>().map_err(|e| format!("--at parse: {e}"))?);
            }
            "--lookback" => {
                let v = iter.next().ok_or("--lookback requires a duration")?;
                lookback_ms = parse_duration_ms(v)?;
            }
            "--step" => {
                let v = iter.next().ok_or("--step requires a duration")?;
                step_ms = parse_duration_ms(v)?;
                if step_ms <= 0 {
                    return Err("--step must be positive".into());
                }
            }
            "--all-samples" => {
                latest_only = false;
            }
            "--range" => {
                range = Some(
                    iter.next()
                        .ok_or("--range requires a duration (e.g. 1m) or `all`")?
                        .to_string(),
                );
            }
            other if other.starts_with("--range=") => {
                range = Some(other["--range=".len()..].to_string());
            }
            // Repeatable family selector. Each `--family <name>` adds a
            // metric family to the query; their series merge into one
            // output. This is the autocomplete-friendly way to select
            // multiple families — veks completes a flag's value on every
            // occurrence (positionals only complete the first token).
            "--family" => {
                queries.push(
                    iter.next()
                        .ok_or("--family requires a metric family name")?
                        .to_string(),
                );
            }
            other if other.starts_with("--family=") => {
                queries.push(other["--family=".len()..].to_string());
            }
            "--stale-window" => {
                let v = iter.next().ok_or("--stale-window requires a duration")?;
                stale_window_ms = parse_duration_ms(v)?;
                if stale_window_ms < 0 {
                    return Err("--stale-window must be non-negative".into());
                }
            }
            // Tolerate the umbrella session flags so the
            // command composes with the global flag set.
            "--session"
            | "--session-name"
            | "--session-path"
            | "--session-reuse"
            | "--session-keep"
            | "--session-shelflife" => {
                let _ = iter.next();
            }
            other if other.starts_with("--session") => {}
            other if !other.starts_with("--") => {
                // Each bare positional is a query: one quoted metricsql
                // expression, or several family names to merge.
                queries.push(other.to_string());
            }
            other => {
                return Err(format!("unknown flag '{other}'"));
            }
        }
    }
    if queries.is_empty() {
        return Err("metricsql expression or metric family name(s) required \
                    (positional argument)"
            .to_string());
    }
    let db_path = resolve_db(db_path, args);
    Ok(ParsedArgs {
        db_path,
        queries,
        anchor_ms,
        lookback_ms,
        step_ms,
        stale_window_ms,
        latest_only,
        range,
    })
}

/// Minimal duration parser: `5m`, `1h30m`, `500ms`, `2.5s`,
/// `30`. Bare numbers are seconds. Mirrors the eval-side
/// parser's unit set; doesn't need step-relative `i` here
/// (no Polydat execution context to inherit from).
fn parse_duration_ms(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if let Ok(n) = s.parse::<f64>() {
        return Ok((n * 1000.0) as i64);
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut total: i64 = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == start {
            return Err(format!("duration {s:?}: expected number"));
        }
        let n: f64 = s[start..i]
            .parse()
            .map_err(|e| format!("duration {s:?}: bad number: {e}"))?;
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit = &s[unit_start..i];
        let mult = match unit {
            "ms" => 1.0,
            "s" => 1_000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            "d" => 86_400_000.0,
            "" => return Err(format!("duration {s:?}: missing unit")),
            other => return Err(format!("duration {s:?}: unknown unit {other:?}")),
        };
        total = total.saturating_add((n * mult) as i64);
    }
    Ok(total)
}

/// One-shot probe for the db's latest sample timestamp. Used
/// when `--at` is not supplied — defaulting to wall-clock
/// `now()` would yield empty results against a stale db.
fn latest_sample_ts(db: &std::path::Path) -> Result<Option<i64>, String> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    conn.query_row("SELECT MAX(timestamp_ms) FROM sample_value", [], |row| {
        row.get::<_, Option<i64>>(0)
    })
    .map_err(|e| e.to_string())
}

/// One-shot probe for the db's earliest sample timestamp — the start of
/// the test span. Used by `--range all` to size the `[<dur>]` window so it
/// covers the whole run.
fn earliest_sample_ts(db: &std::path::Path) -> Result<Option<i64>, String> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    conn.query_row("SELECT MIN(timestamp_ms) FROM sample_value", [], |row| {
        row.get::<_, Option<i64>>(0)
    })
    .map_err(|e| e.to_string())
}

fn emit(series: &[Series], latest_only: bool) {
    if series.is_empty() {
        println!("(no series matched)");
        return;
    }
    // Stable output ordering: by formatted label set so the
    // command is deterministic across runs.
    let mut rows: Vec<(String, &Series)> = series
        .iter()
        .map(|s| (format_labels(&s.labels), s))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (labels, s) in rows {
        if s.samples.is_empty() {
            println!("{labels} @ - = (no samples)");
            continue;
        }
        if latest_only {
            // Pick the sample with the largest timestamp.
            // Most queries want "what's the value now?"; the
            // window is just there to absorb cadence skew.
            let latest = s
                .samples
                .iter()
                .max_by_key(|sm| sm.timestamp_ms)
                .expect("non-empty checked above");
            println!("{labels} @ {} = {}", latest.timestamp_ms, latest.value);
        } else {
            for sample in &s.samples {
                println!("{labels} @ {} = {}", sample.timestamp_ms, sample.value);
            }
        }
    }
}

/// Render a label set as `{k="v",k="v"}`. `__name__` lifts
/// out as a leading bare token so output reads like the
/// metricsql syntax users typed in.
fn format_labels(labels: &[(String, String)]) -> String {
    let name = labels
        .iter()
        .find(|(k, _)| k == "__name__")
        .map(|(_, v)| v.clone());
    let mut other: Vec<&(String, String)> =
        labels.iter().filter(|(k, _)| k != "__name__").collect();
    other.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    if let Some(n) = name {
        out.push_str(&n);
    }
    if !other.is_empty() {
        out.push('{');
        for (i, (k, v)) in other.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(k);
            out.push_str("=\"");
            for ch in v.chars() {
                if ch == '"' || ch == '\\' {
                    out.push('\\');
                }
                out.push(ch);
            }
            out.push('"');
        }
        out.push('}');
    }
    out
}

/// `nmbrs metrics watch <expr>` — live-update a metricsql
/// expression on a polling interval.
///
/// Two engines:
///
/// - **Runtime** (default): compile via the SRD-48
///   `ContinuousQueryRuntime`. Each tick advances per-leaf
///   watermarks, ingests new samples, republishes the
///   snapshot. The runtime is the same code path that
///   future TUI/web consumers will use — dogfooding it
///   here surfaces production rough edges early.
/// - **Batch fallback**: when the expression doesn't
///   `compile_streaming` (e.g. a shape the streaming layer
///   rejects), drop back to running the full batch
///   evaluator each tick.
///
/// Both engines produce the same observable output for
/// supported shapes — the SRD-47 equivalence property
/// guarantees it.
pub fn watch(args: &[String]) {
    let parsed = match parse_watch_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("nmbrs metrics watch: {e}");
            print_watch_usage();
            std::process::exit(2);
        }
    };
    if !parsed.db_path.exists() {
        eprintln!(
            "nmbrs metrics watch: db not found at '{}'",
            parsed.db_path.display()
        );
        std::process::exit(2);
    }
    let mut expr = match nmbrs_metricsql::parse(&parsed.query) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("nmbrs metrics watch: parse: {e}");
            std::process::exit(2);
        }
    };
    // SRD-77 — same `exec_id="<latest>"` injection as the
    // one-shot query path above. The watch loop re-runs the
    // same expression on a cadence, so the qualifier is
    // pinned once at startup (deliberate — a refine landing
    // mid-watch would otherwise silently jump the watched
    // execution from under the operator).
    {
        let session_dir = parsed
            .db_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        nmbrs_runtime::refine_plan::warn_multi_execution_default(&session_dir);
        let resolved =
            nmbrs_runtime::refine_plan::ExecutionQualifier::latest(&session_dir).specific_id();
        nmbrs_metricsql::query_rewrite::inject_default_exec_id(&mut expr, resolved);
    }
    let expr = expr;

    // Try the runtime path first; fall back to batch if
    // `compile_streaming` rejects the shape. Both paths
    // need their own SqliteDataSource handle since the
    // runtime takes ownership of its feed.
    let runtime_attempt = try_runtime_engine(&parsed);
    match runtime_attempt {
        Ok((runtime, handle)) => {
            eprintln!("# engine: runtime");
            run_runtime_loop(&parsed, runtime, handle);
        }
        Err(reason) => {
            eprintln!("# engine: batch ({reason})");
            // Batch path needs its own data-source handle.
            let ds = match SqliteDataSource::open(&parsed.db_path) {
                Ok(ds) => ds,
                Err(e) => {
                    eprintln!("nmbrs metrics watch: open db: {e}");
                    std::process::exit(2);
                }
            };
            run_batch_loop(&parsed, &ds, &expr);
        }
    }
}

/// Build a `ContinuousQueryRuntime` for the watch CLI and
/// register the user's query. Returns the runtime + the
/// query handle on success; on streaming compile failure,
/// returns the reason string for diagnostic display.
fn try_runtime_engine(parsed: &WatchArgs) -> Result<(ContinuousQueryRuntime, QueryHandle), String> {
    let ds = SqliteDataSource::open(&parsed.db_path).map_err(|e| format!("open db: {e}"))?;
    let feed = SqliteCliFeed {
        source: Box::new(ds),
        db_path: parsed.db_path.clone(),
    };
    let runtime = ContinuousQueryRuntime::with_feed(Box::new(feed));
    let opts = RegisterOptions {
        // The watch CLI wants lifetime accumulation: no
        // surprise resets in the user's terminal. The
        // tumbling default is for live dashboards where
        // bounded memory matters more than stable output.
        window_policy: WindowPolicy::Lifetime,
        warmup_ms: parsed.warmup_ms,
    };
    runtime
        .register_with(&parsed.query, opts)
        .map(|h| (runtime.clone(), h))
        .map_err(|e: RegisterError| e.to_string())
}

/// CLI-specific feed: PullFeed delegates `fetch_since` but
/// shadows `latest_ts` with a direct probe of the db's
/// `MAX(timestamp_ms)`. The default `PullFeed::latest_ts`
/// returns `None` (no general-purpose probe in the
/// `DataSource` trait); the runtime would skip every tick
/// without this shim.
struct SqliteCliFeed {
    source: Box<SqliteDataSource>,
    db_path: PathBuf,
}

impl SampleFeed for SqliteCliFeed {
    fn fetch_since(
        &self,
        matchers: &[Matcher],
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vector, DataSourceError> {
        // Mirror PullFeed's exclusive-since → inclusive
        // conversion.
        let start = since_ms.saturating_add(1);
        if start > until_ms {
            return Ok(Vector::default());
        }
        self.source.select_range(matchers, start, until_ms)
    }

    fn latest_ts(&self) -> Result<Option<i64>, DataSourceError> {
        latest_sample_ts(&self.db_path).map_err(DataSourceError::new)
    }
}

fn run_runtime_loop(parsed: &WatchArgs, runtime: ContinuousQueryRuntime, handle: QueryHandle) {
    // First snapshot is already populated by `register`'s
    // backfill. Render it before sleeping so the user sees
    // data on the first frame.
    redraw(
        &parsed.query,
        &handle.snapshot(),
        parsed.latest_only,
        parsed.no_clear,
    );
    loop {
        std::thread::sleep(std::time::Duration::from_millis(parsed.interval_ms as u64));
        if let Err(e) = runtime.tick() {
            eprintln!("nmbrs metrics watch: tick: {e}");
            std::process::exit(2);
        }
        redraw(
            &parsed.query,
            &handle.snapshot(),
            parsed.latest_only,
            parsed.no_clear,
        );
    }
}

fn run_batch_loop(parsed: &WatchArgs, ds: &SqliteDataSource, expr: &nmbrs_metricsql::ast::Expr) {
    loop {
        let now = match latest_sample_ts(&parsed.db_path) {
            Ok(Some(ts)) => ts,
            _ => 0,
        };
        let start = now - parsed.warmup_ms;
        let ctx = EvalContext {
            data: ds,
            start_ms: start,
            end_ms: now,
            step_ms: 60_000,
            lookback_ms: None,
            query_start_ms: Some(start),
            query_end_ms: Some(now),
        };
        match evaluate(&ctx, expr) {
            Ok(series) => redraw(&parsed.query, &series, parsed.latest_only, parsed.no_clear),
            Err(e) => {
                eprintln!("nmbrs metrics watch: evaluate: {e}");
                std::process::exit(2);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(parsed.interval_ms as u64));
    }
}

#[derive(Debug)]
struct WatchArgs {
    db_path: PathBuf,
    query: String,
    interval_ms: i64,
    /// Initial backfill window — how far back to ingest on
    /// startup so the streaming engine has data on its
    /// first snapshot.
    warmup_ms: i64,
    latest_only: bool,
    no_clear: bool,
}

fn parse_watch_args(args: &[String]) -> Result<WatchArgs, String> {
    let mut db_path: Option<PathBuf> = None;
    let mut query: Option<String> = None;
    let mut interval_ms: i64 = 5_000; // 5 seconds
    let mut warmup_ms: i64 = 5 * 60_000; // 5 minutes
    let mut latest_only = true;
    let mut no_clear = false;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--db" => {
                db_path = iter.next().map(PathBuf::from);
            }
            other if other.starts_with("--db=") => {
                db_path = Some(PathBuf::from(&other[5..]));
            }
            "--interval" => {
                let v = iter.next().ok_or("--interval requires a duration")?;
                interval_ms = parse_duration_ms(v)?;
                if interval_ms <= 0 {
                    return Err("--interval must be positive".into());
                }
            }
            "--warmup" => {
                let v = iter.next().ok_or("--warmup requires a duration")?;
                warmup_ms = parse_duration_ms(v)?;
                if warmup_ms < 0 {
                    return Err("--warmup must be non-negative".into());
                }
            }
            "--all-samples" => {
                latest_only = false;
            }
            "--no-clear" => {
                no_clear = true;
            }
            "--session"
            | "--session-name"
            | "--session-path"
            | "--session-reuse"
            | "--session-keep"
            | "--session-shelflife" => {
                let _ = iter.next();
            }
            other if other.starts_with("--session") => {}
            other if !other.starts_with("--") => {
                if query.is_some() {
                    return Err(format!("unexpected positional argument {other:?}"));
                }
                query = Some(other.to_string());
            }
            other => {
                return Err(format!("unknown flag '{other}'"));
            }
        }
    }
    let query = query.ok_or("metricsql expression required (positional argument)")?;
    let db_path = resolve_db(db_path, args);
    Ok(WatchArgs {
        db_path,
        query,
        interval_ms,
        warmup_ms,
        latest_only,
        no_clear,
    })
}

fn redraw(query: &str, series: &[Series], latest_only: bool, no_clear: bool) {
    if !no_clear {
        // ANSI: clear screen + cursor home. `\x1b[2J\x1b[H`.
        // Fall back gracefully on dumb terminals — the
        // escape codes are visible but harmless.
        print!("\x1b[2J\x1b[H");
    }
    println!("# {query}  @ {}", chrono_like_now());
    println!();
    emit(series, latest_only);
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    // Avoid a chrono dep — render as `<unix-secs>` so
    // operators can pipe through `date -d @<n>` if they
    // want a human-readable form.
    format!("{secs}")
}

pub fn print_watch_usage() {
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  nmbrs metrics watch [<flags>] <metricsql-expression>");
    eprintln!();
    eprintln!("Live-update a metricsql query on a polling interval.");
    eprintln!("Uses the streaming engine when the query shape supports it,");
    eprintln!("batch eval otherwise.");
    eprintln!();
    eprintln!("Flags:");
    eprintln!("  --db <path>                  Override metrics db.");
    eprintln!("  --interval <duration>        Poll cadence. Default: 5s.");
    eprintln!("  --warmup <duration>          Backfill window on startup.");
    eprintln!("                               Default: 5m.");
    eprintln!("  --all-samples                Emit every sample (range mode).");
    eprintln!("  --no-clear                   Don't clear-screen between ticks.");
}

pub fn print_usage() {
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  nmbrs metrics query [<flags>] <metricsql-expression>");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  nmbrs metrics query 'cycles_total'");
    eprintln!("  nmbrs metrics query 'sum(cycles_total) by (op)'");
    eprintln!("  nmbrs metrics query --lookback 5m --step 30s 'rate(cycles_total[1m])'");
    eprintln!("  nmbrs metrics query 'max(latency_p99) by (host)'");
    eprintln!();
    eprintln!("Flags:");
    eprintln!("  --db <path>                  Override metrics db.");
    eprintln!("                               Default: logs/latest/metrics.db");
    eprintln!("  --at <ms>                    Query anchor as Unix ms.");
    eprintln!("                               Default: latest sample in db.");
    eprintln!("  --lookback <duration>        Range query window before anchor.");
    eprintln!("                               Default: 0 (instant query).");
    eprintln!("  --stale-window <duration>    PromQL stale-tolerance for instant.");
    eprintln!("                               Selector picks the latest sample");
    eprintln!("                               within this window before T.");
    eprintln!("                               Default: 5m.");
    eprintln!("  --step <duration>            Step size for range queries.");
    eprintln!("                               Default: 1m.");
    eprintln!("  --all-samples                Emit every sample in the window.");
    eprintln!("                               Default: latest sample per series.");
    eprintln!("  --range <dur|all>            Apply the metricsql `[<dur>]` range");
    eprintln!("                               to the selector. `all` = the whole");
    eprintln!("                               test span (every series); or a");
    eprintln!("                               reporting cadence, e.g. 1m. Tab-");
    eprintln!("                               completes to `all` + this session's");
    eprintln!("                               cadences.");
    eprintln!("  --family <name>              Add a metric family (repeatable;");
    eprintln!("                               tab-completes). Each family's series");
    eprintln!("                               are shown independently — NOT");
    eprintln!("                               aggregated. Bare positionals work");
    eprintln!("                               too (only the first tab-completes).");
    eprintln!();
    eprintln!("Durations: 5m, 1h30m, 500ms, 2.5s. Bare numbers are seconds.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parser_accepts_units_and_chains() {
        assert_eq!(parse_duration_ms("0").unwrap(), 0);
        assert_eq!(parse_duration_ms("30").unwrap(), 30_000);
        assert_eq!(parse_duration_ms("500ms").unwrap(), 500);
        assert_eq!(parse_duration_ms("2.5s").unwrap(), 2500);
        assert_eq!(parse_duration_ms("5m").unwrap(), 300_000);
        assert_eq!(parse_duration_ms("1h30m").unwrap(), 5_400_000);
        assert!(parse_duration_ms("foo").is_err());
        assert!(parse_duration_ms("5z").is_err());
    }

    #[test]
    fn label_format_lifts_name_and_sorts_others() {
        let labels = vec![
            ("zone".to_string(), "z2".to_string()),
            ("__name__".to_string(), "cpu".to_string()),
            ("host".to_string(), "h1".to_string()),
        ];
        assert_eq!(format_labels(&labels), r#"cpu{host="h1",zone="z2"}"#);
    }

    #[test]
    fn label_format_handles_no_name() {
        let labels = vec![
            ("zone".to_string(), "z1".to_string()),
            ("host".to_string(), "h1".to_string()),
        ];
        assert_eq!(format_labels(&labels), r#"{host="h1",zone="z1"}"#);
    }

    #[test]
    fn label_format_escapes_quotes_and_backslashes() {
        let labels = vec![("k".to_string(), r#"a"b\c"#.to_string())];
        assert_eq!(format_labels(&labels), r#"{k="a\"b\\c"}"#);
    }

    #[test]
    fn arg_parser_picks_up_flags_and_positional() {
        let args: Vec<String> = ["--db", "/tmp/x.db", "--lookback", "5m", "sum(cpu)"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = parse_args(&args).unwrap();
        assert_eq!(p.db_path, PathBuf::from("/tmp/x.db"));
        assert_eq!(p.lookback_ms, 300_000);
        assert_eq!(p.queries, vec!["sum(cpu)".to_string()]);
    }

    #[test]
    fn arg_parser_collects_multiple_family_positionals() {
        // Multiple bare family names → one query each, merged at eval.
        let args: Vec<String> = ["attempt_total", "result_total", "--range", "all"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = parse_args(&args).unwrap();
        assert_eq!(
            p.queries,
            vec!["attempt_total".to_string(), "result_total".to_string()]
        );
        assert_eq!(p.range.as_deref(), Some("all"));
    }

    #[test]
    fn arg_parser_rejects_missing_query() {
        let args: Vec<String> = ["--db", "/tmp/x.db"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = parse_args(&args).unwrap_err();
        assert!(err.contains("required"));
    }

    #[test]
    fn watch_args_pick_up_interval_and_warmup() {
        let args: Vec<String> = ["--interval", "10s", "--warmup", "30m", "rate(cpu[1m])"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = parse_watch_args(&args).unwrap();
        assert_eq!(p.interval_ms, 10_000);
        assert_eq!(p.warmup_ms, 30 * 60_000);
        assert_eq!(p.query, "rate(cpu[1m])");
        assert!(p.latest_only);
        assert!(!p.no_clear);
    }

    #[test]
    fn watch_args_reject_zero_interval() {
        let args: Vec<String> = ["--interval", "0", "rate(cpu[1m])"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = parse_watch_args(&args).unwrap_err();
        assert!(err.contains("--interval"));
    }

    #[test]
    fn arg_parser_accepts_multiple_positional_families() {
        // Multiple bare positionals are now distinct families (each
        // evaluated as its own selector, series shown independently —
        // not aggregated), not an error.
        let args: Vec<String> = ["attempt_total", "result_total", "cycles_total"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = parse_args(&args).unwrap();
        assert_eq!(
            p.queries,
            vec![
                "attempt_total".to_string(),
                "result_total".to_string(),
                "cycles_total".to_string(),
            ]
        );
    }

    #[test]
    fn arg_parser_collects_repeatable_family_flag() {
        let args: Vec<String> = ["--family", "attempt_total", "--family", "attempt_success"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = parse_args(&args).unwrap();
        assert_eq!(
            p.queries,
            vec!["attempt_total".to_string(), "attempt_success".to_string()]
        );
    }
}
