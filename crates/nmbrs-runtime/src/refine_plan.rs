// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-77 refine: the per-execution skip plan.
//!
//! When `nmbrs refine` re-attaches to an existing session, it
//! reads the session's `phase_outcomes` table and builds a
//! [`RefinePlan`] — the set of (phase_name, phase_labels)
//! pairs that have already completed across any prior
//! execution, plus the next execution id to record outcomes
//! under.
//!
//! The plan rides on the executor context (`ExecCtx::refine_plan`)
//! and the phase-walk gate checks each phase against
//! [`is_completed`] before dispatching `run_phase`. Skipped
//! phases still get their scene-tree node pushed (so the TUI /
//! progress display shows them with a "skipped — prior outcome"
//! status) but no cycles run and no new outcome row is written.
//!
//! Scope (MVP): `--scope=missing` only — skip phases whose
//! exact identity already has a `completed` outcome row.
//! `--scope=changed` (hash compare) and `--scope=all` are
//! follow-up pushes. `--on-removed=` policies likewise deferred.

use std::collections::HashSet;
use std::path::Path;

/// Pre-computed skip set + next-execution id for one refine
/// invocation.
#[derive(Debug, Clone)]
pub struct RefinePlan {
    /// `(phase_name, phase_labels)` pairs that have at least
    /// one prior outcome with status `"completed"` across any
    /// execution of the session. The phase-walk gate checks
    /// each phase against this set before dispatching its
    /// per-cycle work.
    pub completed: HashSet<(String, String)>,
    /// Every `(phase_name, phase_labels)` pair that has ANY
    /// prior outcome (regardless of status). Used by the
    /// `--on-removed=` policy to detect phases that exist in
    /// the session's history but no longer appear in the
    /// freshly pre-mapped workload — those are candidates
    /// for the error / keep / drop decision.
    pub seen_identities: HashSet<(String, String)>,
    /// Prior `(name, labels) → provenance` for completed
    /// phases: the BASE hash (`phase_outcomes.phase_hash`) plus
    /// the SRD-107 consumed-params JSON. Used by the hash gates:
    /// at phase activation the executor computes the current
    /// base hash and param digests and compares via
    /// [`Self::unchanged_verdict`]. Legacy rows (either field
    /// NULL) always flag as changed, so the conservative
    /// behavior runs the phase rather than wrongly skipping it.
    pub completed_hashes: std::collections::HashMap<(String, String), PriorCompletion>,
    /// The execution id this refine invocation will record
    /// new outcomes under. One greater than the maximum
    /// `exec_id` observed in the prior `phase_outcomes` rows;
    /// at least `1` for sessions with no prior outcomes
    /// (degenerate, but supported).
    pub next_exec_id: u64,
    /// Total prior outcome rows examined. Surfaced in the
    /// startup log so the operator can sanity-check the
    /// session their refine attached to.
    pub prior_outcomes_seen: usize,
    /// SRD-77 scope mode: `Missing` (skip prior-completed),
    /// `Changed` (skip prior-completed AND prior_hash matches
    /// current_hash), or `All` (no skip — empty `completed`).
    /// Set by the runner from the `scope=` CLI param; the
    /// executor's phase walk consults it to decide which gate
    /// to apply.
    pub scope: RefineScope,
}

/// The chronologically-latest completed outcome's provenance
/// for one phase identity (SRD-77 base hash + SRD-107
/// consumed-params JSON).
#[derive(Debug, Clone, Default)]
pub struct PriorCompletion {
    pub phase_hash: Option<String>,
    pub params_consumed: Option<String>,
}

/// Why a phase may NOT skip under the refine hash gate
/// (SRD-107 Push 3) — surfaced in diagnostics so an operator
/// sees "re-running load_train: param 'dataset' changed"
/// instead of a bare hash mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipBlocker {
    /// No prior completed outcome carries comparable provenance
    /// (new phase, legacy row, or unreadable stored map).
    NoPrior,
    /// The base hash differs: an enclosing scope program or the
    /// phase's own declared config changed.
    BaseChanged,
    /// A consumed param's value changed (or the param is no
    /// longer present). Carries the param name.
    ParamChanged(String),
}

impl std::fmt::Display for SkipBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPrior => write!(f, "no comparable prior outcome"),
            Self::BaseChanged => write!(f, "scope or phase config changed"),
            Self::ParamChanged(name) => write!(f, "param '{name}' changed"),
        }
    }
}

/// SRD-77 `--scope=` modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineScope {
    /// Skip every phase identity already completed in any
    /// prior execution of this session. The default.
    Missing,
    /// Skip every phase identity whose `prior_hash ==
    /// current_hash` (program shape unchanged). Phases with
    /// no prior completion fall through; phases with prior
    /// completion but a different hash re-run.
    Changed,
    /// Run every phase. Prior outcomes are preserved as
    /// cardinal history under their original exec_id; the
    /// new run writes under the bumped exec_id.
    All,
}

/// SRD-77 — Every read-side path that touches session data is
/// **execution-qualified**: it accepts an [`ExecutionQualifier`]
/// at the call boundary, and the storage layer applies a
/// matching `exec_id` filter to its queries. The "aggregate
/// across every execution" intent is the explicit
/// [`ExecutionQualifier::All`] variant, not an unqualified
/// default — callers can never accidentally read across
/// multiple executions when they meant the latest.
///
/// Construct via:
/// - [`ExecutionQualifier::latest`] — resolves `max(exec_id)`
///   from the session db at call time; the natural
///   no-flag default for read commands.
/// - [`ExecutionQualifier::specific(n)`] — single execution
///   id (typically from a `--execution=<n>` CLI flag).
/// - [`ExecutionQualifier::all`] — every execution; the
///   `--all-executions` CLI flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionQualifier {
    /// One specific `exec_id`. The storage layer applies a
    /// `WHERE exec_id = <n>` filter.
    Specific(u64),
    /// Every recorded execution. The storage layer applies
    /// no `exec_id` filter — the "aggregate across all
    /// executions" semantic, opted into explicitly.
    All,
}

/// SRD-77 — the reserved CLI-side virtual qualifier that
/// means "the most recent execution recorded in the session
/// store". Resolvers translate this to a concrete exec_id at
/// query-construction time. **Must never appear in stored
/// data** — the metric_instance reserved-word guard refuses
/// any write carrying `session="latest"` or
/// `exec_id="latest"`.
pub const LATEST_LITERAL: &str = "latest";

/// SRD-77 — emit a per-command banner when the active session
/// has more than one execution in its history. Tells the
/// operator which execution the implicit `latest` default
/// resolved to and lists the latest three in temporal order
/// (newest first), with `<-- latest` marking the one their
/// query is currently bound to.
///
/// Silent for sessions with 0 or 1 executions — the banner is
/// only useful when an ambiguity actually exists.
///
/// Writes to stderr so it lands next to other operator-
/// visible logging without colliding with stdout pipelines
/// (a piped `nmbrs report ... | less` still gets clean stdout).
pub fn warn_multi_execution_default(session_dir: &std::path::Path) {
    let db_path = session_dir.join("metrics.db");
    if !db_path.exists() {
        return;
    }
    let conn = match rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_) => return,
    };
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
         WHERE type='table' AND name='executions')",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n != 0)
        .unwrap_or(false);
    if !exists {
        return;
    }
    let mut stmt = match conn.prepare(
        "SELECT exec_id, verb, scope, disposition, started_at_nanos \
         FROM executions ORDER BY exec_id DESC LIMIT 3",
    ) {
        Ok(s) => s,
        Err(_) => return,
    };
    // One execution row: (exec_id, verb, scope, disposition, started_at_nanos).
    type ExecRow = (i64, String, Option<String>, Option<String>, i64);
    let rows: Vec<ExecRow> = match stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)?,
        ))
    }) {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(_) => return,
    };
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM executions", [], |r| r.get(0))
        .unwrap_or(0);
    if total < 2 || rows.is_empty() {
        return;
    }
    eprintln!(
        "session has {total} execution(s); implicit qualifier `exec_id=latest` \
         resolved to exec_id={latest}. recent (newest first):",
        latest = rows[0].0,
    );
    for (i, (exec_id, verb, scope, disposition, _)) in rows.iter().enumerate() {
        let marker = if i == 0 { " <-- latest" } else { "" };
        let scope_part = scope
            .as_deref()
            .map(|s| format!(" scope={s}"))
            .unwrap_or_default();
        let disp_part = disposition
            .as_deref()
            .map(|d| format!(" {d}"))
            .unwrap_or_else(|| " (in-flight)".to_string());
        eprintln!("  exec_id={exec_id} verb={verb}{scope_part}{disp_part}{marker}");
    }
    eprintln!("  (pass `--execution=<n>` to target one, `--all-executions` to aggregate)");
}

impl ExecutionQualifier {
    /// Single execution id.
    pub fn specific(n: u64) -> Self {
        Self::Specific(n)
    }

    /// Aggregate across every execution.
    pub fn all() -> Self {
        Self::All
    }

    /// Resolve "the most recent execution" against the
    /// session db at `session_dir`. Returns
    /// [`Self::Specific(max_exec_id)`] when at least one
    /// execution is recorded, falling back to
    /// [`Self::Specific(1)`] when the db is empty / absent
    /// (so the qualifier still narrows to a specific id —
    /// the caller still gets the explicit qualification
    /// promise, just against an empty target).
    pub fn latest(session_dir: &std::path::Path) -> Self {
        match latest_exec_id_for_session(session_dir) {
            Some(n) => Self::Specific(n),
            None => Self::Specific(1),
        }
    }

    /// True iff this qualifier matches every recorded
    /// execution. Storage-layer query builders use this to
    /// decide whether to attach the `WHERE exec_id = …`
    /// clause.
    pub fn matches_all(&self) -> bool {
        matches!(self, Self::All)
    }

    /// The specific `exec_id` when narrowed, otherwise
    /// `None`. Storage-layer builders use this to bind the
    /// `WHERE exec_id = ?` parameter.
    pub fn specific_id(&self) -> Option<u64> {
        match self {
            Self::Specific(n) => Some(*n),
            Self::All => None,
        }
    }
}

/// Read the maximum `exec_id` recorded in the session's
/// `phase_outcomes` table — i.e. "which execution_id is the
/// most recent one in this session's history". Used by
/// [`ExecutionQualifier::latest`] to resolve the latest-
/// execution intent into a concrete id.
///
/// Returns `None` when:
/// - The session dir doesn't exist
/// - The sqlite file doesn't exist (no run captured yet)
/// - The `phase_outcomes` table is empty or absent
///
/// `O(1)` against the PK index — no full scan.
pub fn latest_exec_id_for_session(session_dir: &std::path::Path) -> Option<u64> {
    let db_path = session_dir.join("metrics.db");
    if !db_path.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let table_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
         WHERE type='table' AND name='phase_outcomes')",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n != 0)
        .unwrap_or(false);
    if !table_exists {
        return None;
    }
    let max: Option<i64> = conn
        .query_row("SELECT MAX(exec_id) FROM phase_outcomes", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .ok()
        .flatten();
    max.map(|v| v.max(0) as u64)
}

impl RefinePlan {
    /// `O(1)` check used by the executor's phase-walk gate.
    pub fn is_completed(&self, phase_name: &str, phase_labels: &str) -> bool {
        self.completed
            .contains(&(phase_name.to_string(), phase_labels.to_string()))
    }

    /// SRD-77 `--scope=changed` — true iff this phase has a
    /// prior completed outcome AND the prior outcome's
    /// `phase_hash` matches `current_hex`. False when:
    /// - No prior completion: phase is new → run.
    /// - Prior completion but hash differs: program shape
    ///   changed → re-run.
    /// - Prior completion with `phase_hash = NULL`: legacy
    ///   row from before the column was added → conservatively
    ///   re-run (we can't prove unchanged, so default to "do
    ///   the work").
    pub fn is_unchanged(
        &self,
        phase_name: &str,
        phase_labels: &str,
        current_hex: &str,
        current_params: &std::collections::HashMap<String, String>,
    ) -> bool {
        self.unchanged_verdict(phase_name, phase_labels, current_hex, current_params)
            .is_ok()
    }

    /// SRD-107 Push 3 — the three-way skip-validity check with a
    /// NAMED blocker on failure: base hash equal AND every stored
    /// consumed param's current value digests to its stored
    /// digest. Conservative on any gap (legacy rows, unreadable
    /// stored map): re-run rather than wrongly skip.
    pub fn unchanged_verdict(
        &self,
        phase_name: &str,
        phase_labels: &str,
        current_hex: &str,
        current_params: &std::collections::HashMap<String, String>,
    ) -> Result<(), SkipBlocker> {
        let key = (phase_name.to_string(), phase_labels.to_string());
        let Some(prior) = self.completed_hashes.get(&key) else {
            return Err(SkipBlocker::NoPrior);
        };
        match prior.phase_hash.as_deref() {
            None => return Err(SkipBlocker::NoPrior),
            Some(prior_hex) if prior_hex != current_hex => return Err(SkipBlocker::BaseChanged),
            Some(_) => {}
        }
        let Some(json) = prior.params_consumed.as_deref() else {
            // A base-matching row without the SRD-107 map should
            // not exist post-upgrade; treat as incomparable.
            return Err(SkipBlocker::NoPrior);
        };
        let Ok(stored) = serde_json::from_str::<std::collections::BTreeMap<String, String>>(json)
        else {
            return Err(SkipBlocker::NoPrior);
        };
        for (name, stored_digest) in stored {
            let current = current_params
                .get(&name)
                .map(|v| crate::checkpoint::params_scope::value_digest(v));
            if current.as_deref() != Some(stored_digest.as_str()) {
                return Err(SkipBlocker::ParamChanged(name));
            }
        }
        Ok(())
    }

    /// Should this phase be skipped per the plan's scope?
    /// Centralises the scope→gate dispatch so the executor
    /// walker stays a single conditional. The hash arg is
    /// only consulted under `Changed`; passing `""` is fine
    /// for `Missing` / `All` callers that don't know it yet.
    pub fn should_skip(
        &self,
        phase_name: &str,
        phase_labels: &str,
        current_hex: &str,
        current_params: &std::collections::HashMap<String, String>,
    ) -> bool {
        match self.scope {
            RefineScope::All => false,
            RefineScope::Missing => self.is_completed(phase_name, phase_labels),
            RefineScope::Changed => {
                self.is_unchanged(phase_name, phase_labels, current_hex, current_params)
            }
        }
    }

    /// Open the session directory's `metrics.db`, read every
    /// `phase_outcomes` row, and compute the skip plan.
    ///
    /// Returns `None` when:
    /// - The session directory doesn't exist
    /// - The sqlite file doesn't exist (no prior run captured outcomes)
    /// - The sqlite file exists but the `phase_outcomes` table is missing
    ///   (legacy session predating SRD-76)
    ///
    /// Sqlite errors mid-query log at WARN and produce an
    /// empty plan — refine falls back to "run everything" rather
    /// than failing the invocation, on the principle that a
    /// half-readable database shouldn't block the operator
    /// from making progress.
    pub fn load_from_session_dir(session_dir: &Path) -> Option<Self> {
        let db_path = session_dir.join("metrics.db");
        if !db_path.exists() {
            return None;
        }
        let conn = match rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(c) => c,
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "refine: failed to open {}: {e}",
                    db_path.display()
                );
                return None;
            }
        };
        // Verify the table exists before querying — a session
        // dir from before SRD-76 lands won't have it, and
        // `prepare` against a missing table errors with a
        // message that's noisier than "no plan available".
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='phase_outcomes')",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n != 0)
            .unwrap_or(false);
        if !exists {
            return None;
        }
        // SRD-107 legacy-read guard: the params_consumed column
        // may be absent on dbs never re-opened by a current
        // writer (this connection is read-only, so no migration
        // here); an absent column reads as NULL.
        let has_params_col: bool = conn
            .prepare("PRAGMA table_info(phase_outcomes)")
            .ok()
            .and_then(|mut s| {
                let mut found = false;
                let mut rows = s.query([]).ok()?;
                while let Ok(Some(r)) = rows.next() {
                    if r.get::<_, String>(1)
                        .map(|n| n == "params_consumed")
                        .unwrap_or(false)
                    {
                        found = true;
                    }
                }
                Some(found)
            })
            .unwrap_or(false);
        let pc_col = if has_params_col {
            "params_consumed"
        } else {
            "NULL"
        };
        let mut stmt = match conn.prepare(&format!(
            "SELECT exec_id, phase_name, phase_labels, status, phase_hash, \
                    {pc_col}, ended_at_nanos \
             FROM phase_outcomes \
             ORDER BY ended_at_nanos"
        )) {
            Ok(s) => s,
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "refine: failed to prepare query: {e}"
                );
                return None;
            }
        };
        let rows = match stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        }) {
            Ok(r) => r,
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "refine: failed to query phase_outcomes: {e}"
                );
                return None;
            }
        };
        let mut completed = HashSet::new();
        let mut seen_identities: HashSet<(String, String)> = HashSet::new();
        let mut completed_hashes: std::collections::HashMap<(String, String), PriorCompletion> =
            std::collections::HashMap::new();
        let mut max_exec_id: u64 = 0;
        let mut count: usize = 0;
        // Rows arrive ordered by `ended_at_nanos`, so the
        // chronologically latest completed outcome's hash wins
        // for a given (name, labels). This is what we want for
        // `scope=changed`: "did the LAST completed run match
        // what we'd compute now?"
        for row in rows.flatten() {
            let (exec_id, name, labels, status, phase_hash, params_consumed) = row;
            let exec_id = exec_id.max(0) as u64;
            if exec_id > max_exec_id {
                max_exec_id = exec_id;
            }
            count += 1;
            seen_identities.insert((name.clone(), labels.clone()));
            if status == "completed" {
                completed.insert((name.clone(), labels.clone()));
                completed_hashes.insert(
                    (name, labels),
                    PriorCompletion {
                        phase_hash,
                        params_consumed,
                    },
                );
            }
        }
        // SRD-77 — `next_exec_id` must consult the `executions`
        // table too. A prior refine that skipped every phase
        // writes ZERO phase_outcomes rows but DOES insert an
        // executions row, so phase_outcomes alone would miss
        // the bump and the next invocation would collide on the
        // executions PK. Take MAX across both sources.
        let executions_max: u64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='executions')",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n != 0)
            .ok()
            .filter(|exists| *exists)
            .and_then(|_| {
                conn.query_row("SELECT MAX(exec_id) FROM executions", [], |r| {
                    r.get::<_, Option<i64>>(0)
                })
                .ok()
                .flatten()
            })
            .map(|v| v.max(0) as u64)
            .unwrap_or(0);
        let max_exec_id = max_exec_id.max(executions_max);
        Some(Self {
            completed,
            seen_identities,
            completed_hashes,
            next_exec_id: max_exec_id + 1,
            prior_outcomes_seen: count,
            scope: RefineScope::Missing,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_db_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(RefinePlan::load_from_session_dir(tmp.path()).is_none());
    }

    #[test]
    fn db_without_phase_outcomes_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("metrics.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE other (x INTEGER)", []).unwrap();
        drop(conn);
        assert!(RefinePlan::load_from_session_dir(tmp.path()).is_none());
    }

    fn make_db_with_outcomes(dir: &Path, rows: &[(u64, &str, &str, &str)]) {
        let db_path = dir.join("metrics.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE phase_outcomes (
                session       TEXT    NOT NULL,
                exec_id       INTEGER NOT NULL,
                phase_name    TEXT    NOT NULL,
                phase_labels  TEXT    NOT NULL,
                status        TEXT    NOT NULL,
                duration_secs REAL    NOT NULL DEFAULT 0,
                started_at_nanos INTEGER NOT NULL DEFAULT 0,
                ended_at_nanos   INTEGER NOT NULL DEFAULT 0,
                phase_hash    TEXT,
                PRIMARY KEY (session, exec_id, phase_name, phase_labels)
            )",
            [],
        )
        .unwrap();
        for (exec, name, labels, status) in rows {
            conn.execute(
                "INSERT INTO phase_outcomes (session, exec_id, phase_name, phase_labels, status) \
                 VALUES ('s', ?1, ?2, ?3, ?4)",
                rusqlite::params![*exec as i64, name, labels, status],
            )
            .unwrap();
        }
    }

    #[test]
    fn completed_phases_populate_skip_set() {
        let tmp = tempfile::tempdir().unwrap();
        make_db_with_outcomes(
            tmp.path(),
            &[
                (1, "schema", "", "completed"),
                (1, "load_data", "k=10", "completed"),
                (1, "query", "k=10,limit=20", "failed"),
            ],
        );
        let plan = RefinePlan::load_from_session_dir(tmp.path()).unwrap();
        assert_eq!(plan.next_exec_id, 2);
        assert_eq!(plan.prior_outcomes_seen, 3);
        assert!(plan.is_completed("schema", ""));
        assert!(plan.is_completed("load_data", "k=10"));
        // failed phases must NOT be in the skip set — refine
        // should re-run them.
        assert!(!plan.is_completed("query", "k=10,limit=20"));
    }

    #[test]
    fn next_exec_id_bumps_past_max_prior() {
        let tmp = tempfile::tempdir().unwrap();
        make_db_with_outcomes(
            tmp.path(),
            &[
                (1, "schema", "", "completed"),
                (3, "query", "", "completed"),
                (2, "load", "", "completed"),
            ],
        );
        let plan = RefinePlan::load_from_session_dir(tmp.path()).unwrap();
        assert_eq!(plan.next_exec_id, 4);
    }

    #[test]
    fn empty_table_yields_exec_id_1() {
        let tmp = tempfile::tempdir().unwrap();
        make_db_with_outcomes(tmp.path(), &[]);
        let plan = RefinePlan::load_from_session_dir(tmp.path()).unwrap();
        assert_eq!(plan.next_exec_id, 1);
        assert_eq!(plan.prior_outcomes_seen, 0);
        assert!(plan.completed.is_empty());
    }

    // ── ExecutionQualifier ────────────────────────────────

    #[test]
    fn execution_qualifier_specific_carries_id() {
        let q = ExecutionQualifier::specific(7);
        assert_eq!(q.specific_id(), Some(7));
        assert!(!q.matches_all());
    }

    #[test]
    fn execution_qualifier_all_carries_no_id() {
        let q = ExecutionQualifier::all();
        assert_eq!(q.specific_id(), None);
        assert!(q.matches_all());
    }

    /// `latest()` resolves to `Specific(max_exec_id)` when the
    /// session db carries phase_outcomes — pins the "no
    /// implicit aggregation default" invariant: even the
    /// `latest` constructor narrows to one execution.
    #[test]
    fn execution_qualifier_latest_resolves_to_max_exec_id() {
        let tmp = tempfile::tempdir().unwrap();
        make_db_with_outcomes(
            tmp.path(),
            &[
                (1, "p1", "", "completed"),
                (3, "p2", "", "completed"),
                (2, "p3", "", "completed"),
            ],
        );
        let q = ExecutionQualifier::latest(tmp.path());
        assert_eq!(
            q.specific_id(),
            Some(3),
            "latest MUST resolve to max(exec_id)=3"
        );
        assert!(
            !q.matches_all(),
            "latest MUST narrow to a specific id, not aggregate"
        );
    }

    /// Empty db falls back to `Specific(1)` rather than
    /// silently degrading to aggregate. This is the
    /// "qualifier always narrows" promise: read paths can rely
    /// on a concrete exec_id even on a session with no prior
    /// runs.
    #[test]
    fn execution_qualifier_latest_on_empty_db_yields_specific_1() {
        let tmp = tempfile::tempdir().unwrap();
        let q = ExecutionQualifier::latest(tmp.path());
        assert_eq!(
            q.specific_id(),
            Some(1),
            "latest on empty db MUST yield Specific(1), not All"
        );
    }

    /// Db with no `phase_outcomes` table at all (legacy /
    /// pre-SRD-77 session) must STILL produce Specific(1) —
    /// callers can't drift into aggregate just because the
    /// table is missing.
    #[test]
    fn execution_qualifier_latest_on_missing_table_yields_specific_1() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("metrics.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE other (x INTEGER)", []).unwrap();
        drop(conn);
        let q = ExecutionQualifier::latest(tmp.path());
        assert_eq!(q.specific_id(), Some(1));
    }
}
