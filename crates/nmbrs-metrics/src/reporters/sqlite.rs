// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SQLite metrics reporter with normalized OpenMetrics-aligned schema.
//!
//! Feature-gated behind `sqlite`.

#[cfg(feature = "sqlite")]
mod inner {
    use std::collections::HashMap;
    use std::path::Path;

    use rusqlite::{Connection, params};

    use crate::labels::Labels;
    use crate::scheduler::Reporter;
    use crate::snapshot::{Metric, MetricFamily, MetricSet, MetricType, MetricValue};

    /// Latest-execution metadata rows for keys matching `key_like`
    /// (a `LIKE` pattern), from `execution_metadata` at the highest
    /// `exec_id` that carries such a key. Falls back to legacy
    /// `session_metadata` for dbs written before the per-execution
    /// metadata split. The natural read for "the report/summary/etc.
    /// definitions of the most recent execution".
    pub fn latest_execution_metadata_like(
        conn: &Connection,
        key_like: &str,
    ) -> Vec<(String, String)> {
        latest_execution_with_metadata_like(conn, key_like).1
    }

    /// Like [`latest_execution_metadata_like`], but also surfaces the
    /// `exec_id` the rows came from. `Some(id)` when the rows were read
    /// from `execution_metadata` (the per-execution split); `None` when
    /// they fell back to legacy `session_metadata` (pre-split dbs, which
    /// carry no `exec_id`) or when nothing matched.
    ///
    /// The exec_id is what makes a **workload-declared report
    /// workload-scoped**: a `report:` section belongs to the execution
    /// that declared it, so its data query must be narrowed to that
    /// execution's `exec_id` rather than spanning every execution that
    /// shares the session (a refine sequence, or SRD-88 concurrent
    /// executions). Tables already pass `Some(exec_id)` to their
    /// `ReportConfig`; this lets the plot path do the same.
    pub fn latest_execution_with_metadata_like(
        conn: &Connection,
        key_like: &str,
    ) -> (Option<i64>, Vec<(String, String)>) {
        let exec_id: Option<i64> = conn
            .query_row(
                "SELECT MAX(exec_id) FROM execution_metadata WHERE key LIKE ?1",
                [key_like],
                |r| r.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten();
        let rows: Vec<(String, String)> = conn
            .prepare(
                "SELECT key, value FROM execution_metadata \
                 WHERE key LIKE ?1 \
                   AND exec_id = (SELECT MAX(exec_id) FROM execution_metadata WHERE key LIKE ?1) \
                 ORDER BY key",
            )
            .and_then(|mut s| {
                s.query_map([key_like], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map(|it| it.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();
        if !rows.is_empty() {
            return (exec_id, rows);
        }
        let legacy: Vec<(String, String)> = conn
            .prepare("SELECT key, value FROM session_metadata WHERE key LIKE ?1 ORDER BY key")
            .and_then(|mut s| {
                s.query_map([key_like], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map(|it| it.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();
        (None, legacy)
    }

    /// Latest-execution single value for an exact `key`. Same
    /// fallback rule as [`latest_execution_metadata_like`].
    pub fn latest_execution_metadata_value(conn: &Connection, key: &str) -> Option<String> {
        conn.query_row(
            "SELECT value FROM execution_metadata \
             WHERE key = ?1 \
               AND exec_id = (SELECT MAX(exec_id) FROM execution_metadata WHERE key = ?1)",
            [key],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .or_else(|| {
            conn.query_row(
                "SELECT value FROM session_metadata WHERE key = ?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .ok()
        })
    }

    pub struct SqliteReporter {
        conn: Connection,
        /// `metric_family.name → metric_family.id`.
        family_cache: HashMap<String, i64>,
        /// Canonical spec (OpenMetrics sample identifier:
        /// `name{k="v",…}`) → `metric_instance.id`. Two label
        /// dicts that are equal as a mapping produce the same
        /// spec text → resolve to the same instance id, even
        /// when constructed in different orders by different
        /// code paths. This is the canonical identity for the
        /// post-cutover schema (SRD: hard cutover, denormalised
        /// — no more `label_set` indirection).
        instance_cache: HashMap<String, i64>,
        /// SRD-93 M2 — process-monotonic clock anchor captured at
        /// open: event stamps derive `utc = anchor_utc + elapsed`
        /// (no NTP steps mid-run) and
        /// `session = utc − session_epoch_utc_nanos`.
        anchor_instant: std::time::Instant,
        anchor_utc_nanos: i64,
        /// The session's durable epoch (session_metadata key
        /// `session_epoch_utc_nanos`; INSERT-only — resume/refine
        /// never move it).
        session_epoch_utc_nanos: i64,
        /// SRD-93 M4 — `instance_id → (session, exec_id, spec)`,
        /// populated beside `instance_cache` so flush-boundary exit
        /// events resolve join-free. Same lifetime as the cache: a
        /// cold reopen repopulates on the re-sight (miss) path.
        instance_meta: HashMap<i64, (String, i64, String)>,
        /// Instance ids the current `report()` batch touched — the
        /// exit-event target set when the batch carries a lifecycle
        /// close reason.
        batch_touched: std::collections::HashSet<i64>,
    }

    impl SqliteReporter {
        pub fn new(path: impl AsRef<Path>) -> Result<Self, String> {
            let conn = Connection::open(path).map_err(|e| format!("failed to open SQLite: {e}"))?;
            // WAL mode: readers don't block writers, no fsync on every commit.
            // synchronous=NORMAL: fsync only on WAL checkpoint, not every transaction.
            conn.execute_batch(
                // SRD-77 — `foreign_keys=ON` enables the schema's
                // FK constraints on `metric_instance(session, exec_id)`
                // → `executions(session, exec_id)` so every metric
                // sample is transitively tied to a real execution.
                // Off-by-default in sqlite; must be enabled per-
                // connection.
                "PRAGMA journal_mode=WAL;\
                 PRAGMA synchronous=NORMAL;\
                 PRAGMA wal_autocheckpoint=1000;\
                 PRAGMA foreign_keys=ON;",
            )
            .map_err(|e| format!("failed to set SQLite pragmas: {e}"))?;
            let mut reporter = Self::from_connection(conn)?;
            reporter.create_schema()?;
            reporter.session_epoch_utc_nanos =
                Self::resolve_session_epoch(&reporter.conn, reporter.anchor_utc_nanos);
            Ok(reporter)
        }

        /// Shared constructor tail: capture the SRD-93 M2 monotonic
        /// anchor pair at open. The epoch field is finalized by the
        /// caller after `create_schema` (session_metadata must exist).
        fn from_connection(conn: Connection) -> Result<Self, String> {
            let anchor_instant = std::time::Instant::now();
            let anchor_utc_nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            Ok(Self {
                conn,
                family_cache: HashMap::new(),
                instance_cache: HashMap::new(),
                anchor_instant,
                anchor_utc_nanos,
                session_epoch_utc_nanos: anchor_utc_nanos,
                instance_meta: HashMap::new(),
                batch_touched: std::collections::HashSet::new(),
            })
        }

        /// SRD-93 M2 — the session's durable epoch. Existing key
        /// wins (the epoch NEVER moves once set); a legacy db
        /// without the key derives it from the earliest recorded
        /// execution start; a fresh db anchors at open. Whatever is
        /// resolved is persisted INSERT-only.
        fn resolve_session_epoch(conn: &Connection, anchor_utc_nanos: i64) -> i64 {
            if let Ok(v) = conn.query_row(
                "SELECT CAST(value AS INTEGER) FROM session_metadata \
                 WHERE key = 'session_epoch_utc_nanos'",
                [],
                |r| r.get::<_, i64>(0),
            ) {
                return v;
            }
            let derived: Option<i64> = conn
                .query_row(
                    "SELECT MIN(started_at_nanos) FROM executions \
                 WHERE started_at_nanos > 0",
                    [],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let epoch = derived.unwrap_or(anchor_utc_nanos);
            let _ = conn.execute(
                "INSERT OR IGNORE INTO session_metadata (key, value) \
                 VALUES ('session_epoch_utc_nanos', ?1)",
                params![epoch.to_string()],
            );
            epoch
        }

        /// SRD-93 A5 — one event-time stamp pair `(utc_nanos,
        /// session_nanos)`, both derived from the same monotonic
        /// anchor so the two columns always agree.
        fn clock_now(&self) -> (i64, i64) {
            let utc = self.anchor_utc_nanos + self.anchor_instant.elapsed().as_nanos() as i64;
            (utc, utc - self.session_epoch_utc_nanos)
        }

        pub fn in_memory() -> Result<Self, String> {
            let conn = Connection::open_in_memory()
                .map_err(|e| format!("failed to open in-memory SQLite: {e}"))?;
            // SRD-77 — enable FK enforcement on the in-memory
            // connection too so the test-time path mirrors
            // production's constraint surface exactly. No
            // bootstrap row, no test-only arms: production opens
            // a reporter via `new()`, then `insert_execution_start`
            // before any metric write — tests follow the same
            // shape.
            conn.execute_batch("PRAGMA foreign_keys=ON;")
                .map_err(|e| format!("failed to set foreign_keys pragma: {e}"))?;
            let mut reporter = Self::from_connection(conn)?;
            reporter.create_schema()?;
            reporter.session_epoch_utc_nanos =
                Self::resolve_session_epoch(&reporter.conn, reporter.anchor_utc_nanos);
            Ok(reporter)
        }

        /// Wholesale-purge every sample row whose owning
        /// `metric_instance` carries the supplied label set as a
        /// **superset match** — i.e. every (key, value) pair in
        /// `labels` is present on the instance, regardless of
        /// extra labels the instance may also carry.
        ///
        /// Used by the checkpoint resume path (SRD-44 §"Wholesale
        /// metrics-purge"): a phase that re-runs from scratch on
        /// resume must invalidate the prior invocation's rows so
        /// downstream summaries don't double-count.
        ///
        /// Returns the number of `sample_value` rows deleted.
        /// Best-effort under SQL errors — logs and returns 0
        /// rather than propagating, since a purge failure
        /// shouldn't abort the run (it surfaces as a duplicate-
        /// counting metric, not silent corruption of state).
        pub fn purge_samples_with_labels(&mut self, labels: &Labels) -> usize {
            // For each (k, v) in labels, every kept instance
            // must own a matching `instance_label` row. Build
            // the AND-of-EXISTS query against the denormalised
            // schema — one EXISTS per pair, scanning the
            // (key, value, instance_id) index.
            let pairs: Vec<(String, String)> = labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            if pairs.is_empty() {
                return 0;
            }
            let exists_clauses: Vec<String> = (0..pairs.len())
                .map(|i| {
                    let kparam = i * 2 + 1;
                    let vparam = i * 2 + 2;
                    format!(
                        "EXISTS (SELECT 1 FROM instance_label e \
                     WHERE e.instance_id = mi.id \
                     AND e.key = ?{kparam} AND e.value = ?{vparam})",
                    )
                })
                .collect();
            let sql = format!(
                "DELETE FROM sample_value WHERE instance_id IN (\
                   SELECT mi.id FROM metric_instance mi WHERE {})",
                exists_clauses.join(" AND "),
            );
            let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(pairs.len() * 2);
            for (k, v) in &pairs {
                bound.push(k as &dyn rusqlite::ToSql);
                bound.push(v as &dyn rusqlite::ToSql);
            }
            match self
                .conn
                .execute(&sql, rusqlite::params_from_iter(bound.iter().copied()))
            {
                Ok(n) => n,
                Err(e) => {
                    crate::diag::warn(&format!(
                        "warning: sqlite purge_samples_with_labels failed: {e}"
                    ));
                    0
                }
            }
        }

        /// Store a session-INVARIANT metadata key-value pair. Use
        /// [`Self::set_execution_metadata`] for anything that varies
        /// per execution (the common case) so a `refine`'s newer
        /// execution doesn't clobber a prior execution's value.
        pub fn set_metadata(&mut self, key: &str, value: &str) {
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO session_metadata (key, value) VALUES (?1, ?2)",
                    params![key, value],
                )
                .unwrap_or_else(|e| {
                    crate::diag::warn(&format!("warning: sqlite metadata write: {e}"));
                    0
                });
        }

        /// Store a per-execution metadata key-value pair under
        /// `(session, exec_id)`. Each execution keeps its own value;
        /// readers select the execution they want (latest by default).
        pub fn set_execution_metadata(
            &mut self,
            session: &str,
            exec_id: u64,
            key: &str,
            value: &str,
        ) {
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO execution_metadata (session, exec_id, key, value) \
                 VALUES (?1, ?2, ?3, ?4)",
                    params![session, exec_id as i64, key, value],
                )
                .unwrap_or_else(|e| {
                    crate::diag::warn(&format!("warning: sqlite execution metadata write: {e}"));
                    0
                });
        }

        /// `PRAGMA user_version` levels — the single source of truth for how
        /// far a db has been initialised. Monotonic: a db only moves up. Bump
        /// the relevant constant when the layout changes so an existing db
        /// re-runs just the missing migration on its next open.
        ///
        /// - 1: v1 tables. A write-time open leaves the db here — indexes
        ///   are DEFERRED off the hot write path (per-row B-tree
        ///   maintenance is what amplifies WAL volume).
        /// - 2: v1 tables + all v1 read indexes.
        /// - [`Self::SCHEMA_VERSION`] (3): v2 tables — adds the SRD-93
        ///   `instance_scope_event` lifecycle table + the session-time
        ///   views. A v1/v2 db converges on its next read-write open
        ///   (all DDL is IF NOT EXISTS).
        /// - [`Self::INDEXED_VERSION`] (4): v2 tables + read indexes,
        ///   built by [`Self::ensure_read_indexes`] at shutdown / by a
        ///   read-write maintenance opener.
        const SCHEMA_VERSION: i64 = 3;
        const INDEXED_VERSION: i64 = 4;

        /// SRD-107 — add `phase_outcomes.params_consumed` to dbs
        /// created before the column existed. Probed (not blindly
        /// ALTERed) because SQLite has no ADD COLUMN IF NOT
        /// EXISTS; skipped entirely on brand-new dbs where the
        /// CREATE below carries the column.
        fn ensure_phase_outcomes_params_column(conn: &Connection) -> Result<(), String> {
            let table_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='table' AND name='phase_outcomes'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n > 0)
                .map_err(|e| format!("phase_outcomes probe: {e}"))?;
            if !table_exists {
                return Ok(());
            }
            let mut stmt = conn
                .prepare("PRAGMA table_info(phase_outcomes)")
                .map_err(|e| format!("phase_outcomes pragma: {e}"))?;
            let mut rows = stmt
                .query([])
                .map_err(|e| format!("phase_outcomes pragma query: {e}"))?;
            while let Ok(Some(r)) = rows.next() {
                if r.get::<_, String>(1)
                    .map(|n| n == "params_consumed")
                    .unwrap_or(false)
                {
                    return Ok(());
                }
            }
            conn.execute(
                "ALTER TABLE phase_outcomes ADD COLUMN params_consumed TEXT",
                [],
            )
            .map_err(|e| format!("phase_outcomes params migration: {e}"))?;
            Ok(())
        }

        /// All read-path indexes, `IF NOT EXISTS` so a partially-indexed db
        /// converges. Built by [`Self::ensure_read_indexes`] — never on the
        /// hot write path. Add an index here AND bump [`Self::INDEXED_VERSION`].
        const READ_INDEX_DDL: &'static str = "\
            CREATE INDEX IF NOT EXISTS idx_instance_label_kv \
                ON instance_label(key, value, instance_id);\
            CREATE INDEX IF NOT EXISTS idx_instance_label_instance \
                ON instance_label(instance_id);\
            CREATE INDEX IF NOT EXISTS idx_sample_value_inst_ts \
                ON sample_value(instance_id, timestamp_ms);\
            CREATE INDEX IF NOT EXISTS idx_metric_instance_family \
                ON metric_instance(family_id);\
            CREATE INDEX IF NOT EXISTS idx_exemplar_inst_ts \
                ON exemplar(instance_id, sample_timestamp_ms);\
            CREATE INDEX IF NOT EXISTS idx_phase_errors_phase \
                ON phase_errors(session, exec_id, phase_name, phase_labels);\
            CREATE INDEX IF NOT EXISTS idx_phase_outcomes_ended \
                ON phase_outcomes(ended_at_nanos);";

        /// Build the read-path indexes iff the db isn't already at
        /// [`Self::INDEXED_VERSION`], then stamp it. Idempotent and
        /// self-healing: a db left at [`Self::SCHEMA_VERSION`] (deferred
        /// indexes, or a crash before shutdown) is completed the next time a
        /// read-write opener calls this, and bumping `INDEXED_VERSION` for a
        /// new index migrates old dbs on their next such open. Called at
        /// shutdown ([`Self::consolidate_wal`]) so the durable db is fully
        /// indexed for external (non-runtime) readers. Takes a bare
        /// `&Connection` (not `&self`) so any read-write opener can complete
        /// the indexing.
        pub(crate) fn ensure_read_indexes(conn: &Connection) -> Result<(), String> {
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .map_err(|e| format!("failed to read index version: {e}"))?;
            if version >= Self::INDEXED_VERSION {
                return Ok(());
            }
            conn.execute_batch(Self::READ_INDEX_DDL)
                .map_err(|e| format!("index creation failed: {e}"))?;
            // Stamp what is now TRUE of this db: indexed-v2 (4) only
            // if the v2 tables are present; a caller that indexed a
            // v1-table db records indexed-v1 (2) so the next
            // read-write open still runs the v2 table DDL.
            let stamp = if version >= Self::SCHEMA_VERSION {
                Self::INDEXED_VERSION
            } else {
                2
            };
            conn.execute_batch(&format!("PRAGMA user_version = {stamp};"))
                .map_err(|e| format!("failed to stamp index version: {e}"))?;
            Ok(())
        }

        /// Create the TABLES once per database — no indexes; those are deferred
        /// to [`Self::ensure_indexes`]. A db already at [`Self::SCHEMA_VERSION`]
        /// or beyond is assumed to have its tables, so the DDL — notably the
        /// SRD-44 resume/refine reopen that appends in place — is skipped as
        /// wasted parse + catalog work.
        fn create_schema(&mut self) -> Result<(), String> {
            // SRD-107 — the `params_consumed` column migration runs
            // on EVERY read-write open, BEFORE the version
            // early-return: version stamps predate the column, so
            // gating on them would skip upgrading dbs already at the
            // current version. One PRAGMA probe (and at most one
            // ALTER, once per db) per open.
            Self::ensure_phase_outcomes_params_column(&self.conn)?;
            let version: i64 = self
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .map_err(|e| format!("failed to read schema version: {e}"))?;
            if version >= Self::SCHEMA_VERSION {
                return Ok(());
            }
            self.conn
                .execute_batch(
                    // SRD-77 — `executions` MUST be defined before any
                    // table that FK-references it; the FK on
                    // metric_instance(session, exec_id) below targets
                    // this table. One row per `nmbrs run` / `nmbrs
                    // refine` / `nmbrs resume` invocation that reached
                    // the runner. `verb` records the launching CLI
                    // verb; `scope` records the `--scope=` setting for
                    // refine (NULL elsewhere). `workload_yaml_snapshot`
                    // stores the workload yaml verbatim so an operator
                    // can reconstruct what THIS execution ran without
                    // needing the workload file to still exist on disk.
                    "CREATE TABLE IF NOT EXISTS executions (
                    session                 TEXT    NOT NULL,
                    exec_id                 INTEGER NOT NULL,
                    verb                    TEXT    NOT NULL,
                    scope                   TEXT,
                    started_at_nanos        INTEGER NOT NULL,
                    ended_at_nanos          INTEGER,
                    disposition             TEXT,
                    workload_yaml_snapshot  TEXT NOT NULL DEFAULT '',
                    cli_params_snapshot     TEXT NOT NULL DEFAULT '',
                    PRIMARY KEY (session, exec_id)
                );
                CREATE TABLE IF NOT EXISTS metric_family (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    type TEXT NOT NULL,
                    unit TEXT,
                    help TEXT,
                    UNIQUE(name, type)
                );
                -- Denormalised metric_instance: identity IS
                -- the OpenMetrics-canonical sample spec
                -- (`name{k=\"v\",…}`, sorted by key, OpenMetrics
                -- escape rules). No `label_set` indirection,
                -- no `hash` — two label dicts equal as a
                -- mapping produce equal `spec` text and
                -- therefore the same row. See
                -- [`Labels::to_canonical_spec`].
                CREATE TABLE IF NOT EXISTS metric_instance (
                    id INTEGER PRIMARY KEY,
                    family_id INTEGER NOT NULL REFERENCES metric_family(id),
                    spec TEXT NOT NULL UNIQUE,
                    -- SRD-77 — every metric instance MUST be
                    -- tied to a concrete session + execution.
                    -- These columns denormalize the values that
                    -- already appear as `session` / `exec_id`
                    -- labels in the spec text; promoting them to
                    -- first-class columns lets the FK below
                    -- ENFORCE the relationship structurally,
                    -- instead of trusting writers to keep the
                    -- spec well-formed. sample_value transits
                    -- to executions through this column pair via
                    -- instance_id (already FK'd to
                    -- metric_instance). Pre-SRD-77 rows get the
                    -- sentinel session=''/exec_id=0 — the
                    -- migration `ALTER TABLE` adds the columns
                    -- with that default, and the FK is added
                    -- only when no legacy rows remain.
                    session TEXT NOT NULL DEFAULT '',
                    exec_id INTEGER NOT NULL DEFAULT 0,
                    FOREIGN KEY (session, exec_id)
                        REFERENCES executions(session, exec_id)
                        DEFERRABLE INITIALLY DEFERRED
                );
                -- Per-instance label rows. Holds `__name__`
                -- alongside every other label so queries that
                -- filter `WHERE key='__name__'` work the same
                -- way as queries against any other dimension —
                -- the metric family name surfaces uniformly
                -- through the label-filter machinery.
                CREATE TABLE IF NOT EXISTS instance_label (
                    instance_id INTEGER NOT NULL REFERENCES metric_instance(id),
                    key TEXT NOT NULL,
                    value TEXT NOT NULL,
                    PRIMARY KEY (instance_id, key)
                );
                CREATE TABLE IF NOT EXISTS sample_value (
                    instance_id INTEGER NOT NULL REFERENCES metric_instance(id),
                    timestamp_ms INTEGER NOT NULL,
                    interval_ms INTEGER NOT NULL,
                    count INTEGER,
                    sum REAL,
                    min REAL,
                    max REAL,
                    mean REAL,
                    stddev REAL,
                    p50 REAL, p75 REAL, p90 REAL, p95 REAL,
                    p98 REAL, p99 REAL, p999 REAL,
                    -- OpenMetrics §5.1 / §5.3 / §5.5: counters,
                    -- histograms, summaries MAY carry a Created
                    -- timestamp marking the series-start instant.
                    -- Counter resets bump it. Stored per-sample
                    -- (NULL when the producer didn't supply one)
                    -- so the reader can surface the standard
                    -- `<name>_created` virtual series.
                    created_ms INTEGER
                );
                -- Add the column to existing databases that
                -- predate it. SQLite ignores the ALTER if the
                -- column is already present (within the same
                -- run), and the IF NOT EXISTS dance below uses a
                -- pragma_table_info probe to skip the ALTER on
                -- DBs that already have it.
                -- (PRAGMA-based migration runs once per open in
                -- create_schema so existing dbs upgrade
                -- on-the-fly without manual SQL.)
                -- OpenMetrics §4.6.1 exemplars. Linked to a
                -- specific sample observation by
                -- (instance_id, sample_timestamp_ms). The
                -- pair-not-FK link reflects the schema
                -- reality — sample_value has no synthetic id;
                -- writers MUST insert the sample row first
                -- and then exemplar rows pointing at the
                -- same (instance_id, timestamp_ms) tuple.
                --
                -- One row per exemplar; OpenMetrics 1.0
                -- counter+histogram-bucket allow ≤ 1 per
                -- sample, OpenMetrics 2.0 allows arbitrary
                -- counts. The schema is forward-compatible
                -- with both.
                --
                -- `labels_spec` is the same denormalized
                -- shape as `metric_instance.spec` —
                -- `key=\"value\",key=\"value\"` — for cheap
                -- spec reconstruction without joining.
                CREATE TABLE IF NOT EXISTS exemplar (
                    id INTEGER PRIMARY KEY,
                    instance_id INTEGER NOT NULL REFERENCES metric_instance(id),
                    sample_timestamp_ms INTEGER NOT NULL,
                    value REAL NOT NULL,
                    -- Optional exemplar timestamp per spec
                    -- (distinct from the sample timestamp).
                    timestamp_ms INTEGER,
                    -- Denormalized exemplar labels in
                    -- spec-textual form (key=value pairs).
                    -- §4.7: the serialized LabelSet MUST be
                    -- <= 128 UTF-8 chars; validation lives
                    -- at exposition time, the recording
                    -- path here is permissive.
                    labels_spec TEXT NOT NULL
                );
                -- Session-INVARIANT metadata only (the session id).
                -- Anything that varies per execution (start/end time,
                -- workload/scenario, params, report & summary defs)
                -- lives in `execution_metadata`, keyed by exec_id —
                -- a flat `(key)` PK here would let a `refine`'s newer
                -- execution silently clobber a prior execution's value.
                CREATE TABLE IF NOT EXISTS session_metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT
                );
                -- SRD-77 — per-execution metadata. Keyed by
                -- (session, exec_id, key) so each execution in a
                -- refined session keeps its own values; readers pick
                -- the execution they want (latest by default). No FK
                -- to `executions`: metadata is written during run
                -- setup, before the `executions` row exists, and it's
                -- logically scoped to the current execution by
                -- construction.
                CREATE TABLE IF NOT EXISTS execution_metadata (
                    session TEXT NOT NULL,
                    exec_id INTEGER NOT NULL,
                    key     TEXT NOT NULL,
                    value   TEXT,
                    PRIMARY KEY (session, exec_id, key)
                );
                -- Read-path indexes are DEFERRED — see `ensure_indexes` /
                -- `READ_INDEX_DDL`. Building them here would make every insert
                -- maintain 6 extra B-trees (the WAL-volume amplifier); instead
                -- they are built on first read and guaranteed at shutdown.
                -- SRD-63 §6.4: per-(slot, subject, readout, lod)
                -- snapshot of the latest render for that tuple.
                -- Insert-or-replace upsert keeps memory bounded.
                -- The body is stored both with ANSI escapes
                -- (so live tooling can reproduce styling) and
                -- as a stripped fallback for grep / structured
                -- consumers.
                CREATE TABLE IF NOT EXISTS readout_snapshots (
                    slot TEXT NOT NULL,
                    exec_id INTEGER NOT NULL,
                    subject_kind TEXT NOT NULL,
                    subject_id TEXT NOT NULL,
                    readout_name TEXT NOT NULL,
                    lod TEXT NOT NULL,
                    rendered_at INTEGER NOT NULL,
                    body_ansi BLOB,
                    body_plain TEXT NOT NULL,
                    PRIMARY KEY (slot, exec_id, subject_kind, subject_id, readout_name, lod)
                );
                -- SRD-76: per-phase terminal outcome. The
                -- identity is (session, exec_id, phase_name,
                -- phase_labels): a sweep cell runs once per
                -- (session, exec_id) pair under its
                -- (name, labels), so re-running the same phase
                -- under a later `refine` execution (SRD-77)
                -- lands in a distinct row. Insert-or-replace
                -- keeps the row idempotent under rare re-install
                -- scenarios (late error promotion, resume-on-
                -- restart). `session` + `exec_id` ride on the
                -- root component as dimensional labels (see
                -- nmbrs-runtime::session::Session::new_with_args)
                -- so every per-component metric carries them
                -- alongside the existing phase / workload axes.
                --
                -- `status` is the short label form
                -- (`completed` / `failed` / `skipped` /
                -- `cursor_suspended`) the in-memory enum's
                -- `label()` method emits — stable so an
                -- `errors:` policy or a CI grep can match on it.
                CREATE TABLE IF NOT EXISTS phase_outcomes (
                    session       TEXT    NOT NULL,
                    exec_id       INTEGER NOT NULL,
                    phase_name    TEXT    NOT NULL,
                    phase_labels  TEXT    NOT NULL,
                    status        TEXT    NOT NULL,
                    duration_secs REAL    NOT NULL,
                    started_at_nanos INTEGER NOT NULL,
                    ended_at_nanos   INTEGER NOT NULL,
                    -- SRD-77 — GK chain-hash (hex). NULL for
                    -- legacy rows from before this column
                    -- was added; populated for every row
                    -- written by SRD-77-aware code paths so
                    -- `refine --scope=changed` can compare
                    -- prior vs current program shape.
                    phase_hash    TEXT,
                    -- SRD-83 (C3) — reason class for short-ended
                    -- phases (timeout / stop_condition / error /
                    -- panic / operator). NULL for succeeded phases
                    -- and legacy rows.
                    reason_class  TEXT,
                    -- SRD-107 — consumed-params map as canonical
                    -- JSON (name -> sha256 hex of the value);
                    -- the per-param leg of skip validity. NULL
                    -- for legacy rows and skipped phases.
                    params_consumed TEXT,
                    PRIMARY KEY (session, exec_id, phase_name, phase_labels)
                );
                -- SRD-76: per-error detail rows, 0..N per phase
                -- within a (session, exec_id) pair. The pair-not-
                -- FK link to `phase_outcomes` matches the
                -- `exemplar`/`sample_value` shape elsewhere in
                -- this schema. `seq` is the chronological
                -- position within the phase so replay can sort
                -- the errors in the order they were recorded
                -- without an extra timestamp sort.
                CREATE TABLE IF NOT EXISTS phase_errors (
                    session      TEXT    NOT NULL,
                    exec_id      INTEGER NOT NULL,
                    phase_name   TEXT    NOT NULL,
                    phase_labels TEXT    NOT NULL,
                    seq          INTEGER NOT NULL,
                    class        TEXT    NOT NULL,
                    message      TEXT    NOT NULL,
                    op_name      TEXT,
                    cycle        INTEGER,
                    op_template  TEXT,
                    op_resolved  TEXT,
                    at_nanos     INTEGER NOT NULL,
                    retryable    INTEGER NOT NULL,
                    PRIMARY KEY (session, exec_id, phase_name, phase_labels, seq)
                );
                -- SRD-93 M1 — instance scope lifecycle: exactly one
                -- enter and at most one exit row per (instance,
                -- execution). Append-only, O(instance-lifetime)
                -- writes at the flush boundary — never O(pulse).
                -- `instance_id` piggybacks the existing
                -- normalization (no label_set indirection in this
                -- schema); `spec` is retained denormalized exactly
                -- as exemplar.labels_spec so the event feed is
                -- self-describing without joins. Dual temporal
                -- columns per SRD-93 A5: UTC epoch nanos + nanos of
                -- session time (both stamped at event time from the
                -- writer's monotonic anchor). enter reason:
                -- 'first_sample'; exit reasons: 'scope_close' |
                -- 'shutdown'. An enter with no exit after session
                -- end is a truthful crash/interrupt marker (A7).
                CREATE TABLE IF NOT EXISTS instance_scope_event (
                    instance_id      INTEGER NOT NULL REFERENCES metric_instance(id),
                    session          TEXT    NOT NULL,
                    exec_id          INTEGER NOT NULL,
                    event            TEXT    NOT NULL CHECK (event IN ('enter','exit')),
                    reason           TEXT    NOT NULL,
                    at_utc_nanos     INTEGER NOT NULL,
                    at_session_nanos INTEGER NOT NULL,
                    spec             TEXT    NOT NULL,
                    PRIMARY KEY (instance_id, exec_id, event)
                ) WITHOUT ROWID;
                -- SRD-93 M2 — session time over the legacy tables is
                -- a pure derivation from the durable epoch; views
                -- instead of columns so the hot tables don't change.
                CREATE VIEW IF NOT EXISTS v_executions_session AS
                    SELECT e.*,
                           e.started_at_nanos - (SELECT CAST(value AS INTEGER)
                               FROM session_metadata
                               WHERE key = 'session_epoch_utc_nanos')
                               AS started_at_session_nanos,
                           e.ended_at_nanos - (SELECT CAST(value AS INTEGER)
                               FROM session_metadata
                               WHERE key = 'session_epoch_utc_nanos')
                               AS ended_at_session_nanos
                    FROM executions e;
                CREATE VIEW IF NOT EXISTS v_phase_outcomes_session AS
                    SELECT p.*,
                           p.started_at_nanos - (SELECT CAST(value AS INTEGER)
                               FROM session_metadata
                               WHERE key = 'session_epoch_utc_nanos')
                               AS started_at_session_nanos,
                           p.ended_at_nanos - (SELECT CAST(value AS INTEGER)
                               FROM session_metadata
                               WHERE key = 'session_epoch_utc_nanos')
                               AS ended_at_session_nanos
                    FROM phase_outcomes p;",
                )
                .map_err(|e| format!("schema creation failed: {e}"))?;
            // Tables done — stamp SCHEMA_VERSION so a reopen skips the table
            // DDL. Indexes are deferred (a higher version) to `ensure_indexes`.
            self.conn
                .execute_batch(&format!("PRAGMA user_version = {};", Self::SCHEMA_VERSION))
                .map_err(|e| format!("failed to stamp schema version: {e}"))?;
            Ok(())
        }

        /// SRD-76 — persist a phase's terminal outcome. The
        /// write is a single transaction: the `phase_outcomes`
        /// row is upserted (most-recent install wins), and the
        /// `phase_errors` rows for that phase identity are
        /// replaced wholesale (delete-then-insert) so the on-
        /// disk error list always matches the in-memory
        /// `PhaseOutcome.errors`.
        ///
        /// Best-effort: a sqlite failure logs at Warn and does
        /// not propagate. The scene tree's in-memory outcome
        /// remains the canonical state; the database surface
        /// degrades gracefully when persistence is partial.
        pub fn write_phase_outcome(&mut self, row: &PhaseOutcomeRow) {
            let tx = match self.conn.transaction() {
                Ok(t) => t,
                Err(e) => {
                    crate::diag::warn(&format!("sqlite phase_outcome tx open failed: {e}"));
                    return;
                }
            };
            let res = (|| -> rusqlite::Result<()> {
                tx.execute(
                    "INSERT OR REPLACE INTO phase_outcomes \
                     (session, exec_id, phase_name, phase_labels, status, \
                      duration_secs, started_at_nanos, ended_at_nanos, \
                      phase_hash, reason_class, params_consumed) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        row.session,
                        row.exec_id as i64,
                        row.phase_name,
                        row.phase_labels,
                        row.status,
                        row.duration_secs,
                        row.started_at_nanos,
                        row.ended_at_nanos,
                        row.phase_hash,
                        row.reason_class,
                        row.params_consumed,
                    ],
                )?;
                tx.execute(
                    "DELETE FROM phase_errors \
                     WHERE session = ?1 AND exec_id = ?2 \
                       AND phase_name = ?3 AND phase_labels = ?4",
                    params![
                        row.session,
                        row.exec_id as i64,
                        row.phase_name,
                        row.phase_labels,
                    ],
                )?;
                for (seq, e) in row.errors.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO phase_errors \
                         (session, exec_id, phase_name, phase_labels, seq, \
                          class, message, op_name, cycle, op_template, \
                          op_resolved, at_nanos, retryable) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                                 ?11, ?12, ?13)",
                        params![
                            row.session,
                            row.exec_id as i64,
                            row.phase_name,
                            row.phase_labels,
                            seq as i64,
                            e.class,
                            e.message,
                            e.op_name,
                            e.cycle.map(|c| c as i64),
                            e.op_template,
                            e.op_resolved,
                            e.at_nanos,
                            e.retryable as i64,
                        ],
                    )?;
                }
                Ok(())
            })();
            match res {
                Ok(()) => {
                    if let Err(e) = tx.commit() {
                        crate::diag::warn(&format!("sqlite phase_outcome commit failed: {e}"));
                    }
                }
                Err(e) => {
                    crate::diag::warn(&format!("sqlite phase_outcome write failed: {e}"));
                }
            }
        }

        /// SRD-77 — record the in-flight execution row at session
        /// open. `ended_at_nanos` / `disposition` stay `NULL`
        /// until the matching [`Self::update_execution_end`]
        /// call at shutdown.
        ///
        /// This is an UPSERT that **completes a placeholder** without
        /// clobbering a real row. A metric write may have already
        /// created a minimal FK-parent placeholder for this
        /// `(session, exec_id)` — `(verb='pending', started_at_nanos=0)`
        /// — because the deferred `metric_instance → executions` FK
        /// must be satisfiable at the snapshot COMMIT even when a metric
        /// races ahead of this call (see the guard in
        /// [`Self::upsert_instance`]). When such a placeholder is
        /// present the real verb/started_at/snapshots overwrite it;
        /// when a **completed** row already exists (a genuine duplicate
        /// `exec_id`, e.g. a resume that reuses an id) the
        /// `WHERE …='pending'` guard makes the update abstain, the prior
        /// row is preserved verbatim, and a WARN is emitted — keeping
        /// the SRD-77 "this exec_id is already taken" signal.
        // Args map 1:1 to the `executions` table columns written in
        // one statement; bundling them into a struct would only
        // shuffle the same fields across a call boundary.
        #[allow(clippy::too_many_arguments)]
        pub fn insert_execution_start(
            &mut self,
            session: &str,
            exec_id: u64,
            verb: &str,
            scope: Option<&str>,
            started_at_nanos: i64,
            workload_yaml_snapshot: &str,
            cli_params_snapshot: &str,
        ) {
            let res = self.conn.execute(
                "INSERT INTO executions \
                 (session, exec_id, verb, scope, started_at_nanos, \
                  workload_yaml_snapshot, cli_params_snapshot) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(session, exec_id) DO UPDATE SET \
                     verb                   = excluded.verb, \
                     scope                  = excluded.scope, \
                     started_at_nanos       = excluded.started_at_nanos, \
                     workload_yaml_snapshot = excluded.workload_yaml_snapshot, \
                     cli_params_snapshot    = excluded.cli_params_snapshot \
                 WHERE executions.verb = 'pending'",
                params![
                    session,
                    exec_id as i64,
                    verb,
                    scope,
                    started_at_nanos,
                    workload_yaml_snapshot,
                    cli_params_snapshot,
                ],
            );
            match res {
                // Zero rows changed = the conflict hit a completed row
                // whose `WHERE …='pending'` guard abstained: a genuine
                // duplicate exec_id. Surface it (matching the prior
                // strict-INSERT behaviour) without overwriting history.
                Ok(0) => crate::diag::warn(&format!(
                    "sqlite executions insert skipped: (session={session}, \
                     exec_id={exec_id}) already records a completed execution"
                )),
                Ok(_) => {}
                Err(e) => crate::diag::warn(&format!(
                    "sqlite executions insert failed (session={session}, \
                     exec_id={exec_id}): {e}"
                )),
            }
        }

        /// SRD-77 — update the in-flight execution row at
        /// shutdown. Sets `ended_at_nanos` + `disposition` so
        /// the cardinal-history table records the run's terminal
        /// state. Idempotent: the WHERE clause keys on the
        /// in-flight row (ended_at_nanos IS NULL) so a second
        /// call after the row has already been closed is a no-op.
        pub fn update_execution_end(
            &mut self,
            session: &str,
            exec_id: u64,
            ended_at_nanos: i64,
            disposition: &str,
        ) {
            let res = self.conn.execute(
                "UPDATE executions \
                 SET ended_at_nanos = ?1, disposition = ?2 \
                 WHERE session = ?3 AND exec_id = ?4 \
                   AND ended_at_nanos IS NULL",
                params![ended_at_nanos, disposition, session, exec_id as i64,],
            );
            if let Err(e) = res {
                crate::diag::warn(&format!(
                    "sqlite executions update failed (session={session}, \
                     exec_id={exec_id}): {e}"
                ));
            }
        }

        /// SRD-77 — read execution rows scoped by the supplied
        /// `exec_id_filter`. **Every** caller MUST decide:
        /// `Some(n)` narrows to one execution; `None` reads
        /// every execution (the explicit "aggregate" semantic).
        /// Higher layers translate `ExecutionQualifier` (in
        /// nmbrs-runtime) into this primitive — the storage
        /// layer doesn't depend on the activity crate. Ordered
        /// by `exec_id` so callers see them in cardinal
        /// sequence.
        pub fn read_executions(&self, exec_id_filter: Option<u64>) -> Vec<ExecutionRow> {
            let (sql, params): (&str, Vec<i64>) = match exec_id_filter {
                Some(id) => (
                    "SELECT session, exec_id, verb, scope, \
                            started_at_nanos, ended_at_nanos, disposition, \
                            workload_yaml_snapshot, cli_params_snapshot \
                     FROM executions WHERE exec_id = ?1 \
                     ORDER BY session, exec_id",
                    vec![id as i64],
                ),
                None => (
                    "SELECT session, exec_id, verb, scope, \
                            started_at_nanos, ended_at_nanos, disposition, \
                            workload_yaml_snapshot, cli_params_snapshot \
                     FROM executions \
                     ORDER BY session, exec_id",
                    Vec::new(),
                ),
            };
            let mut stmt = match self.conn.prepare(sql) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
                Ok(ExecutionRow {
                    session: r.get(0)?,
                    exec_id: r.get::<_, i64>(1)? as u64,
                    verb: r.get(2)?,
                    scope: r.get(3)?,
                    started_at_nanos: r.get(4)?,
                    ended_at_nanos: r.get(5)?,
                    disposition: r.get(6)?,
                    workload_yaml_snapshot: r.get(7)?,
                    cli_params_snapshot: r.get(8)?,
                })
            });
            rows.map(|iter| iter.filter_map(Result::ok).collect())
                .unwrap_or_default()
        }

        /// SRD-76 / SRD-77 — read persisted phase outcomes,
        /// scoped by `exec_id_filter`. `Some(n)` narrows to one
        /// execution; `None` reads across every execution (the
        /// explicit aggregate intent). Higher layers translate
        /// `ExecutionQualifier` (nmbrs-runtime) into this
        /// primitive. Ordered by chronological phase-end time.
        /// Each outcome carries its full error list.
        /// SRD-83 (C3) legacy-read guard: `reason_class` was added
        /// 2026-08; dbs written earlier lack the column. Detected via
        /// PRAGMA (works on read-only opens); an absent column reads
        /// as NULL — the SRD-82 legacy-read discipline, no migration
        /// write required.
        fn phase_outcomes_has_reason_class(&self) -> bool {
            self.phase_outcomes_has_column("reason_class")
        }

        /// SRD-107 shares SRD-83's legacy-read discipline: an
        /// absent column reads as NULL via a probed column-or-NULL
        /// SELECT; no migration write on read-only opens.
        fn phase_outcomes_has_column(&self, name: &str) -> bool {
            let Ok(mut stmt) = self.conn.prepare("PRAGMA table_info(phase_outcomes)") else {
                return false;
            };
            let Ok(mut rows) = stmt.query([]) else {
                return false;
            };
            while let Ok(Some(r)) = rows.next() {
                if r.get::<_, String>(1).map(|n| n == name).unwrap_or(false) {
                    return true;
                }
            }
            false
        }

        pub fn read_phase_outcomes(&self, exec_id_filter: Option<u64>) -> Vec<PhaseOutcomeRow> {
            let mut outcomes: Vec<PhaseOutcomeRow> = Vec::new();
            let rc_col = if self.phase_outcomes_has_reason_class() {
                "reason_class"
            } else {
                "NULL"
            };
            let pc_col = if self.phase_outcomes_has_column("params_consumed") {
                "params_consumed"
            } else {
                "NULL"
            };
            let (sql, params): (String, Vec<i64>) = match exec_id_filter {
                Some(id) => (
                    format!(
                        "SELECT session, exec_id, phase_name, phase_labels, \
                            status, duration_secs, \
                            started_at_nanos, ended_at_nanos, phase_hash, \
                            {rc_col}, {pc_col} \
                     FROM phase_outcomes WHERE exec_id = ?1 \
                     ORDER BY ended_at_nanos, session, exec_id, \
                              phase_name, phase_labels"
                    ),
                    vec![id as i64],
                ),
                None => (
                    format!(
                        "SELECT session, exec_id, phase_name, phase_labels, \
                            status, duration_secs, \
                            started_at_nanos, ended_at_nanos, phase_hash, \
                            {rc_col}, {pc_col} \
                     FROM phase_outcomes \
                     ORDER BY ended_at_nanos, session, exec_id, \
                              phase_name, phase_labels"
                    ),
                    Vec::new(),
                ),
            };
            {
                let mut out_stmt = match self.conn.prepare(&sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                let rows = out_stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
                    Ok(PhaseOutcomeRow {
                        session: row.get(0)?,
                        exec_id: row.get::<_, i64>(1)? as u64,
                        phase_name: row.get(2)?,
                        phase_labels: row.get(3)?,
                        status: row.get(4)?,
                        duration_secs: row.get(5)?,
                        started_at_nanos: row.get(6)?,
                        ended_at_nanos: row.get(7)?,
                        phase_hash: row.get::<_, Option<String>>(8)?,
                        reason_class: row.get::<_, Option<String>>(9)?,
                        params_consumed: row.get::<_, Option<String>>(10)?,
                        errors: Vec::new(),
                    })
                });
                if let Ok(iter) = rows {
                    outcomes.extend(iter.filter_map(Result::ok));
                }
            }
            for outcome in outcomes.iter_mut() {
                outcome.errors = self.load_phase_errors(
                    &outcome.session,
                    outcome.exec_id,
                    &outcome.phase_name,
                    &outcome.phase_labels,
                );
            }
            outcomes
        }

        /// SRD-76 — read a single phase identity's outcome
        /// scoped to a (session, exec_id) pair. `None` when no
        /// persistence row exists for that identity. The error
        /// list is populated when the outcome row is present.
        pub fn read_phase_outcome(
            &self,
            session: &str,
            exec_id: u64,
            phase_name: &str,
            phase_labels: &str,
        ) -> Option<PhaseOutcomeRow> {
            let rc_col = if self.phase_outcomes_has_reason_class() {
                "reason_class"
            } else {
                "NULL"
            };
            let pc_col = if self.phase_outcomes_has_column("params_consumed") {
                "params_consumed"
            } else {
                "NULL"
            };
            let mut row = self
                .conn
                .query_row(
                    &format!(
                        "SELECT status, duration_secs, started_at_nanos, ended_at_nanos, \
                        phase_hash, {rc_col}, {pc_col} \
                 FROM phase_outcomes \
                 WHERE session = ?1 AND exec_id = ?2 \
                   AND phase_name = ?3 AND phase_labels = ?4"
                    ),
                    params![session, exec_id as i64, phase_name, phase_labels],
                    |r| {
                        Ok(PhaseOutcomeRow {
                            session: session.to_string(),
                            exec_id,
                            phase_name: phase_name.to_string(),
                            phase_labels: phase_labels.to_string(),
                            status: r.get(0)?,
                            duration_secs: r.get(1)?,
                            started_at_nanos: r.get(2)?,
                            ended_at_nanos: r.get(3)?,
                            phase_hash: r.get::<_, Option<String>>(4)?,
                            reason_class: r.get::<_, Option<String>>(5)?,
                            params_consumed: r.get::<_, Option<String>>(6)?,
                            errors: Vec::new(),
                        })
                    },
                )
                .ok()?;
            row.errors = self.load_phase_errors(session, exec_id, phase_name, phase_labels);
            Some(row)
        }

        fn load_phase_errors(
            &self,
            session: &str,
            exec_id: u64,
            phase_name: &str,
            phase_labels: &str,
        ) -> Vec<PhaseErrorRow> {
            let mut stmt = match self.conn.prepare(
                "SELECT class, message, op_name, cycle, op_template, \
                        op_resolved, at_nanos, retryable \
                 FROM phase_errors \
                 WHERE session = ?1 AND exec_id = ?2 \
                   AND phase_name = ?3 AND phase_labels = ?4 \
                 ORDER BY seq",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = stmt.query_map(
                params![session, exec_id as i64, phase_name, phase_labels],
                |r| {
                    Ok(PhaseErrorRow {
                        class: r.get(0)?,
                        message: r.get(1)?,
                        op_name: r.get(2)?,
                        cycle: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                        op_template: r.get(4)?,
                        op_resolved: r.get(5)?,
                        at_nanos: r.get(6)?,
                        retryable: r.get::<_, i64>(7)? != 0,
                    })
                },
            );
            match rows {
                Ok(iter) => iter.filter_map(Result::ok).collect(),
                Err(_) => Vec::new(),
            }
        }

        /// Upsert a readout snapshot. Latest render per
        /// `(slot, subject_kind, subject_id, readout_name, lod)`
        /// wins; the table holds at most one row per tuple.
        /// Errors are logged but not propagated — snapshot
        /// retention is a best-effort surface that must never
        /// block the run.
        // Args map 1:1 to the `readout_snapshots` table columns
        // written in one statement; a wrapper struct would only
        // relocate the same fields across the call boundary.
        #[allow(clippy::too_many_arguments)]
        pub fn upsert_readout_snapshot(
            &mut self,
            slot: &str,
            exec_id: u64,
            subject_kind: &str,
            subject_id: &str,
            readout_name: &str,
            lod: &str,
            rendered_at_nanos: i64,
            body_ansi: Option<&[u8]>,
            body_plain: &str,
        ) {
            let r = self.conn.execute(
                "INSERT OR REPLACE INTO readout_snapshots \
                 (slot, exec_id, subject_kind, subject_id, readout_name, lod, \
                  rendered_at, body_ansi, body_plain) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    slot,
                    exec_id as i64,
                    subject_kind,
                    subject_id,
                    readout_name,
                    lod,
                    rendered_at_nanos,
                    body_ansi,
                    body_plain,
                ],
            );
            if let Err(e) = r {
                crate::diag::warn(&format!("warning: readout snapshot upsert failed: {e}"));
            }
        }

        /// Read every snapshot from the session, ordered by
        /// (slot, subject_kind, subject_id, readout_name) so
        /// scrollback / replay see a stable sequence.
        pub fn read_readout_snapshots(&self) -> Vec<ReadoutSnapshotRow> {
            let mut stmt = match self.conn.prepare(
                "SELECT slot, exec_id, subject_kind, subject_id, readout_name, lod, \
                        rendered_at, body_ansi, body_plain \
                 FROM readout_snapshots \
                 ORDER BY rendered_at, slot, subject_kind, subject_id, readout_name",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = stmt.query_map([], |row| {
                Ok(ReadoutSnapshotRow {
                    slot: row.get(0)?,
                    exec_id: row.get::<_, i64>(1)? as u64,
                    subject_kind: row.get(2)?,
                    subject_id: row.get(3)?,
                    readout_name: row.get(4)?,
                    lod: row.get(5)?,
                    rendered_at: row.get(6)?,
                    body_ansi: row.get(7)?,
                    body_plain: row.get(8)?,
                })
            });
            match rows {
                Ok(iter) => iter.filter_map(Result::ok).collect(),
                Err(_) => Vec::new(),
            }
        }

        /// Get-or-create the `metric_family` row for a given
        /// `(name, type)` identity, attaching the OpenMetrics unit
        /// when supplied.
        ///
        /// SRD-40b §1 / SRD-40a §4.3: the unit is persisted in the
        /// `metric_family.unit` column for structured read access,
        /// while the family name itself already carries the unit
        /// as a suffix (the writer enforces both surfaces from a
        /// single declaration via [`crate::snapshot::MetricFamily::with_unit`]).
        ///
        /// `unit` is `None` when the family has no declared unit;
        /// the column is left NULL in that case.
        fn get_or_insert_family(&mut self, name: &str, typ: &str, unit: Option<&str>) -> i64 {
            let key = format!("{name}:{typ}");
            if let Some(&id) = self.family_cache.get(&key) {
                return id;
            }
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO metric_family (name, type, unit) VALUES (?1, ?2, ?3)",
                    params![name, typ, unit],
                )
                .unwrap_or_else(|e| {
                    crate::diag::warn(&format!("warning: sqlite write failed: {e}"));
                    0
                });
            let id: i64 = self
                .conn
                .query_row(
                    "SELECT id FROM metric_family WHERE name=?1 AND type=?2",
                    params![name, typ],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            self.family_cache.insert(key, id);
            id
        }

        /// Build the canonical label set for `(family_name,
        /// raw_labels)`: drop the legacy in-code `name` label
        /// (it duplicated the family name and caused per-call-
        /// site drift) and pin `__name__` from the family.
        /// The returned `Labels` is the on-disk view — every
        /// pair becomes an `instance_label` row.
        fn canonical_labels(family_name: &str, raw: &Labels) -> Labels {
            let mut canonical = Labels::empty();
            for (k, v) in raw.iter() {
                if k == "name" || k == "__name__" {
                    continue;
                }
                canonical = canonical.with(k, v);
            }
            canonical.with("__name__", family_name)
        }

        /// Single chokepoint for `metric_instance` upsert.
        ///
        /// 1. Canonicalise labels (`canonical_labels`).
        /// 2. Build the OpenMetrics-canonical sample spec —
        ///    `name{k="v",…}`, sorted by key, OpenMetrics
        ///    escape rules.
        /// 3. Use the spec as the identity: two label dicts
        ///    that are equal as a mapping produce the same
        ///    text and resolve to the same `metric_instance.id`.
        /// 4. On first sight of a spec, write all
        ///    `instance_label` rows (including `__name__`) in
        ///    one shot. Subsequent ticks short-circuit on
        ///    `instance_cache`.
        fn upsert_instance(
            &mut self,
            family_id: i64,
            family_name: &str,
            raw_labels: &Labels,
        ) -> i64 {
            let canonical = Self::canonical_labels(family_name, raw_labels);
            let spec = canonical.to_canonical_spec(family_name);
            if let Some(&id) = self.instance_cache.get(&spec) {
                self.batch_touched.insert(id);
                return id;
            }
            // SRD-77 — extract session + exec_id from the
            // canonical labels and write them as first-class
            // columns. The FK on (session, exec_id) →
            // executions enforces that every metric instance
            // is structurally tied to a real execution. The
            // labels already carry these values (set by
            // Component::root at session-open); promoting them
            // to columns gives the storage layer a constraint
            // surface rather than trusting writers to keep
            // the spec well-formed.
            //
            // Reserved-word guard: `"latest"` is a CLI-side
            // virtual qualifier ("the most recent execution")
            // and MUST NEVER land in storage. If the labels
            // carry it (which would only happen via a
            // bug — the resolver should translate it before
            // any write), refuse the insert and warn.
            let session = canonical.get("session").unwrap_or("");
            if session == "latest" {
                crate::diag::warn(
                    "metric_instance insert rejected: \
                     session=\"latest\" is a reserved CLI \
                     qualifier and must be resolved to a \
                     concrete session id before write.",
                );
                return 0;
            }
            let exec_id_raw = canonical.get("exec_id").unwrap_or("");
            if exec_id_raw == "latest" {
                crate::diag::warn(
                    "metric_instance insert rejected: \
                     exec_id=\"latest\" is a reserved CLI \
                     qualifier and must be resolved to a \
                     concrete integer before write.",
                );
                return 0;
            }
            let exec_id: i64 = exec_id_raw.parse::<i64>().unwrap_or(0);
            // SRD-77 FK-parent guard. The deferred FK
            // `metric_instance(session, exec_id) → executions` is
            // verified at the snapshot COMMIT (`Reporter::report`),
            // NOT at this insert. A metric can reach the reporter for
            // a `(session, exec_id)` whose `executions` row is not
            // present yet:
            //   - the cadence scheduler starts (runner.rs:680) before
            //     `insert_execution_start` runs (runner.rs:1850), so
            //     an early tick can commit a metric first;
            //   - SRD-88 concurrent executions share one connection and
            //     stagger, so one execution's metric can be captured
            //     while another still holds the write path;
            //   - session-tier metrics carry `session` but no `exec_id`
            //     label, so `exec_id` falls back to the sentinel 0,
            //     which never gets an `insert_execution_start` row;
            //   - a skipped/failed `insert_execution_start` (its block
            //     only warns) leaves the row absent entirely.
            // Any of these made the deferred FK fail at COMMIT, which
            // rolled back and DROPPED THE WHOLE SNAPSHOT. Write a
            // minimal idempotent placeholder so the parent always
            // exists at commit; `insert_execution_start` upserts the
            // real verb/started_at/snapshots over the `pending`/0
            // placeholder (INSERT OR IGNORE never clobbers a row that
            // is already present, so a real row is left untouched).
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO executions \
                 (session, exec_id, verb, started_at_nanos) \
                 VALUES (?1, ?2, 'pending', 0)",
                    params![session, exec_id],
                )
                .unwrap_or_else(|e| {
                    crate::diag::warn(&format!(
                        "warning: executions FK-parent placeholder insert failed: {e}"
                    ));
                    0
                });
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO metric_instance \
                 (family_id, spec, session, exec_id) VALUES (?1, ?2, ?3, ?4)",
                    params![family_id, &spec, session, exec_id],
                )
                .unwrap_or_else(|e| {
                    crate::diag::warn(&format!("warning: metric_instance insert failed: {e}"));
                    0
                });
            let id: i64 = self
                .conn
                .query_row(
                    "SELECT id FROM metric_instance WHERE spec = ?1",
                    params![&spec],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            if id != 0 {
                // Write every label pair. PRIMARY KEY
                // (instance_id, key) means re-inserts are
                // INSERT OR IGNORE-safe — duplicate ticks
                // never write twice.
                for (k, v) in canonical.sorted_pairs() {
                    self.conn
                        .execute(
                            "INSERT OR IGNORE INTO instance_label (instance_id, key, value) \
                         VALUES (?1, ?2, ?3)",
                            params![id, k, v],
                        )
                        .unwrap_or_else(|e| {
                            crate::diag::warn(&format!(
                                "warning: instance_label insert failed: {e}"
                            ));
                            0
                        });
                }
                // SRD-93 M3 — first storage sight of this spec IS
                // the enter-scope event; this cache-miss branch is
                // the documented single chokepoint, and `report()`'s
                // transaction is already open around it. A cold-cache
                // re-sight (SRD-44 resume / SRD-77 refine reopen) is
                // a no-op via the (instance, exec, event) PK (A7).
                let (at_utc, at_session) = self.clock_now();
                self.conn
                    .execute(
                        "INSERT OR IGNORE INTO instance_scope_event \
                     (instance_id, session, exec_id, event, reason, \
                      at_utc_nanos, at_session_nanos, spec) \
                     VALUES (?1, ?2, ?3, 'enter', 'first_sample', ?4, ?5, ?6)",
                        params![id, session, exec_id, at_utc, at_session, &spec],
                    )
                    .unwrap_or_else(|e| {
                        crate::diag::warn(&format!("warning: scope-event insert failed: {e}"));
                        0
                    });
                self.instance_meta
                    .insert(id, (session.to_string(), exec_id, spec.clone()));
                self.batch_touched.insert(id);
                self.instance_cache.insert(spec, id);
            }
            id
        }

        fn insert_metric(&mut self, snapshot: &MetricSet, family: &MetricFamily, metric: &Metric) {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;
            let interval_ms = snapshot.interval().as_millis() as i64;
            let name = family.name();
            // SRD-40b §1: the family's declared unit (if any)
            // lands in `metric_family.unit` so the read side can
            // surface it without re-parsing the name. Sibling
            // `_sum` / `_count` families inherit the same unit
            // per OpenMetrics §5.3.
            let unit = family.unit();
            let labels = metric.labels();
            let Some(point) = metric.point() else { return };

            match point.value() {
                MetricValue::Counter(c) => {
                    let family_id = self.get_or_insert_family(name, "counter", unit);
                    let instance_id = self.upsert_instance(family_id, name, labels);

                    // OpenMetrics §5.1: counter MAY carry a
                    // `created` instant (series-start). We
                    // approximate by treating `Instant`'s offset
                    // from the writer's start as a relative
                    // ms value — exposition layer translates
                    // to absolute Unix epoch.
                    let created_ms = c.created.map(|t| {
                        let elapsed = t.elapsed();
                        now_ms - elapsed.as_millis() as i64
                    });
                    // Store the cumulative running total so metrics.db is
                    // numerically a Prometheus/VM-schematic counter series
                    // (monotonic) and MetricsQL rate() is exact over it —
                    // see the cumulative-counter note.
                    self.conn.execute(
                        "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, count, created_ms) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![instance_id, now_ms, interval_ms, c.cumulative as i64, created_ms],
                    ).unwrap_or_else(|e| { crate::diag::warn(&format!("warning: sqlite write failed: {e}")); 0 });
                }
                MetricValue::Gauge(g) => {
                    let family_id = self.get_or_insert_family(name, "gauge", unit);
                    let instance_id = self.upsert_instance(family_id, name, labels);

                    self.conn.execute(
                        "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, mean) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![instance_id, now_ms, interval_ms, g.value],
                    ).unwrap_or_else(|e| { crate::diag::warn(&format!("warning: sqlite write failed: {e}")); 0 });
                }
                MetricValue::Histogram(h) => {
                    let family_id = self.get_or_insert_family(name, "summary", unit);
                    let instance_id = self.upsert_instance(family_id, name, labels);

                    let r = &h.reservoir;
                    // Store the CUMULATIVE observation count (Prometheus/VM-
                    // schematic) so MetricsQL rate()/increase over a histogram
                    // count is correct over sqlite too. The percentiles below
                    // stay windowed (from the per-window reservoir).
                    let obs = h.cumulative_count as i64;
                    let min = r.min() as f64;
                    let max = r.max() as f64;
                    let mean = r.mean();
                    let stddev = r.stdev();
                    let sum = h.sum;

                    let p50 = r.value_at_quantile(0.50) as f64;
                    let p75 = r.value_at_quantile(0.75) as f64;
                    let p90 = r.value_at_quantile(0.90) as f64;
                    let p95 = r.value_at_quantile(0.95) as f64;
                    let p98 = r.value_at_quantile(0.98) as f64;
                    let p99 = r.value_at_quantile(0.99) as f64;
                    let p999 = r.value_at_quantile(0.999) as f64;

                    let created_ms = h.created.map(|t| now_ms - t.elapsed().as_millis() as i64);
                    self.conn.execute(
                        "INSERT INTO sample_value \
                         (instance_id, timestamp_ms, interval_ms, count, sum, min, max, mean, stddev, \
                          p50, p75, p90, p95, p98, p99, p999, created_ms) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                        params![
                            instance_id, now_ms, interval_ms, obs, sum, min, max, mean, stddev,
                            p50, p75, p90, p95, p98, p99, p999, created_ms,
                        ],
                    ).unwrap_or_else(|e| { crate::diag::warn(&format!("warning: sqlite write failed: {e}")); 0 });
                }
                MetricValue::BucketedHistogram(h) => {
                    // OpenMetrics §5.3 / §5.4: write one
                    // sample_value row per bucket with the
                    // `le` label distinguishing buckets, plus
                    // sibling _sum / _count families. The
                    // family_type tag follows the family's
                    // declared MetricType (Histogram vs
                    // GaugeHistogram).
                    let family_type = match family.r#type() {
                        MetricType::GaugeHistogram => "gaugehistogram",
                        _ => "histogram",
                    };
                    let family_id = self.get_or_insert_family(name, family_type, unit);
                    for (le, count) in &h.buckets {
                        let le_str = match le {
                            crate::snapshot::BucketBound::Finite(v) => v.to_string(),
                            crate::snapshot::BucketBound::PositiveInfinity => "+Inf".to_string(),
                        };
                        let bucket_labels = labels.with("le", le_str);
                        let instance_id = self.upsert_instance(family_id, name, &bucket_labels);
                        self.conn.execute(
                            "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, count) \
                             VALUES (?1, ?2, ?3, ?4)",
                            params![instance_id, now_ms, interval_ms, *count as i64],
                        ).unwrap_or_else(|e| {
                            crate::diag::warn(&format!("warning: bucket write failed: {e}"));
                            0
                        });
                    }
                    // Sibling _sum / _count families.
                    if let Some(sum_value) = h.sum {
                        let sum_name = format!("{name}_sum");
                        let sum_id = self.get_or_insert_family(&sum_name, family_type, unit);
                        let instance_id = self.upsert_instance(sum_id, &sum_name, labels);
                        self.conn.execute(
                            "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, sum) \
                             VALUES (?1, ?2, ?3, ?4)",
                            params![instance_id, now_ms, interval_ms, sum_value],
                        ).unwrap_or_else(|e| {
                            crate::diag::warn(&format!("warning: _sum write failed: {e}"));
                            0
                        });
                    }
                    let count_name = format!("{name}_count");
                    let count_id = self.get_or_insert_family(&count_name, family_type, unit);
                    let instance_id = self.upsert_instance(count_id, &count_name, labels);
                    self.conn.execute(
                        "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, count) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![instance_id, now_ms, interval_ms, h.count as i64],
                    ).unwrap_or_else(|e| {
                        crate::diag::warn(&format!("warning: _count write failed: {e}"));
                        0
                    });
                }
                MetricValue::Info(_) => {
                    // OpenMetrics §5.6: always-1 metric
                    // whose data lives in the label set.
                    let family_id = self.get_or_insert_family(name, "info", unit);
                    let instance_id = self.upsert_instance(family_id, name, labels);
                    self.conn.execute(
                        "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, count) \
                         VALUES (?1, ?2, ?3, 1)",
                        params![instance_id, now_ms, interval_ms],
                    ).unwrap_or_else(|e| {
                        crate::diag::warn(&format!("warning: info write failed: {e}"));
                        0
                    });
                }
                MetricValue::StateSet(s) => {
                    // OpenMetrics §5.7: one sample per state
                    // (label-keyed by the family-name as
                    // label-key per spec — using the family
                    // name itself as the key is forbidden;
                    // we use `state` as the label key by
                    // convention).
                    let family_id = self.get_or_insert_family(name, "stateset", unit);
                    for (state_name, active) in &s.states {
                        let state_labels = labels.with("state", state_name.as_str());
                        let instance_id = self.upsert_instance(family_id, name, &state_labels);
                        let v = if *active { 1.0 } else { 0.0 };
                        self.conn.execute(
                            "INSERT INTO sample_value (instance_id, timestamp_ms, interval_ms, mean) \
                             VALUES (?1, ?2, ?3, ?4)",
                            params![instance_id, now_ms, interval_ms, v],
                        ).unwrap_or_else(|e| {
                            crate::diag::warn(&format!("warning: stateset write failed: {e}"));
                            0
                        });
                    }
                }
            }
            let _ = MetricType::Counter; // silence unused-import on the Counter path
        }

        /// Low-level native-sample write API for OpenMetrics
        /// types beyond the [`MetricValue`] enum's current
        /// coverage (Counter / Gauge / HDR-summary).
        ///
        /// External producers — or future code paths that
        /// emit Histogram (bucketed), GaugeHistogram, Info,
        /// or StateSet samples — call this directly with the
        /// type tag and the populated columns. The
        /// [`NativeSample`] struct mirrors the
        /// `sample_value` row schema; populate the columns
        /// the type needs and leave the rest `None`.
        ///
        /// Per [SRD-49](../../../docs/SRD/49_metricsql_supported_scope.md):
        /// the storage convention per type is
        ///
        /// | Type            | Populated columns       |
        /// |-----------------|-------------------------|
        /// | counter         | count                   |
        /// | gauge           | mean                    |
        /// | summary         | count, sum, min, max, mean, stddev, p50–p999 |
        /// | histogram       | count (cumulative ≤ `le` label) |
        /// | gaugehistogram  | count (non-monotonic allowed) |
        /// | info            | count = 1 (always)      |
        /// | stateset        | mean ∈ {0.0, 1.0}       |
        /// | unknown         | mean (defensive)        |
        ///
        /// Histogram bucket samples differentiate via the
        /// `le` label on the metric_instance's label set,
        /// not via a separate column. Cumulative `_sum` /
        /// `_count` siblings are emitted as instances under
        /// the same family with `le` absent (or as `_sum` /
        /// `_count` family-name siblings — both shapes are
        /// accepted by the catalog reader).
        pub fn write_native_sample(
            &mut self,
            family_name: &str,
            family_type: &str,
            labels: &Labels,
            sample: &NativeSample,
        ) {
            let family_id =
                self.get_or_insert_family(family_name, family_type, sample.unit.as_deref());
            let instance_id = self.upsert_instance(family_id, family_name, labels);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            self.conn.execute(
                "INSERT INTO sample_value \
                 (instance_id, timestamp_ms, interval_ms, count, sum, min, max, mean, stddev, \
                  p50, p75, p90, p95, p98, p99, p999, created_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                params![
                    instance_id, now_ms, sample.interval_ms,
                    sample.count, sample.sum, sample.min, sample.max,
                    sample.mean, sample.stddev,
                    sample.p50, sample.p75, sample.p90, sample.p95,
                    sample.p98, sample.p99, sample.p999,
                    sample.created_ms,
                ],
            ).unwrap_or_else(|e| {
                crate::diag::warn(&format!(
                    "warning: native-sample write failed: {e}",
                ));
                0
            });
        }
    }

    /// Sample-row payload for [`SqliteReporter::write_native_sample`].
    /// Mirror of the `sample_value` schema row. Each column is
    /// optional; populate the ones the type uses (see the
    /// table on [`SqliteReporter::write_native_sample`]).
    #[derive(Debug, Default, Clone)]
    pub struct NativeSample {
        /// Cadence interval in ms. `0` is acceptable for
        /// one-shot info / stateset samples.
        pub interval_ms: i64,
        pub count: Option<i64>,
        pub sum: Option<f64>,
        pub min: Option<f64>,
        pub max: Option<f64>,
        pub mean: Option<f64>,
        pub stddev: Option<f64>,
        pub p50: Option<f64>,
        pub p75: Option<f64>,
        pub p90: Option<f64>,
        pub p95: Option<f64>,
        pub p98: Option<f64>,
        pub p99: Option<f64>,
        pub p999: Option<f64>,
        /// OpenMetrics §5.1 / §5.3 / §5.5: series-start
        /// timestamp. NULL when the producer doesn't track
        /// it; the catalog reader exposes `<name>_created`
        /// when populated.
        pub created_ms: Option<i64>,
        /// SRD-40b §1 / SRD-40a §4.3: optional unit suffix
        /// (`ratio`, `seconds`, `bytes`, …) declared on the
        /// family. When present, persisted to
        /// `metric_family.unit` on first-insert. The family
        /// name itself is expected to already carry the
        /// unit as a `_<unit>` suffix per OpenMetrics §4.4
        /// — both surfaces flow from the producer's single
        /// declaration and are kept in sync at the source
        /// (see [`crate::snapshot::MetricFamily::with_unit`]).
        pub unit: Option<String>,
    }

    /// Exemplar payload per OpenMetrics §4.6.1. Attached to a
    /// previously-written sample by [`SqliteReporter::write_exemplar`].
    /// Carries the exemplar's value, optional timestamp, and
    /// label set (trace ids, span ids, …) — the source-of-
    /// observation envelope the spec describes.
    ///
    /// Per spec §4.7 the serialized exemplar LabelSet MUST be
    /// ≤ 128 UTF-8 characters. Validation is recommended at
    /// exposition time; the recording path here is permissive
    /// to avoid silently dropping valuable diagnostic data.
    #[derive(Debug, Default, Clone)]
    pub struct ExemplarRow {
        /// The single observed value the exemplar represents.
        pub value: f64,
        /// Optional exemplar timestamp (distinct from the
        /// sample's timestamp).
        pub timestamp_ms: Option<i64>,
        /// Exemplar labels, e.g. `trace_id="abc",span_id="def"`.
        pub labels: Labels,
    }

    impl SqliteReporter {
        /// Attach `exemplar` to an existing sample row,
        /// identified by `(family_name, family_type, labels,
        /// sample_timestamp_ms)`. The (family + labels) tuple
        /// resolves to a `metric_instance.id`; the (instance,
        /// timestamp) pair anchors the exemplar to its
        /// observation.
        ///
        /// **Caller-side ordering**: write the sample row
        /// first via [`Self::write_native_sample`] (or the
        /// existing `MetricValue` path), then call this
        /// with the same identity tuple. The schema doesn't
        /// enforce the pairing — exemplars whose anchor
        /// timestamp doesn't match a real sample are
        /// orphans, harmless, and just won't surface to
        /// catalog readers (since the read query joins on
        /// the timestamp).
        ///
        /// Multiple calls with the same identity append; the
        /// schema permits 0..N exemplars per sample, so
        /// OpenMetrics 2.0's relaxed cardinality is already
        /// honoured.
        pub fn write_exemplar(
            &mut self,
            family_name: &str,
            family_type: &str,
            instance_labels: &Labels,
            sample_timestamp_ms: i64,
            exemplar: &ExemplarRow,
        ) {
            // Exemplars attach to a previously-written sample
            // — the family row is expected to exist already, so
            // unit is left unspecified here. If the sample-write
            // path wrote it, the cache returns the existing id.
            let family_id = self.get_or_insert_family(family_name, family_type, None);
            let instance_id = self.upsert_instance(family_id, family_name, instance_labels);
            let labels_spec = exemplar
                .labels
                .iter()
                .map(|(k, v)| format!("{k}=\"{v}\""))
                .collect::<Vec<_>>()
                .join(",");
            self.conn
                .execute(
                    "INSERT INTO exemplar \
                 (instance_id, sample_timestamp_ms, value, timestamp_ms, labels_spec) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        instance_id,
                        sample_timestamp_ms,
                        exemplar.value,
                        exemplar.timestamp_ms,
                        labels_spec,
                    ],
                )
                .unwrap_or_else(|e| {
                    crate::diag::warn(&format!("warning: exemplar write failed: {e}",));
                    0
                });
        }
    }

    /// Configuration for the summary report, passed from the runner.
    ///
    /// This is the nmbrs-metrics–local mirror of the workload-level
    /// `SummaryConfig`. The runner converts one to the other so that
    /// nmbrs-metrics does not depend on nmbrs-workload.
    /// One row from `readout_snapshots`. Returned by
    /// [`SqliteReporter::read_readout_snapshots`]. Used by
    /// `nmbrs replay` and by the future TUI scrollback that
    /// re-shows a completed phase's last live render.
    #[derive(Debug, Clone)]
    pub struct ReadoutSnapshotRow {
        pub slot: String,
        pub exec_id: u64,
        pub subject_kind: String,
        pub subject_id: String,
        pub readout_name: String,
        pub lod: String,
        pub rendered_at: i64,
        pub body_ansi: Option<Vec<u8>>,
        pub body_plain: String,
    }

    /// SRD-76 — storage-layer mirror of
    /// `nmbrs-runtime::phase_outcome::PhaseOutcome`.
    /// `nmbrs-metrics` deliberately doesn't depend on
    /// `nmbrs-runtime`, so this POD shape exists at the
    /// storage boundary; the executor converts in the write
    /// direction, and `nmbrs replay` converts back in the read
    /// direction. The two shapes evolve together.
    ///
    /// `status` is the canonical short label
    /// (`completed` / `failed` / `skipped` /
    /// `cursor_suspended`) — same vocabulary the in-memory
    /// `PhaseStatus::label()` emits.
    #[derive(Debug, Clone)]
    pub struct PhaseOutcomeRow {
        /// SRD-77 — session identifier (the session-directory
        /// basename). Persisted alongside the per-phase row so
        /// future read paths that aggregate across sessions
        /// (e.g. cross-session report builders) carry a stable
        /// foreign reference.
        pub session: String,
        /// SRD-77 — per-session execution sequence. `1` for
        /// every row until the SRD-77 `refine` verb lands the
        /// monotonic counter; the column is present from day
        /// one so the SRD-77 migration only flips writer
        /// behaviour, not schema shape.
        pub exec_id: u64,
        pub phase_name: String,
        pub phase_labels: String,
        pub status: String,
        pub duration_secs: f64,
        pub started_at_nanos: i64,
        pub ended_at_nanos: i64,
        /// SRD-77 — hex-encoded chain hash from
        /// `GkProgram::instance_hash`. Used by `refine
        /// --scope=changed` to compare a prior outcome's
        /// program shape against the freshly-computed one;
        /// equal hashes mean the phase's program tree is
        /// byte-identical and the phase can be skipped.
        /// `None` for legacy rows recorded before the column
        /// was added.
        pub phase_hash: Option<String>,
        /// SRD-83 (C3) — machine-readable class of why the phase
        /// ended short of natural completion (`timeout`,
        /// `stop_condition`, `error`, `panic`, `operator`).
        /// Denormalized from the first error's class so report
        /// queries can `GROUP BY` it; `None` for succeeded phases
        /// and for legacy rows written before the column existed.
        pub reason_class: Option<String>,
        /// SRD-107 — consumed-params map as canonical JSON
        /// (name -> sha256 hex of the raw value); the per-param
        /// leg of skip validity. `None` for legacy rows and
        /// skipped phases.
        pub params_consumed: Option<String>,
        pub errors: Vec<PhaseErrorRow>,
    }

    /// SRD-76 — storage-layer mirror of
    /// `nmbrs-runtime::phase_outcome::PhaseErrorDetail`.
    /// `cycle` is `Option<u64>` because phase-level errors
    /// (poll_timeout, validation failures) have no cycle
    /// number.
    #[derive(Debug, Clone)]
    pub struct PhaseErrorRow {
        pub class: String,
        pub message: String,
        pub op_name: Option<String>,
        pub cycle: Option<u64>,
        pub op_template: Option<String>,
        pub op_resolved: Option<String>,
        pub at_nanos: i64,
        pub retryable: bool,
    }

    /// SRD-77 — storage-layer mirror of the cardinal-history
    /// `executions` table. One row per `nmbrs <verb>` invocation
    /// that touched a session. `ended_at_nanos` / `disposition`
    /// are populated only after the matching session's shutdown
    /// flush completes — `None` while the execution is in flight
    /// (or if the process exited uncleanly).
    #[derive(Debug, Clone)]
    pub struct ExecutionRow {
        pub session: String,
        pub exec_id: u64,
        pub verb: String,
        pub scope: Option<String>,
        pub started_at_nanos: i64,
        pub ended_at_nanos: Option<i64>,
        pub disposition: Option<String>,
        pub workload_yaml_snapshot: String,
        pub cli_params_snapshot: String,
    }

    pub struct ReportConfig {
        /// Gauge column filter patterns. Empty = show all.
        pub columns: Vec<String>,
        /// Row filter regex patterns on activity labels.
        pub row_filters: Vec<String>,
        /// Aggregate expressions.
        pub aggregates: Vec<ReportAggregate>,
        /// Whether to show individual data rows.
        pub show_details: bool,
        /// SRD-77 — execution qualifier. `Some(n)` narrows the
        /// summary's metric queries to one execution_id;
        /// `None` aggregates across every execution recorded in
        /// the session. Higher layers translate
        /// `ExecutionQualifier` (nmbrs-runtime) into this
        /// primitive at the call site. There is no aggregate-
        /// by-default fallback — callers MUST decide.
        pub exec_id_filter: Option<u64>,
    }

    /// An aggregate expression for the summary report. Two
    /// shapes:
    ///
    /// 1. Single-key filter (`label_key`/`label_pattern` set,
    ///    `group_by` empty): one aggregate row across rows
    ///    matching the filter.
    /// 2. Multi-key grouping (`group_by` non-empty): one
    ///    aggregate row per distinct tuple of values across
    ///    `group_by` keys, taken across the rows that have
    ///    those values.
    pub struct ReportAggregate {
        /// Function name: `"mean"`, `"min"`, or `"max"`.
        pub function: String,
        /// Column name pattern — only matching gauge columns are aggregated.
        pub column_pattern: String,
        /// Label key to filter rows on (single-key form). Empty
        /// for multi-key grouping.
        pub label_key: String,
        /// Substring pattern for the label value (single-key form).
        pub label_pattern: String,
        /// Multi-key grouping: when non-empty, group rows by
        /// every distinct value-tuple across these label keys
        /// and emit one aggregate row per group.
        pub group_by: Vec<String>,
    }

    impl SqliteReporter {
        /// Print a data-driven summary of all metrics collected in this session.
        ///
        /// Thin wrapper around [`format_summary`] that emits to stdout.
        /// See [`format_summary`] for column-discovery semantics.
        pub fn print_summary(&self, config: &ReportConfig) {
            let rendered = self.format_summary(config);
            if !rendered.is_empty() {
                print!("{rendered}");
            }
        }

        /// Render the data-driven summary as a string.
        ///
        /// One row per distinct label set that has `cycles_total > 0`.
        /// Columns are discovered from the metrics that exist:
        /// - cycles and rate are always shown
        /// - latency columns appear when `cycles_servicetime` data exists
        /// - gauge columns appear when gauge data exists
        ///
        /// The `config` controls column filters, row filters, aggregate
        /// expressions, and whether detail rows are shown. Returns an
        /// empty string when there is no data to report.
        pub fn format_summary(&self, config: &ReportConfig) -> String {
            self.format_summary_with_format(config, "md")
        }

        /// Render the summary in the requested format. Recognized
        /// formats: `"md"` (Markdown table — same as
        /// [`Self::format_summary`]) and `"csv"`. Unknown
        /// formats fall back to Markdown.
        ///
        /// Both formats share the same data-extraction pipeline
        /// (filters, gauge discovery, aggregates) — only the
        /// final stringify step differs.
        pub fn format_summary_with_format(&self, config: &ReportConfig, format: &str) -> String {
            let Some((headers, grid)) = self.build_summary_grid(config) else {
                return String::new();
            };
            match format {
                "csv" => render_csv(&headers, &grid),
                _ => render_markdown(&headers, &grid),
            }
        }

        /// Read every named summary previously persisted into
        /// the metrics db's `session_metadata` table under the
        /// `summary.<name>` key namespace. Returns
        /// `(name, spec_text)` pairs in deterministic
        /// (alphabetical) order so output filenames are stable
        /// across regeneration runs.
        ///
        /// Used by `nmbrs --summary` (no spec given) to enumerate
        /// every report defined by the workload that produced
        /// this db, regenerating each one without needing the
        /// original workload file.
        pub fn read_stored_summaries(&self) -> Vec<(String, String)> {
            // SRD-46: persisted items live under `report.<name>`
            // with a kind keyword on the first line. This call
            // enumerates only the `table` items, stripping the
            // kind/name/label prelude so the returned spec is
            // the body the table renderer expects.
            // Latest execution's report defs (per-execution metadata),
            // falling back to legacy session_metadata.
            let mut out = Vec::new();
            for entry in latest_execution_metadata_like(&self.conn, "report.%") {
                let mut lines = entry.1.lines();
                let head = match lines.next() {
                    Some(h) => h,
                    None => continue,
                };
                let name = match head.strip_prefix("table ") {
                    Some(rest) => rest.trim().to_string(),
                    None => continue,
                };
                let body: String = lines
                    .filter(|l| !l.starts_with("label ") && !l.starts_with("target "))
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push((name, body));
            }
            out
        }

        /// Build the headers + grid (data rows + aggregates) for
        /// a summary, applying every filter and aggregate from
        /// `config`. Returns `None` if there's nothing to
        /// render. Shared between every output-format renderer
        /// (`md`, `csv`, …).
        fn build_summary_grid(
            &self,
            config: &ReportConfig,
        ) -> Option<(Vec<String>, Vec<Vec<String>>)> {
            let row_patterns: Vec<regex::Regex> = config
                .row_filters
                .iter()
                .filter_map(|p| regex::Regex::new(p.trim()).ok())
                .collect();

            let rows = self.query_all_activities(config.exec_id_filter);
            if rows.is_empty() {
                return None;
            }

            // Discover which optional column groups have data
            let has_latency = rows.iter().any(|r| r.latency_p50_ns.is_some());
            let mut gauge_names: Vec<String> = Vec::new();
            for row in &rows {
                for (name, _) in &row.gauges {
                    if !gauge_names.contains(name) {
                        let include = if config.columns.is_empty() {
                            true
                        } else {
                            config.columns.iter().any(|p| name.contains(p))
                        };
                        if include {
                            gauge_names.push(name.clone());
                        }
                    }
                }
            }

            // Build column headers
            let mut headers: Vec<String> = vec!["Activity".into(), "Cycles".into(), "Rate".into()];
            if has_latency {
                headers.extend(["p50".into(), "p99".into(), "mean".into()]);
            }
            for name in &gauge_names {
                headers.push(name.clone());
            }

            // Build cell grid from data rows
            let mut grid: Vec<Vec<String>> = Vec::new();
            for row in &rows {
                if !row_patterns.is_empty()
                    && !row_patterns.iter().any(|p| p.is_match(&row.activity))
                {
                    continue;
                }
                let cells = format_data_row(row, has_latency, &gauge_names);
                grid.push(cells);
            }

            // Compute aggregate rows
            let agg_rows = compute_aggregates(&config.aggregates, &rows, has_latency, &gauge_names);

            // If details=hide, drop data rows and show only aggregates
            if !config.show_details {
                grid.clear();
            }

            if grid.is_empty() && agg_rows.is_empty() {
                return None;
            }

            // Align label components within the Activity column (data rows only).
            align_activity_column(&mut grid);

            // Append aggregate rows after a blank separator
            if !agg_rows.is_empty() && !grid.is_empty() {
                let blank: Vec<String> = (0..headers.len()).map(|_| String::new()).collect();
                grid.push(blank);
            }
            grid.extend(agg_rows);

            Some((headers, grid))
        }

        /// Query all activities that produced data, returning one row per
        /// distinct label set. No hardcoded phase name patterns — the
        /// summary is projected directly from whatever the workload produced.
        ///
        /// SRD-77 — `exec_id_filter` qualifies which execution's
        /// data to project: `Some(n)` narrows; `None` aggregates
        /// across every execution. Other per-label-set query
        /// helpers below (`query_latency`, `query_gauges_for_labels`,
        /// `query_elapsed_ms`) also accept the same qualifier
        /// so the projection is consistent across one summary
        /// pass.
        fn query_all_activities(&self, exec_id_filter: Option<u64>) -> Vec<ActivityRow> {
            // Find every distinct label set that has cycles_total > 0.
            // Phase-level inclusion / exclusion is gone — every
            // active phase contributes a row by default; the
            // `report:` block (SRD-46) decides what gets
            // rendered into which file.
            let (sql, params): (&str, Vec<i64>) = match exec_id_filter {
                Some(id) => (
                    "SELECT mi.spec, MAX(sv.count)
                     FROM sample_value sv
                     JOIN metric_instance mi ON sv.instance_id = mi.id
                     WHERE mi.spec LIKE 'cycles_total%' AND mi.exec_id = ?1
                     GROUP BY mi.id
                     HAVING MAX(sv.count) > 0
                     ORDER BY mi.id",
                    vec![id as i64],
                ),
                None => (
                    "SELECT mi.spec, MAX(sv.count)
                     FROM sample_value sv
                     JOIN metric_instance mi ON sv.instance_id = mi.id
                     WHERE mi.spec LIKE 'cycles_total%'
                     GROUP BY mi.id
                     HAVING MAX(sv.count) > 0
                     ORDER BY mi.id",
                    Vec::new(),
                ),
            };
            let mut stmt = match self.conn.prepare(sql) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };

            let mut rows: Vec<(Vec<(String, String)>, ActivityRow)> = Vec::new();
            let iter = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            });

            if let Ok(iter) = iter {
                for r in iter.filter_map(|r| r.ok()) {
                    let labels = Self::spec_labels(&r.0);
                    if labels.is_empty() {
                        continue;
                    }
                    let display = extract_labels_display(&r.0);
                    let cycles = r.1 as u64;

                    let elapsed = self.query_elapsed_ms(labels);
                    let rate = if elapsed > 0.0 {
                        cycles as f64 * 1000.0 / elapsed
                    } else {
                        0.0
                    };
                    let latency = self.query_latency(labels);
                    let gauges = self.query_gauges_for_labels(labels);

                    let sort_key = parse_label_pairs(labels);
                    rows.push((
                        sort_key,
                        ActivityRow {
                            activity: display,
                            cycles,
                            rate,
                            latency_p50_ns: latency.map(|l| l.0),
                            latency_p99_ns: latency.map(|l| l.1),
                            latency_mean_ns: latency.map(|l| l.2),
                            gauges,
                        },
                    ));
                }
            }

            // Canonical presentation order: sort rows by the
            // alphabetised (key, value) tuples extracted from
            // each row's labels. Values that look like integers
            // compare numerically (`limit=10` after `limit=2`,
            // not before).
            rows.sort_by(|a, b| compare_label_tuples(&a.0, &b.0));
            rows.into_iter().map(|(_, r)| r).collect()
        }

        /// Query latency stats for a label set.
        ///
        /// Returns `(p50_ns, p99_ns, mean_ns)` in nanoseconds, or `None`.
        /// Uses the sample with the most observations (highest `count`)
        /// rather than the chronologically last one, because delta-histogram
        /// snapshots can produce empty trailing samples when a phase ends
        /// between capture intervals.
        fn query_latency(&self, label_part: &str) -> Option<(f64, f64, f64)> {
            let spec = format!("cycles_servicetime{{{label_part}}}");
            self.conn
                .query_row(
                    "SELECT sv.p50, sv.p99, sv.mean
                 FROM sample_value sv
                 JOIN metric_instance mi ON sv.instance_id = mi.id
                 WHERE mi.spec = ?1
                 ORDER BY sv.count DESC
                 LIMIT 1",
                    params![spec],
                    |row| {
                        Ok((
                            row.get::<_, f64>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, f64>(2)?,
                        ))
                    },
                )
                .ok()
        }

        /// Query all gauge values matching a label set.
        /// Returns `(short_name, value)` pairs. Gauge names have the
        /// `.mean`/`.p50`/etc. suffix stripped — only `.mean` is collected.
        ///
        /// Gauge labels may be a superset of the activity labels (e.g.
        /// they include `n="100"`), so we match both exact and extended.
        fn query_gauges_for_labels(&self, label_part: &str) -> Vec<(String, f64)> {
            let exact = format!("%{{{label_part}}}");
            let extended = format!("%{{{label_part},%");
            let mut stmt = match self.conn.prepare(
                "SELECT mi.spec, sv.mean FROM sample_value sv
                 JOIN metric_instance mi ON sv.instance_id = mi.id
                 JOIN metric_family mf ON mi.family_id = mf.id
                 WHERE mf.type = 'gauge'
                   AND (mi.spec LIKE ?1 OR mi.spec LIKE ?2)
                 ORDER BY mi.spec",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let mut seen = std::collections::HashSet::new();
            stmt.query_map(params![exact, extended], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })
            .ok()
            .map(|r| {
                r.filter_map(|r| r.ok())
                    .filter_map(|(spec, val)| {
                        let name = spec.split('{').next().unwrap_or(&spec);
                        // Only collect .mean variants, strip the suffix
                        if !name.ends_with("_mean") {
                            return None;
                        }
                        let short = name.strip_suffix("_mean").unwrap_or(name);
                        if seen.contains(short) {
                            return None;
                        }
                        seen.insert(short.to_string());
                        Some((short.to_string(), val))
                    })
                    .collect()
            })
            .unwrap_or_default()
        }

        /// Extract the labels portion of a spec (everything inside {}).
        fn spec_labels(spec: &str) -> &str {
            spec.split('{')
                .nth(1)
                .and_then(|s| s.strip_suffix('}'))
                .unwrap_or("")
        }

        /// Get elapsed wall-clock time for a label set by finding the time
        /// range across all metrics sharing those labels.
        /// Total activity duration in milliseconds for a given label
        /// set. Uses the sum of `cycles_total` sample intervals — each
        /// row is one closed cadence window, so the sum is the total
        /// time the phase produced data. This is the correct rate
        /// denominator.
        ///
        /// An earlier implementation used `MAX(ts) - MIN(ts)` across
        /// every family, which conflated write-time spread (~ms) with
        /// phase duration (seconds to minutes) — a 2-second phase
        /// would report elapsed ≈ 2ms and blow rates into the hundreds
        /// of thousands per second.
        fn query_elapsed_ms(&self, label_part: &str) -> f64 {
            let spec = format!("cycles_total{{{label_part}}}");
            let result: Result<i64, _> = self.conn.query_row(
                "SELECT COALESCE(SUM(sv.interval_ms), 0)
                 FROM sample_value sv
                 JOIN metric_instance mi ON sv.instance_id = mi.id
                 WHERE mi.spec = ?1",
                params![spec],
                |row| row.get(0),
            );
            result.ok().map(|ms| ms as f64).unwrap_or(0.0)
        }
    }

    /// One row in the summary table — one per distinct label set.
    struct ActivityRow {
        activity: String,
        cycles: u64,
        rate: f64,
        /// Latency percentiles in nanoseconds (sysref: all internal time = nanos).
        latency_p50_ns: Option<f64>,
        latency_p99_ns: Option<f64>,
        latency_mean_ns: Option<f64>,
        /// Gauge values keyed by short name (e.g. "recall_at_10").
        gauges: Vec<(String, f64)>,
    }

    /// Parse a `key="value", key="value"` label string (the
    /// portion between `{...}` in a Prometheus-style spec) into
    /// a `Vec<(key, value)>` sorted alphabetically by key. Used
    /// as the canonical sort tuple for rows in
    /// `build_summary_grid` so dimensional labels — not metric-
    /// instance insertion order — establish presentation order.
    pub(crate) fn parse_label_pairs(label_part: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let bytes = label_part.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b',') {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let key_start = i;
            while i < bytes.len() && bytes[i] != b'=' {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let key = label_part[key_start..i].trim().to_string();
            i += 1; // consume '='
            if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
                let val_start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
                let val = label_part[val_start..i].to_string();
                if i < bytes.len() {
                    i += 1;
                }
                out.push((key, val));
            } else {
                let val_start = i;
                while i < bytes.len() && !matches!(bytes[i], b',' | b' ' | b'\t') {
                    i += 1;
                }
                let val = label_part[val_start..i].to_string();
                out.push((key, val));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Lexicographic compare on alphabetised label tuples with
    /// natural-numeric value compare (so `limit=10` lands after
    /// `limit=2`, not before). Keys are already sorted by
    /// [`parse_label_pairs`]; this just zips and compares.
    pub(crate) fn compare_label_tuples(
        a: &[(String, String)],
        b: &[(String, String)],
    ) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        for (av, bv) in a.iter().zip(b.iter()) {
            match av.0.cmp(&bv.0) {
                Ordering::Equal => {}
                other => return other,
            }
            match natural_value_cmp(&av.1, &bv.1) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        a.len().cmp(&b.len())
    }

    fn natural_value_cmp(a: &str, b: &str) -> std::cmp::Ordering {
        match (a.parse::<i64>(), b.parse::<i64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a.cmp(b),
        }
    }

    /// Auto-select the time unit suffix so the numeric part has significant digits.
    ///
    /// Input is nanoseconds (sysref standard). Output always includes a unit suffix.
    fn format_duration(nanos: f64) -> String {
        if nanos >= 1_000_000_000.0 {
            format!("{:.2}s", nanos / 1_000_000_000.0)
        } else if nanos >= 1_000_000.0 {
            format!("{:.2}ms", nanos / 1_000_000.0)
        } else if nanos >= 1_000.0 {
            format!("{:.2}µs", nanos / 1_000.0)
        } else {
            format!("{:.2}ns", nanos)
        }
    }

    /// Format a single data row into cell strings.
    fn format_data_row(
        row: &ActivityRow,
        has_latency: bool,
        gauge_names: &[String],
    ) -> Vec<String> {
        let rate_str = if row.rate > 0.0 {
            format!("{:.0}/s", row.rate)
        } else {
            "-".to_string()
        };
        let mut cells: Vec<String> = vec![row.activity.clone(), row.cycles.to_string(), rate_str];
        if has_latency {
            if let (Some(p50), Some(p99), Some(mean)) =
                (row.latency_p50_ns, row.latency_p99_ns, row.latency_mean_ns)
            {
                cells.push(format_duration(p50));
                cells.push(format_duration(p99));
                cells.push(format_duration(mean));
            } else {
                cells.extend(["-".into(), "-".into(), "-".into()]);
            }
        }
        for name in gauge_names {
            let val = row
                .gauges
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| format!("{v:.4}"))
                .unwrap_or_else(|| "-".to_string());
            cells.push(val);
        }
        cells
    }

    /// Compute aggregate rows from the data.
    ///
    /// Each `ReportAggregate` produces one row. The activity column shows
    /// the expression (e.g., `mean(recall) over profile~label`). Gauge
    /// columns matching `column_pattern` are aggregated; others show `-`.
    fn compute_aggregates(
        aggregates: &[ReportAggregate],
        rows: &[ActivityRow],
        has_latency: bool,
        gauge_names: &[String],
    ) -> Vec<Vec<String>> {
        let mut agg_rows = Vec::new();

        for agg in aggregates {
            if !agg.group_by.is_empty() {
                // Multi-key grouping: one aggregate row per
                // distinct value-tuple across `group_by` keys.
                agg_rows.extend(compute_grouped_aggregate(
                    agg,
                    rows,
                    has_latency,
                    gauge_names,
                ));
                continue;
            }

            // Single-key filter form: filter rows by
            // `<label_key>~<pattern>`, emit one aggregate row.
            let matching: Vec<&ActivityRow> = rows
                .iter()
                .filter(|r| {
                    // Look for key=value in the activity string where value contains pattern
                    for segment in r.activity.split(", ") {
                        if let Some((k, v)) = segment.split_once('=')
                            && k.trim() == agg.label_key
                            && v.trim().contains(&agg.label_pattern)
                        {
                            return true;
                        }
                    }
                    false
                })
                .collect();

            let label = format!(
                "**{}({}) over {}~{}**",
                agg.function, agg.column_pattern, agg.label_key, agg.label_pattern,
            );

            let mut cells: Vec<String> = vec![
                label,
                "-".into(), // Cycles
                "-".into(), // Rate
            ];

            if has_latency {
                cells.extend(["-".into(), "-".into(), "-".into()]);
            }

            for gauge_name in gauge_names {
                if !gauge_name.contains(&agg.column_pattern) {
                    cells.push("-".into());
                    continue;
                }
                // Collect all values for this gauge across matching rows
                let values: Vec<f64> = matching
                    .iter()
                    .filter_map(|r| {
                        r.gauges
                            .iter()
                            .find(|(n, _)| n == gauge_name)
                            .map(|(_, v)| *v)
                    })
                    .collect();

                if values.is_empty() {
                    cells.push("-".into());
                } else {
                    let result = match agg.function.as_str() {
                        "mean" => values.iter().sum::<f64>() / values.len() as f64,
                        "min" => values.iter().cloned().fold(f64::INFINITY, f64::min),
                        "max" => values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                        _ => 0.0,
                    };
                    cells.push(format!("{result:.4}"));
                }
            }

            agg_rows.push(cells);
        }

        agg_rows
    }

    /// Multi-key grouping form: emit one aggregate row per
    /// distinct value-tuple across `agg.group_by` keys. The
    /// label column reads
    /// `**mean(recall) over k,limit,optimize_for [k=10, limit=20, optimize_for=RECALL]**`
    /// so the user can identify the group from the report.
    fn compute_grouped_aggregate(
        agg: &ReportAggregate,
        rows: &[ActivityRow],
        has_latency: bool,
        gauge_names: &[String],
    ) -> Vec<Vec<String>> {
        use std::collections::BTreeMap;
        let mut groups: BTreeMap<String, Vec<&ActivityRow>> = BTreeMap::new();
        for row in rows {
            let label_map: std::collections::HashMap<&str, &str> = row
                .activity
                .split(", ")
                .filter_map(|seg| seg.split_once('='))
                .map(|(k, v)| (k.trim(), v.trim()))
                .collect();
            let mut tuple_parts: Vec<String> = Vec::with_capacity(agg.group_by.len());
            let mut all_present = true;
            for key in &agg.group_by {
                match label_map.get(key.as_str()) {
                    Some(v) => tuple_parts.push(format!("{key}={v}")),
                    None => {
                        all_present = false;
                        break;
                    }
                }
            }
            if !all_present {
                continue;
            }
            let tuple_key = tuple_parts.join(", ");
            groups.entry(tuple_key).or_default().push(row);
        }

        let mut out = Vec::new();
        let group_by_header = agg.group_by.join(",");
        for (tuple_key, group_rows) in groups {
            let label = format!(
                "**{}({}) over {} [{tuple_key}]**",
                agg.function, agg.column_pattern, group_by_header,
            );
            let mut cells: Vec<String> = vec![label, "-".into(), "-".into()];
            if has_latency {
                cells.extend(["-".into(), "-".into(), "-".into()]);
            }
            for gauge_name in gauge_names {
                if !gauge_name.contains(&agg.column_pattern) {
                    cells.push("-".into());
                    continue;
                }
                let values: Vec<f64> = group_rows
                    .iter()
                    .filter_map(|r| {
                        r.gauges
                            .iter()
                            .find(|(n, _)| n == gauge_name)
                            .map(|(_, v)| *v)
                    })
                    .collect();
                if values.is_empty() {
                    cells.push("-".into());
                } else {
                    let result = match agg.function.as_str() {
                        "mean" => values.iter().sum::<f64>() / values.len() as f64,
                        "min" => values.iter().cloned().fold(f64::INFINITY, f64::min),
                        "max" => values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                        _ => 0.0,
                    };
                    cells.push(format!("{result:.4}"));
                }
            }
            out.push(cells);
        }
        out
    }

    /// Extract all labels from a spec string into a display-friendly format.
    /// Skips session and n (sample count) — shows the meaningful dimensions.
    fn extract_labels_display(spec: &str) -> String {
        let labels_part = spec
            .split('{')
            .nth(1)
            .and_then(|s| s.strip_suffix('}'))
            .unwrap_or("");
        let parts: Vec<&str> = labels_part
            .split(',')
            .filter(|p| {
                !p.trim().starts_with("session=")
                    && !p.trim().starts_with("n=")
                    && !p.trim().starts_with("name=")
                    && !p.trim().starts_with("nosummary=")
            })
            .collect();
        parts.join(", ").replace('"', "")
    }

    /// Align label components within the Activity column (column 0).
    ///
    /// Each activity string is `"key=val, key=val, ..."`. This function
    /// discovers all distinct keys, orders them so that keys appearing
    /// in more rows sort first (ties broken alphabetically), computes
    /// the max `key=value` width for each key slot, and pads each row
    /// so that the same key starts at the same character position.
    fn align_activity_column(grid: &mut [Vec<String>]) {
        if grid.is_empty() {
            return;
        }

        // Parse each activity into (key, "key=value") pairs
        let parsed: Vec<Vec<(String, String)>> = grid
            .iter()
            .map(|row| {
                row[0]
                    .split(", ")
                    .filter_map(|seg| {
                        let key = seg.split('=').next().unwrap_or("").to_string();
                        if key.is_empty() {
                            None
                        } else {
                            Some((key, seg.to_string()))
                        }
                    })
                    .collect()
            })
            .collect();

        // Discover all keys in component-tree order. Use the row with
        // the most segments as the canonical ordering — it has all the
        // nesting levels. Additional keys from other rows are appended.
        let mut all_keys: Vec<String> = Vec::new();
        let longest = parsed.iter().max_by_key(|r| r.len());
        if let Some(row) = longest {
            for (key, _) in row {
                if !all_keys.contains(key) {
                    all_keys.push(key.clone());
                }
            }
        }
        for row in &parsed {
            for (key, _) in row {
                if !all_keys.contains(key) {
                    all_keys.push(key.clone());
                }
            }
        }

        // Compute max width per key slot
        let mut slot_widths: Vec<usize> = vec![0; all_keys.len()];
        for row in &parsed {
            for (i, key) in all_keys.iter().enumerate() {
                if let Some((_, seg)) = row.iter().find(|(k, _)| k == key) {
                    let w = seg.chars().count();
                    if w > slot_widths[i] {
                        slot_widths[i] = w;
                    }
                }
            }
        }

        // Rebuild each activity string with aligned slots.
        // Each slot occupies a fixed width (segment + separator).
        // Absent keys become blank padding of the same width.
        let sep = ", ";
        let sep_len = sep.len();
        for (row_idx, row) in parsed.iter().enumerate() {
            let mut buf = String::new();
            for (i, key) in all_keys.iter().enumerate() {
                let is_last = i + 1 == all_keys.len();
                let total_w = slot_widths[i] + if is_last { 0 } else { sep_len };
                if let Some((_, seg)) = row.iter().find(|(k, _)| k == key) {
                    if is_last {
                        buf.push_str(&format!("{:<w$}", seg, w = slot_widths[i]));
                    } else {
                        // Pad segment + separator to fixed total width
                        let with_sep = format!("{}{}", seg, sep);
                        buf.push_str(&format!("{:<w$}", with_sep, w = total_w));
                    }
                } else {
                    buf.push_str(&" ".repeat(total_w));
                }
            }
            grid[row_idx][0] = buf.trim_end().to_string();
        }
    }

    /// Render a Markdown table from a (headers, grid) pair.
    /// Same output shape the in-run summary produced before
    /// formats were pluggable.
    fn render_markdown(headers: &[String], grid: &[Vec<String>]) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let ncols = headers.len();
        let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
        for row in grid {
            for (i, cell) in row.iter().enumerate() {
                let w = cell.chars().count();
                if i < ncols && w > widths[i] {
                    widths[i] = w;
                }
            }
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "## Summary");
        let _ = writeln!(out);

        let mut line = String::from("|");
        for (i, h) in headers.iter().enumerate() {
            let _ = write!(line, " {:<w$} |", h, w = widths[i]);
        }
        let _ = writeln!(out, "{line}");

        let mut sep = String::from("|");
        for w in &widths {
            let _ = write!(sep, "-{}-|", "-".repeat(*w));
        }
        let _ = writeln!(out, "{sep}");

        for row in grid {
            let mut line = String::from("|");
            for (i, cell) in row.iter().enumerate() {
                if i < ncols {
                    if i == 0 {
                        let _ = write!(line, " {:<w$} |", cell, w = widths[i]);
                    } else {
                        let _ = write!(line, " {:>w$} |", cell, w = widths[i]);
                    }
                }
            }
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out);
        out
    }

    /// Render a CSV file from a (headers, grid) pair (RFC 4180
    /// quoting). Same data the Markdown renderer sees, just
    /// machine-readable.
    fn render_csv(headers: &[String], grid: &[Vec<String>]) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        // Headers
        let row: Vec<String> = headers.iter().map(|h| csv_quote(h)).collect();
        let _ = writeln!(out, "{}", row.join(","));
        // Data + aggregate rows
        for row in grid {
            let cells: Vec<String> = row.iter().map(|c| csv_quote(c)).collect();
            let _ = writeln!(out, "{}", cells.join(","));
        }
        out
    }

    /// Quote a field for CSV per RFC 4180: wrap in `"..."` and
    /// double inner quotes when the field contains `,`, `"`,
    /// `\n`, or `\r`. Otherwise pass through.
    fn csv_quote(s: &str) -> String {
        if s.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    }

    impl Reporter for SqliteReporter {
        fn report(&mut self, snapshot: &MetricSet) {
            // Batch the whole snapshot into ONE transaction. Without this each
            // insert auto-commits — its own WAL commit record per row — and a
            // snapshot carries many rows (per component × family × metric). One
            // commit per tick collapses N commit records into one; the indexes
            // are still maintained and the consolidation checkpoint still runs,
            // so the durable, externally-readable db is byte-for-byte the same.
            // If BEGIN can't start (already in a txn — shouldn't happen here),
            // fall back to per-row auto-commit rather than skip the write.
            let batched = self.conn.execute_batch("BEGIN").is_ok();
            for family in snapshot.families() {
                for metric in family.metrics() {
                    self.insert_metric(snapshot, family, metric);
                }
            }
            // SRD-93 M4 — a lifecycle-sealed window carries its exit
            // events in the SAME transaction as its final samples.
            // Quiesce (and naturally-closed windows) seal without
            // exiting scope (A6). Coverage is the batch's touched
            // set: the scope_close drain enumerates every registered
            // instrument of the closing component, so the batch IS
            // the component's instrument set.
            let exit_reason = match snapshot.close_reason() {
                Some(crate::snapshot::CloseReason::ScopeClose) => Some("scope_close"),
                Some(crate::snapshot::CloseReason::Shutdown) => Some("shutdown"),
                _ => None,
            };
            if let Some(reason) = exit_reason {
                let (at_utc, at_session) = self.clock_now();
                for id in self.batch_touched.iter() {
                    let Some((session, exec_id, spec)) = self.instance_meta.get(id) else {
                        continue;
                    };
                    self.conn
                        .execute(
                            "INSERT OR IGNORE INTO instance_scope_event \
                         (instance_id, session, exec_id, event, reason, \
                          at_utc_nanos, at_session_nanos, spec) \
                         VALUES (?1, ?2, ?3, 'exit', ?4, ?5, ?6, ?7)",
                            params![*id, session, exec_id, reason, at_utc, at_session, spec],
                        )
                        .unwrap_or_else(|e| {
                            crate::diag::warn(&format!("warning: scope-event insert failed: {e}"));
                            0
                        });
                }
            }
            self.batch_touched.clear();
            if batched {
                if let Err(e) = self.conn.execute_batch("COMMIT") {
                    crate::diag::warn(&format!("sqlite snapshot commit failed: {e}"));
                    let _ = self.conn.execute_batch("ROLLBACK");
                }
            }
        }

        fn flush(&mut self) {
            // Each `report` commits its own snapshot transaction; nothing is
            // left buffered between snapshots.
        }
    }

    impl SqliteReporter {
        /// Merge the WAL+SHM sidecar files into the main `.db`
        /// file so the database is self-contained on disk.
        ///
        /// SQLite's WAL journal mode produces three files at
        /// runtime: `metrics.db`, `metrics.db-wal`, and
        /// `metrics.db-shm`. The `-wal` file holds committed
        /// pages that haven't yet been migrated to the main
        /// file; the `-shm` is a shared-memory index over the
        /// WAL. If a session directory is archived or moved
        /// without these sidecars, readers see the db as if
        /// the trailing writes never happened.
        ///
        /// `PRAGMA wal_checkpoint(TRUNCATE)` forces all
        /// pending WAL frames into the main db file AND
        /// truncates the WAL to zero bytes. We then flip
        /// `PRAGMA journal_mode=DELETE` so the `-wal` and
        /// `-shm` sidecars are actually removed by SQLite
        /// when the connection drops (the TRUNCATE alone
        /// leaves zero-length sidecars on disk under
        /// WAL mode). The main file alone holds the complete
        /// session record after this sequence.
        ///
        /// Called once at session end from
        /// `nmbrs-runtime::runner` after every reporter has
        /// flushed and before the reporter is dropped.
        /// Failures are logged and swallowed — a partial
        /// consolidation is preferable to a panic during
        /// shutdown. Safe to call only at session end: the
        /// journal-mode flip converts the DB back to
        /// rollback-journal mode for any subsequent writes,
        /// so callers must not write after this fires.
        pub fn consolidate_wal(&self) {
            // SRD-93 M4 — terminal sweep: a CLEAN shutdown pairs every
            // still-open enter with exit('shutdown'), catching windows
            // whose dispatch didn't land before teardown and dynamic-
            // capture instruments a hook omitted on the drain pass. A
            // crash never reaches this line, so its unpaired enters
            // remain the truthful record (A7). Set-based; stamped from
            // the same clock as every other event.
            {
                let (at_utc, at_session) = self.clock_now();
                let _ = self
                    .conn
                    .execute(
                        "INSERT OR IGNORE INTO instance_scope_event \
                     (instance_id, session, exec_id, event, reason, \
                      at_utc_nanos, at_session_nanos, spec) \
                     SELECT e.instance_id, e.session, e.exec_id, 'exit', \
                            'shutdown', ?1, ?2, e.spec \
                     FROM instance_scope_event e \
                     WHERE e.event = 'enter' AND NOT EXISTS ( \
                         SELECT 1 FROM instance_scope_event x \
                         WHERE x.instance_id = e.instance_id \
                           AND x.exec_id = e.exec_id \
                           AND x.event = 'exit')",
                        params![at_utc, at_session],
                    )
                    .map_err(|e| {
                        crate::diag::warn(&format!("sqlite scope-event terminal sweep failed: {e}"))
                    });
            }
            // Build the deferred read indexes before the final checkpoint so
            // the durable, externally-readable db is fully indexed (no-op if a
            // read already triggered them). Best-effort: a failure here must
            // not abort shutdown — the worst case is an unindexed db that the
            // next reader self-heals.
            if let Err(e) = Self::ensure_read_indexes(&self.conn) {
                crate::diag::warn(&format!(
                    "sqlite shutdown index build failed: {e} — db left unindexed \
                     until the next read completes it"
                ));
            }
            // PRAGMA returns a row carrying (busy, log_size,
            // checkpointed_count). We don't care about the
            // values — just need the operation to run. Use
            // `query_row` to consume the row so SQLite finalises
            // the checkpoint cleanly.
            let checkpoint_ok = self
                .conn
                .query_row(
                    "PRAGMA wal_checkpoint(TRUNCATE)",
                    rusqlite::params![],
                    |_row| Ok(()),
                )
                .map_err(|e| {
                    crate::diag::warn(&format!(
                        "sqlite WAL consolidation failed: {e} \
                         — `metrics.db-wal` / `metrics.db-shm` may \
                         still hold committed writes; keep them \
                         alongside the .db when archiving"
                    ));
                })
                .is_ok();

            // Only flip the journal mode if the truncate
            // succeeded — flipping with a non-empty WAL would
            // force SQLite to drain it under rollback semantics,
            // which we don't want to risk on an already-failed
            // shutdown path.
            if checkpoint_ok {
                let _ = self
                    .conn
                    .query_row("PRAGMA journal_mode=DELETE", rusqlite::params![], |_row| {
                        Ok(())
                    })
                    .map_err(|e| {
                        crate::diag::warn(&format!(
                            "sqlite journal_mode=DELETE failed: {e} \
                             — `metrics.db-wal` / `metrics.db-shm` \
                             will remain as zero/minimal sidecars \
                             alongside the .db; safe to delete by hand"
                        ));
                    });
            }
        }

        /// Run the session-end WAL consolidation and emit
        /// operator-visible "shutting down" notices to stderr.
        /// Separated from [`Self::consolidate_wal`] so callers
        /// who don't want the stderr chrome (tests, replay
        /// teardown, panic unwind paths where stderr is gone)
        /// can still call the raw checkpoint.
        ///
        /// This is the canonical session-end shutdown sink for
        /// the WAL — wired by the RAII guard
        /// [`SqliteShutdownGuard`] so it runs reliably on every
        /// session-exit path (normal completion, error unwind,
        /// first-Ctrl-C cooperative shutdown). The only exit
        /// path that skips it is `process::exit` (second
        /// Ctrl-C force-exit) — which is the operator's
        /// declared "I don't want to wait" escape hatch.
        pub fn shutdown_consolidate(&self) {
            // Print BEFORE consolidating so the operator
            // sees the "shutting down" notice and knows what
            // a second Ctrl-C would interrupt. Stderr is the
            // right channel — session.log is being closed,
            // and the readout sinks have already
            // shutdown by the time the drop guard fires.
            eprintln!(
                "nmbrs: shutting down — consolidating metrics.db WAL \
                 (press Ctrl-C again to force-exit, leaving the \
                 -wal / -shm sidecars behind)"
            );
            self.consolidate_wal();
            eprintln!("nmbrs: shutdown complete");
        }
    }

    /// RAII guard that runs the session-end WAL consolidation
    /// when dropped. Holds the same
    /// `Arc<Mutex<Option<SqliteReporter>>>` shape the runner
    /// uses, so dropping the guard doesn't drop the reporter
    /// itself — the consolidation runs against the LIVE
    /// reporter while everything else is still wired up,
    /// THEN the reporter drops on its own when its last
    /// strong reference goes out of scope.
    ///
    /// Drop-based design: the consolidation fires reliably on
    /// every session-end path Rust unwinds through (normal
    /// return, `?` error propagation, first-Ctrl-C → flag →
    /// runner unwind). The only skip path is
    /// `std::process::exit`, which is the canonical
    /// force-exit semantic the operator gets on a SECOND
    /// Ctrl-C — declared in `nmbrs-runtime::session_signals`.
    pub struct SqliteShutdownGuard {
        reporter: std::sync::Arc<std::sync::Mutex<Option<SqliteReporter>>>,
        consumed: std::sync::atomic::AtomicBool,
    }

    impl SqliteShutdownGuard {
        /// Construct a guard wrapping the shared reporter
        /// handle. Cheap — no work happens until the guard
        /// drops.
        pub fn new(reporter: std::sync::Arc<std::sync::Mutex<Option<SqliteReporter>>>) -> Self {
            Self {
                reporter,
                consumed: std::sync::atomic::AtomicBool::new(false),
            }
        }

        /// Run the WAL consolidation NOW and mark the guard
        /// consumed so the drop-time fallback is a no-op.
        /// Returns whatever the caller wants — the consolidation
        /// runs silently here so the caller can emit operator-
        /// visible "shutting down" / "shutdown complete"
        /// messages through whatever channel is appropriate
        /// (observer log, stderr, …) and they land in the right
        /// stream relative to other output. The drop-time
        /// fallback's `eprintln!` exists only for unclean
        /// shutdown paths (panic, force-exit during render-
        /// active sinks) where the observer is no longer
        /// available.
        pub fn consume(&self) {
            if self
                .consumed
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                return;
            }
            if let Ok(guard) = self.reporter.lock()
                && let Some(ref r) = *guard
            {
                r.consolidate_wal();
            }
        }
    }

    impl Drop for SqliteShutdownGuard {
        fn drop(&mut self) {
            // Idempotent: if some explicit call already
            // consolidated (and called `mark_consumed`), the
            // drop is a no-op. Today no caller marks; the
            // hook is reserved for future code that wants
            // to consolidate at a known point and disable
            // the drop-time work.
            if self.consumed.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            if let Ok(guard) = self.reporter.lock()
                && let Some(ref r) = *guard
            {
                r.shutdown_consolidate();
            }
        }
    }

    impl SqliteReporter {
        /// Mid-session WAL flush. Merges currently-committed WAL
        /// frames into the main `.db` so concurrent read-only
        /// callers (live `nmbrs report`, `nmbrs replay`, ad-hoc
        /// `sqlite3 metrics.db` inspection) see the latest data
        /// without waiting for session end. Uses `PASSIVE` mode:
        /// non-blocking on writers (current write transactions
        /// continue without interruption) and doesn't truncate
        /// the WAL file — the next write reuses its buffer
        /// rather than re-extending it. Cheap to call often.
        ///
        /// Distinct from [`Self::consolidate_wal`] which uses
        /// `TRUNCATE` mode for session-end finalisation: that
        /// flavour pauses writers and zeroes the WAL file, and
        /// is appropriate only when no more writes are expected.
        pub fn passive_checkpoint(&self) {
            let _ = self
                .conn
                .query_row(
                    "PRAGMA wal_checkpoint(PASSIVE)",
                    rusqlite::params![],
                    |_row| Ok(()),
                )
                .map_err(|e| {
                    // Failure here is a soft event — the next
                    // checkpoint attempt will retry. Log at debug
                    // so we don't pollute the operator's stderr
                    // with periodic noise if (say) the disk is
                    // briefly full.
                    crate::diag::warn(&format!("sqlite passive checkpoint failed: {e}"));
                });
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::{Duration, Instant};

        #[test]
        fn readout_snapshot_round_trip_byte_equal() {
            let mut r = super::SqliteReporter::in_memory().unwrap();
            let ansi: &[u8] = "\x1b[34m[setup]\x1b[0m 100% \x1b[32m✓\x1b[0m".as_bytes();
            let plain = "[setup] 100% ✓";
            r.upsert_readout_snapshot(
                "on_phase_end",
                1,
                "phase",
                "setup#1",
                "phase_outcome",
                "labeled",
                1_000_000_000,
                Some(ansi),
                plain,
            );
            let rows = r.read_readout_snapshots();
            assert_eq!(rows.len(), 1);
            let row = &rows[0];
            assert_eq!(row.slot, "on_phase_end");
            assert_eq!(row.subject_kind, "phase");
            assert_eq!(row.subject_id, "setup#1");
            assert_eq!(row.readout_name, "phase_outcome");
            assert_eq!(row.lod, "labeled");
            assert_eq!(row.rendered_at, 1_000_000_000);
            assert_eq!(row.body_ansi.as_deref(), Some(ansi));
            assert_eq!(row.body_plain, plain);
        }

        #[test]
        fn workload_report_defs_surface_their_declaring_exec_id() {
            // SRD-88 — a workload's `report:` belongs to the execution
            // that declared it. `latest_execution_with_metadata_like`
            // must surface that execution's id so the report's data
            // query can be narrowed to it (workload-scoped) rather than
            // spanning every execution that shares the session.
            let mut r = super::SqliteReporter::in_memory().unwrap();
            // Two executions share one session; each declares a report.
            r.set_execution_metadata("s", 1, "report.early", "plot early");
            r.set_execution_metadata("s", 2, "report.late", "plot late");

            let (exec_id, rows) = latest_execution_with_metadata_like(&r.conn, "report.%");
            // The most recent execution's id, and ONLY its rows.
            assert_eq!(exec_id, Some(2));
            assert_eq!(
                rows,
                vec![("report.late".to_string(), "plot late".to_string())],
            );
        }

        #[test]
        fn legacy_session_metadata_reports_carry_no_exec_id() {
            // Pre-split dbs stored report defs in `session_metadata`
            // with no `exec_id`; the reader falls back and reports
            // `None` (a single-execution db — no scoping possible or
            // needed).
            let mut r = super::SqliteReporter::in_memory().unwrap();
            r.set_metadata("report.legacy", "plot legacy");
            let (exec_id, rows) = latest_execution_with_metadata_like(&r.conn, "report.%");
            assert_eq!(exec_id, None);
            assert_eq!(
                rows,
                vec![("report.legacy".to_string(), "plot legacy".to_string())],
            );
        }

        #[test]
        fn readout_snapshot_upsert_keeps_latest_per_pk() {
            let mut r = super::SqliteReporter::in_memory().unwrap();
            // Two upserts with the same primary key — second
            // wins (latest body, latest timestamp).
            r.upsert_readout_snapshot(
                "on_phase_end",
                1,
                "phase",
                "setup",
                "phase_outcome",
                "labeled",
                1_000,
                None,
                "first",
            );
            r.upsert_readout_snapshot(
                "on_phase_end",
                1,
                "phase",
                "setup",
                "phase_outcome",
                "labeled",
                2_000,
                None,
                "second",
            );
            let rows = r.read_readout_snapshots();
            assert_eq!(
                rows.len(),
                1,
                "PK collision should overwrite, not duplicate"
            );
            assert_eq!(rows[0].body_plain, "second");
            assert_eq!(rows[0].rendered_at, 2_000);
        }

        #[test]
        fn readout_snapshot_distinct_per_exec_id() {
            // SRD-100 §2.6 / §9 — two concurrent executions render the
            // same phase (identical slot/kind/subject/readout/lod). With
            // exec_id in the PK they must NOT collide: both renders
            // survive as distinct rows (pre-SRD-100 this upsert-collided,
            // silently losing one execution's render).
            let mut r = super::SqliteReporter::in_memory().unwrap();
            r.upsert_readout_snapshot(
                "on_phase_end",
                1,
                "phase",
                "setup",
                "phase_outcome",
                "labeled",
                1_000,
                None,
                "exec-1 render",
            );
            r.upsert_readout_snapshot(
                "on_phase_end",
                2,
                "phase",
                "setup",
                "phase_outcome",
                "labeled",
                1_000,
                None,
                "exec-2 render",
            );
            let rows = r.read_readout_snapshots();
            assert_eq!(rows.len(), 2, "distinct exec_id must not upsert-collide");
            let mut bodies: Vec<_> = rows
                .iter()
                .map(|x| (x.exec_id, x.body_plain.clone()))
                .collect();
            bodies.sort();
            assert_eq!(
                bodies,
                vec![
                    (1, "exec-1 render".to_string()),
                    (2, "exec-2 render".to_string()),
                ]
            );
        }

        // ── SRD-76 phase_outcomes / phase_errors ──────────────

        #[test]
        fn phase_outcome_round_trips_completed_with_no_errors() {
            let mut r = super::SqliteReporter::in_memory().unwrap();
            let row = super::PhaseOutcomeRow {
                session: "test-sess".into(),
                exec_id: 1,
                phase_name: "rampup".into(),
                phase_labels: "(profile=alpha)".into(),
                status: "completed".into(),
                duration_secs: 1.234,
                started_at_nanos: 1_000_000,
                ended_at_nanos: 1_001_234_000,
                phase_hash: None,
                reason_class: None,
                params_consumed: None,
                errors: Vec::new(),
            };
            r.write_phase_outcome(&row);
            let read = r.read_phase_outcomes(None);
            assert_eq!(read.len(), 1);
            assert_eq!(read[0].session, "test-sess");
            assert_eq!(read[0].exec_id, 1);
            assert_eq!(read[0].phase_name, "rampup");
            assert_eq!(read[0].status, "completed");
            assert_eq!(read[0].duration_secs, 1.234);
            assert!(read[0].errors.is_empty());
        }

        #[test]
        fn phase_outcome_failed_round_trips_errors_in_order() {
            let mut r = super::SqliteReporter::in_memory().unwrap();
            let row = super::PhaseOutcomeRow {
                session: "test-sess".into(),
                exec_id: 1,
                phase_name: "ensure_compacted".into(),
                phase_labels: "(k=10)".into(),
                status: "failed".into(),
                duration_secs: 14400.0,
                started_at_nanos: 0,
                ended_at_nanos: 14400 * 1_000_000_000,
                phase_hash: None,
                reason_class: None,
                params_consumed: None,
                errors: vec![
                    super::PhaseErrorRow {
                        class: "poll_timeout".into(),
                        message: "deadline reached".into(),
                        op_name: None,
                        cycle: None,
                        op_template: Some("SELECT ...".into()),
                        op_resolved: Some("SELECT * FROM ks.t".into()),
                        at_nanos: 100,
                        retryable: false,
                    },
                    super::PhaseErrorRow {
                        class: "BindError".into(),
                        message: "cycle 42 missing wire".into(),
                        op_name: Some("write".into()),
                        cycle: Some(42),
                        op_template: None,
                        op_resolved: None,
                        at_nanos: 200,
                        retryable: true,
                    },
                ],
            };
            r.write_phase_outcome(&row);
            let read = r
                .read_phase_outcome("test-sess", 1, "ensure_compacted", "(k=10)")
                .expect("outcome present");
            assert_eq!(read.status, "failed");
            assert_eq!(read.errors.len(), 2);
            assert_eq!(read.errors[0].class, "poll_timeout");
            assert_eq!(read.errors[1].class, "BindError");
            assert_eq!(read.errors[1].cycle, Some(42));
            assert!(read.errors[1].retryable);
        }

        #[test]
        fn phase_outcome_rewrite_replaces_error_list() {
            let mut r = super::SqliteReporter::in_memory().unwrap();
            let mut row = super::PhaseOutcomeRow {
                session: "s".into(),
                exec_id: 1,
                phase_name: "p".into(),
                phase_labels: String::new(),
                status: "failed".into(),
                duration_secs: 1.0,
                started_at_nanos: 0,
                ended_at_nanos: 1,
                phase_hash: None,
                reason_class: None,
                params_consumed: None,
                errors: vec![super::PhaseErrorRow {
                    class: "A".into(),
                    message: "m".into(),
                    op_name: None,
                    cycle: None,
                    op_template: None,
                    op_resolved: None,
                    at_nanos: 0,
                    retryable: false,
                }],
            };
            r.write_phase_outcome(&row);
            row.status = "completed".into();
            row.errors.clear();
            r.write_phase_outcome(&row);
            let read = r.read_phase_outcome("s", 1, "p", "").unwrap();
            assert_eq!(read.status, "completed");
            assert!(
                read.errors.is_empty(),
                "rewrite must wipe the prior error list"
            );
        }

        #[test]
        fn phase_outcome_distinct_exec_ids_keep_separate_rows() {
            // SRD-77 forward-looking: two executions of the same
            // phase identity (a `refine` re-run) must each get
            // their own row so the cardinal history survives.
            let mut r = super::SqliteReporter::in_memory().unwrap();
            for exec_id in [1, 2] {
                r.write_phase_outcome(&super::PhaseOutcomeRow {
                    session: "s".into(),
                    exec_id,
                    phase_name: "p".into(),
                    phase_labels: String::new(),
                    status: "completed".into(),
                    duration_secs: 1.0,
                    started_at_nanos: exec_id as i64 * 1_000,
                    ended_at_nanos: exec_id as i64 * 1_000 + 1,
                    phase_hash: None,
                    reason_class: None,
                    params_consumed: None,
                    errors: Vec::new(),
                });
            }
            let all = r.read_phase_outcomes(None);
            assert_eq!(all.len(), 2, "distinct exec_ids must produce distinct rows");
            assert_eq!(
                all.iter().map(|o| o.exec_id).collect::<Vec<_>>(),
                vec![1, 2]
            );
        }

        #[test]
        fn phase_outcome_read_missing_returns_none() {
            let r = super::SqliteReporter::in_memory().unwrap();
            assert!(r.read_phase_outcome("s", 1, "nope", "").is_none());
            assert!(r.read_phase_outcomes(None).is_empty());
        }

        #[test]
        fn readout_snapshot_distinct_pk_components_keep_separate_rows() {
            let mut r = super::SqliteReporter::in_memory().unwrap();
            // Same readout, different LOD → separate rows.
            r.upsert_readout_snapshot(
                "on_phase_end",
                1,
                "phase",
                "setup",
                "phase_outcome",
                "compact",
                1_000,
                None,
                "compact form",
            );
            r.upsert_readout_snapshot(
                "on_phase_end",
                1,
                "phase",
                "setup",
                "phase_outcome",
                "labeled",
                1_000,
                None,
                "labeled form",
            );
            // Same readout, same LOD, different subject → separate.
            r.upsert_readout_snapshot(
                "on_phase_end",
                1,
                "phase",
                "load",
                "phase_outcome",
                "labeled",
                1_000,
                None,
                "load form",
            );
            assert_eq!(r.read_readout_snapshots().len(), 3);
        }

        fn build_activity_row(activity: &str, gauges: &[(&str, f64)]) -> super::ActivityRow {
            super::ActivityRow {
                activity: activity.to_string(),
                cycles: 0,
                rate: 0.0,
                latency_p50_ns: None,
                latency_p99_ns: None,
                latency_mean_ns: None,
                gauges: gauges.iter().map(|(n, v)| (n.to_string(), *v)).collect(),
            }
        }

        #[test]
        fn aggregate_single_key_filter_mean_of_three_profiles() {
            // Three rows, each a different profile, with recall@10
            // values 0.91 / 0.92 / 0.93. Single-key filter on
            // profile~label keeps all three; mean = 0.92 exactly.
            let rows = vec![
                build_activity_row("profile=label_01", &[("recall_at_10", 0.91)]),
                build_activity_row("profile=label_02", &[("recall_at_10", 0.92)]),
                build_activity_row("profile=label_03", &[("recall_at_10", 0.93)]),
            ];
            let agg = ReportAggregate {
                function: "mean".into(),
                column_pattern: "recall_at_10".into(),
                label_key: "profile".into(),
                label_pattern: "label".into(),
                group_by: Vec::new(),
            };
            let result = compute_aggregates(&[agg], &rows, false, &["recall_at_10".to_string()]);
            assert_eq!(result.len(), 1);
            // Cells: [label, "-", "-", "0.9200"] (no latency)
            assert_eq!(result[0][3], "0.9200");
        }

        #[test]
        fn aggregate_multi_key_grouping_emits_row_per_tuple() {
            // Six rows across two (k, optimize_for) tuples × three
            // profiles. Multi-key grouping should emit two rows:
            //   k=10, optimize_for=RECALL: mean of (0.90, 0.92, 0.94) = 0.92
            //   k=10, optimize_for=LATENCY: mean of (0.70, 0.74, 0.78) = 0.74
            let rows = vec![
                build_activity_row(
                    "k=10, optimize_for=RECALL, profile=label_01",
                    &[("recall_at_10", 0.90)],
                ),
                build_activity_row(
                    "k=10, optimize_for=RECALL, profile=label_02",
                    &[("recall_at_10", 0.92)],
                ),
                build_activity_row(
                    "k=10, optimize_for=RECALL, profile=label_03",
                    &[("recall_at_10", 0.94)],
                ),
                build_activity_row(
                    "k=10, optimize_for=LATENCY, profile=label_01",
                    &[("recall_at_10", 0.70)],
                ),
                build_activity_row(
                    "k=10, optimize_for=LATENCY, profile=label_02",
                    &[("recall_at_10", 0.74)],
                ),
                build_activity_row(
                    "k=10, optimize_for=LATENCY, profile=label_03",
                    &[("recall_at_10", 0.78)],
                ),
            ];
            let agg = ReportAggregate {
                function: "mean".into(),
                column_pattern: "recall_at_10".into(),
                label_key: String::new(),
                label_pattern: String::new(),
                group_by: vec!["k".into(), "optimize_for".into()],
            };
            let result = compute_aggregates(&[agg], &rows, false, &["recall_at_10".to_string()]);
            assert_eq!(result.len(), 2);
            // BTreeMap orders alphabetically by tuple key —
            // "k=10, optimize_for=LATENCY" sorts before
            // "k=10, optimize_for=RECALL".
            assert_eq!(
                result[0][3], "0.7400",
                "LATENCY group mean (0.70+0.74+0.78)/3 ≠ 0.74"
            );
            assert_eq!(
                result[1][3], "0.9200",
                "RECALL group mean (0.90+0.92+0.94)/3 ≠ 0.92"
            );
        }

        #[test]
        fn aggregate_multi_key_min_picks_lowest_per_group() {
            let rows = vec![
                build_activity_row("k=10, opt=A", &[("g", 0.9)]),
                build_activity_row("k=10, opt=A", &[("g", 0.5)]),
                build_activity_row("k=20, opt=A", &[("g", 0.7)]),
                build_activity_row("k=20, opt=A", &[("g", 0.3)]),
            ];
            let agg = ReportAggregate {
                function: "min".into(),
                column_pattern: "g".into(),
                label_key: String::new(),
                label_pattern: String::new(),
                group_by: vec!["k".into()],
            };
            let result = compute_aggregates(&[agg], &rows, false, &["g".to_string()]);
            assert_eq!(result.len(), 2);
            assert_eq!(result[0][3], "0.5000", "k=10 min ≠ 0.5");
            assert_eq!(result[1][3], "0.3000", "k=20 min ≠ 0.3");
        }

        #[test]
        fn aggregate_multi_key_max_picks_highest_per_group() {
            let rows = vec![
                build_activity_row("k=10", &[("g", 0.5)]),
                build_activity_row("k=10", &[("g", 0.7)]),
                build_activity_row("k=20", &[("g", 0.6)]),
                build_activity_row("k=20", &[("g", 0.9)]),
            ];
            let agg = ReportAggregate {
                function: "max".into(),
                column_pattern: "g".into(),
                label_key: String::new(),
                label_pattern: String::new(),
                group_by: vec!["k".into()],
            };
            let result = compute_aggregates(&[agg], &rows, false, &["g".to_string()]);
            assert_eq!(result.len(), 2);
            assert_eq!(result[0][3], "0.7000", "k=10 max ≠ 0.7");
            assert_eq!(result[1][3], "0.9000", "k=20 max ≠ 0.9");
        }

        #[test]
        fn aggregate_multi_key_skips_rows_missing_group_label() {
            // Row missing the `optimize_for` label is excluded
            // from groups (rather than silently grouping it with
            // a different tuple).
            let rows = vec![
                build_activity_row("k=10, optimize_for=RECALL", &[("g", 0.9)]),
                build_activity_row(
                    "k=10", // missing optimize_for
                    &[("g", 0.5)],
                ),
            ];
            let agg = ReportAggregate {
                function: "mean".into(),
                column_pattern: "g".into(),
                label_key: String::new(),
                label_pattern: String::new(),
                group_by: vec!["k".into(), "optimize_for".into()],
            };
            let result = compute_aggregates(&[agg], &rows, false, &["g".to_string()]);
            assert_eq!(result.len(), 1, "row missing group label was excluded");
            assert_eq!(result[0][3], "0.9000");
        }

        #[test]
        fn sqlite_creates_schema() {
            let reporter = SqliteReporter::in_memory().unwrap();
            let count: i64 = reporter
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(count >= 7, "expected 7+ tables, got {count}");
        }

        /// Helper: tests use their own session/exec_id qualifier
        /// to satisfy the SRD-77 metric_instance FK. Pre-insert
        /// an executions row matching `(session_id, 1)` and
        /// return labels with those qualifiers attached. Tests
        /// can share the in-memory store across each other by
        /// picking distinct `session_id`s, but the more common
        /// pattern (one in_memory per test) keeps tests
        /// independent.
        fn test_session_setup(r: &mut SqliteReporter, session_id: &str) -> Labels {
            r.insert_execution_start(session_id, 1, "test", None, 0, "", "");
            Labels::of("session", session_id).with("exec_id", "1")
        }

        #[test]
        fn sqlite_inserts_counter() {
            let mut reporter = SqliteReporter::in_memory().unwrap();
            let qualifier = test_session_setup(&mut reporter, "test_inserts_counter");
            let mut snapshot = MetricSet::new(Duration::from_secs(1));
            snapshot.insert_counter(
                "ops_total",
                qualifier.with("activity", "write"),
                42,
                Instant::now(),
            );
            reporter.report(&snapshot);

            let count: i64 = reporter
                .conn
                .query_row("SELECT COUNT(*) FROM sample_value", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 1);
        }

        #[test]
        fn sqlite_inserts_timer() {
            let mut reporter = SqliteReporter::in_memory().unwrap();
            let qualifier = test_session_setup(&mut reporter, "test_inserts_timer");
            let mut h = hdrhistogram::Histogram::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
            for i in 1..=100 {
                let _ = h.record(i * 1_000_000);
            }

            let mut snapshot = MetricSet::new(Duration::from_secs(1));
            snapshot.insert_histogram(
                "latency",
                qualifier.with("activity", "read"),
                h,
                Instant::now(),
            );
            reporter.report(&snapshot);

            let p99: f64 = reporter
                .conn
                .query_row("SELECT p99 FROM sample_value", [], |row| row.get(0))
                .unwrap();
            assert!(p99 > 0.0, "p99 should be recorded");
        }

        #[test]
        fn sqlite_deduplicates_families() {
            let mut reporter = SqliteReporter::in_memory().unwrap();
            let qualifier = test_session_setup(&mut reporter, "test_dedupe_families");
            let mut snapshot = MetricSet::new(Duration::from_secs(1));
            snapshot.insert_counter(
                "ops",
                qualifier.clone().with("activity", "a"),
                1,
                Instant::now(),
            );
            snapshot.insert_counter("ops", qualifier.with("activity", "b"), 2, Instant::now());
            reporter.report(&snapshot);

            let families: i64 = reporter
                .conn
                .query_row("SELECT COUNT(*) FROM metric_family", [], |row| row.get(0))
                .unwrap();
            assert_eq!(families, 1, "same metric name should be one family");

            let instances: i64 = reporter
                .conn
                .query_row("SELECT COUNT(*) FROM metric_instance", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                instances, 2,
                "different labels should be different instances"
            );
        }

        /// Regression guard for SRD-40b §1 / SRD-40a §4.3.
        ///
        /// When a `MetricFamily` is declared with a unit, the unit
        /// MUST land in **two** surfaces — concatenated onto the
        /// family name as a `_<unit>` suffix per OpenMetrics §4.4
        /// **and** stored in the `metric_family.unit` column for
        /// structured access from the read side. Both surfaces
        /// derive from the single `with_unit` declaration so they
        /// cannot drift.
        ///
        /// This test asserts the round-trip through the sqlite
        /// reporter — the cited drift mode is a real regression risk
        /// (e.g. someone adding a code path that bypasses
        /// `with_unit` and hand-builds a family with `name="overscan"`
        /// and a separate unit string would break the invariant).
        #[test]
        fn unit_round_trips_into_name_suffix_and_unit_column() {
            use crate::snapshot::{GaugeValue, MetricPoint};
            let mut reporter = SqliteReporter::in_memory().unwrap();
            reporter.insert_execution_start("s", 1, "run", None, 0, "", "");
            let mut snapshot = MetricSet::new(Duration::from_secs(1));

            // Build a family with name="overscan" + unit="ratio" via
            // the canonical `with_unit` path. The single declaration
            // SHOULD produce both surfaces in sync — name becomes
            // `overscan_ratio` and the unit field carries `ratio`.
            let mut family = MetricFamily::new("overscan", MetricType::Gauge).with_unit("ratio");
            family.insert(Metric::single(
                Labels::of("activity", "search")
                    .with("session", "s")
                    .with("exec_id", "1"),
                MetricPoint::untimed(MetricValue::Gauge(GaugeValue::new(0.97))),
            ));
            snapshot.insert(family);
            reporter.report(&snapshot);

            // Both surfaces should be present and consistent.
            let row: (String, Option<String>) = reporter
                .conn
                .query_row(
                    "SELECT name, unit FROM metric_family WHERE type = 'gauge'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                row.0, "overscan_ratio",
                "OpenMetrics §4.4: unit MUST be a `_<unit>` suffix of family name"
            );
            assert_eq!(
                row.1.as_deref(),
                Some("ratio"),
                "SRD-40a §4.3: unit MUST also land in metric_family.unit column"
            );
        }

        /// Counterpart for the no-op case: when the caller's family
        /// name already carries the unit suffix, `with_unit` does
        /// not double-suffix, and the unit column is still
        /// populated.
        #[test]
        fn unit_column_populated_when_name_already_carries_suffix() {
            use crate::snapshot::{GaugeValue, MetricPoint};
            let mut reporter = SqliteReporter::in_memory().unwrap();
            reporter.insert_execution_start("s", 1, "run", None, 0, "", "");
            let mut snapshot = MetricSet::new(Duration::from_secs(1));

            let mut family =
                MetricFamily::new("memory_bytes", MetricType::Gauge).with_unit("bytes");
            family.insert(Metric::single(
                Labels::of("activity", "load")
                    .with("session", "s")
                    .with("exec_id", "1"),
                MetricPoint::untimed(MetricValue::Gauge(GaugeValue::new(1024.0))),
            ));
            snapshot.insert(family);
            reporter.report(&snapshot);

            let row: (String, Option<String>) = reporter
                .conn
                .query_row(
                    "SELECT name, unit FROM metric_family WHERE type = 'gauge'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(row.0, "memory_bytes", "no double suffixing");
            assert_eq!(row.1.as_deref(), Some("bytes"));
        }

        /// Counterpart for the no-unit case: families with no
        /// declared unit leave the column NULL. Guards the "unit
        /// is optional" half of the contract — adding the column
        /// shouldn't have introduced a forced default.
        #[test]
        fn unit_column_null_when_family_has_no_unit() {
            let mut reporter = SqliteReporter::in_memory().unwrap();
            reporter.insert_execution_start("s", 1, "run", None, 0, "", "");
            let mut snapshot = MetricSet::new(Duration::from_secs(1));
            snapshot.insert_counter(
                "ops_total",
                Labels::of("activity", "x")
                    .with("session", "s")
                    .with("exec_id", "1"),
                1,
                Instant::now(),
            );
            reporter.report(&snapshot);

            let unit: Option<String> = reporter
                .conn
                .query_row(
                    "SELECT unit FROM metric_family WHERE name = 'ops_total'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                unit.is_none(),
                "no `with_unit` declaration → unit column must be NULL"
            );
        }

        /// Visual test: prints a summary table to stderr so you can
        /// verify column alignment. Run with `--nocapture`.
        #[test]
        fn sqlite_summary_alignment() {
            let mut r = SqliteReporter::in_memory().unwrap();
            let now = Instant::now();
            let interval = Duration::from_secs(1);

            // Helper: insert a counter + timer for a phase
            let mut inject = |labels: Labels, cycles: u64, mean_ns: f64| {
                let mut h =
                    hdrhistogram::Histogram::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
                let _ = h.record(mean_ns as u64);
                let mut snapshot = MetricSet::new(interval);
                snapshot.insert_counter("cycles_total", labels.clone(), cycles, now);
                snapshot.insert_histogram("cycles_servicetime", labels.clone(), h, now);
                r.report(&snapshot);
            };

            let rampup = Labels::of("session", "test")
                .with("profile", "label_00")
                .with("phase", "rampup");
            inject(rampup, 82993, 146_000_000.0);

            let search_k10 = Labels::of("session", "test")
                .with("profile", "label_00")
                .with("k", "10")
                .with("phase", "search_pre_compaction");
            inject(search_k10, 100, 3_740_000.0);

            let search_k100_pre = Labels::of("session", "test")
                .with("profile", "label_00")
                .with("k", "100")
                .with("phase", "search_pre_compaction");
            inject(search_k100_pre, 100, 17_940_000.0);

            let await_idx = Labels::of("session", "test")
                .with("profile", "label_00")
                .with("phase", "await_index");
            inject(await_idx, 1, 550_000.0);

            let search_k10_post = Labels::of("session", "test")
                .with("profile", "label_00")
                .with("k", "10")
                .with("phase", "search_post_compaction");
            inject(search_k10_post, 100, 4_550_000.0);

            let search_k100_post = Labels::of("session", "test")
                .with("profile", "label_00")
                .with("k", "100")
                .with("phase", "search_post_compaction");
            inject(search_k100_post, 100, 17_580_000.0);

            // Gauges: recall for all search phases
            let mut gauges = MetricSet::new(interval);
            gauges.insert_gauge(
                "recall_at_10_mean",
                Labels::of("session", "test")
                    .with("profile", "label_00")
                    .with("k", "10")
                    .with("phase", "search_pre_compaction")
                    .with("n", "100"),
                0.8410,
                now,
            );
            gauges.insert_gauge(
                "recall_at_100_mean",
                Labels::of("session", "test")
                    .with("profile", "label_00")
                    .with("k", "100")
                    .with("phase", "search_pre_compaction")
                    .with("n", "100"),
                0.9837,
                now,
            );
            gauges.insert_gauge(
                "recall_at_10_mean",
                Labels::of("session", "test")
                    .with("profile", "label_00")
                    .with("k", "10")
                    .with("phase", "search_post_compaction")
                    .with("n", "100"),
                0.8410,
                now,
            );
            gauges.insert_gauge(
                "recall_at_100_mean",
                Labels::of("session", "test")
                    .with("profile", "label_00")
                    .with("k", "100")
                    .with("phase", "search_post_compaction")
                    .with("n", "100"),
                0.9837,
                now,
            );
            r.report(&gauges);

            eprintln!("--- summary output (all columns, no aggregates) ---");
            let config = ReportConfig {
                columns: vec![],
                row_filters: vec![],
                aggregates: vec![],
                show_details: true,
                exec_id_filter: None,
            };
            r.print_summary(&config);

            eprintln!("--- summary with aggregate ---");
            let config_agg = ReportConfig {
                columns: vec!["recall".into()],
                row_filters: vec![],
                aggregates: vec![ReportAggregate {
                    function: "mean".into(),
                    column_pattern: "recall".into(),
                    label_key: "profile".into(),
                    label_pattern: "label".into(),
                    group_by: Vec::new(),
                }],
                show_details: true,
                exec_id_filter: None,
            };
            r.print_summary(&config_agg);
            eprintln!("--- end ---");
        }

        // ── SRD-49: native OpenMetrics-type writer support ──

        #[test]
        fn write_native_sample_round_trips_histogram_with_le_buckets() {
            let mut r = SqliteReporter::in_memory().unwrap();
            let qualifier = test_session_setup(&mut r, "test_native_histogram");
            // Three bucket instances differing only on `le`.
            for le in ["0.1", "0.5", "+Inf"] {
                r.write_native_sample(
                    "request_latency",
                    "histogram",
                    &qualifier.clone().with("phase", "run").with("le", le),
                    &NativeSample {
                        interval_ms: 1000,
                        count: Some(42),
                        ..NativeSample::default()
                    },
                );
            }
            // Sibling _sum and _count families.
            r.write_native_sample(
                "request_latency_sum",
                "histogram",
                &qualifier.clone().with("phase", "run"),
                &NativeSample {
                    interval_ms: 1000,
                    sum: Some(123.4),
                    ..NativeSample::default()
                },
            );
            r.write_native_sample(
                "request_latency_count",
                "histogram",
                &qualifier.with("phase", "run"),
                &NativeSample {
                    interval_ms: 1000,
                    count: Some(100),
                    ..NativeSample::default()
                },
            );

            // Verify via raw SQL — the read-side test in
            // nmbrs-metricsql exercises the catalog adapter
            // separately.
            let n_families: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM metric_family WHERE type = 'histogram'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                n_families, 3,
                "expected 3 histogram families: bucket, sum, count"
            );

            let n_bucket_instances: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM metric_instance mi \
                 JOIN metric_family mf ON mf.id = mi.family_id \
                 WHERE mf.name = 'request_latency'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                n_bucket_instances, 3,
                "expected one instance per `le` boundary"
            );
        }

        #[test]
        fn write_native_sample_round_trips_info_type() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.write_native_sample(
                "build_info",
                "info",
                &Labels::of("version", "1.2.3").with("commit", "abc123"),
                &NativeSample {
                    interval_ms: 0,
                    count: Some(1),
                    ..NativeSample::default()
                },
            );
            let ty: String = r
                .conn
                .query_row(
                    "SELECT type FROM metric_family WHERE name = 'build_info'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(ty, "info");
        }

        #[test]
        fn write_native_sample_round_trips_stateset_type() {
            let mut r = SqliteReporter::in_memory().unwrap();
            let qualifier = test_session_setup(&mut r, "test_native_stateset");
            // Three states with active/inactive per-state samples.
            for (state, on) in [("alpha", 1.0), ("beta", 0.0), ("gamma", 1.0)] {
                r.write_native_sample(
                    "feature_flags",
                    "stateset",
                    &qualifier.clone().with("feature", state),
                    &NativeSample {
                        interval_ms: 0,
                        mean: Some(on),
                        ..NativeSample::default()
                    },
                );
            }
            let n: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM metric_instance mi \
                 JOIN metric_family mf ON mf.id = mi.family_id \
                 WHERE mf.name = 'feature_flags'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(n, 3, "one instance per state name");
        }

        #[test]
        fn write_native_sample_round_trips_gauge_histogram_type() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.write_native_sample(
                "queue_size_buckets",
                "gaugehistogram",
                &Labels::of("le", "10"),
                &NativeSample {
                    interval_ms: 1000,
                    count: Some(5),
                    ..NativeSample::default()
                },
            );
            let ty: String = r
                .conn
                .query_row(
                    "SELECT type FROM metric_family WHERE name = 'queue_size_buckets'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(ty, "gaugehistogram");
        }

        #[test]
        fn write_native_sample_round_trips_unknown_type() {
            let mut r = SqliteReporter::in_memory().unwrap();
            // OpenMetrics 'unknown' is reserved for
            // un-typed metrics; the writer accepts it.
            r.write_native_sample(
                "ad_hoc",
                "unknown",
                &Labels::of("source", "external"),
                &NativeSample {
                    interval_ms: 1000,
                    mean: Some(42.0),
                    ..NativeSample::default()
                },
            );
            let ty: String = r
                .conn
                .query_row(
                    "SELECT type FROM metric_family WHERE name = 'ad_hoc'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(ty, "unknown");
        }

        #[test]
        fn write_native_sample_dedupes_family_and_instance() {
            // Repeat writes against the same (family, type,
            // labels) reuse the cached family_id and
            // instance_id rather than creating duplicates.
            let mut r = SqliteReporter::in_memory().unwrap();
            let qualifier = test_session_setup(&mut r, "test_native_dedupes");
            for _ in 0..3 {
                r.write_native_sample(
                    "build_info",
                    "info",
                    &qualifier.clone().with("version", "1.2.3"),
                    &NativeSample {
                        interval_ms: 0,
                        count: Some(1),
                        ..NativeSample::default()
                    },
                );
            }
            let n_families: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM metric_family WHERE name = 'build_info'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let n_instances: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM metric_instance mi \
                 JOIN metric_family mf ON mf.id = mi.family_id \
                 WHERE mf.name = 'build_info'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let n_samples: i64 = r
                .conn
                .query_row("SELECT COUNT(*) FROM sample_value", [], |row| row.get(0))
                .unwrap();
            assert_eq!(n_families, 1);
            assert_eq!(n_instances, 1);
            assert_eq!(n_samples, 3, "three sample rows on the single instance");
        }

        #[test]
        fn instance_identity_is_order_independent() {
            // Regression: two code paths that construct the
            // same logical label set in different orders MUST
            // resolve to the same metric_instance.id and the
            // same `instance_label` row set. Otherwise we
            // double-count when summing across them.
            let mut r = SqliteReporter::in_memory().unwrap();
            use crate::scheduler::Reporter;
            use std::time::{Duration, Instant};
            let qualifier = test_session_setup(&mut r, "test_instance_identity");
            let labels_a = qualifier
                .clone()
                .with("phase", "ann_query")
                .with("k", "1")
                .with("optimize_for", "recall");
            let labels_b = qualifier
                .with("optimize_for", "recall")
                .with("k", "1")
                .with("phase", "ann_query");
            let mut snap = MetricSet::new(Duration::from_secs(1));
            snap.insert_counter("recall_mean", labels_a, 10, Instant::now());
            snap.insert_counter("recall_mean", labels_b, 5, Instant::now());
            r.report(&snap);

            let n_instances: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM metric_instance \
                 WHERE family_id = (SELECT id FROM metric_family WHERE name='recall_mean')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                n_instances, 1,
                "the same logical labels in two orders MUST resolve to one instance"
            );

            // Both code paths agree on the canonical spec.
            let spec: String = r
                .conn
                .query_row("SELECT spec FROM metric_instance", [], |row| row.get(0))
                .unwrap();
            // OpenMetrics canonical form: metric name as
            // prefix, `__name__` excluded from the labels
            // block, the rest sorted by key.
            // Session + exec_id labels show up in the canonical
            // spec alongside the test-supplied labels (sorted
            // alphabetically per OpenMetrics).
            assert_eq!(
                spec,
                r#"recall_mean{exec_id="1",k="1",optimize_for="recall",phase="ann_query",session="test_instance_identity"}"#
            );

            // `__name__` is stored alongside the other
            // labels so queries can filter on it uniformly.
            let n_name_rows: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM instance_label WHERE key='__name__'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(n_name_rows, 1);
        }

        // ── SRD-77 executions table ──────────────────────────

        #[test]
        fn execution_insert_then_update_completes_cardinal_history() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.insert_execution_start(
                "sess",
                1,
                "run",
                None,
                1_000_000_000,
                "phases: { schema: { ops: { noop: { op: x }}}}",
                "cycles=10\nconcurrency=4",
            );
            // ended_at_nanos / disposition land via the update.
            r.update_execution_end("sess", 1, 5_000_000_000, "SUCCESS");
            let rows = r.read_executions(None);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].session, "sess");
            assert_eq!(rows[0].exec_id, 1);
            assert_eq!(rows[0].verb, "run");
            assert_eq!(rows[0].scope, None);
            assert_eq!(rows[0].started_at_nanos, 1_000_000_000);
            assert_eq!(rows[0].ended_at_nanos, Some(5_000_000_000));
            assert_eq!(rows[0].disposition.as_deref(), Some("SUCCESS"));
            assert!(rows[0].workload_yaml_snapshot.contains("schema"));
            assert!(rows[0].cli_params_snapshot.contains("cycles=10"));
        }

        /// PK collision: a second insert with the same
        /// (session, exec_id) must NOT overwrite the prior row.
        /// This is the regression guard for the bug found
        /// earlier this session — `next_exec_id` was computed
        /// from `phase_outcomes` alone, missed exec_ids from
        /// skip-only refines, and collided on the executions
        /// PK.
        #[test]
        fn execution_insert_pk_collision_is_no_op_and_preserves_prior_row() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.insert_execution_start("sess", 1, "run", None, 1_000, "first", "");
            // Second insert with the same PK — should be
            // logged at WARN inside `insert_execution_start`
            // and produce no row mutation.
            r.insert_execution_start(
                "sess",
                1,
                "refine",
                Some("missing"),
                9_999,
                "DIFFERENT_PAYLOAD",
                "x=y",
            );
            let rows = r.read_executions(None);
            assert_eq!(rows.len(), 1, "collision must NOT have created a 2nd row");
            assert_eq!(
                rows[0].verb, "run",
                "prior row must remain intact (no overwrite)"
            );
            assert_eq!(
                rows[0].started_at_nanos, 1_000,
                "prior timestamp must remain intact"
            );
            assert_eq!(
                rows[0].workload_yaml_snapshot, "first",
                "prior workload yaml must remain intact"
            );
        }

        /// SRD-77 FK regression — a snapshot whose metrics reference a
        /// `(session, exec_id)` with **no** `executions` row must still
        /// commit. The deferred `metric_instance(session, exec_id) →
        /// executions` FK is checked at `Reporter::report`'s COMMIT; a
        /// metric can reach the reporter before/without its
        /// `insert_execution_start` (the scheduler's first ticks race
        /// session-open at runner.rs:680 vs :1850; concurrent SRD-88
        /// executions stagger; session-tier metrics carry `session` but
        /// no `exec_id` → exec_id 0, which never gets a start row). Before
        /// the FK-parent guard in `upsert_instance` this dropped the WHOLE
        /// snapshot (COMMIT → "FOREIGN KEY constraint failed" → ROLLBACK).
        #[test]
        fn snapshot_commits_when_execution_row_is_absent() {
            // (a)/(c)/(d): a concrete exec_id whose executions row was
            // never written (skipped/raced insert_execution_start).
            let mut r = SqliteReporter::in_memory().unwrap();
            let mut snap = MetricSet::new(Duration::from_secs(1));
            snap.insert_counter(
                "ops_total",
                Labels::of("session", "no_exec_row")
                    .with("exec_id", "7")
                    .with("activity", "write"),
                42,
                Instant::now(),
            );
            r.report(&snap);
            let samples: i64 = r
                .conn
                .query_row("SELECT COUNT(*) FROM sample_value", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                samples, 1,
                "metric for an execution with no start row MUST still commit \
                 (FK-parent placeholder), not be dropped by a failed COMMIT"
            );
            // A minimal placeholder executions row now backs the FK.
            let placeholder: (String, i64, String, i64) = r
                .conn
                .query_row(
                    "SELECT session, exec_id, verb, started_at_nanos FROM executions \
                 WHERE session = 'no_exec_row' AND exec_id = 7",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(placeholder, ("no_exec_row".into(), 7, "pending".into(), 0));

            // (b): a session-tier metric carrying `session` but NO
            // `exec_id` label — exec_id resolves to the sentinel 0.
            let mut r2 = SqliteReporter::in_memory().unwrap();
            let mut snap2 = MetricSet::new(Duration::from_secs(1));
            snap2.insert_gauge(
                "control_rate",
                Labels::of("session", "session_tier"),
                1.0,
                Instant::now(),
            );
            r2.report(&snap2);
            let samples2: i64 = r2
                .conn
                .query_row("SELECT COUNT(*) FROM sample_value", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                samples2, 1,
                "session-tier metric (exec_id 0) MUST still commit"
            );
        }

        /// The FK-parent placeholder is COMPLETED — not collided-with —
        /// when `insert_execution_start` arrives afterward (the raced
        /// ordering: metric first, start second). The real
        /// verb/started_at/snapshots overwrite the `pending`/0 placeholder,
        /// and no spurious duplicate warning is raised.
        #[test]
        fn insert_execution_start_completes_a_metric_written_placeholder() {
            let mut r = SqliteReporter::in_memory().unwrap();
            // Metric arrives first → creates placeholder (verb='pending').
            let mut snap = MetricSet::new(Duration::from_secs(1));
            snap.insert_counter(
                "ops_total",
                Labels::of("session", "raced").with("exec_id", "2"),
                5,
                Instant::now(),
            );
            r.report(&snap);
            // Now the real start record lands and completes the row.
            r.insert_execution_start("raced", 2, "run", None, 1_234, "yaml", "cli");
            let rows = r.read_executions(Some(2));
            assert_eq!(
                rows.len(),
                1,
                "placeholder must be completed in place, not duplicated"
            );
            assert_eq!(
                rows[0].verb, "run",
                "placeholder verb must be overwritten by the real one"
            );
            assert_eq!(rows[0].started_at_nanos, 1_234);
            assert_eq!(rows[0].workload_yaml_snapshot, "yaml");
            assert_eq!(rows[0].cli_params_snapshot, "cli");
        }

        /// `update_execution_end` is idempotent — calling it
        /// twice doesn't reopen the row or stamp a different
        /// disposition. The `WHERE ended_at_nanos IS NULL`
        /// clause is what enforces this.
        #[test]
        fn execution_update_end_is_idempotent() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.insert_execution_start("sess", 1, "run", None, 100, "", "");
            r.update_execution_end("sess", 1, 200, "SUCCESS");
            // Second update — different disposition / timestamp.
            // Must be a no-op because the row is already closed.
            r.update_execution_end("sess", 1, 300, "FAILURE");
            let rows = r.read_executions(None);
            assert_eq!(
                rows[0].ended_at_nanos,
                Some(200),
                "second update must NOT overwrite ended_at_nanos"
            );
            assert_eq!(
                rows[0].disposition.as_deref(),
                Some("SUCCESS"),
                "second update must NOT overwrite disposition"
            );
        }

        /// SRD-77 — `read_phase_outcomes(Some(n))` MUST filter at
        /// the SQL boundary, not in memory. Pins the "every read
        /// path is execution-qualified" invariant: a caller asking
        /// for exec_id=2 receives only exec_id=2 rows even when
        /// other execs exist.
        #[test]
        fn read_phase_outcomes_filters_by_exec_id() {
            let mut r = SqliteReporter::in_memory().unwrap();
            for (exec_id, name) in [(1, "alpha"), (2, "beta"), (3, "gamma")] {
                r.write_phase_outcome(&PhaseOutcomeRow {
                    session: "s".into(),
                    exec_id,
                    phase_name: name.into(),
                    phase_labels: String::new(),
                    status: "completed".into(),
                    duration_secs: 1.0,
                    started_at_nanos: 0,
                    ended_at_nanos: exec_id as i64,
                    phase_hash: None,
                    reason_class: None,
                    params_consumed: None,
                    errors: Vec::new(),
                });
            }
            let exec_2 = r.read_phase_outcomes(Some(2));
            assert_eq!(exec_2.len(), 1);
            assert_eq!(exec_2[0].exec_id, 2);
            assert_eq!(exec_2[0].phase_name, "beta");
        }

        /// `None` is the explicit aggregate-across-executions
        /// intent — every recorded outcome must surface.
        #[test]
        fn read_phase_outcomes_none_returns_every_row() {
            let mut r = SqliteReporter::in_memory().unwrap();
            for exec_id in [1, 2, 3] {
                r.write_phase_outcome(&PhaseOutcomeRow {
                    session: "s".into(),
                    exec_id,
                    phase_name: "p".into(),
                    phase_labels: String::new(),
                    status: "completed".into(),
                    duration_secs: 1.0,
                    started_at_nanos: 0,
                    ended_at_nanos: exec_id as i64,
                    phase_hash: None,
                    reason_class: None,
                    params_consumed: None,
                    errors: Vec::new(),
                });
            }
            let all = r.read_phase_outcomes(None);
            assert_eq!(
                all.len(),
                3,
                "None MUST aggregate across executions: {all:?}"
            );
            let ids: Vec<u64> = all.iter().map(|o| o.exec_id).collect();
            assert!(ids.contains(&1) && ids.contains(&2) && ids.contains(&3));
        }

        /// Same shape for `read_executions`. SQL-level filter,
        /// in-memory result MUST NOT include any row outside the
        /// qualifier.
        #[test]
        fn read_executions_filters_by_exec_id() {
            let mut r = SqliteReporter::in_memory().unwrap();
            for (exec_id, verb) in [(1, "run"), (2, "refine"), (3, "refine")] {
                r.insert_execution_start("s", exec_id, verb, None, exec_id as i64 * 100, "", "");
            }
            let exec_2 = r.read_executions(Some(2));
            assert_eq!(exec_2.len(), 1);
            assert_eq!(exec_2[0].exec_id, 2);
            assert_eq!(exec_2[0].verb, "refine");
        }

        /// `read_executions(None)` is the aggregate read; every
        /// execution row must surface in cardinal order.
        #[test]
        fn read_executions_none_returns_every_row_in_cardinal_order() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.insert_execution_start("s", 3, "refine", None, 300, "", "");
            r.insert_execution_start("s", 1, "run", None, 100, "", "");
            r.insert_execution_start("s", 2, "refine", None, 200, "", "");
            let all = r.read_executions(None);
            assert_eq!(all.len(), 3);
            // ORDER BY exec_id — insert order doesn't dictate output.
            let ids: Vec<u64> = all.iter().map(|e| e.exec_id).collect();
            assert_eq!(ids, vec![1, 2, 3]);
        }

        /// SRD-77 — `"latest"` is a CLI-side virtual qualifier
        /// that resolves to max(exec_id) at query-construction
        /// time. It MUST NEVER land in stored data — the
        /// metric_instance reserved-word guard refuses any write
        /// whose `session` or `exec_id` label literally equals
        /// `"latest"`. Without this guard, a buggy resolver
        /// could pollute storage with rows that can never be
        /// queried back consistently.
        #[test]
        fn metric_write_with_session_latest_label_is_rejected() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.insert_execution_start("real_sess", 1, "run", None, 0, "", "");
            let mut snapshot = MetricSet::new(Duration::from_secs(1));
            snapshot.insert_counter(
                "ops_total",
                Labels::of("session", "latest").with("exec_id", "1"),
                42,
                Instant::now(),
            );
            r.report(&snapshot);
            let n: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM metric_instance \
                 WHERE session = 'latest'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                n, 0,
                "metric_instance MUST refuse `session=\"latest\"`; \
                 the reserved word should never land in storage"
            );
        }

        #[test]
        fn metric_write_with_exec_id_latest_label_is_rejected() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.insert_execution_start("real_sess", 1, "run", None, 0, "", "");
            let mut snapshot = MetricSet::new(Duration::from_secs(1));
            snapshot.insert_counter(
                "ops_total",
                Labels::of("session", "real_sess").with("exec_id", "latest"),
                42,
                Instant::now(),
            );
            r.report(&snapshot);
            let n: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM metric_instance \
                 WHERE session = 'real_sess'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "metric_instance MUST refuse `exec_id=\"latest\"`");
        }

        /// SRD-77 — the metric_instance FK stays ENFORCED
        /// (`foreign_keys=ON`, FK not dropped): a metric_instance
        /// row can NEVER exist without an `executions` parent. But
        /// the "schema-enforced execution qualification" promise is
        /// kept by PROVISIONING the parent, not by dropping the
        /// metric: when a metric's `(session, exec_id)` labels don't
        /// match any executions row, `upsert_instance` writes a
        /// minimal `pending` placeholder so the deferred FK is
        /// satisfiable at COMMIT and the sample is captured rather
        /// than silently rolled back with the whole snapshot. (Before
        /// this guard the COMMIT raised "FOREIGN KEY constraint
        /// failed" and dropped every metric in the tick.)
        #[test]
        fn metric_write_without_start_row_provisions_fk_parent() {
            // NOTE: this test bypasses `in_memory()`'s bootstrap
            // helper deliberately — we open a raw connection,
            // run create_schema, and verify the FK-parent guard.
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
            let mut r = super::SqliteReporter::from_connection(conn).unwrap();
            r.create_schema().unwrap();
            // No executions row up front. The metric write must
            // provision a placeholder parent and commit the sample.
            let mut snapshot = MetricSet::new(Duration::from_secs(1));
            snapshot.insert_counter(
                "ops_total",
                Labels::of("session", "nonexistent").with("exec_id", "999"),
                42,
                Instant::now(),
            );
            r.report(&snapshot);
            let n_instances: i64 = r
                .conn
                .query_row("SELECT COUNT(*) FROM metric_instance", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                n_instances, 1,
                "metric_instance MUST be captured — the reporter \
                 provisions the FK parent rather than dropping the \
                 metric."
            );
            // The FK is intact: every metric_instance references a
            // present executions row (the placeholder just created).
            let orphans: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM metric_instance mi \
                 WHERE NOT EXISTS (SELECT 1 FROM executions e \
                     WHERE e.session = mi.session AND e.exec_id = mi.exec_id)",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                orphans, 0,
                "no metric_instance may reference a missing executions row"
            );
        }

        /// Mirror: when the executions row IS present, the
        /// metric write succeeds. Pins that the FK doesn't
        /// over-reject — well-formed metrics still land.
        #[test]
        fn metric_write_with_matching_executions_row_succeeds() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.insert_execution_start("real_sess", 1, "run", None, 0, "", "");
            let mut snapshot = MetricSet::new(Duration::from_secs(1));
            snapshot.insert_counter(
                "ops_total",
                Labels::of("session", "real_sess").with("exec_id", "1"),
                42,
                Instant::now(),
            );
            r.report(&snapshot);
            let n_instances: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM metric_instance \
                 WHERE session = 'real_sess' AND exec_id = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                n_instances, 1,
                "metric_instance MUST have one row when its \
                 (session, exec_id) references an existing \
                 executions row."
            );
        }

        /// `read_executions` returns rows ordered by
        /// `(session, exec_id)` so callers see them in cardinal
        /// sequence regardless of insertion order.
        #[test]
        fn execution_rows_are_returned_in_cardinal_order() {
            let mut r = SqliteReporter::in_memory().unwrap();
            // Insert out of order to verify the ORDER BY clause.
            r.insert_execution_start("sess", 3, "refine", Some("all"), 300, "", "");
            r.insert_execution_start("sess", 1, "run", None, 100, "", "");
            r.insert_execution_start("sess", 2, "refine", Some("missing"), 200, "", "");
            let rows = r.read_executions(None);
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0].exec_id, 1);
            assert_eq!(rows[1].exec_id, 2);
            assert_eq!(rows[2].exec_id, 3);
            assert_eq!(rows[1].scope.as_deref(), Some("missing"));
            assert_eq!(rows[2].scope.as_deref(), Some("all"));
        }

        #[test]
        fn read_indexes_are_deferred_until_shutdown() {
            // SRD-90: tables are created at write time, the read indexes are
            // deferred off the hot write path. The db sits at SCHEMA_VERSION
            // with no read indexes until `consolidate_wal` builds them at
            // shutdown — so the durable db an external (non-runtime) reader
            // opens is always fully indexed.
            let r = SqliteReporter::in_memory().unwrap();
            let idx_count = |r: &SqliteReporter| -> i64 {
                r.conn
                    .query_row(
                        "SELECT count(*) FROM sqlite_master \
                     WHERE type='index' AND name LIKE 'idx_%'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap()
            };
            let user_version = |r: &SqliteReporter| -> i64 {
                r.conn
                    .query_row("PRAGMA user_version", [], |row| row.get(0))
                    .unwrap()
            };
            // Write time: tables only.
            assert_eq!(user_version(&r), SqliteReporter::SCHEMA_VERSION);
            assert_eq!(idx_count(&r), 0, "read indexes deferred at write time");
            // Shutdown builds them and bumps the marker.
            r.consolidate_wal();
            assert_eq!(user_version(&r), SqliteReporter::INDEXED_VERSION);
            assert_eq!(idx_count(&r), 7, "all read indexes built at shutdown");
            // Idempotent + self-healing: a second call is a no-op.
            r.consolidate_wal();
            assert_eq!(idx_count(&r), 7);
        }

        // ── SRD-93 stages 2 + 4 — session clock + scope lifecycle ──

        fn scope_events(r: &SqliteReporter) -> Vec<(String, String, i64, i64)> {
            let mut stmt = r
                .conn
                .prepare(
                    "SELECT event, reason, at_utc_nanos, at_session_nanos \
                 FROM instance_scope_event ORDER BY event, at_utc_nanos",
                )
                .unwrap();
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })
                .unwrap();
            rows.flatten().collect()
        }

        fn labeled_snapshot(count: u64) -> MetricSet {
            let mut s = MetricSet::new(Duration::from_secs(1));
            s.insert_counter(
                "ops_total",
                Labels::of("session", "s").with("exec_id", "1"),
                count,
                Instant::now(),
            );
            s
        }

        /// SRD-93 M3/M4 — first storage sight writes the enter event;
        /// a ScopeClose-sealed batch writes the exit in the same
        /// transaction; re-sights and re-closes are idempotent (A7);
        /// and both temporal columns agree through the durable epoch
        /// (A5: `utc − session == session_epoch_utc_nanos`, exactly,
        /// for every event — one clock, two projections).
        #[test]
        fn scope_events_pair_enter_and_exit_with_dual_clocks() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.insert_execution_start("s", 1, "run", None, 0, "", "");

            r.report(&labeled_snapshot(1));
            let ev = scope_events(&r);
            assert_eq!(ev.len(), 1, "first sight = one enter event");
            assert_eq!(
                (ev[0].0.as_str(), ev[0].1.as_str()),
                ("enter", "first_sample")
            );

            let mut closing = labeled_snapshot(2);
            closing.mark_close(crate::snapshot::CloseReason::ScopeClose);
            r.report(&closing);
            let ev = scope_events(&r);
            assert_eq!(ev.len(), 2, "scope-close batch adds the exit");
            assert_eq!(
                (ev[1].0.as_str(), ev[1].1.as_str()),
                ("exit", "scope_close")
            );

            // Idempotent under a repeated close and a re-sight.
            r.report(&closing);
            r.report(&labeled_snapshot(3));
            assert_eq!(scope_events(&r).len(), 2);

            let epoch: i64 = r
                .conn
                .query_row(
                    "SELECT CAST(value AS INTEGER) FROM session_metadata \
                 WHERE key = 'session_epoch_utc_nanos'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            for (event, _, utc, session) in scope_events(&r) {
                assert_eq!(
                    utc - session,
                    epoch,
                    "{event}: utc − session must equal the durable epoch"
                );
            }
        }

        /// SRD-93 A6 — quiesce seals windows without ending scope:
        /// a Quiesce-sealed batch MUST NOT write exit events.
        #[test]
        fn quiesce_sealed_batch_writes_no_exit() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.insert_execution_start("s", 1, "run", None, 0, "", "");
            let mut quiescing = labeled_snapshot(1);
            quiescing.mark_close(crate::snapshot::CloseReason::Quiesce);
            r.report(&quiescing);
            let ev = scope_events(&r);
            assert_eq!(ev.len(), 1, "enter only");
            assert_eq!(ev[0].0, "enter");
        }

        /// SRD-93 M4 — the terminal sweep at clean shutdown pairs
        /// every still-open enter with exit('shutdown'), and leaves
        /// already-paired instances alone (their scope_close reason
        /// survives). A crash never reaches the sweep, so unpaired
        /// enters remain the truthful record — untestable here except
        /// by omission: no sweep call, no exit rows.
        #[test]
        fn terminal_sweep_pairs_only_the_unpaired() {
            let mut r = SqliteReporter::in_memory().unwrap();
            r.insert_execution_start("s", 1, "run", None, 0, "", "");

            // Instance A enters and exits mid-run via scope_close.
            let mut a = MetricSet::new(Duration::from_secs(1));
            a.insert_counter(
                "a_total",
                Labels::of("session", "s").with("exec_id", "1"),
                1,
                Instant::now(),
            );
            a.mark_close(crate::snapshot::CloseReason::ScopeClose);
            r.report(&a);
            // Instance B only enters.
            let mut b = MetricSet::new(Duration::from_secs(1));
            b.insert_counter(
                "b_total",
                Labels::of("session", "s").with("exec_id", "1"),
                1,
                Instant::now(),
            );
            r.report(&b);

            r.consolidate_wal();

            let by_reason = |reason: &str| -> i64 {
                r.conn
                    .query_row(
                        "SELECT COUNT(*) FROM instance_scope_event \
                     WHERE event = 'exit' AND reason = ?1",
                        [reason],
                        |row| row.get(0),
                    )
                    .unwrap()
            };
            assert_eq!(by_reason("scope_close"), 1, "A keeps its reason");
            assert_eq!(by_reason("shutdown"), 1, "B swept at shutdown");
            let unpaired: i64 = r
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM instance_scope_event e \
                 WHERE e.event = 'enter' AND NOT EXISTS ( \
                     SELECT 1 FROM instance_scope_event x \
                     WHERE x.instance_id = e.instance_id \
                       AND x.exec_id = e.exec_id AND x.event = 'exit')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(unpaired, 0, "clean shutdown pairs everything");
        }

        /// SRD-93 M2 — the durable epoch is INSERT-only: a reopen of
        /// the same db (resume/refine) keeps the original value, so
        /// session time never re-anchors mid-session.
        #[test]
        fn session_epoch_survives_reopen() {
            let dir = std::env::temp_dir().join(format!(
                "nmbrs-epoch-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let db = dir.join("metrics.db");

            let first = SqliteReporter::new(&db).unwrap().session_epoch_utc_nanos;
            std::thread::sleep(Duration::from_millis(10));
            let second = SqliteReporter::new(&db).unwrap().session_epoch_utc_nanos;
            assert_eq!(first, second, "the epoch must never move once set");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(feature = "sqlite")]
pub use inner::SqliteReporter;
pub use inner::SqliteShutdownGuard;
#[cfg(feature = "sqlite")]
pub use inner::{
    ExemplarRow, NativeSample, PhaseErrorRow, PhaseOutcomeRow, ReportAggregate, ReportConfig,
    latest_execution_metadata_like, latest_execution_metadata_value,
    latest_execution_with_metadata_like,
};

/// Split a summary name into `(basename, format)`.
///
/// Names without an extension default to Markdown:
///
/// - `recall`         → `("recall", "md")`
/// - `recallnmore`    → `("recallnmore", "md")`
///
/// Names with a recognized extension select the format from
/// the suffix:
///
/// - `recallnmore.csv` → `("recallnmore", "csv")`
/// - `recall.md`       → `("recall", "md")`
///
/// Output filenames combine the two as `{basename}_summary.{format}`,
/// so all three of the above produce filenames matching the
/// user's desired shape (`recall_summary.md`, etc.).
///
/// Unrecognized extensions fall through to Markdown — better to
/// produce something than to panic on an unknown suffix.
pub fn derive_name_and_format(name: &str) -> (String, String) {
    if let Some(idx) = name.rfind('.') {
        let suffix = &name[idx + 1..];
        if matches!(suffix, "md" | "csv") {
            return (name[..idx].to_string(), suffix.to_string());
        }
    }
    (name.to_string(), "md".to_string())
}
