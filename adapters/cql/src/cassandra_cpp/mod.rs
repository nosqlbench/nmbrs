// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! CQL/Cassandra adapter for nmbrs.
//!
//! Uses the Apache Cassandra C++ driver via the `cassandra-cpp`
//! crate. Compatible with Apache Cassandra, ScyllaDB, and DataStax
//! Astra.
//!
//! The engine-agnostic surface — config parsing, consistency enum,
//! op-mode dispatch, the `cql_timeuuid` Polydat node, default status
//! metrics — lives in [`crate::common`]. This module only contains
//! the cassandra-cpp-specific pieces: connection setup, the three
//! dispenser shapes, and the type-aware value binders.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use cass::LendingIterator;
use cassandra_cpp as cass;

mod binder_meta;
mod op_modifier;
mod settings;
mod tracing;
use tracing::{LazyTraceLog, TraceRecord};

/// One-shot guard for the cpp-driver's process-global log level.
/// The driver requires `cass_log_set_level` to fire **before** any
/// `cass_cluster_*` / `cass_ssl_*` call; we honor that by setting
/// it on the very first `CqlAdapter::connect` and ignoring any
/// later attempts. Last-write-wins isn't safe here because the
/// driver caches the level at first session creation.
static LOG_LEVEL_INIT: OnceLock<()> = OnceLock::new();

/// Default cpp-driver log threshold. ERROR squelches the noisy
/// "Server-side warning" decoder messages (SAI ANN experimental
/// notices etc.) while still surfacing real connection / auth /
/// driver-internal errors. Override per session via the
/// `cassandra_log_level=` workload param.
const DEFAULT_LOG_LEVEL: cass::LogLevel = cass::LogLevel::ERROR;

fn parse_log_level(s: &str) -> Option<cass::LogLevel> {
    match s.to_ascii_uppercase().as_str() {
        "DISABLED" | "OFF" | "NONE" => Some(cass::LogLevel::DISABLED),
        "CRITICAL" => Some(cass::LogLevel::CRITICAL),
        "ERROR" => Some(cass::LogLevel::ERROR),
        "WARN" | "WARNING" => Some(cass::LogLevel::WARN),
        "INFO" => Some(cass::LogLevel::INFO),
        "DEBUG" => Some(cass::LogLevel::DEBUG),
        "TRACE" => Some(cass::LogLevel::TRACE),
        _ => None,
    }
}

fn apply_log_level_once(params: &HashMap<String, String>) -> Result<(), String> {
    // Decide once: parse the user value (if any) and apply it on
    // the first connect. The OnceLock guard prevents later calls
    // from racing — the cpp-driver doesn't honor level changes
    // after the first session is created anyway.
    let level = match params.get("cassandra_log_level") {
        Some(raw) => parse_log_level(raw).ok_or_else(|| {
            format!(
                "invalid cassandra_log_level '{raw}' — expected one of \
             DISABLED, CRITICAL, ERROR, WARN, INFO, DEBUG, TRACE"
            )
        })?,
        None => DEFAULT_LOG_LEVEL,
    };
    LOG_LEVEL_INIT.get_or_init(|| {
        cass::set_level(level);
    });
    Ok(())
}

use crate::common::{CqlConfig, CqlConsistency, STMT_FIELD_NAMES};
use nmbrs_runtime::adapter::{
    AdapterError, DriverAdapter, ExecutionError, OpDispenser, OpResult, ResultBody,
};
use nmbrs_workload::model::ParsedOp;

// Bridge: `crate::common::CqlConsistency` → `cass::Consistency`.
// Engine-specific because each driver has its own consistency
// enum; the shared type stays driver-agnostic.
fn to_cass_consistency(c: CqlConsistency) -> cass::Consistency {
    match c {
        CqlConsistency::Any => cass::Consistency::ANY,
        CqlConsistency::One => cass::Consistency::ONE,
        CqlConsistency::Two => cass::Consistency::TWO,
        CqlConsistency::Three => cass::Consistency::THREE,
        CqlConsistency::Quorum => cass::Consistency::QUORUM,
        CqlConsistency::All => cass::Consistency::ALL,
        CqlConsistency::LocalQuorum => cass::Consistency::LOCAL_QUORUM,
        CqlConsistency::EachQuorum => cass::Consistency::EACH_QUORUM,
        CqlConsistency::LocalOne => cass::Consistency::LOCAL_ONE,
    }
}

// =========================================================================
// CqlResultBody: native result type for validation and capture
// =========================================================================

/// Native CQL result body carrying typed row data.
///
/// Consumers can downcast via `as_any()` to extract typed column values
/// without JSON round-tripping — used by `ValidatingDispenser` for
/// relevancy measurement.
#[derive(Debug)]
pub struct CqlResultBody {
    /// Returned rows as JSON-compatible maps: column_name → value.
    /// Populated for a result that carries a row-set (a SELECT, or an
    /// LWT `[applied]` acknowledgment); empty for a plain write.
    pub rows: Vec<HashMap<String, serde_json::Value>>,
    /// Rows WRITTEN by this op. A CQL write (INSERT/UPDATE/DELETE, a
    /// whole BATCH, or a DDL ack) returns no row-set, so its result
    /// carries the count of rows it wrote here instead. `element_count`
    /// reports this when there are no returned rows — so a write's
    /// `count` reflects the rows it wrote exactly as a read's reflects
    /// the rows it returned (the two are mutually exclusive). `0` for a
    /// read.
    written_rows: u64,
}

impl CqlResultBody {
    /// Build from a cassandra-cpp CassResult, iterating rows and
    /// columns. Classifies read vs write by the presence of a
    /// result-set schema: a rows result (SELECT/LWT) has one or more
    /// columns, while a write/DDL acknowledgment has zero columns and
    /// no row-set. A read carries its returned rows and writes 0; a
    /// single write/DDL statement acknowledges one written row.
    fn from_cass_result(result: &cass::CassResult) -> Self {
        let col_count = result.column_count() as usize;
        if col_count == 0 {
            // No result-set schema → a write/DDL acknowledgment. This
            // statement wrote one unit (a row for DML, the statement
            // for DDL); its `count` is 1.
            return Self {
                rows: Vec::new(),
                written_rows: 1,
            };
        }
        let row_count = result.row_count() as usize;
        let mut rows = Vec::with_capacity(row_count);
        let mut iter = result.iter();
        while let Some(row) = iter.next() {
            let mut map = HashMap::new();
            for col_idx in 0..col_count {
                let col_name = result.column_name(col_idx).unwrap_or("?").to_string();
                let value = Self::extract_column_value(&row, col_idx);
                map.insert(col_name, value);
            }
            rows.push(map);
        }
        Self {
            rows,
            written_rows: 0,
        }
    }

    /// A write result carrying an explicit rows-written count. Used
    /// where the dispenser knows the count directly — a BATCH of `n`
    /// rows. No returned rows, so `element_count()` reports `n`.
    fn write_ack(rows_written: u64) -> Self {
        Self {
            rows: Vec::new(),
            written_rows: rows_written,
        }
    }

    /// Extract a single column value as serde_json::Value.
    ///
    /// Type accessor order: native numeric / bool first,
    /// `get_string()` last as a fallback for genuine TEXT
    /// columns (and as a safety net for any type cassandra-cpp
    /// happens to stringify). The earlier ordering put
    /// `get_string()` first, which is fragile: for any
    /// cassandra-cpp version where `get_string()` succeeds on a
    /// numeric column (returning the stringified form), our
    /// numbers would silently land as JSON strings and break
    /// downstream arithmetic / metric coercion.
    ///
    /// On the test cluster (Cassandra-converged 5.x / Jolokia
    /// 1.7.1) `get_string()` on a DOUBLE column returns Err, so
    /// the previous order happened to work. The reorder is
    /// defensive — it removes the type-ambiguous fast path that
    /// could swallow scientific-notation doubles like
    /// `9.2178e-07` as `Value::Str("9.2178e-07")` (which polydat
    /// arithmetic would then mishandle).
    ///
    /// For doubles that DO arrive as JSON Number via `get_f64()`,
    /// the f64 is preserved internally — serde_json's
    /// `as_f64()` returns the right number regardless of display
    /// format. Scientific notation only surfaces in the JSON
    /// serialised form for very small / very large values; the
    /// in-memory Value::Number stays an f64.
    fn extract_column_value(row: &cass::Row, col_idx: usize) -> serde_json::Value {
        // Booleans before numeric: cassandra-cpp may coerce
        // bool to int in get_i64/get_i32, hiding the true type.
        if let Ok(v) = row.get_column(col_idx).and_then(|c| c.get_bool()) {
            return serde_json::json!(v);
        }
        // 64-bit signed (BIGINT, COUNTER, TIMESTAMP-as-i64) first
        // so a column with a value that fits in i64 doesn't
        // get coerced down to i32 by an over-eager accessor.
        if let Ok(v) = row.get_column(col_idx).and_then(|c| c.get_i64()) {
            return serde_json::json!(v);
        }
        if let Ok(v) = row.get_column(col_idx).and_then(|c| c.get_i32()) {
            return serde_json::json!(v);
        }
        // Doubles before floats: f64 has the wider range; trying
        // f32 first would clip very small values like 9.2178e-39
        // to 0 if cassandra-cpp coerces the column.
        if let Ok(v) = row.get_column(col_idx).and_then(|c| c.get_f64()) {
            return serde_json::json!(v);
        }
        if let Ok(v) = row.get_column(col_idx).and_then(|c| c.get_f32()) {
            return serde_json::json!(v);
        }
        // get_string LAST — it's the genuine "TEXT / VARCHAR /
        // ASCII column" accessor + the safety-net for whatever
        // type the typed accessors above didn't cover (UUID,
        // INET, etc., which cassandra-cpp typically renders to a
        // canonical string form).
        if let Ok(v) = row.get_column(col_idx).and_then(|c| c.get_string()) {
            return serde_json::Value::String(v);
        }
        // Fallback: null for unsupported types
        serde_json::Value::Null
    }

    /// Get a column value from the first row as i64 (for relevancy extraction).
    pub fn get_column_i64_values(&self, column: &str) -> Vec<i64> {
        self.rows
            .iter()
            .filter_map(|row| row.get(column)?.as_i64())
            .collect()
    }

    /// Get a column value from the first row as string (for capture).
    pub fn get_column_string_values(&self, column: &str) -> Vec<String> {
        self.rows
            .iter()
            .filter_map(|row| {
                let v = row.get(column)?;
                match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                }
            })
            .collect()
    }
}

impl ResultBody for CqlResultBody {
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.rows
                .iter()
                .map(|row| {
                    serde_json::Value::Object(
                        row.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    )
                })
                .collect(),
        )
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn element_count(&self) -> u64 {
        // A read reports rows returned; a write reports rows written.
        // The two are mutually exclusive — a write carries no returned
        // rows — so returned rows win when present and the written
        // count carries the write case.
        if self.rows.is_empty() {
            self.written_rows
        } else {
            self.rows.len() as u64
        }
    }
}

// CqlConfig + consistency parsing live in `crate::common`.
// Use `CqlConfig::from_params(params)` to parse; convert the
// resulting `CqlConsistency` to `cass::Consistency` via
// `to_cass_consistency()` above.

/// CQL adapter using the Apache Cassandra C++ driver.
pub struct CqlAdapter {
    session: cass::Session,
    /// The parsed connection config this adapter was built from. Retained so
    /// `map_op` can derive the phase's own SRD-35 resource fingerprint
    /// (`config.to_resource_key("cassandra-cpp").render_key()`) and bind it as
    /// the `cql_session_key` scope constant for GK-resolved `max_batch_size`
    /// (SRD-103 §3–4). The stored config equals the one the pool's
    /// `SharedDriverRegistration.resource_key` used, so the render-key matches
    /// the pool entry's key exactly and `resource_lookup` is a sync hit.
    config: CqlConfig,
    consistency: cass::Consistency,
    /// Per-execute tracing probability (0.0–1.0). Stored as
    /// `f64::to_bits` in an AtomicU64 so dispensers can read
    /// without locking. Backs the `cql_trace_rate` dynamic
    /// control declared in [`Self::declare_controls`]; the
    /// control's applier is the single writer. Zero by default
    /// (tracing off).
    trace_rate_bits: Arc<AtomicU64>,
    /// Bounded retirement queue + JSON-line log writer for
    /// traced ops. Cloned into every dispenser the adapter
    /// materialises; `submit` is the only producer surface and
    /// is non-blocking. `None` when the operator declined
    /// tracing entirely (`trace_log=` set to a sentinel like
    /// `off`, or initial trace_rate==0 *and* no override path
    /// configured) — but in practice we always allocate one
    /// because the rate is a live dynamic control and we
    /// don't want to require a process restart to enable it.
    trace_log: std::sync::Arc<LazyTraceLog>,
}

unsafe impl Send for CqlAdapter {}
unsafe impl Sync for CqlAdapter {}

/// Collapse a multi-line statement to a single line for
/// error-diagnostic display: trim each line, drop empty
/// lines, and join with a single space. Truncates at a
/// SRD-68 Push 5c — walk a CQL statement text and resolve every
/// `{name}` placeholder, classifying each into one of two buckets:
///
/// - **Structural** — `lookup(name)` returns `Some(v)` against the
///   dispenser's canonical kernel at construction time. The value
///   is stable for the duration of a phase activation (workload
///   param, iter var, cascaded extern) and CAN'T be a `?` marker
///   in CQL's prepared-statement grammar (table names, keyspace,
///   option values). Substitute the value as text inline.
///
/// - **Per-cycle** — `lookup(name)` returns `None`. The name is
///   either an output binding (phase `bindings:` LHS, `result:`
///   LHS) that varies per cycle, or a coordinate / capture port.
///   Replace with `?`; remember the name in `bind_names` in
///   declaration order so the dispenser can pull the value via
///   `wires.get(name)` at cycle time.
///
/// Honours the same brace-discipline as
/// `nmbrs_workload::bindpoints::extract_bind_points`: `{` followed
/// by `'`/`"` is a CQL map-literal opener (emit the brace,
/// continue scanning so nested `{name}` placeholders inside still
/// resolve); depth-tracking finds the true matching `}`;
/// inline-expression `{{...}}` and qualifier-prefixed
/// `{bind:name}` shapes pass through unchanged.
fn resolve_structural_and_mark_remaining<F>(template: &str, mut lookup: F) -> (String, Vec<String>)
where
    F: FnMut(&str) -> Option<polydat::ast::Value>,
{
    let chars: Vec<char> = template.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(template.len());
    let mut bind_names: Vec<String> = Vec::new();
    let mut i = 0;
    while i < n {
        // `\{` / `\}` — pass through, two chars.
        if chars[i] == '\\' && i + 1 < n && (chars[i + 1] == '{' || chars[i + 1] == '}') {
            out.push(chars[i]);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        // `{{ ... }}` — inline-expression form, pass through.
        if i + 1 < n && chars[i] == '{' && chars[i + 1] == '{' {
            let start = i;
            let mut j = i + 2;
            while j + 1 < n && !(chars[j] == '}' && chars[j + 1] == '}') {
                j += 1;
            }
            let end = (j + 2).min(n);
            for k in start..end {
                out.push(chars[k]);
            }
            i = end;
            continue;
        }
        if chars[i] != '{' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // CQL map / JSON object literal: `{` followed by `'`/`"`.
        // Emit just the `{` and continue scanning so any nested
        // `{name}` placeholders inside still resolve.
        if i + 1 < n && (chars[i + 1] == '\'' || chars[i + 1] == '"') {
            out.push('{');
            i += 1;
            continue;
        }
        let body_start = i + 1;
        let mut j = body_start;
        let mut depth: u32 = 1;
        while j < n {
            if chars[j] == '{' {
                depth += 1;
            }
            if chars[j] == '}' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            j += 1;
        }
        if j >= n {
            out.push('{');
            i += 1;
            continue;
        }
        let body: String = chars[body_start..j].iter().collect();
        let body = body.trim();
        let after = j + 1;
        if body.is_empty() {
            out.push('{');
            out.push('}');
            i = after;
            continue;
        }
        // Strip a lvalue-spec suffix (`:*` or `:<polydat-type>`)
        // from the body BEFORE deciding qualifier vs bare-name.
        // The suffix is metadata for the binder verifier, not
        // part of the wire name; treating it as part of the body
        // would either misclassify as qualifier-prefixed (wrong)
        // or leave the suffix in the prepared statement text
        // (also wrong — the cluster would reject it). The bare
        // name carries through; the spec is recovered separately
        // at binder-construction time via `extract_bind_points`.
        let (body_bare, _spec) = nmbrs_workload::bindpoints::split_lvalue_spec(body);
        // Qualifier-prefixed (`{bind:name}`, etc.) and non-bare
        // identifiers pass through verbatim — same discipline as
        // `nmbrs_runtime::wires::substitute_via_wires`.
        if body_bare.contains(':')
            || !body_bare
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !body_bare
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            out.push('{');
            out.push_str(body_bare);
            out.push('}');
            i = after;
            continue;
        }
        // Re-bind `body` to the bare form for the lookup/marker
        // logic below — the original `body` variable shadowed
        // here so the rest of the function works on the stripped
        // name uniformly.
        let body = body_bare;
        // Bare identifier — try construction-time lookup. Some →
        // structural inline; None → per-cycle `?` marker.
        match lookup(body) {
            Some(v) => {
                // Structural: substitute the value as text. If
                // the workload-author wrote `'{name}'` with
                // surrounding quotes (the typical CQL pattern for
                // string-typed values), the substituted display
                // form lands inside those quotes — same shape as
                // the legacy text-mutation pass.
                out.push_str(&v.to_display_string());
            }
            None => {
                // Per-cycle binding becomes a `?` marker. If the
                // workload wrapped this placeholder in matching
                // quotes (`'{name}'`), strip them — CQL `?`
                // markers stand in place of the entire quoted
                // string literal, never inside quotes. Mirrors
                // the legacy `replace_bind_points_with_markers`
                // quoted-form-first heuristic.
                let next_after = chars.get(after).copied();
                let last_emitted = out.chars().last();
                let strip_quotes = match (last_emitted, next_after) {
                    (Some('\''), Some('\'')) => true,
                    (Some('"'), Some('"')) => true,
                    _ => false,
                };
                if strip_quotes {
                    out.pop();
                    out.push('?');
                    bind_names.push(body.to_string());
                    i = after + 1;
                    continue;
                }
                out.push('?');
                bind_names.push(body.to_string());
            }
        }
        i = after;
    }
    (out, bind_names)
}

/// generous bound so a hand-rolled `BATCH` with thousands
/// of statements doesn't blow the error message size.
fn flatten_one_line(s: &str) -> String {
    const MAX: usize = 400;
    let joined: String = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if joined.len() > MAX {
        // Honour char boundaries — naive [..MAX] could
        // split a multi-byte char and panic.
        let cutoff = joined
            .char_indices()
            .take_while(|(i, _)| *i < MAX)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}…", &joined[..cutoff])
    } else {
        joined
    }
}

/// Wrap a connect-error string with actionable resource-exhaustion
/// diagnostics when the cpp-driver's failure code points at
/// per-process limits (`LIB_UNABLE_TO_INIT`,
/// `LIB_NO_HOSTS_AVAILABLE` after a long-running session, …).
///
/// libuv (which the cpp-driver uses for its event loop) reports
/// `UNABLE_TO_INIT` when `epoll_create1` / `eventfd` / pipe2
/// returns `EMFILE` / `ENFILE` / `EAGAIN` — i.e. the process has
/// run through its file-descriptor or thread allowance. The bare
/// driver string ("Unable to initialize cluster event loop")
/// gives the operator no hint that this is environmental rather
/// than a Cassandra-side problem; with the driver also being a C++
/// dependency, the chase up the stack is non-obvious.
///
/// We append a snapshot of the relevant per-process limits and
/// counters so the operator can see at a glance whether they're
/// up against `RLIMIT_NOFILE` or `RLIMIT_NPROC`. The snapshot is
/// best-effort — `/proc` reads may fail in a sandbox; on those
/// platforms the suffix is just the contextual hint without raw
/// numbers.
fn enrich_connect_error(stage: &str, raw: String) -> String {
    let needs_diag = raw.contains("LIB_UNABLE_TO_INIT") || raw.contains("Unable to initialize");
    if !needs_diag {
        return format!("{stage}: {raw}");
    }
    let snap = process_resource_snapshot();
    format!(
        "{stage}: {raw}\n\
         \n\
         This error from the Cassandra C++ driver almost always\n\
         indicates *per-process resource exhaustion* — usually\n\
         file descriptors or threads — not a Cassandra-side\n\
         problem. The driver's libuv backend reports this when\n\
         `epoll_create1` / `eventfd` / `pipe2` fails inside\n\
         `uv_loop_init`, which on Linux means the kernel is\n\
         refusing the syscall (`EMFILE`/`ENFILE`/`EAGAIN`).\n\
         \n\
         Process resource snapshot:\n\
         {snap}\n\
         \n\
         If `fds_in_use` is at or near `nofile_soft`, raise the\n\
         FD limit (e.g. `ulimit -n 65536` before the run, or set\n\
         `LimitNOFILE=` in the systemd unit). If the run\n\
         exhausted FDs over many phases (consistent failure at\n\
         the same phase index), suspect a per-phase\n\
         CqlAdapter/session leak — each phase rebuilds the\n\
         adapter and the previous session's resources need to\n\
         release fully before the next phase opens a new one."
    )
}

/// Best-effort snapshot of the per-process resource counters
/// we care about for the LIB_UNABLE_TO_INIT diagnostic. Each
/// line is rendered as `key: value` with `?` filling in for
/// platforms or sandboxes where the source isn't readable.
fn process_resource_snapshot() -> String {
    fn read_or_q(path: &str) -> String {
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".into())
    }
    let fds = std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count().to_string())
        .unwrap_or_else(|_| "?".into());
    let nofile_soft = read_or_q("/proc/self/limits");
    // /proc/self/limits is multi-line; pull just the rows we need.
    let limit_for = |needle: &str| -> (String, String) {
        if nofile_soft == "?" {
            return ("?".into(), "?".into());
        }
        for line in nofile_soft.lines() {
            if line.starts_with(needle) {
                // Format: "Max open files            65536                65536                files"
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.len() >= 4 {
                    let n = cols.len();
                    return (cols[n - 3].into(), cols[n - 2].into());
                }
            }
        }
        ("?".into(), "?".into())
    };
    let (nofile_s, nofile_h) = limit_for("Max open files");
    let (nproc_s, nproc_h) = limit_for("Max processes");
    let threads = std::fs::read_dir("/proc/self/task")
        .map(|d| d.count().to_string())
        .unwrap_or_else(|_| "?".into());
    format!(
        "  fds_in_use:    {fds}\n\
         \x20 nofile_soft:   {nofile_s}\n\
         \x20 nofile_hard:   {nofile_h}\n\
         \x20 threads_alive: {threads}\n\
         \x20 nproc_soft:    {nproc_s}\n\
         \x20 nproc_hard:    {nproc_h}",
    )
}

impl CqlAdapter {
    pub async fn connect(config: &CqlConfig) -> Result<Self, String> {
        let mut cluster = cass::Cluster::default();
        cluster
            .set_contact_points(&config.hosts)
            .map_err(|e| format!("set contact points: {e}"))?;
        cluster
            .set_port(config.port)
            .map_err(|e| format!("set port: {e}"))?;
        if let (Some(u), Some(p)) = (&config.username, &config.password) {
            cluster
                .set_credentials(u, p)
                .map_err(|e| format!("set credentials: {e}"))?;
        }
        cluster.set_request_timeout(std::time::Duration::from_millis(config.request_timeout_ms));
        // Bound connection establishment (control + per-host connect). The
        // driver default is 5000ms — the `Connection timeout` control-connection
        // failure surfaces at exactly that boundary — so raise `connect_timeout`
        // for distant / slow-to-accept clusters.
        cluster.set_connect_timeout(std::time::Duration::from_millis(config.connect_timeout_ms));
        // Exponential reconnection policy (the driver default, made explicit so
        // the delays are operator-tunable).
        cluster.set_exponential_reconnect(
            std::time::Duration::from_millis(config.reconnect_base_delay_ms),
            std::time::Duration::from_millis(config.reconnect_max_delay_ms),
        );
        // Connection-survival knobs (see `CqlConfig`): the idle timeout is the
        // host-ejection trigger during server stalls — a connection that goes
        // this long without a successful heartbeat response is terminated, and
        // with no live connections the host leaves the policy
        // (LIB_NO_HOSTS_AVAILABLE bursts until reconnect). Raise
        // `connection_idle_timeout` past the longest expected stall to ride
        // through it; in-flight requests still honour their own request
        // timeouts.
        cluster.set_connection_heartbeat_interval(std::time::Duration::from_millis(
            config.heartbeat_interval_ms,
        ));
        cluster.set_connection_idle_timeout(std::time::Duration::from_millis(
            config.connection_idle_timeout_ms,
        ));

        // `common::CqlConfig::from_params` already validated the
        // consistency string at parse time, so this conversion is
        // total.
        let consistency = to_cass_consistency(config.consistency);

        // Try to connect to the specified keyspace. If it doesn't exist,
        // fall back to connecting without a keyspace (needed for DDL phases
        // that create the keyspace).
        let session = if config.keyspace.is_empty() {
            cluster
                .connect()
                .await
                .map_err(|e| enrich_connect_error("connect", e.to_string()))?
        } else {
            match cluster.connect_keyspace(&config.keyspace).await {
                Ok(s) => s,
                Err(e) => {
                    let msg = e.to_string();
                    // Only fall back for keyspace-not-found errors.
                    // Auth failures, network errors, etc. should propagate.
                    if msg.contains("Keyspace")
                        || msg.contains("keyspace")
                        || msg.contains("not found")
                    {
                        nmbrs_runtime::observer::log(
                            nmbrs_runtime::observer::LogLevel::Warn,
                            &format!(
                                "cql/cassandra-cpp: keyspace '{}' not found, connecting without keyspace",
                                config.keyspace
                            ),
                        );
                        cluster.connect().await.map_err(|e| {
                            enrich_connect_error("connect (no keyspace)", e.to_string())
                        })?
                    } else {
                        return Err(enrich_connect_error(
                            &format!("connect to keyspace '{}'", config.keyspace),
                            msg,
                        ));
                    }
                }
            }
        };

        // Initial trace rate is the workload-param value (0.0
        // when absent / off). Stored as f64-bits in the Atomic
        // so dispensers can `f64::from_bits(load())` per cycle.
        let initial_trace_rate: f64 = config.trace_rate.unwrap_or(0.0);
        let trace_rate_bits = Arc::new(AtomicU64::new(initial_trace_rate.to_bits()));

        // Trace log: per-process file the retirement worker
        // appends to. Default lives under the active session
        // dir so traces ride along with the rest of the run's
        // artifacts; explicit override via `trace_log=` lets
        // operators redirect to a known stable path.
        let trace_log_path = resolve_trace_log_path(config);
        // Lazy: resolve the path but open NOTHING here. The file, the
        // retirement worker, and the `system_traces` prepare are all deferred
        // to the first op that is actually traced (`cql_trace_rate > 0`). With
        // tracing off — the default — the cluster is never touched for tracing
        // and no artifact file is created. See [`LazyTraceLog`].
        let trace_log = std::sync::Arc::new(LazyTraceLog::new(trace_log_path, session.clone()));

        Ok(Self {
            session,
            config: config.clone(),
            consistency,
            trace_rate_bits,
            trace_log,
        })
    }

    // Note: dispensers read `trace_rate_bits` and `trace_log`
    // directly via `self.trace_rate_bits.clone()` /
    // `self.trace_log.clone()` from inside `map_op`. Earlier
    // getter wrappers (`trace_rate_handle` / `trace_log_handle`)
    // were removed as dead code — the field-direct path is the
    // canonical one.
}

/// Resolve where the trace log gets written. Operator override
/// via the `trace_log=` workload param wins; otherwise the
/// `logs/latest/cql_traces.jsonl` path that the runner's
/// `logs/latest -> logs/<session_id>` symlink keeps current.
/// The symlink is created by `Session::new` before adapters
/// connect, so this resolves consistently across the run.
fn resolve_trace_log_path(config: &CqlConfig) -> std::path::PathBuf {
    if let Some(ref explicit) = config.trace_log_path {
        return std::path::PathBuf::from(explicit);
    }
    std::path::PathBuf::from("logs/latest/cql_traces.jsonl")
}

// =========================================================================
// DriverAdapter: dispatch to the correct dispenser based on field name
// =========================================================================

// `STMT_FIELD_NAMES` is imported from `crate::common`; its
// dispatch logic is the same across every CQL engine.

impl DriverAdapter for CqlAdapter {
    fn name(&self) -> &str {
        "cql"
    }

    fn default_status_metrics(&self) -> Vec<nmbrs_runtime::adapter::StatusMetric> {
        crate::common::default_status_metrics()
    }

    /// SRD-103/104 — publish a [`CqlSessionHandle`](crate::common::CqlSessionHandle)
    /// over this adapter's connected session as the pool-entry accessor
    /// payload. Kernels reach it via `cql_session(cql_session_key)`; the
    /// settings source runs on the SAME `cass::Session` the op path uses.
    fn accessor_payload(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        let source = settings::CassSettingsSource::new(self.session.clone());
        Some(Arc::new(crate::common::CqlSessionHandle::new(
            "cassandra-cpp",
            Arc::new(source),
        )))
    }

    fn declare_controls(
        &self,
        parent: &Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
    ) {
        use nmbrs_metrics::component::{Component, attach};
        use nmbrs_metrics::controls::SyncApplier;
        use nmbrs_metrics::labels::Labels;

        // One subcomponent per adapter instance, attached to the
        // activity. The name `cql` matches the user-facing adapter
        // name so the control's effective-labels path reads
        // `…/adapter=cql/cql_trace_rate` — discoverable in
        // dryrun=controls and the web /api/controls listing.
        //
        // Idempotency (SRD 23): `declare_controls` is the trait
        // contract for adapter control surface. The runtime calls
        // it both at phase-attach time (so `dryrun=controls` can
        // walk the tree before any cycles run) and at run start
        // (so adapters that materialize only at run time still get
        // a chance). Look up an existing `adapter=cql` subcomponent
        // before creating one so a second call doesn't produce a
        // duplicate sibling, and short-circuit if `cql_trace_rate`
        // is already declared on it.
        let cql_component = {
            let parent_guard = parent.read().unwrap_or_else(|e| e.into_inner());
            let existing = parent_guard
                .children()
                .find(|c| {
                    let g = c.read().unwrap_or_else(|e| e.into_inner());
                    g.labels().get("adapter") == Some("cql")
                })
                .cloned();
            drop(parent_guard);
            match existing {
                Some(c) => {
                    if c.read()
                        .unwrap_or_else(|e| e.into_inner())
                        .controls()
                        .get_erased("cql_trace_rate")
                        .is_some()
                    {
                        return;
                    }
                    c
                }
                None => {
                    let new_c = Arc::new(std::sync::RwLock::new(Component::new(
                        Labels::of("adapter", "cql"),
                        std::collections::HashMap::new(),
                    )));
                    attach(parent, &new_c);
                    new_c
                }
            }
        };

        // The applier writes f64-bits into the AtomicU64 the
        // dispensers read per cycle. SyncApplier is fine here:
        // the write is just an atomic store, no I/O.
        let bits_for_apply = self.trace_rate_bits.clone();
        let initial_rate = f64::from_bits(self.trace_rate_bits.load(Ordering::Acquire));
        // Derive the live control from its single-source capability descriptor
        // (SRD-23): name, `[0,1]` range, and gauge all come from
        // `CQL_TRACE_RATE`, the same descriptor `describe controls` reads — so
        // the discovery surface and the real knob cannot drift.
        let trace_control = crate::common::CQL_TRACE_RATE.build_f64(initial_rate);
        trace_control.register_applier(SyncApplier::new(move |v: f64| {
            bits_for_apply.store(v.to_bits(), Ordering::Release);
            Ok(())
        }));
        cql_component
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .controls()
            .declare(trace_control);
    }

    fn map_op<'a>(
        &'a self,
        template: &'a ParsedOp,
        parent: std::sync::Arc<polydat::kernel::PolydatKernel>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Find the statement text and determine execution mode from the field name.
            let (stmt_text, mode) = STMT_FIELD_NAMES
                .iter()
                .find_map(|key| -> Option<(String, &str)> {
                    let v = template.op.get(*key)?;
                    let text = v.as_str()?;
                    Some((text.to_string(), *key))
                })
                .ok_or_else(|| {
                    "CQL op requires a 'raw:', 'simple:', 'prepared:', or 'stmt:' field".to_string()
                })?;

            // SRD-68 Push 5c — construction-time structural resolution
            // for prepared mode. Walk every `{name}` in the statement
            // text against the dispenser's canonical kernel:
            //   - If `canonical.lookup(name)` returns `Some(v)` the
            //     name resolves to a stable-per-phase-activation value
            //     (workload param, iter var, cascaded extern). Inline
            //     `v.to_display_string()` directly into the SQL — the
            //     CQL prepared-statement compiler can't accept `?`
            //     markers for structural positions like keyspace /
            //     table / option values.
            //   - Else the name is a per-cycle output binding (phase
            //     `bindings:` LHS, `result:` LHS). Mark it with `?`
            //     and remember its name for cycle-time `wires.get`
            //     binding.
            // The result is a CQL-parameterised `prepared_text` plus
            // `bind_names` in `?`-position order.
            //
            // The dispenser is now self-sufficient — it doesn't
            // depend on the upstream `resolve_placeholders_via_kernel`
            // mutation pass having pre-resolved structural names. When
            // that pass lands its validator-only form (Push 5c step 3)
            // the dispenser keeps working unchanged.
            let parent_for_lookup = parent.clone();
            let (prepared_text, bind_names) =
                resolve_structural_and_mark_remaining(&stmt_text, |name| {
                    parent_for_lookup.lookup(name)
                });
            // Workload-author lvalue assertions per per-cycle bind
            // point: a `{name:*}` or `{name:<polydat-type>}` suffix
            // in the original statement text overrides the cluster-
            // side parameter type for binder verification. Indices
            // here line up with `bind_names` — both walk the same
            // statement text in the same per-cycle-bind order, so
            // the i-th element of each list refers to the same `?`
            // position. Bind points that the structural resolver
            // inlined (workload-param substitutions like {keyspace}
            // / {table}) drop out of both lists symmetrically.
            let lvalue_specs: Vec<Option<nmbrs_workload::bindpoints::LvalueSpec>> = {
                use nmbrs_workload::bindpoints::{BindPoint, extract_bind_points};
                extract_bind_points(&stmt_text)
                    .into_iter()
                    .filter_map(|bp| match bp {
                        BindPoint::Reference {
                            ref name,
                            ref lvalue_spec,
                            ..
                        } => {
                            // Keep only points that are also in
                            // `bind_names` (the per-cycle survivors).
                            // Use position to align: a kernel-resolved
                            // name won't appear in bind_names so its
                            // spec must be filtered out too.
                            if bind_names.iter().any(|bn| bn == name) {
                                Some(lvalue_spec.clone())
                            } else {
                                None
                            }
                        }
                        BindPoint::InlineDefinition(_) => None,
                    })
                    .collect()
            };

            let session = SessionHandle(&self.session as *const cass::Session);
            let consistency = self.consistency;

            // Check for batch configuration on this op.
            // batch: <integer> — batch size (rows per batch), type defaults to unlogged.
            // max_batch_size: <bytes> — byte budget bounding the encoded batch size (SRD-103 §6).
            // batchtype: logged|unlogged|counter — overrides batch type.
            // A `batch:` row cap OR a `max_batch_size:` byte budget both select the
            // batch executor; `max_batch_size` alone byte-bounds a dynamically-sized batch.
            let has_batch = template.params.contains_key("batch")
                || template.params.contains_key("max_batch_size");
            // SRD-103 §3 — both `batch:` (cursor stride / row cap) and
            // `max_batch_size:` (byte budget) are GK-resolved op fields. A literal
            // (`batch: 8`, `64KB`) resolves directly; an expression referencing the
            // CQL session nodes is evaluated against a subscope with this phase's
            // own `cql_session_key` bound, after the referenced settings are
            // pre-read (async) into the session memo. The phase's session is
            // pre-attached before op fields resolve, so `render_key` is a sync hit.
            let session_key = self.config.to_resource_key("cassandra-cpp").render_key();
            // Workload-authored `batch:` cursor stride (0 = unset → the byte budget,
            // if any, drives the row count). Used directly, unfloored downstream.
            let batch_n: usize = crate::common::session_handle::resolve_batch_count(
                &parent,
                &session_key,
                template.params.get("batch"),
            )
            .await
            .map_err(|e| format!("op '{}': {e}", template.name))?
            .unwrap_or(0);
            let max_batch_bytes = crate::common::session_handle::resolve_max_batch_bytes(
                &parent,
                &session_key,
                template.params.get("max_batch_size"),
            )
            .await
            .map_err(|e| format!("op '{}': {e}", template.name))?;
            let batch_type_name = template
                .params
                .get("batchtype")
                .and_then(|v| v.as_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            let batch_type = match batch_type_name.as_str() {
                "logged" => cass::BatchType::LOGGED,
                "counter" => cass::BatchType::COUNTER,
                _ => cass::BatchType::UNLOGGED,
            };
            // A batch DERIVES its retry-safety from its inner statements —
            // uniform template with stride, so computed ONCE here, never per
            // stanza: counter batches (increments are not idempotent) and LWT
            // statements are not retry-safe; plain PK-keyed upserts are. Gates
            // the transient-error retry classification at the execute site.
            let batch_retry_safe =
                batch_type_name != "counter" && crate::common::cql_statement_retry_safe(&stmt_text);

            // SRD-68 invariant I-3: dispenser owns its canonical kernel.
            // For Push 2b, no op-level Polydat matter is assembled here yet —
            // the canonical kernel is the parent (phase scope) directly.
            // Push 3 will fan out per-fiber kernels from this canonical;
            // a follow-up will let CQL ops with their own `bindings:` /
            // `result:` block materialise a child subscope via
            // `parent.build_subscope(matter)`.
            // SRD 73: build the per-op universal-field modifier chain
            // BEFORE we move `parent` into `canonical_kernel`. The chain
            // captures resolved values out of the Polydat scope once at
            // initializer time; per-cycle execute() just calls
            // `chain.apply`. Only one match arm below moves `modifiers`
            // into its dispenser.
            let modifiers = crate::common::op_modifier::build_cql_modifier_chain::<
                op_modifier::CassModifierFactory<cass::Statement>,
            >(&parent, template.name.clone())?;
            // A batch is a statement too — resolve the SAME universal fields into a
            // batch-targeted chain so consistency / serial / request timeout /
            // tracing all reach the batch itself (the driver ignores member-
            // statement aspects for batch execution). Built only when this op is a
            // batch; drops out of the hot path otherwise.
            let batch_modifiers = if has_batch {
                Some(crate::common::op_modifier::build_cql_modifier_chain::<
                    op_modifier::CassModifierFactory<cass::Batch>,
                >(&parent, template.name.clone())?)
            } else {
                None
            };
            let canonical_kernel = parent;

            match mode {
                "raw" => Ok(Box::new(CqlRawDispenser {
                    session,
                    stmt_template: stmt_text.clone(),
                    canonical_kernel,
                    trace_rate_bits: self.trace_rate_bits.clone(),
                    trace_log: self.trace_log.clone(),
                    modifiers,
                }) as Box<dyn OpDispenser>),
                "simple" => Ok(Box::new(CqlRawDispenser {
                    session,
                    stmt_template: stmt_text.clone(),
                    canonical_kernel,
                    trace_rate_bits: self.trace_rate_bits.clone(),
                    trace_log: self.trace_log.clone(),
                    modifiers,
                }) as Box<dyn OpDispenser>),
                _ => {
                    if bind_names.is_empty() && !has_batch {
                        // No bind points — execute as raw (DDL, simple queries).
                        // No prepare needed; nothing to verify.
                        Ok(Box::new(CqlRawDispenser {
                            session,
                            stmt_template: stmt_text.clone(),
                            canonical_kernel,
                            trace_rate_bits: self.trace_rate_bits.clone(),
                            trace_log: self.trace_log.clone(),
                            modifiers,
                        }) as Box<dyn OpDispenser>)
                    } else {
                        // Prepare against the cluster and verify the
                        // typed binder against the dispenser's parent
                        // kernel — the per-op dispenser-init
                        // compulsion. Both prepared-mode and batch-mode
                        // use the same inner prepared statement.
                        let prepared_raw =
                            self.session.prepare(&prepared_text).await.map_err(|e| {
                                format!("cassandra-cpp prepare '{}': {e}", prepared_text,)
                            })?;
                        let prepared_arc = Arc::new(prepared_raw);

                        // Build per-position binder functions from
                        // prepared statement metadata. For CUSTOM
                        // (vector) columns we additionally extract the
                        // class name so make_binder can specialise per
                        // VectorType element (FloatType / IntType /
                        // DoubleType / LongType / ShortType / Float16Type)
                        // and dispatch native typed-vector inputs directly
                        // without round-tripping through to_display_string.
                        let binders: Vec<BinderFn> = (0..bind_names.len())
                            .map(|i| {
                                let dt = prepared_arc.parameter_data_type(i);
                                let vt = get_const_data_type_value_type(&dt);
                                let class_name = if vt == cass::ValueType::CUSTOM {
                                    get_const_data_type_class_name(&dt)
                                } else {
                                    None
                                };
                                make_binder(vt, class_name.as_deref())
                            })
                            .collect();

                        // Build the polydat typed binder from the same
                        // metadata, with per-bindpoint workload-author
                        // lvalue assertions (`{name:*}` / `{name:<type>}`)
                        // overriding the cluster-side type when present.
                        // See the scylla driver's `map_op` for the
                        // matching surface; the diagnostics here mirror
                        // those exactly.
                        let mut slot_build_err: Option<String> = None;
                        let slots: Vec<polydat::binder::BinderSlot> = (0..bind_names.len())
                        .map(|i| {
                            use nmbrs_workload::bindpoints::LvalueSpec;
                            let name = &bind_names[i];
                            // Cluster-side polydat type for this `?`
                            // position — always computed (used in all
                            // arms below as the honest lvalue type).
                            // For CUSTOM, also extract the class
                            // name so cass_to_polydat can detect
                            // VectorType (and any future precise
                            // CUSTOM mappings) directly.
                            let dt = prepared_arc.parameter_data_type(i);
                            let vt = get_const_data_type_value_type(&dt);
                            let class_name = if vt == cass::ValueType::CUSTOM {
                                get_const_data_type_class_name(&dt)
                            } else {
                                None
                            };
                            let (cluster_lvalue, policy) =
                                binder_meta::cass_to_polydat(vt, class_name.as_deref());
                            let (lvalue_type, allow_fusion) =
                                match lvalue_specs.get(i).and_then(|s| s.as_ref()) {
                                    Some(LvalueSpec::Wildcard) => {
                                        // Workload author wrote `:*` — they
                                        // licensed fusion deliberately. No log:
                                        // the syntax IS the announcement.
                                        (cluster_lvalue, true)
                                    }
                                    Some(LvalueSpec::Explicit(type_name)) => {
                                        match polydat::ast::PortType::from_workload_name(type_name) {
                                            Some(pt) => {
                                                // Workload author asserted a
                                                // specific polydat type via
                                                // `:<type>`. Quiet — the source
                                                // text is the record.
                                                (pt, false)
                                            }
                                            None => {
                                                slot_build_err = Some(format!(
                                                    "cassandra-cpp op '{op}' field 'prepared' slot [{i}] \
                                                     wire `{name}`: unknown polydat type name \
                                                     `{type_name}` in lvalue spec `:{type_name}`. \
                                                     Accepted names: u64, f64, u32, i32, i64, f32, \
                                                     bool, str, bytes, json, vec_f32, vec_i32, \
                                                     vec_f64, vec_i64, vec_f16, vec_i16.",
                                                    op = template.name,
                                                ));
                                                (cluster_lvalue, false)
                                            }
                                        }
                                    }
                                    None => {
                                        // Fallback policy is the only path
                                        // that warns: the workload author
                                        // didn't ask for anything special,
                                        // yet the slot lost typed verification
                                        // because `cass_to_polydat` has no
                                        // precise arm for this CQL type.
                                        // Strict and TextNatural slots are
                                        // silent — they're honest, deliberate
                                        // mappings.
                                        if let Some(cql_label) = policy.fallback_label() {
                                            nmbrs_runtime::diag!(
                                                nmbrs_runtime::observer::LogLevel::Warn,
                                                "cassandra-cpp op '{op}' field 'prepared' slot [{i}] \
                                                 wire `{name}`: CQL type {cql_label} has no precise \
                                                 polydat mapping yet — slot accepts any rvalue via \
                                                 Str fallback (typed verification bypassed). Add a \
                                                 precise arm in binder_meta.rs::cass_to_polydat, or \
                                                 mark intent with `{{{name}:*}}` to silence.",
                                                op = template.name,
                                            );
                                        }
                                        (cluster_lvalue, policy.allow_fusion())
                                    }
                                };
                            polydat::binder::BinderSlot {
                                wire: name.clone(),
                                lvalue_type,
                                allow_fusion,
                            }
                        })
                        .collect();
                        if let Some(msg) = slot_build_err {
                            return Err(msg);
                        }
                        if !slots.is_empty() {
                            let binder = polydat::binder::Binder::Positional {
                                field: "prepared".to_string(),
                                slots,
                            };
                            polydat::binder::verify_against_kernel(&[binder], &canonical_kernel)
                                .map_err(|violations| {
                                    violations
                                        .into_iter()
                                        .map(|v| v.message)
                                        .collect::<Vec<_>>()
                                        .join("; ")
                                })?;
                        }

                        if has_batch {
                            // SRD-22 cover-once — settle the FIXED uniform stride N
                            // now (not per-execute). Only characterize a row when a
                            // byte budget must be converted to a row count; `batch:N`
                            // / single-row need no probe.
                            let row_size = if max_batch_bytes.is_some() {
                                crate::common::size_estimator::characterize_row_size(
                                    &canonical_kernel,
                                    &bind_names,
                                )
                            } else {
                                0
                            };
                            let stride_n = crate::common::size_estimator::fixed_batch_stride(
                                row_size,
                                batch_n,
                                max_batch_bytes,
                            );
                            Ok(Box::new(CqlBatchDispenser {
                                session,
                                consistency,
                                stmt_text: prepared_text.clone(),
                                stmt_field: "stmt".to_string(),
                                bind_names,
                                canonical_kernel,
                                batch_n,
                                n: stride_n,
                                max_batch_bytes,
                                oversize_warned: std::sync::atomic::AtomicBool::new(false),
                                prepared: prepared_arc,
                                binders,
                                batch_type,
                                retry_safe: batch_retry_safe,
                                rows_timer: nmbrs_metrics::instruments::timer::Timer::new(
                                    nmbrs_metrics::labels::Labels::of("name", "rows_inserted"),
                                ),
                                rows_total: std::sync::atomic::AtomicU64::new(0),
                                batch_writes: std::sync::atomic::AtomicU64::new(0),
                                trace_rate_bits: self.trace_rate_bits.clone(),
                                trace_log: self.trace_log.clone(),
                                // The batch-targeted chain — applied to the batch
                                // itself, not its member statements.
                                modifiers: batch_modifiers
                                    .expect("batch modifier chain is built when has_batch"),
                            }) as Box<dyn OpDispenser>)
                        } else {
                            Ok(Box::new(CqlPreparedDispenser {
                                session,
                                consistency,
                                stmt_text: prepared_text,
                                bind_names,
                                canonical_kernel,
                                prepared: prepared_arc,
                                binders,
                                trace_rate_bits: self.trace_rate_bits.clone(),
                                trace_log: self.trace_log.clone(),
                                modifiers,
                            }) as Box<dyn OpDispenser>)
                        }
                    }
                }
            }
        })
    }

    fn known_op_params(&self) -> &'static [&'static str] {
        // SRD 73: universal per-op field surface.
        crate::common::op_modifier::CQL_UNIVERSAL_FIELDS
    }

    /// SRD-35 Push D — graceful CQL session close.
    ///
    /// The vendored `cassandra-cpp::Session::close()` wraps
    /// `cass_session_close()`, which the C++ driver implements
    /// as a flush of in-flight requests followed by a connection
    /// teardown. We await the resulting future inside a 5-second
    /// timeout so a hung node doesn't pin the runtime; on
    /// timeout the underlying `Drop` still runs the synchronous
    /// close as a fallback when the adapter's last reference
    /// goes away.
    ///
    /// This override fires from
    /// [`nmbrs_runtime::resource_pool::SharedAdapterResource::close`]
    /// when the pool determines a shared CQL adapter has no
    /// remaining users.
    fn shutdown<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let close_future = self.session.close();
            let timeout = std::time::Duration::from_secs(5);
            match tokio::time::timeout(timeout, close_future).await {
                Ok(Ok(())) => {
                    nmbrs_runtime::diag!(
                        nmbrs_runtime::observer::LogLevel::Info,
                        "cql session closed cleanly",
                    );
                }
                Ok(Err(e)) => {
                    nmbrs_runtime::diag!(
                        nmbrs_runtime::observer::LogLevel::Error,
                        "cql session close returned error: {e}; falling back to Drop teardown",
                    );
                }
                Err(_) => {
                    nmbrs_runtime::diag!(
                        nmbrs_runtime::observer::LogLevel::Error,
                        "cql session close timed out after 5s; falling back to Drop teardown",
                    );
                }
            }
        })
    }
}

// =========================================================================
// Session handle wrapper (Send+Sync for raw pointer)
// =========================================================================

struct SessionHandle(*const cass::Session);
unsafe impl Send for SessionHandle {}
unsafe impl Sync for SessionHandle {}

impl SessionHandle {
    fn get(&self) -> &cass::Session {
        unsafe { &*self.0 }
    }
}

// =========================================================================
// CqlRawDispenser: string interpolation, direct execute
// =========================================================================

/// Executes the fully-interpolated statement text directly.
///
/// Used for:
/// - `raw:` mode (all bind points resolved to text by the executor)
/// - `simple:` mode (same driver path, distinction preserved for API)
/// - `prepared:`/`stmt:` mode when there are no bind points (DDL)
struct CqlRawDispenser {
    session: SessionHandle,
    /// Statement template captured at `map_op` time —
    /// retains the `{name}` bind-point placeholders the
    /// operator wrote in the workload yaml. Used by
    /// [`OpDispenser::describe`] to surface the op shape
    /// in error diagnostics. Per-cycle execution reads
    /// the fully-interpolated text from `ctx.fields`; this
    /// field is informational only.
    stmt_template: String,
    /// SRD-68 invariant I-3: dispenser-owned canonical GK
    /// kernel for op-template-scope name resolution. Push 2b
    /// stores the parent reference directly; Push 3 will fan
    /// out per-fiber kernels from this canonical via
    /// `build_subscope` for cycle-time reads through the
    /// narrow `WireSource` trait.
    #[allow(dead_code)]
    canonical_kernel: std::sync::Arc<polydat::kernel::PolydatKernel>,
    /// Live tracing probability (f64 bits). Loaded per execute;
    /// `cql_trace_rate` control writes here.
    trace_rate_bits: Arc<AtomicU64>,
    /// Bounded retirement queue handle for traced ops.
    /// `None` when the trace log file couldn't be opened at
    /// adapter init — dispenser still respects the rate
    /// (sets the tracing flag on the statement) but skips the
    /// post-execute submit.
    trace_log: std::sync::Arc<LazyTraceLog>,
    /// SRD 73 universal per-op field overrides applied per
    /// execute. Empty chain → execute() takes the existing
    /// string fast-path. Non-empty chain → execute() builds
    /// a Statement, applies modifiers, then runs the explicit
    /// statement-path execute.
    modifiers: nmbrs_runtime::op_modifier::ModifierChain<cass::Statement>,
}

impl OpDispenser for CqlRawDispenser {
    fn describe(&self) -> Option<String> {
        // Single-line shape of the statement template,
        // collapsing internal whitespace runs to one
        // space so an indented multi-line `raw: |` block
        // reads cleanly in an error message.
        Some(format!(
            "CQL raw: {}",
            flatten_one_line(&self.stmt_template)
        ))
    }

    fn describe_resolved(&self, wires: &dyn nmbrs_runtime::wires::WireSource) -> Option<String> {
        // SRD-68 Push 5: render the post-substitution statement
        // through the same `substitute_via_wires` path the cycle
        // uses so the operator sees exactly what was sent. Bind
        // failures (unresolved name) surface in the message; the
        // describe-resolved is best-effort, so a None on error is
        // fine.
        nmbrs_runtime::wires::substitute_via_wires(&self.stmt_template, wires)
            .ok()
            .map(|s| format!("CQL raw: {}", flatten_one_line(&s)))
    }

    fn canonical_kernel(&self) -> Option<&std::sync::Arc<nmbrs_runtime::adapter::PolydatKernel>> {
        Some(&self.canonical_kernel)
    }

    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let wires = ctx.wires;
        Box::pin(async move {
            // SRD-68 Push 5: cycle-time bind-point resolution
            // through the dispenser's per-fiber Polydat Kernel via the
            // narrow `WireSource` trait. Walks the pristine
            // statement template stored at construction and
            // resolves each `{name}` against the per-fiber kernel
            // slot that the executor handed in via
            // `ExecCtx::wires`. Single resolution surface per
            // SRD-68 invariant I-1; legacy `fields.get_str` path
            // retired for CQL raw mode.
            let stmt_text_owned =
                nmbrs_runtime::wires::substitute_via_wires(&self.stmt_template, wires).map_err(
                    |msg| {
                        ExecutionError::Op(AdapterError {
                            error_name: "unresolved_bind_point".into(),
                            message: msg,
                            retryable: false,
                        })
                    },
                )?;
            let stmt_text: &str = stmt_text_owned.as_str();

            // Sparse-tracing decision per execute. Atomic load is
            // cheap (single 64-bit read); the RNG roll only fires
            // when the rate is non-zero, so the no-tracing hot
            // path stays effectively free.
            let trace_rate = f64::from_bits(self.trace_rate_bits.load(Ordering::Acquire));
            let trace_this = trace_rate > 0.0 && rand::random::<f64>() < trace_rate;

            // Capture metadata for the trace log before running.
            // `started_at` is the wall-clock for the
            // `system_traces.sessions` time-window correlation;
            // `started` is the monotonic clock for latency.
            let started_at = std::time::SystemTime::now();
            let started = std::time::Instant::now();

            // Two execute paths so the no-trace hot path stays
            // exactly the existing shape — `Session::execute(&str)`
            // doesn't expose tracing, so on hit we explicitly
            // build the Statement, set the tracing flag, and use
            // the vendored `execute_with_tracing` surface that
            // pairs result with `cass_future_tracing_id`.
            // SRD 73: if any per-op universal-field modifiers are
            // bound for this op, the raw path takes the Statement
            // route (so modifiers can apply); otherwise it stays
            // on the string fast-path. The trace branch also takes
            // the Statement route for set_tracing.
            let need_statement = trace_this || !self.modifiers.is_empty();
            let exec_outcome = if need_statement {
                let mut stmt = self.session.get().statement(stmt_text);
                self.modifiers.apply(&mut stmt);
                if trace_this {
                    let _ = stmt.set_tracing(true);
                    self.session
                        .get()
                        .execute_with_tracing(&stmt)
                        .await
                        .map(|(r, tid)| (r, tid))
                } else {
                    stmt.execute().await.map(|r| (r, None))
                }
            } else {
                self.session
                    .get()
                    .execute(stmt_text)
                    .await
                    .map(|r| (r, None))
            };

            let latency_nanos = started.elapsed().as_nanos() as u64;

            let exec_result = match exec_outcome {
                Ok((result, trace_id)) => {
                    if trace_this && let Some(log) = self.trace_log.get().await {
                        log.submit(TraceRecord {
                            cycle,
                            started_at,
                            query: stmt_text.to_string(),
                            // Raw ops have no bind points — the
                            // statement text is already fully
                            // interpolated by the executor.
                            binds: Vec::new(),
                            latency_nanos,
                            ok: true,
                            error_name: None,
                            trace_id: trace_id.map(|u| {
                                let std_uuid: uuid::Uuid = u.into();
                                std_uuid.to_string()
                            }),
                        });
                    }
                    Ok(result)
                }
                Err(e) => {
                    let truncated = if stmt_text.len() > 200 {
                        format!("{}...", &stmt_text[..200])
                    } else {
                        stmt_text.to_string()
                    };
                    if trace_this && let Some(log) = self.trace_log.get().await {
                        log.submit(TraceRecord {
                            cycle,
                            started_at,
                            query: stmt_text.to_string(),
                            binds: Vec::new(),
                            latency_nanos,
                            ok: false,
                            error_name: Some("cql_error".into()),
                            trace_id: None,
                        });
                    }
                    Err(ExecutionError::Op(AdapterError {
                        error_name: "cql_error".into(),
                        message: format!("{e}\n  statement: {truncated}"),
                        retryable: crate::common::cql_error_is_retryable(&e.to_string()),
                    }))
                }
            };

            let result = exec_result?;

            // Build the result body, classifying read vs write inside
            // `from_cass_result`: a SELECT reports its returned rows, a
            // write reports one row written. An empty SELECT (a read
            // that matched nothing) has element_count 0 → no body,
            // preserving the prior no-body-for-empty-read behavior.
            let body_obj = CqlResultBody::from_cass_result(&result);
            let body = if body_obj.element_count() > 0 {
                Some(Box::new(body_obj) as Box<dyn ResultBody>)
            } else {
                None
            };
            Ok(OpResult {
                body,
                skipped: false,
            })
        })
    }
}

// =========================================================================
// CqlPreparedDispenser: prepare once, bind typed values per cycle
// =========================================================================

/// Prepares the statement lazily on first execute, then binds typed
/// values by name for each subsequent cycle.
struct CqlPreparedDispenser {
    session: SessionHandle,
    consistency: cass::Consistency,
    /// Statement text — retained for error diagnostics and the
    /// `describe_resolved` walk only; not used on the execute
    /// hot path.
    stmt_text: String,
    /// Names of bind point fields to extract from ResolvedFields.
    bind_names: Vec<String>,
    /// SRD-68 invariant I-3: dispenser-owned canonical Polydat Kernel.
    /// See `CqlRawDispenser::canonical_kernel`.
    #[allow(dead_code)]
    canonical_kernel: std::sync::Arc<polydat::kernel::PolydatKernel>,
    /// Pre-prepared statement. Constructed at `map_op` time as part
    /// of the dispenser-init stack frame so the per-cycle path has
    /// no preparation latency.
    prepared: Arc<cass::PreparedStatement>,
    /// Type-aware per-position binders built at `map_op` from
    /// prepared statement metadata. One per `?` placeholder.
    binders: Vec<BinderFn>,
    /// Live tracing probability (f64 bits). Loaded per execute;
    /// `cql_trace_rate` control writes here.
    trace_rate_bits: Arc<AtomicU64>,
    /// Bounded retirement queue handle for traced ops.
    /// `None` when the trace log file couldn't be opened at
    /// adapter init — dispenser still respects the rate
    /// (sets the tracing flag on the statement) but skips the
    /// post-execute submit.
    trace_log: std::sync::Arc<LazyTraceLog>,
    /// SRD 73 universal per-op field overrides applied per
    /// execute after the session-level consistency is set.
    /// Empty chain is a hot-path no-op.
    modifiers: nmbrs_runtime::op_modifier::ModifierChain<cass::Statement>,
}

impl OpDispenser for CqlPreparedDispenser {
    fn describe(&self) -> Option<String> {
        Some(format!(
            "CQL prepared: {}",
            flatten_one_line(&self.stmt_text)
        ))
    }

    fn canonical_kernel(&self) -> Option<&std::sync::Arc<nmbrs_runtime::adapter::PolydatKernel>> {
        Some(&self.canonical_kernel)
    }

    fn describe_resolved(&self, wires: &dyn nmbrs_runtime::wires::WireSource) -> Option<String> {
        // SRD-68 Push 5: walk the prepared text and replace each
        // `?` placeholder with the bound name's value pulled
        // through the dispenser's wires surface. The output is
        // not a literal replayable SQL statement (vector / blob
        // literals need adapter-side encoding), but it's an
        // honest representation of what the bind step received
        // for this cycle. String-typed values get single-quoted
        // so operators can spot quoting / escape issues at a
        // glance.
        let mut out = String::with_capacity(self.stmt_text.len() + 64);
        let mut bind_idx = 0usize;
        for ch in self.stmt_text.chars() {
            if ch == '?' {
                if let Some(name) = self.bind_names.get(bind_idx) {
                    let rendered = match wires.get(name) {
                        Some(polydat::ast::Value::Str(s)) => format!("'{s}'"),
                        Some(v) => v.to_display_string(),
                        None => format!("{{?{name}}}"),
                    };
                    out.push_str(&rendered);
                } else {
                    out.push('?');
                }
                bind_idx += 1;
            } else {
                out.push(ch);
            }
        }
        Some(format!("CQL prepared: {}", flatten_one_line(&out)))
    }

    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let wires = ctx.wires;
        Box::pin(async move {
            // `self.prepared` and `self.binders` are both fully
            // constructed at `map_op` time — no per-cycle init.
            let mut stmt = self.prepared.bind();
            let _ = stmt.set_consistency(self.consistency).map_err(|e| {
                ExecutionError::Op(AdapterError {
                    error_name: "bind_error".into(),
                    message: format!("set consistency: {e}"),
                    retryable: false,
                })
            })?;
            // SRD 73: per-op universal-field overrides on top of
            // the session-level consistency. No-op when the user
            // didn't bind any field.
            self.modifiers.apply(&mut stmt);

            // SRD-68 Push 5b: cycle-time `?`-parameter binding
            // through the dispenser's per-fiber Polydat Kernel via the
            // narrow `WireSource` trait. `wires.get(bind_name)`
            // returns the typed `Value` for the position's bind
            // point — same name resolution surface the raw mode
            // uses, no adapter-specific fields path.
            for (bind_idx, name) in self.bind_names.iter().enumerate() {
                if let Some(value) = wires.get(name) {
                    self.binders[bind_idx](&mut stmt, bind_idx, &value).map_err(|e| {
                        ExecutionError::Op(AdapterError {
                            error_name: "bind_error".into(),
                            message: format!("bind position {bind_idx} ('{name}'): {e}"),
                            retryable: false,
                        })
                    })?;
                }
            }

            // Sparse-tracing decision per execute. Atomic load is
            // cheap (single 64-bit read); the RNG roll only fires
            // when the rate is non-zero, so the no-tracing hot
            // path stays effectively free.
            let trace_rate = f64::from_bits(self.trace_rate_bits.load(Ordering::Acquire));
            let trace_this = trace_rate > 0.0 && rand::random::<f64>() < trace_rate;
            if trace_this {
                let _ = stmt.set_tracing(true);
            }

            // Capture metadata for the trace log before consuming
            // the statement. `started_at` is the wall-clock for
            // the `system_traces.sessions` time-window correlation;
            // `started` is the monotonic clock for latency.
            let started_at = std::time::SystemTime::now();
            let started = std::time::Instant::now();

            // Two execute paths so the no-trace hot path stays
            // exactly the existing shape. The `execute_with_tracing`
            // route is only taken when we actually want the
            // server-side trace UUID — it's the vendored
            // cassandra-cpp surface that pairs result with
            // `cass_future_tracing_id`.
            let exec_outcome = if trace_this {
                self.session
                    .get()
                    .execute_with_tracing(&stmt)
                    .await
                    .map(|(r, tid)| (r, tid))
            } else {
                stmt.execute().await.map(|r| (r, None))
            };

            let latency_nanos = started.elapsed().as_nanos() as u64;

            let exec_result = match exec_outcome {
                Ok((result, trace_id)) => {
                    if trace_this {
                        if let Some(log) = self.trace_log.get().await {
                            let binds = self
                                .bind_names
                                .iter()
                                .map(|name| match wires.get(name) {
                                    Some(v) => tracing::format_bind_value(name, &v),
                                    None => format!("{name}=<missing>"),
                                })
                                .collect();
                            log.submit(TraceRecord {
                                cycle,
                                started_at,
                                query: self.stmt_text.clone(),
                                binds,
                                latency_nanos,
                                ok: true,
                                error_name: None,
                                trace_id: trace_id.map(|u| {
                                    let std_uuid: uuid::Uuid = u.into();
                                    std_uuid.to_string()
                                }),
                            });
                        }
                    }
                    Ok(result)
                }
                Err(e) => {
                    let truncated = if self.stmt_text.len() > 200 {
                        format!("{}...", &self.stmt_text[..200])
                    } else {
                        self.stmt_text.clone()
                    };
                    if trace_this && let Some(log) = self.trace_log.get().await {
                        let binds = self
                            .bind_names
                            .iter()
                            .map(|name| match wires.get(name) {
                                Some(v) => tracing::format_bind_value(name, &v),
                                None => format!("{name}=<missing>"),
                            })
                            .collect();
                        log.submit(TraceRecord {
                            cycle,
                            started_at,
                            query: self.stmt_text.clone(),
                            binds,
                            latency_nanos,
                            ok: false,
                            error_name: Some("cql_error".into()),
                            trace_id: None,
                        });
                    }
                    Err(ExecutionError::Op(AdapterError {
                        error_name: "cql_error".into(),
                        message: format!("{e}\n  statement: {truncated}"),
                        retryable: crate::common::cql_error_is_retryable(&e.to_string()),
                    }))
                }
            };

            let result = exec_result?;

            // Build the result body, classifying read vs write inside
            // `from_cass_result`: a SELECT reports its returned rows, a
            // write reports one row written. An empty SELECT (a read
            // that matched nothing) has element_count 0 → no body,
            // preserving the prior no-body-for-empty-read behavior.
            let body_obj = CqlResultBody::from_cass_result(&result);
            let body = if body_obj.element_count() > 0 {
                Some(Box::new(body_obj) as Box<dyn ResultBody>)
            } else {
                None
            };
            Ok(OpResult {
                body,
                skipped: false,
            })
        })
    }
}

// =========================================================================
// CqlBatchDispenser: groups multiple bound statements into one CQL BATCH
// =========================================================================

/// Wraps a prepared statement template and executes batches of bound
/// statements as one CQL BATCH call.
///
/// The executor calls `execute_batch()` with N resolved field sets
/// (one per cycle in the batch). Each is bound to the prepared
/// statement and added to a `cass::Batch`. The batch is executed
/// once. Per-cycle latency is meaningless — only batch latency matters.
struct CqlBatchDispenser {
    session: SessionHandle,
    consistency: cass::Consistency,
    /// Statement text — retained for error diagnostics and the
    /// `describe_resolved` walk only.
    stmt_text: String,
    /// The op field name carrying the statement (for finding it in resolved fields).
    #[allow(dead_code)]
    stmt_field: String,
    bind_names: Vec<String>,
    /// SRD-68 invariant I-3: dispenser-owned canonical Polydat Kernel.
    /// See `CqlRawDispenser::canonical_kernel`.
    #[allow(dead_code)]
    canonical_kernel: std::sync::Arc<polydat::kernel::PolydatKernel>,
    /// Raw `batch: N` row cap from the op param (`0` → unset).
    /// Retained for the `describe_resolved` footer (shows the operator
    /// the configured cap); the per-invocation row count now comes from
    /// the fixed stride `n` / `ExecCtx::run_len`, not from this field.
    batch_n: usize,
    /// FIXED uniform batch stride — the nominal number of cursor
    /// ordinals one invocation reads AND advances (SRD-22 cover-once).
    /// Settled once at `map_op` from `batch:` / `max_batch_size` / the
    /// characterized row size (see [`crate::common::size_estimator::fixed_batch_stride`]).
    /// Reported via [`OpDispenser::rows_per_op`] so the executor drives
    /// the phase cursor with `Σ rows_per_op`; the ACTUAL per-invocation
    /// row count is `ExecCtx::run_len` (== `n` except at the short tail).
    n: usize,
    /// SRD-103 §6 byte budget (`max_batch_size`, a literal
    /// magnitude). `None` → no byte cap. When set, rows are packed
    /// into sub-batches each held under this estimated encoded size
    /// as a safety valve; one op invocation may emit multiple batch
    /// round-trips.
    max_batch_bytes: Option<u64>,
    /// Fires the "single row exceeds max_batch_size" warning at most
    /// once per dispenser lifetime.
    oversize_warned: std::sync::atomic::AtomicBool,
    /// Pre-prepared statement (constructed at `map_op` time).
    prepared: Arc<cass::PreparedStatement>,
    /// Per-position type-aware binders built at `map_op` time
    /// from the prepared statement's metadata. One per `?`.
    binders: Vec<BinderFn>,
    batch_type: cass::BatchType,
    /// Derived ONCE at dispenser init from the batch's inner statements
    /// (uniform template with stride ⇒ one prepared statement): `false` for
    /// counter batches and LWT statements, `true` for plain PK-keyed
    /// upserts. Gates the transient-error retry classification — an
    /// execute failure is `retryable` only when the batch is retry-safe
    /// AND the error is transient. See `cql_statement_retry_safe`.
    retry_safe: bool,
    /// Per-row timer: records amortized latency (batch_nanos / row_count)
    /// for each row in the batch. Enables rows/s throughput in the summary.
    rows_timer: nmbrs_metrics::instruments::timer::Timer,
    /// Cumulative row counter for the status line. Not reset on snapshot.
    rows_total: std::sync::atomic::AtomicU64,
    /// Cumulative count of batch ops that actually wrote ≥1 row — one
    /// inc per successful `execute` (a failed op returns early and is
    /// never counted). Drives the display's true average batch size
    /// (`rows_inserted / batch_writes`) instead of the attempt-based
    /// `rows_inserted / stanzas_total`, which over-counts retries /
    /// non-inserting ops in the denominator. Not reset on snapshot.
    batch_writes: std::sync::atomic::AtomicU64,
    /// Live tracing probability (f64 bits). Loaded once per
    /// batch execute; `cql_trace_rate` control writes here.
    /// Sparse means "trace this batch invocation" — we roll
    /// once for the whole batch, not per row.
    trace_rate_bits: Arc<AtomicU64>,
    /// Bounded retirement queue handle for traced batches.
    /// `None` when the trace log file couldn't be opened at
    /// adapter init — dispenser still respects the rate
    /// (sets the tracing flag on each statement) but skips
    /// the post-execute submit.
    trace_log: std::sync::Arc<LazyTraceLog>,
    /// SRD 73 universal per-op field overrides. A batch is a statement too,
    /// so these target the `cass::Batch` itself — the driver reads the batch's
    /// own aspects (consistency, request timeout, tracing), NOT those of the
    /// member statements added to it.
    modifiers: nmbrs_runtime::op_modifier::ModifierChain<cass::Batch>,
}

impl OpDispenser for CqlBatchDispenser {
    fn describe(&self) -> Option<String> {
        Some(format!("CQL batch: {}", flatten_one_line(&self.stmt_text)))
    }

    fn canonical_kernel(&self) -> Option<&std::sync::Arc<nmbrs_runtime::adapter::PolydatKernel>> {
        Some(&self.canonical_kernel)
    }

    fn describe_resolved(&self, wires: &dyn nmbrs_runtime::wires::WireSource) -> Option<String> {
        // SRD-68 Push 5: render the head row by pulling each
        // bind name through the wires surface. The footer reports
        // the configured batch size so the operator sees how many
        // rows the failing batch was sized for. Wires reflect the
        // current per-fiber coord; for diagnostic intent that is
        // close enough — the row in question is whatever was last
        // active, and the operator's interest is whether the bind
        // values look right at all.
        let mut out = String::with_capacity(self.stmt_text.len() + 64);
        let mut bind_idx = 0usize;
        for ch in self.stmt_text.chars() {
            if ch == '?' {
                if let Some(name) = self.bind_names.get(bind_idx) {
                    let rendered = match wires.get(name) {
                        Some(polydat::ast::Value::Str(s)) => format!("'{s}'"),
                        Some(v) => v.to_display_string(),
                        None => format!("{{?{name}}}"),
                    };
                    out.push_str(&rendered);
                } else {
                    out.push('?');
                }
                bind_idx += 1;
            } else {
                out.push(ch);
            }
        }
        let suffix = match (self.batch_n, self.max_batch_bytes) {
            (n, Some(b)) if n > 0 => format!("  -- batch={n}, max_batch_size={b}B"),
            (0, Some(b)) => format!("  -- max_batch_size={b}B"),
            (n, None) if n > 1 => format!("  -- batch={n}"),
            _ => String::new(),
        };
        Some(format!("CQL batch: {}{}", flatten_one_line(&out), suffix))
    }

    fn status_counters(&self) -> Vec<(&str, u64)> {
        let total = self.rows_total.load(std::sync::atomic::Ordering::Relaxed);
        if total == 0 {
            return Vec::new();
        }
        let batches = self.batch_writes.load(std::sync::atomic::Ordering::Relaxed);
        // Publish `_batch_writes` (INTERNAL, leading underscore) alongside
        // `rows_inserted` through the same status-counter surface so the
        // display can derive the true average batch size
        // (`rows_inserted / _batch_writes`). The underscore keeps it out of
        // the visible rows/s chip row — it exists only as that denominator.
        let mut out = vec![("rows_inserted", total)];
        if batches > 0 {
            out.push(("_batch_writes", batches));
        }
        out
    }

    fn adapter_metrics(
        &self,
    ) -> Vec<(
        String,
        nmbrs_metrics::labels::Labels,
        nmbrs_metrics::snapshot::MetricValue,
    )> {
        use nmbrs_metrics::snapshot::{
            CounterValue, HistogramValue, MetricValue, split_name_label,
        };
        let snap = self.rows_timer.snapshot();
        let total = self.rows_total.load(std::sync::atomic::Ordering::Relaxed);
        let mut out = Vec::new();
        if snap.count > 0 {
            let (name, labels) = split_name_label(self.rows_timer.labels());
            out.push((
                name,
                labels,
                MetricValue::Histogram(HistogramValue::from_hdr(snap.histogram)),
            ));
        }
        if total > 0 {
            out.push((
                "rows_inserted_total".to_string(),
                nmbrs_metrics::labels::Labels::default(),
                MetricValue::Counter(CounterValue::new(total)),
            ));
        }
        out
    }

    fn rows_per_op(&self) -> usize {
        self.n
    }

    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let wires = ctx.wires;
        // SRD-22 cover-once: read exactly the reserved sub-run — the
        // executor advanced the phase cursor by this many ordinals, so
        // the op inserts `[cycle, cycle + total)` and nothing beyond.
        // `run_len` is the fixed stride `n` except at the cursor tail,
        // where the final reservation was short (the partial batch is
        // still inserted in full).
        let total = ctx.run_len.max(1);
        Box::pin(async move {
            // Sparse-tracing decision once for the whole invocation
            // (not per row / per sub-batch). Atomic load is cheap;
            // the RNG roll only fires when the rate is non-zero, so
            // the no-tracing hot path stays effectively free.
            let trace_rate = f64::from_bits(self.trace_rate_bits.load(Ordering::Acquire));
            let trace_this = trace_rate > 0.0 && rand::random::<f64>() < trace_rate;

            // SRD-68 Push 5b' batch contract: "each iteration of the
            // batch is considered another pull … an iteration
            // container." Drive the rows by advancing the per-fiber
            // kernel coord via `wires.advance(coord)` and pulling
            // each bind name via `wires.get(name)`.
            let gather_row = |coord: u64| -> Vec<Option<polydat::ast::Value>> {
                wires.advance(coord);
                self.bind_names.iter().map(|n| wires.get(n)).collect()
            };
            let mut all_rows: Vec<Vec<Option<polydat::ast::Value>>> = Vec::with_capacity(total);
            for row_idx in 0..total {
                all_rows.push(gather_row(cycle + row_idx as u64));
            }

            // Fill-to-budget: pack rows into the current sub-batch,
            // flushing before a row that would push the estimated
            // encoded size over `max_batch_bytes`. Always ≥1 row per
            // sub-batch; `batch: N` (= `total` here) bounds the
            // overall pull. One op invocation may emit multiple
            // batch round-trips — summed into one OpResult; a
            // failure on any sub-batch fails the whole op.
            let mut submitted: usize = 0;
            let mut total_nanos: u64 = 0;
            let mut cur: Vec<Vec<Option<polydat::ast::Value>>> = Vec::new();
            let mut cur_bytes: u64 = 0;
            for row in all_rows {
                let row_bytes = Self::estimate_optional_row(&row);
                if let Some(budget) = self.max_batch_bytes {
                    if !cur.is_empty() && cur_bytes + row_bytes > budget {
                        total_nanos += self.exec_sub_batch(cycle, &cur, trace_this).await?;
                        submitted += cur.len();
                        cur.clear();
                        cur_bytes = 0;
                    }
                    if cur.is_empty()
                        && row_bytes > budget
                        && !self
                            .oversize_warned
                            .swap(true, std::sync::atomic::Ordering::Relaxed)
                    {
                        nmbrs_runtime::diag!(
                            nmbrs_runtime::observer::LogLevel::Warn,
                            "cql batch: a single row (~{row_bytes} B) exceeds \
                             max_batch_size ({budget} B); sending it as a one-row \
                             batch — the server may still reject it",
                        );
                    }
                }
                cur.push(row);
                cur_bytes += row_bytes;
            }
            if !cur.is_empty() {
                total_nanos += self.exec_sub_batch(cycle, &cur, trace_this).await?;
                submitted += cur.len();
            }

            // Amortize the summed batch latency across every
            // submitted row for rows/s throughput reporting.
            let per_row_nanos = total_nanos / submitted.max(1) as u64;
            for _ in 0..submitted {
                self.rows_timer.record(per_row_nanos);
            }
            self.rows_total
                .fetch_add(submitted as u64, std::sync::atomic::Ordering::Relaxed);
            // Count this op as one batch write iff it actually wrote a
            // row — the denominator for the display's true average
            // batch size. Reached only on the success path (a failed
            // sub-batch returns early above), so retried / failed ops
            // never inflate it.
            if submitted >= 1 {
                self.batch_writes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }

            // `rows_inserted` lands on the per-fiber kernel via
            // ctx.wires.write — wrappers above this layer see it
            // through wires.get on the same cycle.
            let _ = ctx
                .wires
                .write("rows_inserted", polydat::ast::Value::U64(submitted as u64));
            // A CQL write reports the rows it wrote as its result: the
            // batch's `element_count` is `submitted` (rows written),
            // just as a SELECT's is the rows it returned. Carried as a
            // single `write_ack` for the whole op (the executed sub-
            // batches acknowledge a write; the driver returns no rows).
            Ok(OpResult {
                body: Some(
                    Box::new(CqlResultBody::write_ack(submitted as u64)) as Box<dyn ResultBody>
                ),
                skipped: false,
            })
        })
    }
}

impl CqlBatchDispenser {
    /// Estimated server-accounted size of one pulled row: the present
    /// values' encoded sizes plus the per-mutation `ROW_OVERHEAD` the
    /// server's batch accounting adds (same accounting as
    /// `estimate_row_size`). Absent wires (`None`) contribute nothing,
    /// matching the bind loop that skips them.
    fn estimate_optional_row(row: &[Option<polydat::ast::Value>]) -> u64 {
        crate::common::size_estimator::ROW_OVERHEAD
            + row
                .iter()
                .filter_map(|o| o.as_ref())
                .map(crate::common::size_estimator::estimate_value_size)
                .sum::<u64>()
    }

    /// Bind and execute one CQL BATCH over the given pulled rows,
    /// exactly as a single batch was executed before byte-bounding.
    /// Returns the round-trip latency in nanos; submits a trace
    /// record when `trace_this`. Any bind/add/execute failure maps to
    /// an `ExecutionError::Op` and fails the whole op.
    async fn exec_sub_batch(
        &self,
        cycle: u64,
        rows: &[Vec<Option<polydat::ast::Value>>],
        trace_this: bool,
    ) -> Result<u64, ExecutionError> {
        let mut batch = self.session.get().batch(self.batch_type);
        // Aspects govern the BATCH itself — a batch is a statement too, and
        // `cass_session_execute_batch` reads the batch's own consistency,
        // request timeout, and tracing, IGNORING those of the member
        // statements. Set them once here (session consistency + the SRD-73
        // universal-field chain + trace flag); the member statements below
        // carry only their bound values.
        let _ = batch.set_consistency(self.consistency).map_err(|e| {
            ExecutionError::Op(AdapterError {
                error_name: "batch_error".into(),
                message: format!("set batch consistency: {e}"),
                retryable: false,
            })
        })?;
        self.modifiers.apply(&mut batch);
        if trace_this {
            let _ = batch.set_tracing(true);
        }
        for (row_idx, row) in rows.iter().enumerate() {
            let mut stmt = self.prepared.bind();
            for (idx, value_opt) in row.iter().enumerate() {
                if let Some(value) = value_opt {
                    self.binders[idx](&mut stmt, idx, value).map_err(|e| {
                        ExecutionError::Op(AdapterError {
                            error_name: "bind_error".into(),
                            message: format!(
                                "bind position {idx} ('{}') row {row_idx}: {e}",
                                self.bind_names.get(idx).map(String::as_str).unwrap_or("?"),
                            ),
                            retryable: false,
                        })
                    })?;
                }
            }
            batch.add_statement(stmt).map_err(|e| {
                ExecutionError::Op(AdapterError {
                    error_name: "batch_error".into(),
                    message: format!("add_statement (row {row_idx}): {e}"),
                    retryable: false,
                })
            })?;
        }
        let row_count = rows.len();

        // `started_at` correlates with `system_traces.sessions`;
        // `batch_start` feeds both metrics and the trace latency.
        let started_at = std::time::SystemTime::now();
        let batch_start = std::time::Instant::now();
        let exec_outcome = if trace_this {
            self.session.get().execute_batch_with_tracing(&batch).await
        } else {
            self.session
                .get()
                .execute_batch(&batch)
                .await
                .map(|r| (r, None))
        };
        let batch_nanos = batch_start.elapsed().as_nanos() as u64;

        match exec_outcome {
            Ok((_result, trace_id)) => {
                if trace_this && let Some(log) = self.trace_log.get().await {
                    log.submit(TraceRecord {
                        cycle,
                        started_at,
                        query: self.stmt_text.clone(),
                        // Batches don't render every row's binds
                        // (could be thousands). One synthetic entry
                        // summarises the sub-batch dispatch.
                        binds: vec![format!("batch of {row_count} rows")],
                        latency_nanos: batch_nanos,
                        ok: true,
                        error_name: None,
                        trace_id: trace_id.map(|u| {
                            let std_uuid: uuid::Uuid = u.into();
                            std_uuid.to_string()
                        }),
                    });
                }
                Ok(batch_nanos)
            }
            Err(e) => {
                if trace_this && let Some(log) = self.trace_log.get().await {
                    log.submit(TraceRecord {
                        cycle,
                        started_at,
                        query: self.stmt_text.clone(),
                        binds: vec![format!("batch of {row_count} rows")],
                        latency_nanos: batch_nanos,
                        ok: false,
                        error_name: Some("cql_error".into()),
                        trace_id: None,
                    });
                }
                // A batch is a statement too: the execute failure carries the
                // SAME class ("cql_error") as a single statement's, so
                // `errors:` routing patterns treat batch timeouts / overloads
                // uniformly. Retryability = the batch's DERIVED retry-safety
                // (from its inner statements, computed once at init) AND the
                // error's transience. (Previously hardcoded
                // `batch_error`/retryable:false — a batch LIB_REQUEST_TIMED_OUT
                // never retried, silently defeating the op's tries budget.)
                // `batch_error` remains the class for STRUCTURAL failures
                // (set-consistency / add_statement), which genuinely aren't
                // retryable requests.
                Err(ExecutionError::Op(AdapterError {
                    error_name: "cql_error".into(),
                    message: format!("execute_batch ({row_count} statements): {e}"),
                    retryable: self.retry_safe
                        && crate::common::cql_error_is_retryable(&e.to_string()),
                }))
            }
        }
    }
}

// =========================================================================
// CqlTimeuuid Polydat node + its inventory registration moved to
// `crate::common::nodes`. Every CQL engine that links this
// adapter gets the node for free regardless of which engine
// feature is enabled.
// =========================================================================

// =========================================================================
// Adapter Registration (inventory-based, link-time)
// =========================================================================

// Register `cassandra-cpp` as a driver implementation of the
// `cql` adapter. `cassandra-cpp` is the internal driver name —
// `adapter=cql` is the user-facing knob; `cqldriver=cassandra-cpp`
// selects this driver from inside that adapter.
//
// Lower rank wins; cassandra-cpp ranks 100 so binaries that
// link both drivers default to cassandra-cpp ahead of scylla
// (200).
inventory::submit! {
    nmbrs_runtime::adapter::DriverImpl {
        adapter: "cql",
        driver: "cassandra-cpp",
        default_rank: 100,
        create: |params| Box::pin(async move {
            // Set the cpp-driver log threshold *before* any
            // cass_cluster_* call — the driver only honors the
            // level set prior to first session construction.
            apply_log_level_once(&params)?;
            let config = CqlConfig::from_params(&params)
                .map_err(|e| format!("CQL config error: {e}"))?;
            CqlAdapter::connect(&config).await
                .map(|a| std::sync::Arc::new(a) as std::sync::Arc<dyn nmbrs_runtime::adapter::DriverAdapter>)
                .map_err(|e| format!("CQL connection failed: {e}"))
        }),
        known_params: || &[
            "hosts", "host", "port", "keyspace", "connect_keyspace", "consistency",
            "username", "password", "timeout", "request_timeout_ms",
            "connect_timeout", "request_timeout",
            "reconnect_base_delay", "reconnect_max_delay",
            "heartbeat_interval", "connection_idle_timeout",
            "cassandra_log_level",
            "trace_rate", "trace_log",
        ],
    }
}

// SRD-35 Push B: declare the cassandra-cpp engine as
// pool-shareable. Phases whose params produce equal
// `CqlConfig::to_resource_key("cassandra-cpp")` keys share
// a single `CqlAdapter` (and therefore a single
// `cass::Session`) for the whole workload — directly
// fixing the per-phase open/close storm that motivates
// SRD-35.
inventory::submit! {
    nmbrs_runtime::adapter::SharedDriverRegistration {
        adapter: "cql",
        driver: "cassandra-cpp",
        share_capability: nmbrs_runtime::resource_pool::ShareCapability::Shared,
        resource_key: |params| {
            let cfg = crate::common::CqlConfig::from_params(params)
                .map_err(|e| format!("CQL config error: {e}"))?;
            Ok(cfg.to_resource_key("cassandra-cpp"))
        },
    }
}

// =========================================================================
// Type-aware value binders
// =========================================================================

/// Extract the CQL ValueType from a ConstDataType.
///
/// The safe cassandra-cpp wrapper doesn't expose get_type() on
/// ConstDataType, so we call the C FFI directly. ConstDataType
/// is a newtype over `*const CassDataType` (with PhantomData).
fn get_const_data_type_value_type<T>(dt: &T) -> cass::ValueType {
    // ConstDataType layout: (*const _CassDataType, PhantomData)
    // We read the first pointer-sized field.
    let raw: *const cassandra_cpp_sys::CassDataType_ =
        unsafe { *(dt as *const _ as *const *const cassandra_cpp_sys::CassDataType_) };
    let cass_vt = unsafe { cassandra_cpp_sys::cass_data_type_type(raw) };
    // Map the C enum value to the Rust ValueType.
    // CassValueType_ values match ValueType variant ordering.
    cass_value_type_from_raw(cass_vt)
}

/// Read the class name of a CUSTOM-typed CassDataType, if present.
///
/// Calls the C FFI `cass_data_type_class_name` directly because
/// the safe Rust wrapper takes ownership of a `DataType` for
/// this accessor, which we can't do — we're inspecting a
/// `ConstDataType` borrowed from the live `PreparedStatement`.
///
/// For non-CUSTOM types the class name is empty/null; we return
/// `None` in that case. For vector columns the class name is the
/// fully-qualified Java type expression the cluster uses
/// internally (e.g. `org.apache.cassandra.db.marshal.VectorType`
/// with element type / dimensions encoded in the parameter
/// list). Capturing this verbatim in the binder-fallback
/// diagnostic gives us the evidence we need to add precise
/// VECTOR detection to `binder_meta::cass_to_polydat` without
/// guessing the format.
fn get_const_data_type_class_name<T>(dt: &T) -> Option<String> {
    use std::os::raw::c_char;
    let raw: *const cassandra_cpp_sys::CassDataType_ =
        unsafe { *(dt as *const _ as *const *const cassandra_cpp_sys::CassDataType_) };
    // `cass_data_type_class_name(dt, &out_ptr, &out_len)` writes
    // a borrowed UTF-8 slice on success (pointer + length). The
    // slice's lifetime is tied to the data type — we copy it out
    // into an owned String so the result is independent of the
    // CassDataType's lifetime.
    let mut name_ptr: *const c_char = std::ptr::null();
    let mut name_len: usize = 0;
    let err =
        unsafe { cassandra_cpp_sys::cass_data_type_class_name(raw, &mut name_ptr, &mut name_len) };
    if err != cassandra_cpp_sys::CassError_::CASS_OK || name_ptr.is_null() || name_len == 0 {
        return None;
    }
    // Safety: the C API returns a valid byte slice on success;
    // we trust the cassandra-cpp driver's class-name strings to
    // be UTF-8 (they're Java fully-qualified names — ASCII in
    // practice).
    let bytes = unsafe { std::slice::from_raw_parts(name_ptr as *const u8, name_len) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Convert a raw CassValueType_ C enum to cass::ValueType.
fn cass_value_type_from_raw(raw: cassandra_cpp_sys::CassValueType_) -> cass::ValueType {
    use cassandra_cpp_sys::CassValueType_::*;
    match raw {
        CASS_VALUE_TYPE_ASCII => cass::ValueType::ASCII,
        CASS_VALUE_TYPE_BIGINT => cass::ValueType::BIGINT,
        CASS_VALUE_TYPE_BLOB => cass::ValueType::BLOB,
        CASS_VALUE_TYPE_BOOLEAN => cass::ValueType::BOOLEAN,
        CASS_VALUE_TYPE_COUNTER => cass::ValueType::COUNTER,
        CASS_VALUE_TYPE_DOUBLE => cass::ValueType::DOUBLE,
        CASS_VALUE_TYPE_FLOAT => cass::ValueType::FLOAT,
        CASS_VALUE_TYPE_INT => cass::ValueType::INT,
        CASS_VALUE_TYPE_TEXT => cass::ValueType::TEXT,
        CASS_VALUE_TYPE_VARCHAR => cass::ValueType::VARCHAR,
        CASS_VALUE_TYPE_SMALL_INT => cass::ValueType::SMALL_INT,
        CASS_VALUE_TYPE_TINY_INT => cass::ValueType::TINY_INT,
        CASS_VALUE_TYPE_CUSTOM => cass::ValueType::CUSTOM,
        _ => cass::ValueType::UNKNOWN,
    }
}

/// Create a binder function for a given CQL column type.
///
/// The returned closure converts a Polydat `Value` to the correct CQL
/// type and binds it at the given position. Built once per `?`
/// position in a prepared statement; applied per row.
/// Parse a Polydat vector string `[0.1, 0.2, ...]` into CQL vector
/// binary encoding (big-endian IEEE 754 floats, concatenated).
fn parse_vector_to_bytes(s: &str) -> Vec<u8> {
    let trimmed = s.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Vec::new();
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let mut bytes = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        if let Ok(f) = part.parse::<f32>() {
            bytes.extend_from_slice(&f.to_be_bytes());
        } else {
            return Vec::new(); // not a float vector
        }
    }
    bytes
}

/// Convert LE f32 bytes (GK native) to BE f32 bytes (CQL vector encoding).
///
/// Swaps each 4-byte group from little-endian to big-endian in place.
/// If the length is not a multiple of 4, trailing bytes are passed through.
fn le_to_be_f32_bytes(le: &[u8]) -> Vec<u8> {
    let mut be = Vec::with_capacity(le.len());
    for chunk in le.chunks(4) {
        if chunk.len() == 4 {
            // Reinterpret as LE f32, emit as BE f32
            be.extend_from_slice(&[chunk[3], chunk[2], chunk[1], chunk[0]]);
        } else {
            be.extend_from_slice(chunk);
        }
    }
    be
}

/// Encode a typed `f32` slice as a contiguous BE `[u8]` buffer in
/// the CQL vector wire format. One pre-sized allocation; no
/// intermediate string round-trip.
fn vec_f32_to_be_bytes(slice: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(slice.len() * 4);
    for v in slice {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out
}

/// Encode a typed `i32` slice as a contiguous BE `[u8]` buffer.
fn vec_i32_to_be_bytes(slice: &[i32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(slice.len() * 4);
    for v in slice {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out
}

/// Encode a typed `f64` slice as a contiguous BE `[u8]` buffer.
fn vec_f64_to_be_bytes(slice: &[f64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(slice.len() * 8);
    for v in slice {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out
}

/// Encode a typed `i64` slice as a contiguous BE `[u8]` buffer.
fn vec_i64_to_be_bytes(slice: &[i64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(slice.len() * 8);
    for v in slice {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out
}

/// Encode a typed half-precision-float slice as a contiguous BE
/// `[u8]` buffer (2 bytes per element).
fn vec_f16_to_be_bytes(slice: &[half::f16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(slice.len() * 2);
    for v in slice {
        out.extend_from_slice(&v.to_bits().to_be_bytes());
    }
    out
}

/// Encode a typed `i16` slice as a contiguous BE `[u8]` buffer.
fn vec_i16_to_be_bytes(slice: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(slice.len() * 2);
    for v in slice {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out
}

type BinderFn = Box<
    dyn Fn(&mut cass::Statement, usize, &polydat::ast::Value) -> cass::Result<()> + Send + Sync,
>;

/// Bind a CQL `vector<float, N>` (CUSTOM column with FloatType
/// element) from any polydat value variant the runtime might
/// supply. The native paths (`VecF32`, `Bytes`) avoid any
/// `to_display_string` round-trip; widening / narrowing arms
/// cover the cross-precision cases.
fn bind_vector_float(
    stmt: &mut cass::Statement,
    idx: usize,
    value: &polydat::ast::Value,
) -> cass::Result<()> {
    match value {
        polydat::ast::Value::VecF32(arc) => {
            stmt.bind_bytes(idx, vec_f32_to_be_bytes(arc))?;
        }
        polydat::ast::Value::VecF64(arc) => {
            let narrowed: Vec<f32> = arc.iter().map(|v| *v as f32).collect();
            stmt.bind_bytes(idx, vec_f32_to_be_bytes(&narrowed))?;
        }
        polydat::ast::Value::VecF16(arc) => {
            let widened: Vec<f32> = arc.iter().map(|v| v.to_f32()).collect();
            stmt.bind_bytes(idx, vec_f32_to_be_bytes(&widened))?;
        }
        polydat::ast::Value::Bytes(le_bytes) => {
            stmt.bind_bytes(idx, le_to_be_f32_bytes(le_bytes))?;
        }
        other => {
            // Fallback (e.g. Str holding a `[…]` literal). Keep the
            // legacy parse path so workloads that build the literal
            // by hand still work, but at the cost of allocation +
            // parse — the warning surface lives in `map_op`.
            let s = other.to_display_string();
            let bytes = parse_vector_to_bytes(&s);
            if bytes.is_empty() {
                stmt.bind_string(idx, &s)?;
            } else {
                stmt.bind_bytes(idx, bytes)?;
            }
        }
    }
    Ok(())
}

/// Bind a CQL `vector<int, N>` (CUSTOM column with IntType element).
fn bind_vector_int(
    stmt: &mut cass::Statement,
    idx: usize,
    value: &polydat::ast::Value,
) -> cass::Result<()> {
    match value {
        polydat::ast::Value::VecI32(arc) => {
            stmt.bind_bytes(idx, vec_i32_to_be_bytes(arc))?;
        }
        polydat::ast::Value::VecI64(arc) => {
            let narrowed: Vec<i32> = arc.iter().map(|v| *v as i32).collect();
            stmt.bind_bytes(idx, vec_i32_to_be_bytes(&narrowed))?;
        }
        polydat::ast::Value::VecI16(arc) => {
            let widened: Vec<i32> = arc.iter().map(|v| *v as i32).collect();
            stmt.bind_bytes(idx, vec_i32_to_be_bytes(&widened))?;
        }
        polydat::ast::Value::Bytes(le_bytes) => {
            // Polydat byte buffers for i32 vectors land LE; CQL wants BE.
            let mut be = Vec::with_capacity(le_bytes.len());
            for chunk in le_bytes.chunks(4) {
                if chunk.len() == 4 {
                    be.extend_from_slice(&[chunk[3], chunk[2], chunk[1], chunk[0]]);
                } else {
                    be.extend_from_slice(chunk);
                }
            }
            stmt.bind_bytes(idx, be)?;
        }
        other => {
            stmt.bind_string(idx, &other.to_display_string())?;
        }
    }
    Ok(())
}

/// Bind a CQL `vector<double, N>` (CUSTOM column with DoubleType element).
fn bind_vector_double(
    stmt: &mut cass::Statement,
    idx: usize,
    value: &polydat::ast::Value,
) -> cass::Result<()> {
    match value {
        polydat::ast::Value::VecF64(arc) => {
            stmt.bind_bytes(idx, vec_f64_to_be_bytes(arc))?;
        }
        polydat::ast::Value::VecF32(arc) => {
            let widened: Vec<f64> = arc.iter().map(|v| *v as f64).collect();
            stmt.bind_bytes(idx, vec_f64_to_be_bytes(&widened))?;
        }
        polydat::ast::Value::VecF16(arc) => {
            let widened: Vec<f64> = arc.iter().map(|v| v.to_f32() as f64).collect();
            stmt.bind_bytes(idx, vec_f64_to_be_bytes(&widened))?;
        }
        polydat::ast::Value::Bytes(le_bytes) => {
            let mut be = Vec::with_capacity(le_bytes.len());
            for chunk in le_bytes.chunks(8) {
                if chunk.len() == 8 {
                    be.extend_from_slice(&[
                        chunk[7], chunk[6], chunk[5], chunk[4], chunk[3], chunk[2], chunk[1],
                        chunk[0],
                    ]);
                } else {
                    be.extend_from_slice(chunk);
                }
            }
            stmt.bind_bytes(idx, be)?;
        }
        other => {
            stmt.bind_string(idx, &other.to_display_string())?;
        }
    }
    Ok(())
}

/// Bind a CQL `vector<bigint, N>` (CUSTOM column with LongType element).
fn bind_vector_long(
    stmt: &mut cass::Statement,
    idx: usize,
    value: &polydat::ast::Value,
) -> cass::Result<()> {
    match value {
        polydat::ast::Value::VecI64(arc) => {
            stmt.bind_bytes(idx, vec_i64_to_be_bytes(arc))?;
        }
        polydat::ast::Value::VecI32(arc) => {
            let widened: Vec<i64> = arc.iter().map(|v| *v as i64).collect();
            stmt.bind_bytes(idx, vec_i64_to_be_bytes(&widened))?;
        }
        polydat::ast::Value::VecI16(arc) => {
            let widened: Vec<i64> = arc.iter().map(|v| *v as i64).collect();
            stmt.bind_bytes(idx, vec_i64_to_be_bytes(&widened))?;
        }
        polydat::ast::Value::Bytes(le_bytes) => {
            let mut be = Vec::with_capacity(le_bytes.len());
            for chunk in le_bytes.chunks(8) {
                if chunk.len() == 8 {
                    be.extend_from_slice(&[
                        chunk[7], chunk[6], chunk[5], chunk[4], chunk[3], chunk[2], chunk[1],
                        chunk[0],
                    ]);
                } else {
                    be.extend_from_slice(chunk);
                }
            }
            stmt.bind_bytes(idx, be)?;
        }
        other => {
            stmt.bind_string(idx, &other.to_display_string())?;
        }
    }
    Ok(())
}

/// Bind a CQL `vector<smallint, N>` (CUSTOM column with ShortType element).
fn bind_vector_short(
    stmt: &mut cass::Statement,
    idx: usize,
    value: &polydat::ast::Value,
) -> cass::Result<()> {
    match value {
        polydat::ast::Value::VecI16(arc) => {
            stmt.bind_bytes(idx, vec_i16_to_be_bytes(arc))?;
        }
        polydat::ast::Value::VecI32(arc) => {
            let narrowed: Vec<i16> = arc.iter().map(|v| *v as i16).collect();
            stmt.bind_bytes(idx, vec_i16_to_be_bytes(&narrowed))?;
        }
        polydat::ast::Value::VecI64(arc) => {
            let narrowed: Vec<i16> = arc.iter().map(|v| *v as i16).collect();
            stmt.bind_bytes(idx, vec_i16_to_be_bytes(&narrowed))?;
        }
        polydat::ast::Value::Bytes(le_bytes) => {
            let mut be = Vec::with_capacity(le_bytes.len());
            for chunk in le_bytes.chunks(2) {
                if chunk.len() == 2 {
                    be.extend_from_slice(&[chunk[1], chunk[0]]);
                } else {
                    be.extend_from_slice(chunk);
                }
            }
            stmt.bind_bytes(idx, be)?;
        }
        other => {
            stmt.bind_string(idx, &other.to_display_string())?;
        }
    }
    Ok(())
}

/// Bind a CQL `vector<half_float, N>` (CUSTOM column with
/// HalfFloatType / Float16Type element).
fn bind_vector_half(
    stmt: &mut cass::Statement,
    idx: usize,
    value: &polydat::ast::Value,
) -> cass::Result<()> {
    match value {
        polydat::ast::Value::VecF16(arc) => {
            stmt.bind_bytes(idx, vec_f16_to_be_bytes(arc))?;
        }
        polydat::ast::Value::VecF32(arc) => {
            let narrowed: Vec<half::f16> = arc.iter().map(|v| half::f16::from_f32(*v)).collect();
            stmt.bind_bytes(idx, vec_f16_to_be_bytes(&narrowed))?;
        }
        polydat::ast::Value::VecF64(arc) => {
            let narrowed: Vec<half::f16> = arc.iter().map(|v| half::f16::from_f64(*v)).collect();
            stmt.bind_bytes(idx, vec_f16_to_be_bytes(&narrowed))?;
        }
        polydat::ast::Value::Bytes(le_bytes) => {
            let mut be = Vec::with_capacity(le_bytes.len());
            for chunk in le_bytes.chunks(2) {
                if chunk.len() == 2 {
                    be.extend_from_slice(&[chunk[1], chunk[0]]);
                } else {
                    be.extend_from_slice(chunk);
                }
            }
            stmt.bind_bytes(idx, be)?;
        }
        other => {
            stmt.bind_string(idx, &other.to_display_string())?;
        }
    }
    Ok(())
}

/// Per-column-type binder factory.
///
/// `class_name` is only meaningful for `CUSTOM` columns — it
/// carries the Cassandra Java FQN (`...VectorType(FloatType, N)`)
/// that disambiguates the vector element type. For non-CUSTOM
/// columns it's ignored.
///
/// The CUSTOM arm specialises per vector element type so a value
/// arriving as `Value::VecF32` / `VecI32` / `VecF64` / `VecI64` /
/// `VecF16` / `VecI16` binds native — pre-sized BE encode, single
/// `bind_bytes` call, no `to_display_string` allocation, no
/// parse-the-string-back round trip.
fn make_binder(cql_type: cass::ValueType, class_name: Option<&str>) -> BinderFn {
    match cql_type {
        // String types — fast-path Value::Str so a wire carrying
        // an already-owned Arc<str> binds without allocating a
        // fresh String through to_display_string().
        cass::ValueType::ASCII | cass::ValueType::TEXT | cass::ValueType::VARCHAR => {
            Box::new(|stmt, idx, value| {
                match value {
                    polydat::ast::Value::Str(arc) => {
                        stmt.bind_string(idx, arc)?;
                    }
                    other => {
                        stmt.bind_string(idx, &other.to_display_string())?;
                    }
                }
                Ok(())
            })
        }
        // 32-bit integer types
        cass::ValueType::INT | cass::ValueType::SMALL_INT | cass::ValueType::TINY_INT => {
            Box::new(|stmt, idx, value| {
                let n = match value {
                    polydat::ast::Value::U64(v) => *v as i32,
                    polydat::ast::Value::F64(v) => *v as i32,
                    polydat::ast::Value::Str(s) => s.parse::<i32>().unwrap_or(0),
                    _ => 0,
                };
                stmt.bind_int32(idx, n)?;
                Ok(())
            })
        }
        // 64-bit integer types
        cass::ValueType::BIGINT | cass::ValueType::COUNTER => Box::new(|stmt, idx, value| {
            let n = match value {
                polydat::ast::Value::U64(v) => *v as i64,
                polydat::ast::Value::F64(v) => *v as i64,
                polydat::ast::Value::Str(s) => s.parse::<i64>().unwrap_or(0),
                _ => 0,
            };
            stmt.bind_int64(idx, n)?;
            Ok(())
        }),
        // Float
        cass::ValueType::FLOAT => Box::new(|stmt, idx, value| {
            let f = match value {
                polydat::ast::Value::F64(v) => *v as f32,
                polydat::ast::Value::U64(v) => *v as f32,
                polydat::ast::Value::Str(s) => s.parse::<f32>().unwrap_or(0.0),
                _ => 0.0,
            };
            stmt.bind_float(idx, f)?;
            Ok(())
        }),
        // Double
        cass::ValueType::DOUBLE => Box::new(|stmt, idx, value| {
            let f = match value {
                polydat::ast::Value::F64(v) => *v,
                polydat::ast::Value::U64(v) => *v as f64,
                polydat::ast::Value::Str(s) => s.parse::<f64>().unwrap_or(0.0),
                _ => 0.0,
            };
            stmt.bind_double(idx, f)?;
            Ok(())
        }),
        // Boolean
        cass::ValueType::BOOLEAN => Box::new(|stmt, idx, value| {
            let b = match value {
                polydat::ast::Value::Bool(v) => *v,
                polydat::ast::Value::U64(v) => *v != 0,
                polydat::ast::Value::Str(s) => &**s == "true" || &**s == "1",
                _ => false,
            };
            stmt.bind_bool(idx, b)?;
            Ok(())
        }),
        // CUSTOM type — predominantly CQL vectors. The element type
        // comes from the parsed class name. Each branch builds a
        // closure specialised to the cluster-reported element so
        // matching typed-vec inputs take the zero-copy native path.
        cass::ValueType::CUSTOM => {
            let element = class_name.and_then(binder_meta::parse_vector_element);
            match element {
                Some(binder_meta::VectorElement::Float) => {
                    Box::new(|stmt, idx, value| bind_vector_float(stmt, idx, value))
                }
                Some(binder_meta::VectorElement::Int) => {
                    Box::new(|stmt, idx, value| bind_vector_int(stmt, idx, value))
                }
                Some(binder_meta::VectorElement::Double) => {
                    Box::new(|stmt, idx, value| bind_vector_double(stmt, idx, value))
                }
                Some(binder_meta::VectorElement::Long) => {
                    Box::new(|stmt, idx, value| bind_vector_long(stmt, idx, value))
                }
                Some(binder_meta::VectorElement::Short) => {
                    Box::new(|stmt, idx, value| bind_vector_short(stmt, idx, value))
                }
                Some(binder_meta::VectorElement::Half) => {
                    Box::new(|stmt, idx, value| bind_vector_half(stmt, idx, value))
                }
                // Unknown / Other / no class name — keep the legacy
                // Bytes-or-string round-trip behaviour. Logs from
                // map_op already WARN about the missing typed
                // mapping, so this is the safety-net path.
                _ => Box::new(|stmt, idx, value| {
                    match value {
                        polydat::ast::Value::Bytes(le_bytes) => {
                            let be_bytes = le_to_be_f32_bytes(le_bytes);
                            stmt.bind_bytes(idx, be_bytes)?;
                        }
                        _ => {
                            let s = value.to_display_string();
                            let bytes = parse_vector_to_bytes(&s);
                            if bytes.is_empty() {
                                stmt.bind_string(idx, &s)?;
                            } else {
                                stmt.bind_bytes(idx, bytes)?;
                            }
                        }
                    }
                    Ok(())
                }),
            }
        }
        // BLOB: raw bytes binding
        cass::ValueType::BLOB => Box::new(|stmt, idx, value| {
            match value {
                polydat::ast::Value::Bytes(bytes) => {
                    stmt.bind_bytes(idx, bytes.to_vec())?;
                }
                _ => {
                    stmt.bind_string(idx, &value.to_display_string())?;
                }
            }
            Ok(())
        }),
        // Everything else: bind as string
        _ => Box::new(|stmt, idx, value| {
            stmt.bind_string(idx, &value.to_display_string())?;
            Ok(())
        }),
    }
}

#[cfg(test)]
mod connect_diag_tests {
    use super::{enrich_connect_error, process_resource_snapshot};

    #[test]
    fn unrelated_errors_pass_through_unchanged() {
        // Auth, network, syntax — anything that isn't a libuv
        // init failure — must NOT trigger the resource-limit
        // diagnostic. The user shouldn't be told "check your
        // ulimit" when the password was wrong.
        let out = enrich_connect_error("connect", "Bad credentials".into());
        assert_eq!(out, "connect: Bad credentials");
        assert!(
            !out.contains("RLIMIT"),
            "no resource diag expected, got: {out}"
        );
        assert!(
            !out.contains("nofile_soft"),
            "no resource diag expected, got: {out}"
        );
    }

    #[test]
    fn lib_unable_to_init_attaches_resource_snapshot() {
        // The exact error string the user pasted in the bug
        // report — confirms the contains-match catches it and
        // appends the actionable section.
        let raw = "Cassandra error LIB_UNABLE_TO_INIT: \
                   Unable to initialize cluster event loop";
        let out = enrich_connect_error("connect to keyspace 'baselines'", raw.into());
        assert!(out.contains("LIB_UNABLE_TO_INIT"), "raw error preserved");
        assert!(
            out.contains("per-process resource exhaustion"),
            "diagnostic explanation present"
        );
        assert!(
            out.contains("Process resource snapshot:"),
            "snapshot section present"
        );
        assert!(out.contains("fds_in_use:"), "FD count line present");
        assert!(
            out.contains("nofile_soft:") && out.contains("nofile_hard:"),
            "FD limit lines present"
        );
        assert!(out.contains("ulimit -n"), "remediation hint present");
    }

    #[test]
    fn snapshot_renders_numeric_values_on_linux() {
        // On Linux (the typical CI / test environment) the
        // /proc reads succeed and we should see real numbers
        // rather than `?` placeholders. Skip the assertion if
        // /proc isn't available (sandboxed CI, non-Linux dev
        // host) — the function must still return *something*.
        let snap = process_resource_snapshot();
        assert!(snap.contains("fds_in_use:"));
        if std::path::Path::new("/proc/self/fd").exists() {
            assert!(
                !snap.contains("fds_in_use:    ?"),
                "/proc/self/fd should yield a numeric count on Linux, got: {snap}"
            );
        }
    }
}

#[cfg(test)]
mod result_body_tests {
    use super::CqlResultBody;
    use nmbrs_runtime::adapter::ResultBody;

    #[test]
    fn write_result_reports_rows_written_as_element_count() {
        // A CQL write carries no returned rows; its element_count is
        // the number of rows it wrote — a single write = 1, a batch = n.
        assert_eq!(CqlResultBody::write_ack(1).element_count(), 1);
        assert_eq!(CqlResultBody::write_ack(500).element_count(), 500);
        // A write body still serializes to an empty row array — its
        // `count`, not its `body`, carries the write result.
        assert_eq!(
            CqlResultBody::write_ack(7).to_json(),
            serde_json::Value::Array(Vec::new()),
        );
    }

    #[test]
    fn read_result_reports_returned_rows_as_element_count() {
        // A read (returned rows present) reports the returned-row count
        // and ignores any written count.
        let mut row = std::collections::HashMap::new();
        row.insert("key".to_string(), serde_json::json!("v"));
        let body = CqlResultBody {
            rows: vec![row],
            written_rows: 0,
        };
        assert_eq!(body.element_count(), 1);
    }

    #[test]
    fn empty_read_reports_zero_not_a_write() {
        // A SELECT that matched no rows is still a read: no returned
        // rows AND no written count, so element_count is 0 (preserving
        // the pre-change no-body behavior for empty reads).
        let body = CqlResultBody {
            rows: Vec::new(),
            written_rows: 0,
        };
        assert_eq!(body.element_count(), 0);
    }
}
