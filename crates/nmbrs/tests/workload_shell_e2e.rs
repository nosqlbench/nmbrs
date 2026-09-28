// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-83 — workload-shell stop-condition evaluation, end to end.
//!
//! The workload is SRD-82's outermost execution shell: as each child
//! phase finishes, the executor folds its outcome into the shell's
//! runtime-state aggregate (`children_*`, `op_count`, `error_count`)
//! and evaluates the workload's `stop_when:` predicates. The first
//! trip latches a walk-stop that every dispatch loop consults before
//! starting the next sibling — halting the remaining walk.
//!
//! These tests drive the `testkit` adapter (which echoes each op's
//! `stmt` to stdout) so the presence / absence of a phase's marker in
//! stdout is a direct witness of whether that phase ran.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

// ─── Harness ──────────────────────────────────────────────────────

fn nmbrs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .expect("workspace root");
    cmd.current_dir(workspace_root);
    cmd
}

fn write_workload(label: &str, body: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    // `.cargo/config.toml` redirects TMPDIR to `target/test-tmp/`,
    // which cargo doesn't create — make it on demand.
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create_dir_all {dir:?}: {e}"));
    dir.push(format!("nmbrs_wlshell_{label}_{}.yaml", std::process::id()));
    let mut f = std::fs::File::create(&dir).unwrap_or_else(|e| panic!("create {dir:?}: {e}"));
    f.write_all(body.as_bytes())
        .unwrap_or_else(|e| panic!("write {dir:?}: {e}"));
    dir
}

/// Run `nmbrs run workload=... <extra>` with a per-invocation
/// `--session-path` (so parallel tests don't race on the session dir)
/// and return (stdout, stderr, success).
fn run(workload: &Path, extra: &[&str]) -> (String, String, bool) {
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-wlshell-{}-{}-{}",
        scratch::seq(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&session_parent).expect("create session parent");
    let session_path = session_parent.join("session");
    let mut cmd = nmbrs();
    cmd.arg("run");
    cmd.arg(format!("workload={}", workload.display()));
    cmd.arg("tui=off");
    cmd.arg("--session-path");
    cmd.arg(&session_path);
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd.output().expect("failed to exec nmbrs");
    // Evidence = stderr + session.log: `tui=off` claims the log-only
    // surface, so in-run diagnostics (✓ outcome blocks, graceful
    // workload-stop lines) land in session.log and are deliberately
    // suppressed from the console.
    let session_log = std::fs::read_to_string(session_path.join("session.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&session_parent);
    let mut evidence = String::from_utf8_lossy(&out.stderr).to_string();
    evidence.push('\n');
    evidence.push_str(&session_log);
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        evidence,
        out.status.success(),
    )
}

// ─── Tests ────────────────────────────────────────────────────────

/// A workload-level `stop_when:` (`each: workload`) halts the
/// remaining walk after the condition trips on the aggregate.
///
/// `phase_one` runs 10 ops; at its completion the workload aggregate
/// `op_count` is 10, which trips `op_count > 5`, latching the
/// walk-stop. `phase_two` is then never dispatched — its marker is
/// absent from stdout. This is a *graceful* stop (no phase failed), so
/// the only thing that can have skipped `phase_two` is the workload
/// shell — the `Err`-cascade stop-on-error path can't, because nothing
/// errored.
#[test]
fn workload_stop_when_halts_later_phase() {
    let wl = write_workload(
        "opcount",
        r#"
stop_when:
  - when: "cycles_total > 5"
    each: workload
phases:
  phase_one:
    cycles: 10
    concurrency: 1
    ops:
      mark:
        stmt: "PHASE_ONE_OP"
  phase_two:
    cycles: 10
    concurrency: 1
    ops:
      mark:
        stmt: "PHASE_TWO_OP"
scenarios:
  default: [phase_one, phase_two]
"#,
    );
    let (stdout, stderr, _ok) = run(&wl, &["adapter=testkit"]);

    let p1 = stdout
        .lines()
        .filter(|l| l.trim() == "PHASE_ONE_OP")
        .count();
    let p2 = stdout
        .lines()
        .filter(|l| l.trim() == "PHASE_TWO_OP")
        .count();
    assert_eq!(
        p1, 10,
        "phase_one should have run all 10 ops; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        p2, 0,
        "phase_two must NOT run — the workload stop condition should have \
         halted the walk; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("workload stop condition tripped"),
        "expected the workload-shell stop log; stderr:\n{stderr}"
    );
}

/// Without a tripping `stop_when:`, every phase runs — the shell folds
/// outcomes into its aggregate but never latches the walk-stop.
#[test]
fn no_trip_runs_all_phases() {
    let wl = write_workload(
        "notrip",
        r#"
stop_when:
  - when: "cycles_total > 1000000"
    each: workload
phases:
  phase_one:
    cycles: 4
    concurrency: 1
    ops:
      mark:
        stmt: "PHASE_ONE_OP"
  phase_two:
    cycles: 4
    concurrency: 1
    ops:
      mark:
        stmt: "PHASE_TWO_OP"
scenarios:
  default: [phase_one, phase_two]
"#,
    );
    let (stdout, stderr, ok) = run(&wl, &["adapter=testkit"]);
    assert!(ok, "run should succeed; stderr:\n{stderr}");

    let p1 = stdout
        .lines()
        .filter(|l| l.trim() == "PHASE_ONE_OP")
        .count();
    let p2 = stdout
        .lines()
        .filter(|l| l.trim() == "PHASE_TWO_OP")
        .count();
    assert_eq!(p1, 4, "phase_one should run; stdout:\n{stdout}");
    assert_eq!(
        p2, 4,
        "phase_two should run — the condition never trips; stdout:\n{stdout}"
    );
    assert!(
        !stderr.contains("workload stop condition tripped"),
        "no stop should have fired; stderr:\n{stderr}"
    );
}

/// SRD-82 Part 4/6 — the scenario-graph default `*Failed:stop`. A phase
/// that FAILS halts the remaining walk: the workload shell's default
/// `children_failed > 0` rule trips with a `fail` effect (a fault, not a
/// graceful stop), so the run exits non-zero AND `after`'s marker is
/// absent. `testkit_throw_at(cycle, 0, ...)` panics in `boom`'s binding at cycle
/// 0, failing the phase. The skipped tail must not be reported as
/// "stranded" (it was deliberately halted), and the log must read as a
/// scenario stop-on-error (fault), distinct from the graceful
/// "workload stop condition tripped" wording.
#[test]
fn failed_phase_halts_walk_with_fault() {
    let wl = write_workload(
        "faultstop",
        r#"
phases:
  boom:
    cycles: 1
    concurrency: 1
    bindings: |
      x := testkit_throw_at(cycle, 0, "boom")
    ops:
      mark:
        stmt: "BOOM_OP x={x}"
  after:
    cycles: 1
    concurrency: 1
    ops:
      mark:
        stmt: "AFTER_OP"
scenarios:
  default: [boom, after]
"#,
    );
    let (stdout, stderr, ok) = run(&wl, &["adapter=testkit"]);

    assert!(
        !ok,
        "a failed phase must make the run exit non-zero (fault); \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let after = stdout.lines().filter(|l| l.trim() == "AFTER_OP").count();
    assert_eq!(
        after, 0,
        "after-phase must NOT run — the failed phase halts the walk; \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("scenario stop-on-error"),
        "expected the scenario stop-on-error (fault) log; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("pre-mapped phase(s) were not executed"),
        "the deliberately-skipped tail must not be reported as stranded; \
         stderr:\n{stderr}"
    );
}

/// SRD-82 Part 4 — cooperative abort of an ALREADY-IN-FLIGHT concurrent
/// sibling phase. With `schedule=*` the scenario runs `slow` and `boom`
/// concurrently; `boom` fails immediately (a fault), latching this
/// execution's walk-stop. `slow` polls that flag at its cooperative
/// boundaries and aborts mid-flight, running far fewer than its 20 ops
/// instead of draining to completion. Per-op `result-latency` keeps
/// `slow` in flight long enough that the fault lands while it runs. A
/// drained (un-aborted) `slow` would emit exactly 20 markers.
#[test]
fn inflight_concurrent_sibling_aborts_on_fault() {
    let wl = write_workload(
        "inflight",
        r#"
phases:
  slow:
    concurrency: 1
    cycles: 20
    ops:
      mark:
        result-latency: 150
        stmt: "SLOW_OP"
  boom:
    concurrency: 1
    cycles: 1
    bindings: |
      x := testkit_throw_at(cycle, 0, "boom")
    ops:
      mark:
        stmt: "BOOM_OP x={x}"
scenarios:
  default: [slow, boom]
"#,
    );
    let (stdout, stderr, ok) = run(&wl, &["adapter=testkit", "schedule=*"]);

    assert!(!ok, "the fault must fail the run; stderr:\n{stderr}");
    let slow = stdout.lines().filter(|l| l.trim() == "SLOW_OP").count();
    assert!(
        slow < 20,
        "the in-flight `slow` phase must abort mid-flight when the concurrent \
         `boom` phase faults — it ran {slow}/20 ops (20 = drained, no abort); \
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// SRD-82 Part 6 — a DAEMON phase runs CONCURRENTLY with its foreground
/// sibling (off the foreground budget) and is stopped cleanly when the
/// foreground completes. `work` runs 6 ops; `monitor` (daemon) is sized
/// at 1000 but is cut when `work` finishes — so it emits far fewer than
/// 1000 ticks AND more than zero (it ran during `work`), and the run
/// exits 0 (a daemon's clean stop is Interrupted+Succeeded, not a
/// failure). No `schedule=*` needed: daemons run off the foreground
/// budget by construction.
#[test]
fn daemon_phase_runs_concurrently_and_stops_with_foreground() {
    let wl = write_workload(
        "daemon",
        r#"
phases:
  work:
    concurrency: 1
    cycles: 6
    ops:
      mark:
        result-latency: 100
        stmt: "WORK_OP"
  monitor:
    daemon: true
    concurrency: 1
    cycles: 1000
    ops:
      mark:
        result-latency: 100
        stmt: "MONITOR_OP"
scenarios:
  default: [work, monitor]
"#,
    );
    let (stdout, stderr, ok) = run(&wl, &["adapter=testkit"]);
    assert!(
        ok,
        "a cleanly-stopped daemon must not fail the run; stderr:\n{stderr}"
    );
    let work = stdout.lines().filter(|l| l.trim() == "WORK_OP").count();
    let monitor = stdout.lines().filter(|l| l.trim() == "MONITOR_OP").count();
    assert_eq!(
        work, 6,
        "the foreground phase should complete; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        monitor > 0,
        "the daemon must run concurrently with the foreground (ran {monitor}); \
         stdout:\n{stdout}"
    );
    assert!(
        monitor < 1000,
        "the daemon must STOP when the foreground completes, not run its full \
         budget (ran {monitor}/1000); stdout:\n{stdout}"
    );
}

/// SRD-82 Part 6 regression — a daemon phase inside a `for:` loop must
/// NOT keep spawning for post-halt iterations. `work` fails on the first
/// iteration (a `testkit_throw_at` binding at cycle 0), latching the walk-stop;
/// the scenario shell's early `should_stop` guard then skips the
/// remaining iterations entirely, daemon included — so `MON i=3` (a
/// clearly post-halt iteration) never appears. (Without the guard the
/// comprehension kept iterating and each iteration re-spawned the
/// daemon.)
#[test]
fn daemon_does_not_spawn_after_walk_halt() {
    let wl = write_workload(
        "daemonhalt",
        r#"
phases:
  work:
    concurrency: 1
    cycles: 1
    bindings: |
      x := testkit_throw_at(cycle, 0, "boom")
    ops:
      mark:
        stmt: "WORK i={i} x={x}"
  mon:
    daemon: true
    concurrency: 1
    cycles: 1000
    ops:
      mark:
        result-latency: 50
        stmt: "MON i={i}"
scenarios:
  default:
    - for: "i in 0..4"
      phases: [mon, work]
"#,
    );
    let (stdout, stderr, ok) = run(&wl, &["adapter=testkit"]);
    assert!(
        !ok,
        "the failing foreground must fail the run; stderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("MON i=3"),
        "the daemon must NOT spawn for post-halt for-loop iterations \
         (found `MON i=3`); stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// SRD-82 Part 6 — a daemon phase whose ops ERROR bubbles up and fails
/// the run (failures stay in scope, as for an op-daemon).
#[test]
fn daemon_phase_failure_bubbles_up() {
    let wl = write_workload(
        "daemonfail",
        r#"
phases:
  work:
    concurrency: 1
    cycles: 3
    ops:
      mark:
        result-latency: 80
        stmt: "WORK_OP"
  monitor:
    daemon: true
    concurrency: 1
    cycles: 1
    bindings: |
      x := testkit_throw_at(cycle, 0, "daemon_boom")
    ops:
      mark:
        stmt: "MON_OP x={x}"
scenarios:
  default: [work, monitor]
"#,
    );
    let (_stdout, stderr, ok) = run(&wl, &["adapter=testkit"]);
    assert!(
        !ok,
        "a failing daemon phase must fail the run; stderr:\n{stderr}"
    );
}

/// Bare-name config scalars resolve scope-aware: `concurrency:
/// concurrency` reads the workload param the same way `batch: stride`
/// reads a scope const (regression: the bare form failed the phase —
/// invisibly — while braces worked).
#[test]
fn bare_name_concurrency_resolves() {
    let wl = write_workload(
        "bare_conc",
        r#"
params:
  concurrency: 3
scenarios:
  default:
    - for: "i in 1, 2"
      phases:
        - loader
phases:
  loader:
    adapter: testkit
    concurrency: concurrency
    cycles: 20
    ops:
      insert:
        stmt: "load i={i}"
"#,
    );
    let (_stdout, stderr, ok) = run(&wl, &[]);
    assert!(ok, "bare-name concurrency must resolve: {stderr}");
    let loads = stderr
        .lines()
        .filter(|l| l.contains("[loader]") && l.contains("✓"))
        .count();
    assert!(
        loads >= 2,
        "loader runs every iteration ({loads}): {stderr}"
    );
}

/// An early CONFIG failure (pre-activity: unresolvable concurrency,
/// bad cycles, missing kernel) must be loud and counted — regression:
/// it returned a bare failed Outcome with no observer event, no
/// scene-tree outcome, and no workload-shell fold, so the walk raced
/// through every remaining iteration and the reason surfaced once at
/// process exit.
#[test]
fn early_config_failure_is_loud_and_halts_the_walk() {
    let wl = write_workload(
        "bad_conc",
        r#"
scenarios:
  default:
    - for: "i in 1, 2, 3"
      phases:
        - loader
phases:
  loader:
    adapter: testkit
    concurrency: bogus_name
    cycles: 20
    ops:
      insert:
        stmt: "load i={i}"
"#,
    );
    let (_stdout, stderr, ok) = run(&wl, &[]);
    assert!(!ok, "invalid concurrency must fail the run: {stderr}");
    assert!(
        stderr.contains("failing phase (config)"),
        "mid-run ERR line expected: {stderr}"
    );
    assert!(
        stderr.contains("halting remaining walk"),
        "stop-on-error must trip on the config failure: {stderr}"
    );
    let tiers = stderr.lines().filter(|l| l.contains("· i=")).count();
    assert!(
        tiers <= 2,
        "the walk must halt at the first failed iteration          (live + log-tail replay = at most 2 mentions), saw {tiers}: {stderr}"
    );
}

mod scratch;
