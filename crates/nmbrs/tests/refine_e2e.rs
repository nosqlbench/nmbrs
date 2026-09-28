// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-77 `nmbrs refine` end-to-end tests.
//!
//! Each test:
//! 1. Spawns `nmbrs run` against a fixed `--session-path` to
//!    seed the session's `phase_outcomes` / `executions`.
//! 2. Spawns `nmbrs refine` (or a subsequent run/refine) against
//!    the same session path to layer a new execution.
//! 3. Reads back `executions` + `phase_outcomes` via sqlite to
//!    verify the cardinal-history invariants:
//!    - `exec_id` bumps per refine
//!    - `verb` / `scope` columns reflect the launching cmd
//!    - `disposition` lands after each session's shutdown
//!    - the refine skip-plan / scope=all / on_removed policies
//!      behave per spec

use std::path::{Path, PathBuf};
use std::process::Command;

fn nmbrs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.env("HOME", "/nonexistent");
    cmd
}

/// Sandbox holds a temp dir and the workload file path inside
/// it. Cleanup happens when the sandbox drops.
struct Sandbox {
    _parent: tempfile::TempDir,
    workload_path: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let parent = tempfile::tempdir().expect("create sandbox tmpdir");
        let workload_path = parent.path().join("wkl.yaml");
        Self {
            _parent: parent,
            workload_path,
        }
    }

    fn write_workload(&self, content: &str) {
        std::fs::write(&self.workload_path, content).expect("write workload yaml");
    }

    /// Spawn `nmbrs <verb> [args] workload=<path>` from inside
    /// the sandbox dir. Returns `(stdout, evidence, ok)` where
    /// `evidence` is stderr plus the latest session's
    /// `session.log` — `tui=off` claims the log-only surface, so
    /// in-run diagnostics (the `refine: skipping phase …` lines)
    /// land in session.log and are deliberately suppressed from
    /// the console. Working dir is the sandbox so
    /// `sessions/latest` resolves under the sandbox — this lets
    /// `nmbrs refine` (with no explicit session selector) attach
    /// to the most recent session here without contaminating the
    /// project tree.
    fn invoke(&self, verb: &str, extra: &[&str]) -> (String, String, bool) {
        let mut cmd = nmbrs();
        cmd.current_dir(self._parent.path());
        cmd.arg(verb);
        for a in extra {
            cmd.arg(a);
        }
        cmd.arg(format!("workload={}", self.workload_path.display()));
        cmd.arg("cycles=1");
        cmd.arg("tui=off");
        let output = cmd.output().expect("nmbrs spawn");
        let session_log = std::fs::read_to_string(
            self._parent
                .path()
                .join("sessions")
                .join("latest")
                .join("session.log"),
        )
        .unwrap_or_default();
        let mut evidence = String::from_utf8_lossy(&output.stderr).to_string();
        evidence.push('\n');
        evidence.push_str(&session_log);
        (
            String::from_utf8_lossy(&output.stdout).to_string(),
            evidence,
            output.status.success(),
        )
    }

    /// Resolve the most recent session's metrics.db under the
    /// sandbox's `logs/` dir. Follows `logs/latest` symlink.
    fn db_path(&self) -> PathBuf {
        self._parent
            .path()
            .join("sessions")
            .join("latest")
            .join("metrics.db")
    }
}

/// Read every `executions` row (verb, scope, disposition,
/// has_ended_timestamp) ordered by exec_id. Returns a Vec of
/// tuples for easy assertion shapes.
// (exec_id, verb, scope, disposition, has_ended_timestamp) rows.
#[allow(clippy::type_complexity)]
fn read_executions(db: &Path) -> Vec<(u64, String, Option<String>, Option<String>, bool)> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open sqlite");
    let mut stmt = conn
        .prepare(
            "SELECT exec_id, verb, scope, disposition, ended_at_nanos \
         FROM executions ORDER BY exec_id",
        )
        .expect("prepare executions");
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?.is_some(),
            ))
        })
        .expect("query");
    rows.filter_map(Result::ok).collect()
}

/// Read (exec_id, phase_name, status) tuples from
/// `phase_outcomes`, ordered.
fn read_outcomes(db: &Path) -> Vec<(u64, String, String)> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open sqlite");
    let mut stmt = conn
        .prepare(
            "SELECT exec_id, phase_name, status FROM phase_outcomes \
         ORDER BY exec_id, phase_name",
        )
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .expect("query");
    rows.filter_map(Result::ok).collect()
}

const WORKLOAD_TWO_PHASES: &str = r#"
bindings:
  cycle_val := cycle
phases:
  schema:
    ops:
      noop:
        op: "schema {cycle_val}"
  query:
    ops:
      noop:
        op: "query {cycle_val}"
"#;

const WORKLOAD_THREE_PHASES: &str = r#"
bindings:
  cycle_val := cycle
phases:
  schema:
    ops:
      noop:
        op: "schema {cycle_val}"
  query:
    ops:
      noop:
        op: "query {cycle_val}"
  verify:
    ops:
      noop:
        op: "verify {cycle_val}"
"#;

// ── Default refine semantics (scope=missing) ────────────

/// Bare `nmbrs refine` re-attaches to the prior session,
/// detects the prior phase outcomes, and skips every phase
/// that already completed. The executions table gains a row
/// with verb=refine, scope=missing.
#[test]
fn refine_default_skips_all_prior_completed_phases() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);

    let (_, stderr, ok) = sbx.invoke("run", &[]);
    assert!(ok, "initial run failed: {stderr}");

    let (_, stderr, ok) = sbx.invoke("refine", &[]);
    assert!(ok, "refine failed: {stderr}");
    assert!(
        stderr.contains("refine: skipping phase 'schema'")
            && stderr.contains("refine: skipping phase 'query'"),
        "refine MUST emit the skip log for every prior phase; \
         got stderr:\n{stderr}"
    );

    let executions = read_executions(&sbx.db_path());
    assert_eq!(executions.len(), 2, "executions table must have 2 rows");
    assert_eq!(executions[0].0, 1);
    assert_eq!(executions[0].1, "run");
    assert_eq!(executions[0].2, None, "run verb writes NULL scope");
    assert_eq!(executions[1].0, 2);
    assert_eq!(executions[1].1, "refine");
    assert_eq!(
        executions[1].2.as_deref(),
        Some("missing"),
        "refine default scope must be 'missing'"
    );

    // Every prior outcome row must still be readable under exec_id=1.
    // The skip-only refine must NOT have written new phase_outcomes
    // rows under exec_id=2.
    let outcomes = read_outcomes(&sbx.db_path());
    assert!(
        outcomes.iter().all(|(eid, _, _)| *eid == 1),
        "skip-only refine MUST NOT write any phase_outcomes rows; \
         got: {outcomes:?}"
    );
}

/// `refine` after the workload added a new phase: the prior 2
/// skip cleanly, the new 1 runs and writes outcomes under the
/// bumped exec_id.
#[test]
fn refine_runs_new_phase_skips_prior() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);

    sbx.write_workload(WORKLOAD_THREE_PHASES);
    let (_, stderr, ok) = sbx.invoke("refine", &[]);
    assert!(ok, "refine failed: {stderr}");

    let outcomes = read_outcomes(&sbx.db_path());
    // exec_id=1 rows: schema + query from the initial run
    // exec_id=2 row: verify from the refine
    let exec_2: Vec<&(u64, String, String)> =
        outcomes.iter().filter(|(eid, _, _)| *eid == 2).collect();
    assert_eq!(
        exec_2.len(),
        1,
        "only the new `verify` phase must write to exec_id=2; got: {outcomes:?}"
    );
    assert_eq!(exec_2[0].1, "verify");
    assert_eq!(exec_2[0].2, "completed");
}

// ── scope=all forces every phase to run ──────────────────

/// `scope=all` empties the skip set: every phase fires again,
/// new outcomes are recorded under the bumped exec_id, prior
/// outcomes remain as cardinal history under their exec_id.
#[test]
fn refine_scope_all_runs_every_phase_under_bumped_exec_id() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);

    let (_, stderr, ok) = sbx.invoke("refine", &["scope=all"]);
    assert!(ok, "refine scope=all failed: {stderr}");
    assert!(
        !stderr.contains("refine: skipping phase"),
        "scope=all MUST NOT skip any phase; got stderr:\n{stderr}"
    );

    let outcomes = read_outcomes(&sbx.db_path());
    let exec_1: Vec<_> = outcomes.iter().filter(|(eid, _, _)| *eid == 1).collect();
    let exec_2: Vec<_> = outcomes.iter().filter(|(eid, _, _)| *eid == 2).collect();
    assert_eq!(exec_1.len(), 2, "prior outcomes MUST be preserved");
    assert_eq!(exec_2.len(), 2, "every phase MUST write a new outcome row");

    let executions = read_executions(&sbx.db_path());
    let refine_row = executions
        .iter()
        .find(|(eid, _, _, _, _)| *eid == 2)
        .expect("refine execution row");
    assert_eq!(
        refine_row.2.as_deref(),
        Some("all"),
        "executions.scope must be 'all'"
    );
}

// ── on_removed policy (Never Ignore Silently) ────────────

/// Default policy (`on_removed=error`): when a refine workload
/// drops a phase that has prior outcomes, the refine REFUSES
/// to proceed. The error names the removed phase explicitly.
#[test]
fn refine_on_removed_error_default_refuses() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);

    // Workload now declares only `schema` — `query` was removed.
    sbx.write_workload(
        r#"
bindings:
  cycle_val := cycle
phases:
  schema:
    ops:
      noop:
        op: "schema {cycle_val}"
"#,
    );
    let (_, stderr, ok) = sbx.invoke("refine", &[]);
    assert!(
        !ok,
        "refine MUST refuse when workload drops a phase with prior outcomes"
    );
    assert!(
        stderr.contains("removes 1 phase") && stderr.contains("query"),
        "error must name the removed phase 'query'; got stderr:\n{stderr}"
    );
}

/// `on_removed=keep` accepts the workload trim — prior outcomes
/// remain (no work, no error). The remaining declared phase
/// still skips per scope=missing.
#[test]
fn refine_on_removed_keep_retains_prior_outcomes() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);

    sbx.write_workload(
        r#"
bindings:
  cycle_val := cycle
phases:
  schema:
    ops:
      noop:
        op: "schema {cycle_val}"
"#,
    );
    let (_, stderr, ok) = sbx.invoke("refine", &["on_removed=keep"]);
    assert!(ok, "on_removed=keep MUST succeed: {stderr}");

    let outcomes = read_outcomes(&sbx.db_path());
    assert_eq!(
        outcomes.len(),
        2,
        "prior outcomes MUST be preserved under on_removed=keep: {outcomes:?}"
    );
    assert!(
        outcomes.iter().any(|(_, n, _)| n == "query"),
        "removed phase's prior outcome MUST remain: {outcomes:?}"
    );
}

/// Unknown `on_removed=` policy value is rejected with a
/// clear error rather than silently falling through.
#[test]
fn refine_on_removed_unknown_value_is_rejected() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);

    sbx.write_workload(
        r#"
bindings:
  cycle_val := cycle
phases:
  schema:
    ops:
      noop:
        op: "schema {cycle_val}"
"#,
    );
    let (_, stderr, ok) = sbx.invoke("refine", &["on_removed=banana"]);
    assert!(!ok, "unknown on_removed value MUST be rejected");
    assert!(
        stderr.contains("unknown on_removed= policy") && stderr.contains("banana"),
        "error must name the bad value; got stderr:\n{stderr}"
    );
}

// ── exec_id bump across multiple refines ────────────────

/// Multi-refine sequence: a skip-only refine still bumps
/// exec_id (and the next refine sees the bump). This is the
/// regression guard for the bug where `next_exec_id` was
/// computed from `phase_outcomes` alone and missed the
/// skip-only refine's executions row.
#[test]
fn exec_id_bumps_correctly_across_skip_only_refine() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]); // exec 1
    sbx.invoke("refine", &[]); // exec 2 (skip only)
    sbx.invoke("refine", &["scope=all"]); // exec 3 (must NOT collide)

    let executions = read_executions(&sbx.db_path());
    assert_eq!(
        executions.len(),
        3,
        "three invocations must produce three executions rows: {executions:?}"
    );
    let exec_ids: Vec<u64> = executions.iter().map(|(e, _, _, _, _)| *e).collect();
    assert_eq!(
        exec_ids,
        vec![1, 2, 3],
        "exec_ids must be 1, 2, 3 — no collisions: {executions:?}"
    );

    // Every row must have ended_at_nanos populated (clean shutdown).
    for (e, _, _, _, has_end) in &executions {
        assert!(
            *has_end,
            "exec_id={e} must have ended_at_nanos populated after clean shutdown"
        );
    }
}

// ── scope=changed (hash-based skip) ─────────────────────

/// `scope=changed` with an untouched workload: every phase's
/// freshly-computed hash matches the prior outcome's hash, so
/// every phase skips. Cardinal-history rows under exec_id=1
/// stay intact; no new phase_outcomes rows under exec_id=2.
#[test]
fn refine_scope_changed_skips_when_workload_unchanged() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);

    let (_, stderr, ok) = sbx.invoke("refine", &["scope=changed"]);
    assert!(ok, "scope=changed failed: {stderr}");
    assert!(
        stderr.contains("prior completed outcome, hash unchanged"),
        "scope=changed MUST log the hash-match skip; got stderr:\n{stderr}"
    );

    // No new phase_outcomes rows from the unchanged refine.
    let outcomes = read_outcomes(&sbx.db_path());
    assert!(
        outcomes.iter().all(|(eid, _, _)| *eid == 1),
        "unchanged-workload scope=changed MUST NOT write new outcome rows; \
         got: {outcomes:?}"
    );
}

/// `scope=changed` with an edited workload: the hash changes,
/// so the phase re-runs and writes new outcomes under exec_id=2.
/// Prior outcomes preserved.
#[test]
fn refine_scope_changed_reruns_when_bindings_edit_changes_hash() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);

    // Edit the workload's binding — `cycle_val := cycle` →
    // `cycle_val := cycle * 2`. The program-tree shape changes,
    // so instance_hash changes for every phase that inherits.
    sbx.write_workload(
        r#"
bindings:
  cycle_val := cycle * 2
phases:
  schema:
    ops:
      noop:
        op: "schema {cycle_val}"
  query:
    ops:
      noop:
        op: "query {cycle_val}"
"#,
    );
    let (_, stderr, ok) = sbx.invoke("refine", &["scope=changed"]);
    assert!(ok, "scope=changed (edited) failed: {stderr}");
    assert!(
        !stderr.contains("scope=changed: prior hash matches"),
        "edited-workload scope=changed MUST NOT report hash-match; \
         got stderr:\n{stderr}"
    );

    let outcomes = read_outcomes(&sbx.db_path());
    let exec_2: Vec<_> = outcomes.iter().filter(|(eid, _, _)| *eid == 2).collect();
    assert_eq!(
        exec_2.len(),
        2,
        "edited-binding refine MUST write 2 new outcome rows; got: {outcomes:?}"
    );
    // Prior outcomes (exec_id=1) still present as cardinal history.
    let exec_1: Vec<_> = outcomes.iter().filter(|(eid, _, _)| *eid == 1).collect();
    assert_eq!(
        exec_1.len(),
        2,
        "prior outcomes MUST be preserved: {outcomes:?}"
    );
}

// ── nmbrs replay --execution / --all-executions ────────

/// Default `nmbrs replay` (no flag) qualifies to the most-
/// recent execution — operator's natural "show me the latest
/// refine" intent. With a 3-execution session, the bare replay
/// shows ONLY exec_id=3's outcomes. This is the SRD-77
/// "every read path is execution-qualified" invariant in
/// action — no aggregate-by-default leak.
#[test]
fn replay_default_qualifies_to_latest_execution() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]); // exec 1
    sbx.invoke("refine", &["scope=all"]); // exec 2
    sbx.invoke("refine", &["scope=all"]); // exec 3

    // Spawn replay with NO --execution flag (the implicit
    // latest-execution default).
    let mut cmd = nmbrs();
    cmd.current_dir(sbx._parent.path());
    cmd.args(["replay", "--plain"]);
    let output = cmd.output().expect("nmbrs replay spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(output.status.success(), "replay failed: {stdout}");

    // exec 3's outcome lines exist; exec 1 and exec 2 must
    // NOT — `replay` ran the latest-qualifier resolve at the
    // storage boundary, not after-the-fact in-memory filtering.
    let outcome_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("[schema]") || l.contains("[query]"))
        .collect();
    // Two phases × one execution = 2 outcome lines maximum.
    assert!(
        outcome_lines.len() <= 2,
        "replay default MUST narrow to one execution; saw {} outcome lines:\n{stdout}",
        outcome_lines.len()
    );
}

/// Explicit `--execution=1` reads only that execution's
/// outcomes, regardless of how many later refines added rows.
#[test]
fn replay_explicit_execution_filter_targets_one_exec_id() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]); // exec 1
    sbx.invoke("refine", &["scope=all"]); // exec 2

    let mut cmd = nmbrs();
    cmd.current_dir(sbx._parent.path());
    cmd.args(["replay", "--plain", "--execution", "1"]);
    let output = cmd.output().expect("nmbrs replay spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "replay --execution=1 failed: {stdout}"
    );
    // Both phases ran under exec 1 — should see both.
    assert!(stdout.contains("[schema]"));
    assert!(stdout.contains("[query]"));
}

/// `--all-executions` is the explicit aggregate intent. With
/// 2 executions × 2 phases, replay must render 4 outcome
/// lines.
#[test]
fn replay_all_executions_aggregates_across_every_execution() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);
    sbx.invoke("refine", &["scope=all"]);

    let mut cmd = nmbrs();
    cmd.current_dir(sbx._parent.path());
    cmd.args(["replay", "--plain", "--all-executions"]);
    let output = cmd.output().expect("nmbrs replay spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "replay --all-executions failed: {stdout}"
    );
    let outcome_lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("[schema]") || l.contains("[query]"))
        .collect();
    assert_eq!(
        outcome_lines.len(),
        4,
        "--all-executions MUST aggregate across every execution; \
         saw {} lines:\n{stdout}",
        outcome_lines.len()
    );
}

// ── Multi-execution disambiguation banner ──────────────

/// SRD-77 — when the implicit-latest-default kicks in AND the
/// session has more than one execution, read commands MUST
/// emit a banner so the operator sees which execution they're
/// looking at. Single-execution sessions stay silent (no
/// ambiguity).
#[test]
fn replay_default_emits_multi_exec_banner_when_more_than_one() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]); // exec 1
    sbx.invoke("refine", &["scope=all"]); // exec 2

    let mut cmd = nmbrs();
    cmd.current_dir(sbx._parent.path());
    cmd.args(["replay", "--plain"]);
    let output = cmd.output().expect("nmbrs replay spawn");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("session has 2 execution(s)")
            && stderr.contains("resolved to exec_id=2")
            && stderr.contains("<-- latest"),
        "banner MUST surface when N>1 and the default-latest \
         qualifier kicks in. stderr:\n{stderr}"
    );
    // The latest 3 listing in newest-first order — for a
    // session with 2 executions, both rows appear.
    assert!(stderr.contains("exec_id=2"));
    assert!(stderr.contains("exec_id=1"));
}

#[test]
fn replay_default_stays_silent_when_only_one_execution() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]); // single execution

    let mut cmd = nmbrs();
    cmd.current_dir(sbx._parent.path());
    cmd.args(["replay", "--plain"]);
    let output = cmd.output().expect("nmbrs replay spawn");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !stderr.contains("session has"),
        "banner MUST stay silent for single-execution sessions; \
         stderr:\n{stderr}"
    );
}

#[test]
fn replay_explicit_execution_does_not_emit_banner() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);
    sbx.invoke("refine", &["scope=all"]);

    let mut cmd = nmbrs();
    cmd.current_dir(sbx._parent.path());
    cmd.args(["replay", "--plain", "--execution", "1"]);
    let output = cmd.output().expect("nmbrs replay spawn");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    // Explicit --execution=N means the operator already knows
    // what they're targeting — the banner would be noise.
    assert!(
        !stderr.contains("session has 2 execution"),
        "banner MUST NOT fire under explicit --execution; \
         stderr:\n{stderr}"
    );
}

// ── nmbrs metrics commands honor the execution qualifier ──

/// `nmbrs metrics match <pattern>` qualifies its
/// metric_instance listing by the resolved exec_id. With two
/// executions in the session, the default-latest filter must
/// produce results that only mention exec_id=<latest>.
#[test]
fn metrics_match_defaults_to_latest_execution() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);
    sbx.invoke("refine", &["scope=all"]);

    let mut cmd = nmbrs();
    cmd.current_dir(sbx._parent.path());
    cmd.args(["metrics", "match", "cycles_total"]);
    let output = cmd.output().expect("spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "metrics match failed: stderr={stderr}"
    );
    // Every printed spec MUST carry exec_id="2" (latest), never exec_id="1".
    for line in stdout.lines().filter(|l| l.contains("cycles_total")) {
        assert!(
            line.contains(r#"exec_id="2""#),
            "default-latest qualifier should narrow to exec_id=2; got: {line}"
        );
        assert!(
            !line.contains(r#"exec_id="1""#),
            "default-latest MUST NOT include exec_id=1 rows: {line}"
        );
    }
    // Multi-exec banner surfaces on stderr.
    assert!(
        stderr.contains("session has 2 execution(s)"),
        "multi-exec banner MUST fire under default-latest; stderr:\n{stderr}"
    );
}

/// `--execution=<n>` narrows to one execution.
#[test]
fn metrics_match_explicit_execution_narrows() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);
    sbx.invoke("refine", &["scope=all"]);

    let mut cmd = nmbrs();
    cmd.current_dir(sbx._parent.path());
    cmd.args(["metrics", "match", "cycles_total", "--execution", "1"]);
    let output = cmd.output().expect("spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "metrics match --execution=1 failed: stderr={stderr}"
    );
    for line in stdout.lines().filter(|l| l.contains("cycles_total")) {
        assert!(
            line.contains(r#"exec_id="1""#),
            "--execution=1 MUST narrow to exec_id=1 rows: {line}"
        );
    }
    // Explicit selector means the operator already knows;
    // banner must stay silent.
    assert!(
        !stderr.contains("session has"),
        "banner MUST NOT fire under explicit --execution=N: {stderr}"
    );
}

/// `--all-executions` pulls every recorded execution.
#[test]
fn metrics_match_all_executions_pulls_every_exec_id() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);
    sbx.invoke("refine", &["scope=all"]);

    let mut cmd = nmbrs();
    cmd.current_dir(sbx._parent.path());
    cmd.args(["metrics", "match", "cycles_total", "--all-executions"]);
    let output = cmd.output().expect("spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "metrics match --all-executions failed"
    );
    let mut saw_1 = false;
    let mut saw_2 = false;
    for line in stdout.lines() {
        if line.contains(r#"exec_id="1""#) {
            saw_1 = true;
        }
        if line.contains(r#"exec_id="2""#) {
            saw_2 = true;
        }
    }
    assert!(
        saw_1 && saw_2,
        "--all-executions MUST surface both exec_id=1 AND exec_id=2 rows; \
         got saw_1={saw_1}, saw_2={saw_2}\n{stdout}"
    );
}

/// Every clean exit marks `disposition=SUCCESS` on the
/// executions row. Catches regressions in the
/// `update_execution_end` lifecycle hook.
#[test]
fn clean_exit_writes_success_disposition() {
    let sbx = Sandbox::new();
    sbx.write_workload(WORKLOAD_TWO_PHASES);
    sbx.invoke("run", &[]);

    let executions = read_executions(&sbx.db_path());
    assert_eq!(
        executions[0].3.as_deref(),
        Some("SUCCESS"),
        "clean run MUST mark disposition=SUCCESS: {executions:?}"
    );
}
