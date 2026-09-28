// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Sparse per-op tracing log for the cassandra-cpp engine.
//!
//! When the dynamic `cql_trace_rate` control is set above zero,
//! each dispenser rolls per-execute against the rate and (on
//! hit) calls
//! [`cassandra_cpp::Statement::set_tracing`](https://docs.rs/cassandra-cpp/3.0.2/cassandra_cpp/struct.Statement.html#method.set_tracing)
//! on the bound statement before executing it. After the execute
//! completes, the dispenser submits a [`TraceRecord`] describing
//! the traced op to this module's bounded retirement queue. A
//! single worker task drains the queue, follows up by querying
//! `system_traces.sessions` / `system_traces.events` for the
//! server-side trace data, and writes one JSONL record combining
//! the client-side metadata with the server-side session +
//! events fields.
//!
//! # Server-side trace fetch
//!
//! For every record with a non-`None` `trace_id`, the worker
//! issues two prepared statements (prepared once at worker start):
//!
//! 1. `SELECT coordinator, duration, started_at, request, \
//!     parameters, client FROM system_traces.sessions \
//!     WHERE session_id = ?` — exactly one row.
//! 2. `SELECT event_id, source, source_elapsed, activity, thread \
//!     FROM system_traces.events WHERE session_id = ?` — many
//!    rows, ordered by `event_id`.
//!
//! C* flushes events to `system_traces` asynchronously after the
//! response is returned, so on a fast worker we can race the
//! flush. The events query retries up to 5 times with 50ms sleep
//! when either zero rows come back or the last `activity` does
//! not look like a request-complete marker. After the budget,
//! whatever has been collected is logged — partial events are
//! more useful than none.
//!
//! If preparing either statement fails at startup (cluster has
//! tracing disabled, `system_traces` schema absent, etc.), the
//! worker logs a `Warn` and falls back to client-side-only mode
//! for the rest of its lifetime: still writes records, just
//! without the `session` / `events` fields. The worker never
//! exits because of trace-fetch failures — this log is
//! best-effort instrumentation.
//!
//! # Backpressure (operator-visible)
//!
//! - **Queue capacity**: 5 records.
//! - **Worker concurrency**: 1 (one trace retired at a time).
//! - **Saturation** (queue depth ≥ 1 at submit): one `Warn`-level
//!   diag per occurrence, telling the operator the worker is
//!   falling behind. With the server-side fetch path one slow
//!   record can take 250ms+ to retire under retry pressure;
//!   under high `cql_trace_rate` the operator should expect more
//!   saturation warnings.
//! - **Queue full** (capacity reached): one `Error`-level diag
//!   per occurrence and the trace record is dropped (not
//!   silently — the count is included in the diag).

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cass::LendingIterator;
use cassandra_cpp as cass;
use tokio::sync::mpsc;

/// A single traced-op record submitted by a dispenser to the
/// retirement worker. Fields are deliberately minimal — the
/// hot path on a traced op is "build this struct and `try_send`
/// it" — and the JSON shape is stable for downstream tooling.
#[derive(Debug)]
pub(super) struct TraceRecord {
    /// Session-monotonic cycle number (the dispenser's input).
    pub cycle: u64,
    /// Wall-clock at execute start. Serialised as RFC 3339.
    pub started_at: SystemTime,
    /// Query text after bind-point substitution. Truncated at
    /// the configured cap (see [`TraceLog::QUERY_TEXT_CAP`])
    /// before submission so the worker doesn't pay copy cost
    /// on huge prepared bodies.
    pub query: String,
    /// One entry per declared bind point, ordered by position.
    /// See [`format_bind_summary`] for the inline-vs-summary
    /// rule.
    pub binds: Vec<String>,
    /// Service-time in nanoseconds — the time the statement
    /// spent on the wire, matching the dispenser's normal
    /// `cycles_servicetime` accounting.
    pub latency_nanos: u64,
    /// `true` when the underlying `Statement::execute` returned
    /// `Ok`, `false` for any error path (including bind errors
    /// that prevented dispatch). Errors don't produce server-
    /// side traces; the record still goes into the log so
    /// operators can correlate "tracing flag set; query failed
    /// before reaching the server".
    pub ok: bool,
    /// Optional adapter-error name when `ok=false`.
    pub error_name: Option<String>,
    /// Server-side trace UUID, retrieved via the cassandra-cpp
    /// `cass_future_tracing_id` path exposed by the vendored
    /// crate's `Session::execute_with_tracing`. The partition
    /// key into `system_traces.sessions` /
    /// `system_traces.events`. `None` indicates the cpp-driver
    /// returned `CASS_ERROR_LIB_NO_TRACING_ID` even though we
    /// requested tracing — usually means the request never
    /// reached a coordinator.
    pub trace_id: Option<String>,
}

/// Soft cap on per-bind-summary inlining. Bind values shorter
/// than this go into the JSON record verbatim; longer ones get
/// summarised as `<type=…, len=…>`. Tunable later via a
/// workload param if anyone has a use case for it.
const BIND_INLINE_CAP: usize = 96;

/// Render one bind value as a single string for the JSON
/// record. Short values inline as text; long ones summarise.
/// The shape is intentionally human-readable — this is a debug
/// log, not a re-execution transcript.
pub(super) fn format_bind_value(name: &str, value: &polydat::ast::Value) -> String {
    use polydat::ast::Value;
    let summary = |kind: &str, len: usize| format!("{name}=<{kind}, len={len}>");
    match value {
        Value::Str(s) => {
            if s.len() <= BIND_INLINE_CAP {
                format!("{name}={s:?}")
            } else {
                summary("str", s.len())
            }
        }
        Value::U64(n) => format!("{name}={n}"),
        Value::F64(n) => format!("{name}={n}"),
        Value::Bool(b) => format!("{name}={b}"),
        Value::VecF32(v) => {
            let len = v.as_slice().len();
            if len * 8 <= BIND_INLINE_CAP {
                format!("{name}={:?}", v.as_slice())
            } else {
                summary("vec_f32", len)
            }
        }
        Value::VecI32(v) => {
            let len = v.as_slice().len();
            if len * 4 <= BIND_INLINE_CAP {
                format!("{name}={:?}", v.as_slice())
            } else {
                summary("vec_i32", len)
            }
        }
        Value::None => format!("{name}=<unset>"),
        // Catch-all for handle / less-common variants. The
        // shape stays grep-friendly without requiring a
        // `kind_name` helper on the Value enum.
        _ => format!("{name}=<other>"),
    }
}

/// Submission handle held by every dispenser. Cheap to clone;
/// dispensers do an atomic load on the rate, RNG roll, and (on
/// hit) `try_send` a [`TraceRecord`] through this handle. The
/// retirement worker owns the receiver and the file.
#[derive(Clone)]
pub(super) struct TraceLog {
    inner: Arc<TraceLogInner>,
}

struct TraceLogInner {
    /// Bounded mpsc, capacity = [`TraceLog::QUEUE_CAPACITY`].
    /// `try_send` is the only send path — never block a
    /// dispenser fiber on tracing.
    tx: mpsc::Sender<TraceRecord>,
    /// Live depth count for saturation diagnostics. Atomic
    /// rather than `tx.len()` so we can compare-and-warn on
    /// the producer side without contending with the consumer.
    queue_depth: AtomicU64,
    /// Cumulative count of dropped records (queue-full). Used
    /// for the per-occurrence error diag and surfaced via
    /// [`Self::dropped_count`] for tests.
    dropped: AtomicU64,
}

/// Lazily-initialised [`TraceLog`] shared by every dispenser of an adapter.
///
/// Tracing is OFF by default (`cql_trace_rate == 0`), so opening the log at
/// adapter connect touched the cluster (preparing `system_traces` queries)
/// and the filesystem (creating the JSONL artifact) for a feature nobody had
/// asked for — and that prepare, issued over the just-connected main session
/// against a cold/remote node, is exactly where a `LIB_REQUEST_TIMED_OUT`
/// surfaces. Worse, a failed startup prepare was cached forever, silently
/// disabling server-side enrichment even if the operator later raised
/// `cql_trace_rate`.
///
/// This defers ALL of that — file open, worker spawn, and the
/// `system_traces` prepare — to the FIRST op that is actually traced. With
/// tracing off nothing is opened, spawned, or prepared; when it is turned on,
/// init happens against a warmed-up session at the moment traces are wanted.
pub(super) struct LazyTraceLog {
    path: PathBuf,
    session: cass::Session,
    /// `None` inside the cell = init ran but failed (logged once); the outer
    /// `OnceCell` distinguishes "not yet initialised" from "initialised".
    cell: tokio::sync::OnceCell<Option<TraceLog>>,
}

impl LazyTraceLog {
    /// Record the inputs; do NOT touch the cluster or filesystem. Cheap and
    /// side-effect-free — safe to build at connect regardless of trace rate.
    pub(super) fn new(path: PathBuf, session: cass::Session) -> Self {
        Self {
            path,
            session,
            cell: tokio::sync::OnceCell::new(),
        }
    }

    /// The initialised [`TraceLog`], creating it on first call: open the
    /// append file and spawn the retirement worker (which prepares the
    /// `system_traces` queries in the background). Returns `None` when init
    /// failed — the file couldn't be opened — after logging once. The
    /// per-op hot path is one `OnceCell` load once initialised.
    pub(super) async fn get(&self) -> Option<&TraceLog> {
        self.cell
            .get_or_init(|| async {
                match TraceLog::open(self.path.clone(), self.session.clone()) {
                    Ok(log) => Some(log),
                    Err(e) => {
                        nmbrs_runtime::observer::log(
                            nmbrs_runtime::observer::LogLevel::Warn,
                            &format!(
                                "cql tracing log unavailable at {}: {} \
                                 — traces for this run will be dropped",
                                self.path.display(),
                                e,
                            ),
                        );
                        None
                    }
                }
            })
            .await
            .as_ref()
    }
}

impl TraceLog {
    /// Bounded queue size. Five matches the SRD-23 spec: above
    /// this, the producer escalates from a saturation warning
    /// to a queue-full error and drops the record.
    pub const QUEUE_CAPACITY: usize = 5;

    /// Hard cap on the query-text field. The cpp-driver doesn't
    /// limit prepared-statement size, but our log is for
    /// debugging and a 64KB SELECT serialised per traced op
    /// would saturate the worker. Truncated past this with a
    /// `…` marker.
    pub const QUERY_TEXT_CAP: usize = 4096;

    /// Build a trace log writer. `path` is the absolute path
    /// the worker appends to (caller resolves the default vs
    /// `trace_log=` workload param). `session` is the live
    /// cpp-driver session — the worker uses it to issue the
    /// follow-up `system_traces.sessions` / `system_traces.events`
    /// queries that turn each `trace_id` into the inlined server-
    /// side trace data.
    ///
    /// The worker is spawned on a tokio task tied to the
    /// current runtime, so this must be called inside one
    /// (e.g. from `CqlAdapter::connect`).
    pub fn open(path: PathBuf, session: cass::Session) -> std::io::Result<Self> {
        // Pre-create the parent dir; many session paths nest
        // under `logs/<sid>/` which the runner has already made,
        // but a workload-supplied absolute path may not.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // OpenOptions append-create so multiple runs don't
        // clobber each other when pointed at the same log.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let (tx, rx) = mpsc::channel::<TraceRecord>(Self::QUEUE_CAPACITY);
        let inner = Arc::new(TraceLogInner {
            tx,
            queue_depth: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let inner_for_worker = inner.clone();
        // Single retirement task — the spec says one trace
        // retired at a time. Spawned on the existing tokio
        // runtime; finishes when every Sender clone is dropped.
        tokio::spawn(retirement_worker(rx, file, inner_for_worker, session));
        Ok(Self { inner })
    }

    /// Submit a record. Non-blocking: returns immediately even
    /// when the queue is saturated, after emitting the right
    /// diag for the operator.
    ///
    /// The two thresholds:
    ///
    /// - Depth ≥ 1 at submit time → `Warn` ("worker falling
    ///   behind"). Distinct from queue-full because the record
    ///   is still accepted; we just took on backlog.
    /// - `try_send` returned `Full` → `Error` ("queue full,
    ///   dropping record"). The cumulative drop count is in
    ///   the message so the operator can see how often it's
    ///   firing without grepping by hand.
    pub fn submit(&self, record: TraceRecord) {
        let prev_depth = self.inner.queue_depth.load(Ordering::Acquire);
        match self.inner.tx.try_send(record) {
            Ok(()) => {
                let new_depth = self.inner.queue_depth.fetch_add(1, Ordering::AcqRel) + 1;
                if prev_depth >= 1 {
                    nmbrs_runtime::diag!(
                        nmbrs_runtime::observer::LogLevel::Warn,
                        "cql_trace: retirement worker behind \
                         (queue_depth={new_depth}/{cap})",
                        cap = Self::QUEUE_CAPACITY,
                    );
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                let dropped = self.inner.dropped.fetch_add(1, Ordering::AcqRel) + 1;
                nmbrs_runtime::diag!(
                    nmbrs_runtime::observer::LogLevel::Error,
                    "cql_trace: queue full at capacity {cap}, \
                     dropping trace record (cumulative drops: {dropped})",
                    cap = Self::QUEUE_CAPACITY,
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Worker has exited. Stay quiet — the worker
                // only closes on adapter shutdown, and emitting
                // a diag at that point would race with teardown.
            }
        }
    }
}

// =========================================================================
// Server-side trace fetch
// =========================================================================

/// Decoded `system_traces.sessions` row. All fields optional —
/// the cpp-driver returns errors for absent / null columns and
/// some Cassandra forks drop columns we don't expect to be
/// missing on stock C*.
#[derive(Default)]
struct SessionMeta {
    coordinator: Option<String>,
    duration_us: Option<i64>,
    started_at_ms: Option<i64>,
    request: Option<String>,
    parameters: Vec<(String, String)>,
    client: Option<String>,
}

/// Decoded `system_traces.events` row.
struct EventRow {
    source: Option<String>,
    source_elapsed_us: Option<i64>,
    activity: Option<String>,
    thread: Option<String>,
}

/// Maximum retries for the events query. Cassandra flushes
/// events asynchronously after the response is returned, so on
/// a fast worker we can race the flush. With 50ms sleeps we
/// give up to ~250ms total before giving up and writing
/// whatever we have. See module docs.
const EVENTS_MAX_RETRIES: u32 = 5;
/// Sleep between events-query retries.
const EVENTS_RETRY_SLEEP: Duration = Duration::from_millis(50);

/// True if `activity` looks like a request-complete marker — at
/// which point we know Cassandra has flushed all events for
/// this trace. Match is case-insensitive on either of the two
/// known shapes (`"complete"` substring covers both
/// `"Request complete"` and the older `"Request is complete"`).
fn is_complete_marker(activity: &str) -> bool {
    activity.to_ascii_lowercase().contains("complete")
}

/// Fetch the single `system_traces.sessions` row. Returns
/// `None` only when the row genuinely doesn't exist (transient
/// flush race that the worker doesn't currently retry — the
/// session row is written before the response is returned, so
/// this path is rare in practice).
async fn fetch_session_meta(
    stmt: &cass::PreparedStatement,
    trace_uuid: cass::Uuid,
) -> Result<Option<SessionMeta>, cass::Error> {
    let mut s = stmt.bind();
    s.set_consistency(cass::Consistency::ONE).ok();
    s.bind_uuid(0, trace_uuid)?;
    let result = s.execute().await?;
    let mut iter = result.iter();
    let Some(row) = iter.next() else {
        return Ok(None);
    };
    let mut meta = SessionMeta::default();
    // Index order matches the prepared SELECT:
    //   0=coordinator 1=duration 2=started_at 3=request 4=parameters 5=client
    if let Ok(c) = row.get_column(0)
        && let Ok(inet) = c.get_inet()
    {
        meta.coordinator = Some(format!("{inet:?}"));
    }
    if let Ok(c) = row.get_column(1)
        && let Ok(n) = c.get_i32()
    {
        meta.duration_us = Some(n as i64);
    }
    if let Ok(c) = row.get_column(2)
        && let Ok(n) = c.get_i64()
    {
        // `started_at` is a CQL timestamp — milliseconds since
        // unix epoch as i64 in cassandra-cpp's wire decoder.
        meta.started_at_ms = Some(n);
    }
    if let Ok(c) = row.get_column(3)
        && let Ok(s) = c.get_string()
    {
        meta.request = Some(s);
    }
    if let Ok(c) = row.get_column(4)
        && let Ok(mut map) = c.get_map()
    {
        while let Some((k, v)) = map.next() {
            let ks = k.get_string().unwrap_or_default();
            let vs = v.get_string().unwrap_or_default();
            meta.parameters.push((ks, vs));
        }
    }
    if let Ok(c) = row.get_column(5)
        && let Ok(inet) = c.get_inet()
    {
        meta.client = Some(format!("{inet:?}"));
    }
    Ok(Some(meta))
}

/// Fetch all `system_traces.events` rows for a trace, retrying
/// when the result looks incomplete (zero rows or last row's
/// `activity` doesn't end with a complete-marker). Returns
/// whatever was last seen, even if incomplete after the budget.
async fn fetch_events(
    stmt: &cass::PreparedStatement,
    trace_uuid: cass::Uuid,
) -> Result<(Vec<EventRow>, u32), cass::Error> {
    let mut last: Vec<EventRow> = Vec::new();
    let mut retries = 0u32;
    for attempt in 0..=EVENTS_MAX_RETRIES {
        let mut s = stmt.bind();
        s.set_consistency(cass::Consistency::ONE).ok();
        s.bind_uuid(0, trace_uuid)?;
        let result = s.execute().await?;
        // Index order matches the prepared SELECT:
        //   0=event_id 1=source 2=source_elapsed 3=activity 4=thread
        let mut events: Vec<EventRow> = Vec::new();
        let mut iter = result.iter();
        while let Some(row) = iter.next() {
            let source = row
                .get_column(1)
                .ok()
                .and_then(|c| c.get_inet().ok())
                .map(|i| format!("{i:?}"));
            let source_elapsed_us = row
                .get_column(2)
                .ok()
                .and_then(|c| c.get_i32().ok())
                .map(|n| n as i64);
            let activity = row.get_column(3).ok().and_then(|c| c.get_string().ok());
            let thread = row.get_column(4).ok().and_then(|c| c.get_string().ok());
            events.push(EventRow {
                source,
                source_elapsed_us,
                activity,
                thread,
            });
        }
        let complete = events
            .last()
            .and_then(|e| e.activity.as_deref())
            .map(is_complete_marker)
            .unwrap_or(false);
        last = events;
        if complete {
            return Ok((last, retries));
        }
        if attempt < EVENTS_MAX_RETRIES {
            retries += 1;
            tokio::time::sleep(EVENTS_RETRY_SLEEP).await;
        }
    }
    Ok((last, retries))
}

/// The single-worker retirement loop. One pull at a time,
/// JSON-line write per record, decrement the depth counter so
/// producers see fresh saturation accurately. Exits cleanly
/// when every `Sender` clone has been dropped (i.e. when the
/// adapter and its dispensers are gone).
///
/// Prepares the two `system_traces` queries at startup. If
/// either fails (cluster has tracing disabled, schema absent,
/// etc.) the worker logs a `Warn` and continues in client-side-
/// only mode for the rest of its lifetime.
async fn retirement_worker(
    mut rx: mpsc::Receiver<TraceRecord>,
    mut file: std::fs::File,
    inner: Arc<TraceLogInner>,
    session: cass::Session,
) {
    // Prepare the two follow-up queries once at startup. If
    // either fails the cluster doesn't have `system_traces` (or
    // it's not readable as the configured user) — fall back to
    // client-side-only mode and don't pester the operator with
    // every subsequent record.
    let prepared = match prepare_trace_queries(&session).await {
        Ok(p) => Some(p),
        Err(e) => {
            nmbrs_runtime::diag!(
                nmbrs_runtime::observer::LogLevel::Warn,
                "cql_trace: failed to prepare system_traces queries: {e} \
                 — falling back to client-side-only trace records",
            );
            None
        }
    };

    while let Some(record) = rx.recv().await {
        inner.queue_depth.fetch_sub(1, Ordering::AcqRel);

        // Per-record server-side fetch. Skipped when
        // `trace_id` is None (cpp-driver returned NO_TRACING_ID)
        // or when prepares failed at startup.
        let (session_meta, events) = match (&prepared, record.trace_id.as_deref()) {
            (Some(p), Some(tid_str)) => fetch_server_side(p, tid_str).await,
            _ => (None, Vec::new()),
        };

        // The record is JSON-serialised manually so we don't
        // pull serde_json into the cassandra-cpp module just
        // for two scalar-shaped writes. Format is stable: one
        // top-level object per line, escaping per JSON spec.
        let line = format_record_jsonl(&record, session_meta.as_ref(), &events);
        if let Err(e) = file.write_all(line.as_bytes()) {
            nmbrs_runtime::diag!(
                nmbrs_runtime::observer::LogLevel::Error,
                "cql_trace: write failed: {e} \
                 — disabling further trace logging",
            );
            // No useful retry — bail out of the worker. Future
            // submits will see a closed channel and fall into
            // the "Closed" arm of `submit`, which is silent.
            return;
        }
    }
}

/// Pair of prepared `system_traces` statements held by the
/// worker.
struct PreparedTraceQueries {
    sessions: cass::PreparedStatement,
    events: cass::PreparedStatement,
}

async fn prepare_trace_queries(
    session: &cass::Session,
) -> Result<PreparedTraceQueries, cass::Error> {
    let sessions = session
        .prepare(
            "SELECT coordinator, duration, started_at, request, \
         parameters, client \
         FROM system_traces.sessions WHERE session_id = ?",
        )
        .await?;
    let events = session
        .prepare(
            "SELECT event_id, source, source_elapsed, activity, thread \
         FROM system_traces.events WHERE session_id = ?",
        )
        .await?;
    Ok(PreparedTraceQueries { sessions, events })
}

/// Run both follow-up queries for one record. Errors are
/// logged at `Warn` and swallowed — the trace log is best-
/// effort instrumentation; never stop the workload over a
/// failed `system_traces` SELECT.
async fn fetch_server_side(
    prepared: &PreparedTraceQueries,
    trace_id_str: &str,
) -> (Option<SessionMeta>, Vec<EventRow>) {
    // Round-trip the string trace_id through `uuid::Uuid` to the
    // cpp-driver's wrapper type — the binder takes the wrapper
    // and the dispensers stored the trace_id as a plain string
    // for cheap mpsc copy.
    let parsed = match uuid::Uuid::parse_str(trace_id_str) {
        Ok(u) => u,
        Err(e) => {
            nmbrs_runtime::diag!(
                nmbrs_runtime::observer::LogLevel::Warn,
                "cql_trace: invalid trace_id '{trace_id_str}': {e}",
            );
            return (None, Vec::new());
        }
    };
    let cass_uuid: cass::Uuid = parsed.into();

    let session_meta = match fetch_session_meta(&prepared.sessions, cass_uuid).await {
        Ok(m) => m,
        Err(e) => {
            nmbrs_runtime::diag!(
                nmbrs_runtime::observer::LogLevel::Warn,
                "cql_trace: system_traces.sessions fetch failed for {trace_id_str}: {e}",
            );
            None
        }
    };
    let (events, retries) = match fetch_events(&prepared.events, cass_uuid).await {
        Ok(p) => p,
        Err(e) => {
            nmbrs_runtime::diag!(
                nmbrs_runtime::observer::LogLevel::Warn,
                "cql_trace: system_traces.events fetch failed for {trace_id_str}: {e}",
            );
            (Vec::new(), 0)
        }
    };
    if retries > 0 {
        nmbrs_runtime::diag!(
            nmbrs_runtime::observer::LogLevel::Debug,
            "cql_trace: events retry fired {retries}x for {trace_id_str} \
             (events flushed asynchronously by Cassandra)",
        );
    }
    (session_meta, events)
}

// =========================================================================
// JSONL formatting
// =========================================================================

/// Serialise a [`TraceRecord`] (plus optional server-side
/// session + events data) as one JSON object plus a trailing
/// newline. Manual hand-roll to avoid pulling serde into this
/// module for a handful of writes per traced op.
fn format_record_jsonl(
    record: &TraceRecord,
    session_meta: Option<&SessionMeta>,
    events: &[EventRow],
) -> String {
    // Emit ts as nanoseconds since the unix epoch — `date -d
    // @<seconds>` and most JSON tooling round-trip this without
    // requiring a humantime / chrono dep on the cassandra-cpp
    // adapter. Pre-1970 timestamps (impossible here) clamp to 0.
    let started_unix_nanos = record
        .started_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let query = if record.query.len() > TraceLog::QUERY_TEXT_CAP {
        format!("{}…", &record.query[..TraceLog::QUERY_TEXT_CAP])
    } else {
        record.query.clone()
    };
    let mut s = String::with_capacity(512 + query.len());
    s.push('{');
    fmt_kv_u64(&mut s, "ts_ns", started_unix_nanos, /*first=*/ true);
    fmt_kv_u64(&mut s, "cycle", record.cycle, false);
    fmt_kv_u64(&mut s, "latency_ns", record.latency_nanos, false);
    fmt_kv_bool(&mut s, "ok", record.ok, false);
    if let Some(err) = &record.error_name {
        fmt_kv_str(&mut s, "error_name", err, false);
    }
    if let Some(tid) = &record.trace_id {
        fmt_kv_str(&mut s, "trace_id", tid, false);
    }
    fmt_kv_str(&mut s, "query", &query, false);
    s.push_str(",\"binds\":[");
    for (i, bind) in record.binds.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        write_escaped(&mut s, bind);
        s.push('"');
    }
    s.push(']');
    if let Some(meta) = session_meta {
        s.push_str(",\"session\":");
        write_session_meta(&mut s, meta);
    }
    if !events.is_empty() {
        s.push_str(",\"events\":[");
        for (i, ev) in events.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            write_event(&mut s, ev);
        }
        s.push(']');
    }
    s.push('}');
    s.push('\n');
    s
}

/// Emit one `system_traces.sessions` row as a JSON object.
/// Every field is conditional — clusters drop columns and the
/// cpp-driver returns errors for nulls, so we only emit what
/// decoded successfully.
fn write_session_meta(out: &mut String, meta: &SessionMeta) {
    out.push('{');
    let mut first = true;
    if let Some(v) = &meta.coordinator {
        fmt_kv_str(out, "coordinator", v, first);
        first = false;
    }
    if let Some(v) = meta.duration_us {
        fmt_kv_i64(out, "duration", v, first);
        first = false;
    }
    if let Some(v) = meta.started_at_ms {
        fmt_kv_i64(out, "started_at_ms", v, first);
        first = false;
    }
    if let Some(v) = &meta.request {
        fmt_kv_str(out, "request", v, first);
        first = false;
    }
    if !meta.parameters.is_empty() {
        if !first {
            out.push(',');
        }
        out.push_str("\"parameters\":{");
        for (i, (k, v)) in meta.parameters.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            write_escaped(out, k);
            out.push_str("\":\"");
            write_escaped(out, v);
            out.push('"');
        }
        out.push('}');
        first = false;
    }
    if let Some(v) = &meta.client {
        fmt_kv_str(out, "client", v, first);
    }
    out.push('}');
}

/// Emit one `system_traces.events` row as a JSON object.
fn write_event(out: &mut String, ev: &EventRow) {
    out.push('{');
    let mut first = true;
    if let Some(v) = &ev.source {
        fmt_kv_str(out, "source", v, first);
        first = false;
    }
    if let Some(v) = ev.source_elapsed_us {
        fmt_kv_i64(out, "source_elapsed", v, first);
        first = false;
    }
    if let Some(v) = &ev.activity {
        fmt_kv_str(out, "activity", v, first);
        first = false;
    }
    if let Some(v) = &ev.thread {
        fmt_kv_str(out, "thread", v, first);
    }
    out.push('}');
}

/// JSON-escape and append `value` (no surrounding quotes).
fn write_escaped(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

fn fmt_kv_str(out: &mut String, key: &str, value: &str, first: bool) {
    if !first {
        out.push(',');
    }
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    write_escaped(out, value);
    out.push('"');
}

fn fmt_kv_u64(out: &mut String, key: &str, value: u64, first: bool) {
    if !first {
        out.push(',');
    }
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    out.push_str(&value.to_string());
}

fn fmt_kv_i64(out: &mut String, key: &str, value: i64, first: bool) {
    if !first {
        out.push(',');
    }
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    out.push_str(&value.to_string());
}

fn fmt_kv_bool(out: &mut String, key: &str, value: bool, first: bool) {
    if !first {
        out.push(',');
    }
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    out.push_str(if value { "true" } else { "false" });
}
