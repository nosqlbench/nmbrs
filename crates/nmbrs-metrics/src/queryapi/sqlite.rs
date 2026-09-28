// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Sqlite [`DataSource`] adapter against the nmbrs
//! `metrics.db` schema. **First non-test `DataSource`
//! impl** — see SRD-47 §"What this push enables next".
//!
//! # Schema contract
//!
//! Mirrors the writer-side schema in
//! `crates/nmbrs-metrics/src/reporters/sqlite.rs::create_schema`
//! (post-cutover, fully denormalised — no `label_set`):
//!
//! - `metric_family(id, name, type, unit, help)` —
//!   `type` ∈ `{"counter", "gauge", "summary"}`.
//! - `metric_instance(id, family_id, spec UNIQUE)` —
//!   `spec` is the OpenMetrics-canonical sample identifier
//!   `name{k="v",…}` (sorted by key). Two logical label sets
//!   that are equal as a mapping produce equal spec text and
//!   resolve to the same instance row.
//! - `instance_label(instance_id, key, value)` —
//!   one row per label pair, including `__name__` (so
//!   queries filter on metric family the same way they
//!   filter on any other dimension).
//! - `sample_value(instance_id, timestamp_ms, interval_ms,
//!   count, sum, min, max, mean, stddev, p50..p999)` —
//!   one row per (instance, sample). Per `metric_family.type`
//!   only some columns are populated:
//!     - `counter`: `count` only
//!     - `gauge`:   `mean` only
//!     - `summary`: all stat columns
//!
//! Indexes: `instance_label(key, value, instance_id)` covers
//! matcher resolution; `instance_label(instance_id)` covers
//! per-instance label materialisation.
//!
//! # Metric naming convention
//!
//! MetricsQL queries reference values by `__name__`. For the
//! nmbrs schema:
//!
//! - **Counter** family `cycles_total` is queried as
//!   `cycles_total` (no suffix).
//! - **Gauge** family `cpu_load` is queried as
//!   `cpu_load`.
//! - **Summary** family `latency` exposes virtual metric
//!   names `latency_count`, `latency_sum`, `latency_min`,
//!   `latency_max`, `latency_mean`, `latency_stddev`,
//!   `latency_p50` … `latency_p999` — each maps to one
//!   stat column on `sample_value`.
//!
//! The adapter resolves `__name__` by trying the bare name
//! against `metric_family.name` first; if no row is found,
//! it strips a known stat suffix (`_count`, `_p99`, etc.)
//! and tries the truncated name. This matches Prometheus'
//! convention for summary/histogram metrics.

use super::catalog::{ExemplarPoint, LabelSet, MetricCatalog, MetricFamilyMeta, MetricType};
use super::{
    MatchOp as MatcherOp, Matcher, MetricAccess, QueryError as DataSourceError, Sample, Series,
    Vector,
};
use rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value};
use std::path::PathBuf;
use std::sync::Mutex;

/// Sqlite-backed [`DataSource`] (and [`MetricCatalog`]).
/// Wraps a [`Connection`] behind a [`Mutex`] so the trait's
/// `&self` methods can serialize statement preparation and
/// execution.
///
/// Open with [`SqliteDataSource::open`] for a path or
/// [`SqliteDataSource::from_connection`] to bring your own
/// connection (useful for in-memory tests). Either path
/// applies the read-side PRAGMAs the adapter wants.
///
/// `db_path` is captured when [`Self::open`] is used so the
/// catalog cache layer can mtime-invalidate against the
/// on-disk file. Sources opened via
/// [`Self::from_connection`] don't have a path and thus
/// can't drive mtime-based invalidation; their cache layer
/// has to fall back to TTL + manual `invalidate()`.
/// How a metric instance is selected across the executions in a
/// (possibly `refine`-d) session store. Reports default to
/// [`LatestPerInstance`](ExecutionSelection::LatestPerInstance).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionSelection {
    /// For each logical metric instance (its dimensional labels,
    /// ignoring `exec_id`/`session`), keep only the data from the
    /// NEWEST execution that produced it. A `refine`'s unchanged-phase
    /// instances survive from their original execution. The default.
    #[default]
    LatestPerInstance,
    /// No execution filtering — every matching sample across every
    /// execution is included.
    All,
    /// Only the single newest execution (max `exec_id`); instances
    /// that only ran in older executions are excluded.
    Latest,
    /// One specific `exec_id`.
    Specific(u64),
}

pub struct SqliteDataSource {
    conn: Mutex<Connection>,
    db_path: Option<PathBuf>,
    selection: ExecutionSelection,
}

impl SqliteDataSource {
    /// Open `metrics.db` at `path` for read queries. Applies
    /// read-tuned PRAGMAs (cache, mmap, temp store) but does
    /// NOT mutate the schema — schema creation is the
    /// writer's responsibility.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, DataSourceError> {
        let path_ref = path.as_ref();
        let conn = Connection::open_with_flags(
            path_ref,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| DataSourceError::new(format!("open metrics.db: {e}")))?;
        let mut src = Self::from_connection(conn)?;
        src.db_path = Some(path_ref.to_path_buf());
        Ok(src)
    }

    /// Path the source was opened against, if any.
    /// `None` when constructed via [`Self::from_connection`]
    /// (e.g. `:memory:` test fixtures).
    ///
    /// Used by [`Self::mtime_fn`] so `CachedCatalog` can
    /// invalidate against on-disk changes when the writer
    /// flushes new data.
    pub fn db_path(&self) -> Option<&std::path::Path> {
        self.db_path.as_deref()
    }

    /// Closure suitable for
    /// [`super::catalog::CachedCatalog::with_mtime_fn`].
    /// Returns the latest `mtime` of the underlying file as
    /// a monotonic [`Instant`] (computed by anchoring the
    /// system-time-difference against an initial fixed
    /// epoch — `Instant` itself isn't constructable from a
    /// system time, but the comparison is what matters and
    /// the offset-relative-to-epoch survives that).
    ///
    /// Returns `None` when the source was opened via
    /// [`Self::from_connection`] (no path available) or when
    /// the file has disappeared since open.
    pub fn mtime_fn(
        &self,
    ) -> Option<impl Fn() -> Option<std::time::Instant> + Send + Sync + 'static> {
        let path = self.db_path.clone()?;
        // Anchor the mtime translation: capture
        // `Instant::now() - SystemTime::now()` once, then
        // mtime → Instant = anchor_instant + (mtime -
        // anchor_system_time). This stays monotonic for as
        // long as the system clock doesn't run backwards.
        let anchor_instant = std::time::Instant::now();
        let anchor_system = std::time::SystemTime::now();
        Some(move || -> Option<std::time::Instant> {
            let meta = std::fs::metadata(&path).ok()?;
            let mtime = meta.modified().ok()?;
            let delta = mtime.duration_since(anchor_system).ok()?;
            anchor_instant.checked_add(delta)
        })
    }

    /// Wrap an existing [`Connection`]. PRAGMAs the adapter
    /// wants are applied here too — safe to call against
    /// connections opened in any mode (the PRAGMAs are
    /// connection-scoped).
    pub fn from_connection(conn: Connection) -> Result<Self, DataSourceError> {
        // Connection-scoped read-side tuning. WAL mode is set
        // by the writer (database-wide and persistent), so
        // we don't touch it here. The adapter's own scope is
        // page cache + temp-table location + mmap window.
        conn.execute_batch(
            "PRAGMA cache_size = -65536;\
             PRAGMA temp_store = MEMORY;\
             PRAGMA mmap_size  = 268435456;",
        )
        .map_err(|e| DataSourceError::new(format!("apply pragmas: {e}")))?;
        register_regexp(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path: None,
            selection: ExecutionSelection::default(),
        })
    }

    /// Set how metric instances are selected across executions.
    /// Defaults to [`ExecutionSelection::LatestPerInstance`].
    pub fn with_execution_selection(mut self, selection: ExecutionSelection) -> Self {
        self.selection = selection;
        self
    }
}

/// Among `series` (which carry an `exec_id` label), keep only the one
/// from the newest execution for each logical instance — its labels
/// excluding `exec_id`/`session`. Ties (shouldn't happen — exec_ids
/// are distinct) keep the first seen.
fn retain_latest_per_instance(series: Vec<Series>) -> Vec<Series> {
    use std::collections::{HashMap, HashSet};
    let mut best: HashMap<Vec<(String, String)>, (i64, usize)> = HashMap::new();
    for (i, s) in series.iter().enumerate() {
        let exec = s
            .labels
            .iter()
            .find(|(k, _)| k == "exec_id")
            .and_then(|(_, v)| v.parse::<i64>().ok())
            .unwrap_or(0);
        let mut logical: Vec<(String, String)> = s
            .labels
            .iter()
            .filter(|(k, _)| k != "exec_id" && k != "session")
            .cloned()
            .collect();
        logical.sort();
        match best.get(&logical) {
            Some(&(e, _)) if e >= exec => {}
            _ => {
                best.insert(logical, (exec, i));
            }
        }
    }
    let keep: HashSet<usize> = best.values().map(|(_, i)| *i).collect();
    series
        .into_iter()
        .enumerate()
        .filter_map(|(i, s)| keep.contains(&i).then_some(s))
        .collect()
}

/// Register a connection-scoped `REGEXP(pattern, value)`
/// scalar function backed by the Rust `regex` crate. SQLite
/// recognises `value REGEXP pattern` as syntactic sugar for
/// `regexp(pattern, value)` (note the argument order — SQLite
/// passes the pattern first, the value second), so the
/// matcher-emitted SQL `v.value REGEXP ?` resolves through
/// this function for every row scan.
///
/// MetricsQL regex matchers are anchored — `label=~"pat"`
/// matches when `pat` matches the full label value, not a
/// substring. We anchor with `^(?:...)$` here so a bare
/// pattern like `label.*` matches values like `label_00`
/// without inadvertently matching `prefix_label_x`.
///
/// The compiled `Regex` is cached per query via a small LRU
/// (capacity 16) so repeated row evaluations don't re-compile
/// the pattern. Compilation errors surface as a sqlite
/// runtime error so the metricsql evaluator's error path
/// reports a useful diagnostic.
fn register_regexp(conn: &Connection) -> Result<(), DataSourceError> {
    use rusqlite::functions::FunctionFlags;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    // Compiled regex cache keyed by pattern string. Bounded
    // by the number of distinct patterns within one query —
    // metricsql evaluators only emit a handful of regex
    // matchers per expression, so a `HashMap` without
    // eviction is the right shape; cleared at the next
    // open() since the closure owns it.
    let cache: StdMutex<HashMap<String, regex::Regex>> = StdMutex::new(HashMap::new());
    conn.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_UTF8,
        move |ctx| {
            let pattern: String = ctx.get(0)?;
            let value: String = ctx.get(1)?;
            let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            let re = match guard.get(&pattern) {
                Some(r) => r.clone(),
                None => {
                    let anchored = format!("^(?:{pattern})$");
                    let r = regex::Regex::new(&anchored).map_err(|e| {
                        rusqlite::Error::UserFunctionError(
                            format!("regexp pattern '{pattern}': {e}").into(),
                        )
                    })?;
                    guard.insert(pattern, r.clone());
                    r
                }
            };
            Ok(re.is_match(&value))
        },
    )
    .map_err(|e| DataSourceError::new(format!("register REGEXP function: {e}")))?;
    Ok(())
}

impl MetricAccess for SqliteDataSource {
    fn select_range(
        &self,
        matchers: &[Matcher],
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vector, DataSourceError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| DataSourceError::new("sqlite mutex poisoned"))?;

        // 1. Resolve __name__ to (family_id, family_name, stat_column).
        //    Without a name matcher we can't build a meaningful
        //    selector — return nothing rather than scanning all
        //    families.
        let Some(name_matcher) = matchers.iter().find(|m| m.label == "__name__") else {
            return Ok(Vector::default());
        };
        let resolved = match name_matcher.op {
            MatcherOp::Eq => resolve_family(&conn, &name_matcher.value)?,
            // `!=` / regex on `__name__` would mean "every
            // family except / matching pattern" — out of
            // scope for this push (would need to enumerate
            // families and dispatch per-family). Cleanly
            // surface the gap.
            _ => {
                return Err(DataSourceError::new(
                    "non-Eq match on __name__ not supported by sqlite adapter yet",
                ));
            }
        };
        let Some(resolved) = resolved else {
            // Family doesn't exist — empty result, not an error.
            return Ok(Vector::default());
        };

        // 2. Resolve label matchers to a set of candidate
        //    label_set_ids. The empty matcher list means
        //    "every label set under this family" — handled
        //    by skipping the IN clause entirely. `exec_id` is a *column*
        //    (`mi.exec_id`), not an `instance_label` row, so it is excluded
        //    here and applied as `exec_label_filter` below (SRD-90 §M6: the
        //    `exec_id` dimensional label, applied where it lives in sqlite).
        let other_matchers: Vec<&Matcher> = matchers
            .iter()
            .filter(|m| m.label != "__name__" && m.label != "exec_id")
            .collect();
        let label_filter = instance_label_filter_clause(&other_matchers)?;

        // 3. JOIN to sample_value. Single query grouped per
        //    instance; per-instance labels are materialised
        //    in a follow-up query. `stat_expr` is a full SQL
        //    expression (referring to `sv.*` columns) so
        //    derived stats like `_rate` can blend `count` and
        //    `interval_ms` per row.
        //
        //    For `_rate` queries we additionally project
        //    `sv.interval_ms` so the loop below can warn when
        //    any sample's underlying window was sub-second —
        //    `_rate` over a sub-second window quantizes harshly
        //    (a phase that completed 10k ops in 800ms gives
        //    `12500 ops/sec`; 700ms or 900ms give the same to
        //    integer precision). Surfacing the warning prevents
        //    silent fictional precision.
        let stat_expr = resolved.stat_expr;
        let interval_proj = if resolved.is_rate {
            ", sv.interval_ms"
        } else {
            ""
        };
        // Execution selection (SRD-77). `Specific` / `Latest` narrow
        // at the SQL level; `LatestPerInstance` fetches every
        // execution and picks the newest per logical instance in a
        // post-pass (below); `All` does no filtering.
        // SRD-89 §3b / SRD-90 §M6 — `exec_id` is a uniform dimensional label:
        // when a read carries an `exec_id="N"` matcher (injected by the scoping
        // layer for the reading execution), apply it to the `exec_id` column
        // here, exactly as the in-memory tier applies it to its label set. This
        // is what scopes a hybrid live read's sqlite tail to the reading
        // execution. It composes with `ExecutionSelection` (the report path,
        // which carries no such matcher).
        let exec_label_filter = matchers
            .iter()
            .find(|m| m.label == "exec_id" && m.op == MatcherOp::Eq)
            .and_then(|m| m.value.parse::<i64>().ok())
            .map(|n| format!("AND mi.exec_id = {n} "))
            .unwrap_or_default();
        let exec_filter = match self.selection {
            ExecutionSelection::Specific(n) => format!("AND mi.exec_id = {n} "),
            ExecutionSelection::Latest => "AND mi.exec_id = \
                (SELECT MAX(exec_id) FROM metric_instance WHERE family_id = ?1) "
                .to_string(),
            ExecutionSelection::All | ExecutionSelection::LatestPerInstance => String::new(),
        };
        let exec_filter = format!("{exec_filter}{exec_label_filter}");
        let sql = format!(
            "SELECT mi.id, sv.timestamp_ms, {stat_expr}{interval_proj} \
             FROM metric_instance mi \
             JOIN sample_value sv ON sv.instance_id = mi.id \
             WHERE mi.family_id = ?1 \
               AND sv.timestamp_ms >= ?2 AND sv.timestamp_ms <= ?3 \
               {label_filter} {exec_filter}\
             ORDER BY mi.id, sv.timestamp_ms"
        );

        let mut params: Vec<Value> = vec![
            Value::Integer(resolved.family_id),
            Value::Integer(start_ms),
            Value::Integer(end_ms),
        ];
        for m in &other_matchers {
            params.push(Value::Text(m.label.clone()));
            params.push(Value::Text(m.value.clone()));
        }

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DataSourceError::new(format!("prepare fetch: {e}")))?;

        // Stream rows, grouping by instance into Series. The
        // ORDER BY lets us batch contiguous rows of the same
        // instance.
        let mut rows = stmt
            .query(params_from_iter(params.iter()))
            .map_err(|e| DataSourceError::new(format!("query fetch: {e}")))?;
        let mut out: Vec<Series> = Vec::new();
        let mut current_instance_id: Option<i64> = None;
        let mut current_samples: Vec<Sample> = Vec::new();
        // Sub-1s rate warning state: track the minimum interval
        // observed for any `_rate` row across the whole query.
        // Emit a single summary warning post-fetch so a
        // multi-series query doesn't fan out one warning per
        // instance.
        let mut min_rate_interval_ms: Option<i64> = None;

        while let Some(row) = rows
            .next()
            .map_err(|e| DataSourceError::new(format!("step fetch: {e}")))?
        {
            let instance_id: i64 = row
                .get(0)
                .map_err(|e| DataSourceError::new(format!("row.get(0): {e}")))?;
            let timestamp_ms: i64 = row
                .get(1)
                .map_err(|e| DataSourceError::new(format!("row.get(1): {e}")))?;
            let value: f64 = row
                .get::<_, Option<f64>>(2)
                .map_err(|e| DataSourceError::new(format!("row.get(2): {e}")))?
                .unwrap_or(f64::NAN);
            if resolved.is_rate {
                let iv: i64 = row
                    .get(3)
                    .map_err(|e| DataSourceError::new(format!("row.get(3): {e}")))?;
                if iv > 0 && iv < 1000 {
                    min_rate_interval_ms =
                        Some(min_rate_interval_ms.map(|m| m.min(iv)).unwrap_or(iv));
                }
            }

            if Some(instance_id) != current_instance_id {
                if let Some(prev) = current_instance_id.take() {
                    out.push(materialize_series(
                        &conn,
                        prev,
                        &resolved.virtual_name,
                        std::mem::take(&mut current_samples),
                    )?);
                }
                current_instance_id = Some(instance_id);
            }
            current_samples.push(Sample {
                timestamp_ms,
                value,
            });
        }
        if let Some(iv) = min_rate_interval_ms {
            eprintln!(
                "warning: `{}` evaluated over samples with sub-1s windows \
                 (shortest seen: {iv}ms). Intervals are precisely measured \
                 (ms-resolution), so the numeric rate is honest — but a short \
                 window samples a brief slice of phase activity and is more \
                 susceptible to instantaneous noise (warmup, GC pause, single \
                 outlier op). For a steady-state view, prefer rate({}[30s]) or \
                 ensure the phase runs long enough to span ≥ 1 cadence window.",
                resolved.virtual_name,
                resolved.virtual_name.trim_end_matches("_rate"),
            );
        }
        if let Some(last) = current_instance_id {
            out.push(materialize_series(
                &conn,
                last,
                &resolved.virtual_name,
                current_samples,
            )?);
        }
        // `LatestPerInstance`: among the series fetched across all
        // executions, keep only the newest execution's data per
        // logical instance. (`Specific`/`Latest`/`All` already
        // applied their filter in SQL or not at all.)
        if self.selection == ExecutionSelection::LatestPerInstance {
            out = retain_latest_per_instance(out);
        }
        Ok(Vector::new(out))
    }
}

// =====================================================================
// MetricCatalog impl
// =====================================================================

impl MetricCatalog for SqliteDataSource {
    fn metric_families(&self) -> Result<Vec<MetricFamilyMeta>, DataSourceError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| DataSourceError::new("sqlite mutex poisoned"))?;
        let mut stmt = conn
            .prepare("SELECT name, type, unit, help FROM metric_family ORDER BY name")
            .map_err(|e| DataSourceError::new(format!("prepare families: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|e| DataSourceError::new(format!("query families: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            let (name, ty_str, unit, help) =
                row.map_err(|e| DataSourceError::new(format!("decode family row: {e}")))?;
            out.push(MetricFamilyMeta {
                name,
                ty: MetricType::parse(&ty_str),
                unit,
                help,
            });
        }
        Ok(out)
    }

    fn label_keys(&self, family_filter: Option<&str>) -> Result<Vec<String>, DataSourceError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| DataSourceError::new("sqlite mutex poisoned"))?;
        let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        // `__name__` lives in `instance_label` for matcher
        // uniformity (so `{__name__="x"}` works the same way
        // as any other label filter), but the catalog's
        // `label_keys()` enumerates dimensional labels — the
        // metric family name surfaces through
        // `metric_families()` instead.
        let sql = match family_filter {
            Some(_) => {
                "SELECT DISTINCT il.key \
                 FROM instance_label il \
                 JOIN metric_instance mi ON mi.id = il.instance_id \
                 JOIN metric_family mf ON mf.id = mi.family_id \
                 WHERE mf.name = ?1 AND il.key != '__name__' \
                 ORDER BY il.key"
            }
            None => {
                "SELECT DISTINCT key FROM instance_label \
                 WHERE key != '__name__' \
                 ORDER BY key"
            }
        };

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| DataSourceError::new(format!("prepare label_keys: {e}")))?;
        let mut rows: Box<dyn Iterator<Item = rusqlite::Result<String>>> = match family_filter {
            Some(name) => Box::new(
                stmt.query_map([name], |r| r.get::<_, String>(0))
                    .map_err(|e| DataSourceError::new(format!("query label_keys: {e}")))?
                    .collect::<Vec<_>>()
                    .into_iter(),
            ),
            None => Box::new(
                stmt.query_map([], |r| r.get::<_, String>(0))
                    .map_err(|e| DataSourceError::new(format!("query label_keys: {e}")))?
                    .collect::<Vec<_>>()
                    .into_iter(),
            ),
        };
        for row in &mut rows {
            let k = row.map_err(|e| DataSourceError::new(format!("decode label_key: {e}")))?;
            keys.insert(k);
        }
        Ok(keys.into_iter().collect())
    }

    fn label_values(
        &self,
        key: &str,
        family_filter: Option<&str>,
    ) -> Result<Vec<String>, DataSourceError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| DataSourceError::new("sqlite mutex poisoned"))?;
        let sql = match family_filter {
            Some(_) => {
                "SELECT DISTINCT il.value \
                 FROM instance_label il \
                 JOIN metric_instance mi ON mi.id = il.instance_id \
                 JOIN metric_family mf ON mf.id = mi.family_id \
                 WHERE il.key = ?1 AND mf.name = ?2 \
                 ORDER BY il.value"
            }
            None => {
                "SELECT DISTINCT value FROM instance_label \
                 WHERE key = ?1 \
                 ORDER BY value"
            }
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| DataSourceError::new(format!("prepare label_values: {e}")))?;
        let mut out = Vec::new();
        let rows: Vec<rusqlite::Result<String>> = match family_filter {
            Some(name) => stmt
                .query_map([key, name], |r| r.get::<_, String>(0))
                .map_err(|e| DataSourceError::new(format!("query label_values: {e}")))?
                .collect(),
            None => stmt
                .query_map([key], |r| r.get::<_, String>(0))
                .map_err(|e| DataSourceError::new(format!("query label_values: {e}")))?
                .collect(),
        };
        for row in rows {
            out.push(row.map_err(|e| DataSourceError::new(format!("decode value: {e}")))?);
        }
        Ok(out)
    }

    fn series(&self, matchers: &[Matcher]) -> Result<Vec<LabelSet>, DataSourceError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| DataSourceError::new("sqlite mutex poisoned"))?;

        // `__name__` matcher restricts to a single family;
        // bare-Eq is supported, regex / Ne is not (same
        // restriction as `fetch`).
        let name_matcher = matchers.iter().find(|m| m.label == "__name__");
        let resolved = match name_matcher.map(|m| m.op) {
            Some(MatcherOp::Eq) => resolve_family(&conn, &name_matcher.unwrap().value)?,
            Some(_) => {
                return Err(DataSourceError::new(
                    "non-Eq match on __name__ not supported by sqlite catalog yet",
                ));
            }
            None => None,
        };

        let other_matchers: Vec<&Matcher> =
            matchers.iter().filter(|m| m.label != "__name__").collect();

        let label_filter = instance_label_filter_clause(&other_matchers)?;

        let sql_with_family = format!(
            "SELECT mi.id FROM metric_instance mi \
             WHERE mi.family_id = ?1 \
             {label_filter} \
             ORDER BY mi.id"
        );
        let sql_no_family = format!(
            "SELECT mi.id FROM metric_instance mi \
             WHERE 1=1 \
             {label_filter} \
             ORDER BY mi.id"
        );
        let sql = if resolved.is_some() {
            sql_with_family
        } else {
            sql_no_family
        };

        let mut params: Vec<Value> = Vec::new();
        if let Some(r) = &resolved {
            params.push(Value::Integer(r.family_id));
        }
        for m in &other_matchers {
            params.push(Value::Text(m.label.clone()));
            params.push(Value::Text(m.value.clone()));
        }

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DataSourceError::new(format!("prepare series: {e}")))?;
        let rows = stmt
            .query_map(params_from_iter(params.iter()), |r| r.get::<_, i64>(0))
            .map_err(|e| DataSourceError::new(format!("query series: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            let instance_id =
                row.map_err(|e| DataSourceError::new(format!("decode series row: {e}")))?;
            // `__name__` is just another row in instance_label
            // (the writer stores it canonically); pull labels
            // sorted, then promote `__name__` to first
            // position so the trait's caller sees the
            // OpenMetrics convention up front.
            let mut labels = materialize_instance_labels(&conn, instance_id)?;
            if let Some(pos) = labels.iter().position(|(k, _)| k == "__name__")
                && pos != 0
            {
                let pair = labels.remove(pos);
                labels.insert(0, pair);
            }
            out.push(labels);
        }
        Ok(out)
    }

    fn exemplars(
        &self,
        matchers: &[Matcher],
        time_range: Option<(i64, i64)>,
    ) -> Result<Vec<ExemplarPoint>, DataSourceError> {
        // Translate matchers into the same instance-id
        // selection `series` uses, then JOIN onto exemplar
        // rows on the (instance_id, sample_timestamp_ms)
        // pair-key.
        let conn = self
            .conn
            .lock()
            .map_err(|_| DataSourceError::new("sqlite mutex poisoned"))?;

        let name_matcher = matchers.iter().find(|m| m.label == "__name__");
        let resolved = match name_matcher.map(|m| m.op) {
            Some(MatcherOp::Eq) => resolve_family(&conn, &name_matcher.unwrap().value)?,
            Some(_) => {
                return Err(DataSourceError::new(
                    "non-Eq match on __name__ not supported by sqlite catalog yet",
                ));
            }
            None => None,
        };
        let other_matchers: Vec<&Matcher> =
            matchers.iter().filter(|m| m.label != "__name__").collect();
        let label_filter = instance_label_filter_clause(&other_matchers)?;

        let (start_ms, end_ms) = time_range.unwrap_or((i64::MIN, i64::MAX));

        let sql_with_family = format!(
            "SELECT mi.id, \
                    e.sample_timestamp_ms, e.value, \
                    e.timestamp_ms, e.labels_spec \
             FROM exemplar e \
             JOIN metric_instance mi ON mi.id = e.instance_id \
             WHERE mi.family_id = ?1 \
               AND e.sample_timestamp_ms >= ?2 \
               AND e.sample_timestamp_ms <= ?3 \
               {label_filter} \
             ORDER BY e.sample_timestamp_ms"
        );
        let sql_no_family = format!(
            "SELECT mi.id, \
                    e.sample_timestamp_ms, e.value, \
                    e.timestamp_ms, e.labels_spec \
             FROM exemplar e \
             JOIN metric_instance mi ON mi.id = e.instance_id \
             WHERE e.sample_timestamp_ms >= ?1 \
               AND e.sample_timestamp_ms <= ?2 \
               {label_filter} \
             ORDER BY e.sample_timestamp_ms"
        );

        let mut params: Vec<Value> = Vec::new();
        let sql = match &resolved {
            Some(r) => {
                params.push(Value::Integer(r.family_id));
                params.push(Value::Integer(start_ms));
                params.push(Value::Integer(end_ms));
                sql_with_family
            }
            None => {
                params.push(Value::Integer(start_ms));
                params.push(Value::Integer(end_ms));
                sql_no_family
            }
        };
        for m in &other_matchers {
            params.push(Value::Text(m.label.clone()));
            params.push(Value::Text(m.value.clone()));
        }

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| DataSourceError::new(format!("prepare exemplars: {e}")))?;
        let rows = stmt
            .query_map(params_from_iter(params.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,         // instance_id
                    r.get::<_, i64>(1)?,         // sample_timestamp_ms
                    r.get::<_, f64>(2)?,         // value
                    r.get::<_, Option<i64>>(3)?, // timestamp_ms
                    r.get::<_, String>(4)?,      // labels_spec
                ))
            })
            .map_err(|e| DataSourceError::new(format!("query exemplars: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            let (instance_id, sample_ts, value, ts, labels_spec) =
                row.map_err(|e| DataSourceError::new(format!("decode exemplar: {e}")))?;
            let mut series = materialize_instance_labels(&conn, instance_id)?;
            if let Some(pos) = series.iter().position(|(k, _)| k == "__name__")
                && pos != 0
            {
                let pair = series.remove(pos);
                series.insert(0, pair);
            }
            let labels = parse_labels_spec(&labels_spec);
            out.push(ExemplarPoint {
                series,
                sample_timestamp_ms: sample_ts,
                value,
                timestamp_ms: ts,
                labels,
            });
        }
        Ok(out)
    }
}

/// Parse the `key="value",key="value"` spec encoding back
/// into a label list. Inverse of the writer's spec
/// formatter. Tolerant of trailing whitespace / empty input.
pub fn parse_labels_spec(spec: &str) -> Vec<(String, String)> {
    let s = spec.trim();
    if s.is_empty() {
        return Vec::new();
    }
    // Manual tokenizer — quoted values may contain commas,
    // which serde_json would parse cleanly but we don't
    // want a JSON dep on this read path. Two-state walker.
    let mut out = Vec::new();
    let mut cur_key = String::new();
    let mut cur_val = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Read key up to '='.
        while i < bytes.len() && bytes[i] != b'=' {
            cur_key.push(bytes[i] as char);
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        i += 1; // consume '='
        // Optional quote.
        let quoted = i < bytes.len() && bytes[i] == b'"';
        if quoted {
            i += 1;
        }
        while i < bytes.len() {
            if quoted {
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
            } else if bytes[i] == b',' {
                break;
            }
            cur_val.push(bytes[i] as char);
            i += 1;
        }
        out.push((cur_key.trim().to_string(), cur_val.clone()));
        cur_key.clear();
        cur_val.clear();
        // Skip optional ',' and any whitespace.
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
    }
    out
}

/// Read every label pair for an instance into a sorted Vec.
/// Post-cutover the labels live denormalised on
/// `instance_label`, including `__name__`.
fn materialize_instance_labels(
    conn: &Connection,
    instance_id: i64,
) -> Result<Vec<(String, String)>, DataSourceError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT key, value FROM instance_label \
         WHERE instance_id = ?1 \
         ORDER BY key",
        )
        .map_err(|e| DataSourceError::new(format!("prepare label set: {e}")))?;
    let rows = stmt
        .query_map([instance_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| DataSourceError::new(format!("query label set: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| DataSourceError::new(format!("decode label entry: {e}")))?);
    }
    Ok(out)
}

/// Lookup result for `__name__` resolution.
struct ResolvedName {
    family_id: i64,
    /// Virtual name as the user wrote it (e.g. `latency_p99`).
    /// Re-applied as `__name__` on every result series so the
    /// downstream evaluator sees the name it queried for.
    virtual_name: String,
    /// Full SQL expression that produces the sample's value
    /// from a `sample_value` row aliased as `sv`. For native
    /// columns this is `sv.<col>`; the synthetic `_rate`
    /// suffix uses a derived expression that blends `count`
    /// and `interval_ms` so single-snapshot counters yield a
    /// useful per-second value without needing PromQL's
    /// `rate([window])` (which requires ≥2 samples).
    stat_expr: &'static str,
    /// `true` when the resolved name carries the synthetic
    /// `_rate` suffix. Used by [`fetch`] to project
    /// `sv.interval_ms` alongside the value column and warn
    /// when any sample's interval is below 1s — a `_rate`
    /// computation over a sub-second window quantizes
    /// heavily (a 50ms sample of a counter that counted N
    /// items gives `N × 20 ops/sec` granularity), and
    /// operators rarely want that quietly.
    is_rate: bool,
}

/// Try to resolve a metric name to a (family, stat-expr)
/// pair. The lookup tries the bare name first (for
/// counter/gauge/summary families with no suffix), then —
/// if no family is found — strips a known stat suffix and
/// tries again (the summary-suffix convention).
fn resolve_family(conn: &Connection, name: &str) -> Result<Option<ResolvedName>, DataSourceError> {
    // Bare-name lookup: matches counter / gauge / summary
    // families whose name equals the query verbatim.
    if let Some((family_id, family_type)) = lookup_family(conn, name)? {
        let stat_expr = default_column_for_type(&family_type);
        return Ok(Some(ResolvedName {
            family_id,
            virtual_name: name.to_string(),
            stat_expr,
            is_rate: false,
        }));
    }
    // Suffix lookup: `<family>_<stat>` against summary,
    // histogram, counter or gauge-with-count families. The
    // suffix list is closed and ordered longest-first so
    // `_p999` is preferred over `_p9`.
    //
    // Each suffix lists which family types it applies to:
    // `_p99` only makes sense on summary/histogram (with a
    // p99 column); `_rate` makes sense on anything with a
    // `count` column (counters, summaries, histograms).
    for suffix in STAT_SUFFIXES {
        if let Some(stripped) = name.strip_suffix(suffix.text)
            && let Some((family_id, family_type)) = lookup_family(conn, stripped)?
            && suffix.applies_to(&family_type)
        {
            return Ok(Some(ResolvedName {
                family_id,
                virtual_name: name.to_string(),
                stat_expr: suffix.expr,
                is_rate: suffix.text == "_rate",
            }));
        }
    }
    Ok(None)
}

fn lookup_family(conn: &Connection, name: &str) -> Result<Option<(i64, String)>, DataSourceError> {
    conn.query_row(
        "SELECT id, type FROM metric_family WHERE name = ?1",
        rusqlite::params![name],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .map_err(|e| DataSourceError::new(format!("family lookup: {e}")))
}

/// Default expression for a family type's bare-name value.
/// Returns a SQL fragment that reads from `sv.*` columns;
/// the suffix lookup overrides this when the user names a
/// specific stat.
pub fn default_column_for_type(family_type: &str) -> &'static str {
    match family_type {
        // Counters: cumulative observation total.
        "counter" => "sv.count",
        // Gauges: instantaneous reading.
        "gauge" => "sv.mean",
        // Summaries: bare name returns observation count;
        // suffixes route to specific stat columns
        // (`_sum` → sum, `_p99` → p99, etc.).
        "summary" => "sv.count",
        // Histograms (bucketed): bare name returns the
        // cumulative count column. Bucket samples are
        // distinguished via the `le` label, not via the
        // metric name; selectors typically look like
        // `latency_bucket{le="0.5"}`. The `_bucket` suffix
        // resolves to the same `count` column, while
        // `_sum` and `_count` siblings resolve through the
        // STAT_SUFFIXES table.
        "histogram" => "sv.count",
        // GaugeHistogram: identical schema shape to
        // Histogram; the type label tells consumers
        // (metricsql evaluator) to allow non-monotonic
        // buckets.
        "gaugehistogram" => "sv.count",
        // Info: always-1 metric whose data lives in its
        // labels.
        "info" => "sv.count",
        // StateSet: one series per state, value 0 or 1
        // indicating whether the state is active. Stored
        // in the `mean` column (gauge convention).
        "stateset" => "sv.mean",
        // Unknown / OpenMetrics fallback: treat as gauge
        // (mean column). Defensive — any type tag we don't
        // explicitly recognise returns the mean column so
        // the query at least produces something rather
        // than empty.
        "unknown" => "sv.mean",
        _ => "sv.mean", // Same fallback for non-spec types.
    }
}

/// Closed table of stat-name suffixes. Order matters
/// only insofar as `strip_suffix` matches the longest first
/// would — Rust's `str::strip_suffix` is exact; we just
/// iterate in the natural order.
struct StatSuffix {
    text: &'static str,
    /// SQL expression read from `sv.*` columns. For native
    /// columns this is `sv.<col>`; the synthetic `_rate`
    /// derives `(count * 1000.0 / interval_ms)` so per-sample
    /// counters yield a per-second rate without needing a
    /// range vector.
    expr: &'static str,
    /// Family-type predicate. Returns true if the suffix can
    /// be applied to a family of the given type. Percentile
    /// stats only make sense on histograms; `_rate` makes
    /// sense on anything with a `count` column.
    applies_to_fn: fn(&str) -> bool,
}

impl StatSuffix {
    fn applies_to(&self, family_type: &str) -> bool {
        (self.applies_to_fn)(family_type)
    }
}

fn applies_summary(t: &str) -> bool {
    matches!(t, "summary" | "histogram" | "gaugehistogram")
}

fn applies_counted(t: &str) -> bool {
    // Anything with a `count` column on `sample_value`. The
    // openmetrics types that fall in here: counter, summary,
    // histogram (bucketed), info. Gauges don't have count.
    matches!(
        t,
        "counter" | "summary" | "histogram" | "gaugehistogram" | "info"
    )
}

fn applies_any(_t: &str) -> bool {
    // Every `sample_value` row carries `interval_ms` regardless of
    // family type — the accumulated window this sample covers.
    true
}

const STAT_SUFFIXES: &[StatSuffix] = &[
    StatSuffix {
        text: "_p999",
        expr: "sv.p999",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_p99",
        expr: "sv.p99",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_p98",
        expr: "sv.p98",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_p95",
        expr: "sv.p95",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_p90",
        expr: "sv.p90",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_p75",
        expr: "sv.p75",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_p50",
        expr: "sv.p50",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_count",
        expr: "sv.count",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_sum",
        expr: "sv.sum",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_min",
        expr: "sv.min",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_max",
        expr: "sv.max",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_mean",
        expr: "sv.mean",
        applies_to_fn: applies_summary,
    },
    StatSuffix {
        text: "_stddev",
        expr: "sv.stddev",
        applies_to_fn: applies_summary,
    },
    // Synthetic per-second rate. Counters are stored CUMULATIVE
    // (Prometheus/VM-schematic, see the cumulative-counter note), so the
    // per-window rate is the increase since the previous sample over its
    // interval: `(count − LAG(count)) * 1000 / interval_ms`. `COALESCE`
    // treats the series start (no predecessor) as 0, so a phase that
    // produced exactly one cadence snapshot still gives
    // `total / phase_duration_seconds` — the per-phase throughput — and
    // multi-sample series give each window's true per-second increase.
    // `NULLIF` guards div-by-zero when interval_ms is 0.
    // (Histograms keep a per-window `count`; aligning histogram counts to
    // cumulative is a separate follow-on — `_rate` over a histogram is
    // not a supported path.)
    StatSuffix {
        text: "_rate",
        expr: "((CAST(sv.count AS REAL) \
                - COALESCE(LAG(CAST(sv.count AS REAL)) \
                    OVER (PARTITION BY sv.instance_id ORDER BY sv.timestamp_ms), 0)) \
               * 1000.0 / NULLIF(sv.interval_ms, 0))",
        applies_to_fn: applies_counted,
    },
    // Synthetic active-window length. Every sample records the wall
    // clock its window accumulated (`interval_ms`), so
    // `sum_over_time(F_interval_ns[…])` is a series' total ACTIVE
    // time — exact even for the single close-flush sample a
    // sub-cadence phase leaves behind, where first/last timestamp
    // arithmetic degenerates to zero. Exposed in nanoseconds to
    // match the ecosystem's raw-ns + display-scaling convention
    // (`TimeUnit::for_max_nanos`); precision is honest to the
    // stored millisecond.
    StatSuffix {
        text: "_interval_ns",
        expr: "(CAST(sv.interval_ms AS REAL) * 1000000.0)",
        applies_to_fn: applies_any,
    },
];

/// Build the SQL fragment that narrows by label matchers.
/// Returns `""` when there are no non-`__name__` matchers.
/// Otherwise produces `AND mi.id IN (... INTERSECT ...)` —
/// one subquery per matcher hits the
/// `instance_label(key, value, instance_id)` covering index.
fn instance_label_filter_clause(matchers: &[&Matcher]) -> Result<String, DataSourceError> {
    if matchers.is_empty() {
        return Ok(String::new());
    }
    let mut parts: Vec<String> = Vec::with_capacity(matchers.len());
    for (i, m) in matchers.iter().enumerate() {
        let kparam = i * 2 + 4; // 1, 2, 3 are family_id + ts range
        let vparam = i * 2 + 5;
        let cmp_clause = match m.op {
            MatcherOp::Eq => format!("il.key = ?{kparam} AND il.value = ?{vparam}"),
            MatcherOp::Ne => format!("il.key = ?{kparam} AND il.value != ?{vparam}"),
            MatcherOp::EqRegex => format!("il.key = ?{kparam} AND il.value REGEXP ?{vparam}"),
            MatcherOp::NeRegex => format!("il.key = ?{kparam} AND NOT (il.value REGEXP ?{vparam})"),
        };
        parts.push(format!(
            "SELECT il.instance_id FROM instance_label il WHERE {cmp_clause}"
        ));
    }
    Ok(format!(" AND mi.id IN ({})", parts.join(" INTERSECT ")))
}

/// Build the `Series.labels` for an instance: every label
/// row, with `__name__` promoted to the first slot per the
/// trait contract. Uses `materialize_instance_labels` so the
/// single materialiser stays the chokepoint.
fn materialize_series(
    conn: &Connection,
    instance_id: i64,
    virtual_name: &str,
    samples: Vec<Sample>,
) -> Result<Series, DataSourceError> {
    let mut labels = materialize_instance_labels(conn, instance_id)?;
    // Replace any stored `__name__` with the virtual name
    // (suffix-stripping resolves `latency_p99` → family
    // `latency` + stat `p99`; we want callers to see the
    // virtual name they queried). Promote to first slot.
    labels.retain(|(k, _)| k != "__name__");
    labels.insert(0, ("__name__".to_string(), virtual_name.to_string()));
    Ok(Series { labels, samples })
}

// Register the sqlite reader as a discoverable access backend so the
// MetricsQL engine (or any consumer) can locate it by scheme without a
// compile dep: `queryapi::provider("sqlite").open(db_path)`.
inventory::submit! {
    super::AccessProvider {
        scheme: "sqlite",
        open: |target| {
            SqliteDataSource::open(target)
                .map(|s| Box::new(s) as Box<dyn super::MetricAccess>)
        },
    }
}
