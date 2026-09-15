// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-83 stop-condition coverage across execution shell levels.
//!
//! Each test runs one scenario from
//! `nmbrs/examples/workloads/controls/stop_conditions_coverage.yaml` and asserts that
//! a stop condition trips when a time-evolving runtime-state wire
//! crosses its threshold:
//!
//! - **Phase shell** — `op_count` (`phase_op_count`) and `elapsed_ms`
//!   (`phase_elapsed_ms`): the activity drain loop re-snapshots these
//!   every ~5 ms, so a `> N` predicate fails the phase the moment the
//!   live value crosses it — well before the phase's 600 cycles.
//! - **Workload shell** — `children_done` (`workload_children`): the
//!   walker re-snapshots the child-phase aggregate after each finished
//!   phase, so `children_done >= 2` halts the remaining walk after two
//!   phases, skipping the third.
//!
//! A phase-level trip with the default `fail` effect fails the phase
//! (non-zero exit + reason on stderr); an explicit `effect: stop` is a
//! clean early halt (SRD-83 Part 5): the phase ends
//! Interrupted+Succeeded, keeps its phase `metrics:`, and the session
//! exits zero. The workload-level trip halts the walk gracefully (zero
//! exit, later phase never dispatched).
//!
//! The harness runs every scenario with cwd set to a throwaway sandbox
//! AND an explicit `--session-path` under it, so a test never writes
//! session state into the project root (per `feedback_tests_no_project_root`).

use std::path::{Path, PathBuf};
use std::process::Command;

const WORKLOAD: &str = "nmbrs/examples/workloads/controls/stop_conditions_coverage.yaml";

/// A throwaway session sandbox under the project tmpdir
/// (`target/test-tmp` via `.cargo/config.toml`), removed on drop.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nmbrs-stopcond-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Run one scenario in its own sandbox. The cwd IS the sandbox (never
/// the project root), the workload is referenced by absolute path, and
/// the session lands under the sandbox via `--session-path` — so
/// nothing touches the repo's `./sessions` (which a concurrently
/// running real session may own).
fn run_scenario(scenario: &str) -> (String, String, bool) {
    let sandbox = Sandbox::new(scenario);
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let workload = workspace_root.join(WORKLOAD);
    let session = sandbox.dir.join("session");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(&sandbox.dir)
        .arg("run")
        .arg(format!("workload={}", workload.display()))
        .arg(format!("scenario={scenario}"))
        .arg("tui=off")
        .arg("--session-path")
        .arg(&session);
    let out = cmd.output().expect("run nmbrs");
    // Evidence = stderr + session.log: `tui=off` claims the log-only
    // surface, so in-run diagnostics (e.g. the graceful "workload stop
    // condition tripped" line) land in session.log and are suppressed
    // from the console; failure-path reasons still print post-run.
    let session_log = std::fs::read_to_string(session.join("session.log")).unwrap_or_default();
    let mut evidence = String::from_utf8_lossy(&out.stderr).to_string();
    evidence.push('\n');
    evidence.push_str(&session_log);
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        evidence,
        out.status.success(),
    )
}

fn count_lines(stdout: &str, needle: &str) -> usize {
    stdout.lines().filter(|l| l.trim() == needle).count()
}

// ─── Phase shell — governance `timeout:` (SRD-83 C3 / GAP-12) ─────

/// `timeout: "150ms"` on a ~2.4 s phase: the synthesized guard trips
/// early, the phase ends Interrupted+Failed (`status = failed`) with
/// `reason_class = timeout` on its persisted row — the protocol
/// OUT-OF-RANGE disposition, structurally distinct from every other
/// failure — and the session exits non-zero.
#[test]
fn phase_timeout_is_out_of_range_not_generic_failure() {
    let sandbox = Sandbox::new("timeout");
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let workload = workspace_root.join(WORKLOAD);
    let session = sandbox.dir.join("session");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(&sandbox.dir)
        .arg("run")
        .arg(format!("workload={}", workload.display()))
        .arg("scenario=phase_timeout")
        .arg("tui=off")
        .arg("--session-path")
        .arg(&session);
    let out = cmd.output().expect("run nmbrs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let session_log = std::fs::read_to_string(session.join("session.log")).unwrap_or_default();
    let mut evidence = String::from_utf8_lossy(&out.stderr).to_string();
    evidence.push('\n');
    evidence.push_str(&session_log);

    assert!(
        !out.status.success(),
        "a governance timeout is a failed run (non-zero exit); evidence:\n{evidence}"
    );
    let ticks = count_lines(&stdout, "TIMEOUT_TICK");
    assert!(
        ticks > 0 && ticks < 400,
        "expected an early time-based cut, got {ticks} ops\nevidence:\n{evidence}"
    );
    assert!(
        evidence.contains("timeout=150ms") && evidence.contains("synthesized"),
        "the desugared guard must be announced; evidence:\n{evidence}"
    );

    let conn =
        rusqlite::Connection::open(session.join("metrics.db")).expect("open session metrics.db");
    let (status, reason_class): (String, Option<String>) = conn
        .query_row(
            "SELECT status, reason_class FROM phase_outcomes \
             WHERE phase_name = 'phase_timeout_trip'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("phase_timeout_trip outcome row");
    assert_eq!(
        status, "failed",
        "a governance timeout fails the phase (Interrupted+Failed)"
    );
    assert_eq!(
        reason_class.as_deref(),
        Some("timeout"),
        "the OUT-OF-RANGE disposition must be class-distinguishable"
    );
}

// ─── Phase shell — graceful `effect: stop` (SRD-83 Part 5) ────────

/// The same `cycles_total > 25` trip as `phase_cycles_total`, declared
/// with `effect: stop`: the condition is a budget, not a breach. The
/// phase must end Interrupted+Succeeded (`status = interrupted` on its
/// persisted outcome row), the checkpoint must record it COMPLETED
/// (never failed), its phase-level `metrics:` must be emitted (a
/// gracefully stopped phase is a valid measurement), and the session
/// must exit zero.
#[test]
fn phase_graceful_stop_exits_zero_and_keeps_metrics() {
    let sandbox = Sandbox::new("graceful");
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let workload = workspace_root.join(WORKLOAD);
    let session = sandbox.dir.join("session");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(&sandbox.dir)
        .arg("run")
        .arg(format!("workload={}", workload.display()))
        .arg("scenario=phase_graceful_stop")
        .arg("tui=off")
        .arg("--session-path")
        .arg(&session);
    let out = cmd.output().expect("run nmbrs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let session_log = std::fs::read_to_string(session.join("session.log")).unwrap_or_default();
    let mut evidence = String::from_utf8_lossy(&out.stderr).to_string();
    evidence.push('\n');
    evidence.push_str(&session_log);

    assert!(
        out.status.success(),
        "a graceful `effect: stop` trip must exit zero; evidence:\n{evidence}"
    );
    let ticks = count_lines(&stdout, "GRACEFUL_TICK");
    assert!(
        ticks > 25,
        "expected the phase to pass cycles_total=25 before stopping, got {ticks}"
    );
    assert!(
        ticks < 300,
        "expected an early stop, not ~600 ops; got {ticks}\nevidence:\n{evidence}"
    );
    assert!(
        evidence.contains("stopping phase"),
        "the graceful trip must log its 'stopping phase' line; evidence:\n{evidence}"
    );
    assert!(
        !evidence.contains("failing phase"),
        "a graceful trip must not take the failure path; evidence:\n{evidence}"
    );

    // Checkpoint: recorded as completed, never as failed.
    let checkpoint = std::fs::read_to_string(session.join("checkpoint.jsonl"))
        .expect("session checkpoint.jsonl");
    assert!(
        checkpoint.contains("phase_completed"),
        "checkpoint must record phase_completed; got:\n{checkpoint}"
    );
    assert!(
        !checkpoint.contains("phase_failed"),
        "checkpoint must not record phase_failed; got:\n{checkpoint}"
    );

    // Persisted outcome row: the SRD-83 Part 5 axes — Interrupted +
    // Succeeded — serialize as the stable label `interrupted`.
    let conn =
        rusqlite::Connection::open(session.join("metrics.db")).expect("open session metrics.db");
    let status: String = conn
        .query_row(
            "SELECT status FROM phase_outcomes WHERE phase_name = 'phase_graceful_trip'",
            [],
            |r| r.get(0),
        )
        .expect("phase_graceful_trip outcome row");
    assert_eq!(
        status, "interrupted",
        "a graceful stop must record Interrupted+Succeeded (label `interrupted`)"
    );

    // Phase `metrics:` emission survived the graceful stop.
    let families: i64 = conn
        .query_row(
            "SELECT count(*) FROM metric_family WHERE name LIKE 'graceful_phase_ms%'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    assert!(
        families > 0,
        "the phase metric `graceful_phase_ms` must be emitted on a graceful stop"
    );
}

// ─── Phase shell ──────────────────────────────────────────────────

/// `op_count > 25` over a 600-cycle phase trips at ~op 26, failing the
/// phase far short of 600. The op-count at stop equals the number of
/// `OP_TICK` lines emitted.
#[test]
fn phase_cycles_total_trips_and_fails() {
    let (stdout, stderr, ok) = run_scenario("phase_cycles_total");
    assert!(
        !ok,
        "a tripped phase stop condition must fail the run; stderr:\n{stderr}"
    );
    let ticks = count_lines(&stdout, "OP_TICK");
    // Crossed the threshold (> 25 ⇒ at least 26 ops ran) but stopped
    // far short of the 600-cycle ceiling. The loose upper bound
    // absorbs drain-loop timing slop without flaking.
    assert!(
        ticks > 25,
        "expected the phase to pass cycles_total=25 before stopping, got {ticks}"
    );
    assert!(
        ticks < 300,
        "expected an early stop, not ~600 ops; got {ticks}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("stop condition tripped"),
        "expected the stop-condition reason on stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("cycles_total > 25"),
        "expected the tripping predicate in the reason:\n{stderr}"
    );
}

/// `elapsed_ms > 150` trips on the phase wall clock, not an op count —
/// the trigger is time, so the phase stops after ~150 ms regardless of
/// how many ops that turned out to be.
#[test]
fn phase_elapsed_ms_trips_on_wall_clock() {
    let (stdout, stderr, ok) = run_scenario("phase_elapsed_ms");
    assert!(
        !ok,
        "a tripped phase stop condition must fail the run; stderr:\n{stderr}"
    );
    let ticks = count_lines(&stdout, "TIME_TICK");
    assert!(
        ticks > 0,
        "the phase should have run some ops before the 150ms stop"
    );
    assert!(
        ticks < 400,
        "expected an early time-based stop, got {ticks} ops\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("stop condition tripped"),
        "expected the stop-condition reason on stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("elapsed_ms > 150"),
        "expected the tripping predicate in the reason:\n{stderr}"
    );
}

// ─── Workload shell ───────────────────────────────────────────────

/// `children_done >= 2` (declared `each: workload`) halts the walk
/// after two phases complete: STEP_A and STEP_B run, STEP_C is never
/// dispatched, and — because nothing FAILED — the session exits zero.
#[test]
fn workload_children_done_halts_remaining_walk() {
    let (stdout, stderr, ok) = run_scenario("workload_children");
    assert!(
        ok,
        "a graceful workload-shell stop should exit zero; stderr:\n{stderr}"
    );
    assert_eq!(
        count_lines(&stdout, "STEP_A"),
        2,
        "step_a should run fully; stdout:\n{stdout}"
    );
    assert_eq!(
        count_lines(&stdout, "STEP_B"),
        2,
        "step_b should run fully; stdout:\n{stdout}"
    );
    assert_eq!(
        count_lines(&stdout, "STEP_C"),
        0,
        "step_c must be skipped once children_done reaches 2; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("workload stop condition tripped"),
        "expected the workload-shell stop log:\n{stderr}"
    );
    assert!(
        stderr.contains("children_done >= 2"),
        "expected the tripping predicate in the reason:\n{stderr}"
    );
}
