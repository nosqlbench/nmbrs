// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the op-composition wrapping subsystem's
//! behaviour under `dryrun=cycle`. These pin the load-bearing
//! invariants — that the DRYRUN wrapper installs outermost and
//! short-circuits the inner adapter + every inner wrapper —
//! by running the `nmbrs` binary end-to-end and observing the
//! externally-visible side effects.
//!
//! Each test compares dryrun-mode behaviour against a baseline
//! (same workload, no `dryrun=`) so the short-circuit signal is
//! unambiguous: the baseline produces an observable side effect
//! (stdout output, validation_failed, etc.); the dryrun run
//! does NOT.
//!
//! The test approach is intentionally black-box: we don't
//! introspect the dispenser chain or count wrap-factory calls;
//! we just verify the outward-facing contract that
//! `dryrun=cycle` short-circuits ALL inner work.
//!
//! Why black-box: the wrapping subsystem's correctness is
//! ultimately about what the inner adapter and the wrapper
//! side-effects DO (or don't) during execution. White-box
//! unit tests in `wrapper_registrations` already pin the
//! plan construction; these tests are the load-bearing
//! end-to-end checks.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn nmbrs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    // Prevent auto-discovery of a running web instance.
    cmd.env("HOME", "/nonexistent");
    cmd
}

struct SessionDir {
    parent: PathBuf,
}

impl SessionDir {
    fn new(tag: &str) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!("nmbrs-dryrun-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&parent).expect("create session parent");
        Self { parent }
    }

    fn session_path(&self) -> PathBuf {
        self.parent.join("session")
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.parent);
    }
}

fn write_workload(dir: &Path, content: &str) -> PathBuf {
    let path = dir.join("workload.yaml");
    let mut f = std::fs::File::create(&path).expect("create workload");
    f.write_all(content.as_bytes()).expect("write workload");
    path
}

/// Run nmbrs with the given workload params and capture (stdout, stderr, success).
fn run(args: &[&str], tag: &str) -> (String, String, bool) {
    let dir = SessionDir::new(tag);
    let session_path = dir.session_path();
    let output = nmbrs()
        .args(["run"].iter().chain(args.iter()))
        .arg("--session-path")
        .arg(&session_path)
        .output()
        .expect("failed to execute nmbrs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.success())
}

// =====================================================
// 1. Baseline vs dryrun for the inner adapter
// =====================================================

/// Baseline: an inline op with `cycles=3` writes 3 lines to
/// stdout via the default stdout adapter. Pins the "inner
/// adapter fires" observable so the dryrun test below has a
/// non-trivial signal to negate.
#[test]
fn baseline_inner_adapter_fires_and_writes_stdout() {
    let (stdout, stderr, ok) = run(&["op=test {{cycle}}", "cycles=3"], "baseline-fires");
    assert!(ok, "nmbrs run failed: stderr={stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec!["test 0", "test 1", "test 2"],
        "baseline must produce 3 stdout lines; got: {stdout:?}"
    );
}

/// Load-bearing: `dryrun=cycle` MUST short-circuit before the
/// inner adapter's execute fires. The signal: zero stdout output
/// even though the same workload as the baseline above produced
/// 3 lines. If even one line appears, the DRYRUN wrapper failed
/// to install, the cascade arm regressed, or the resolver dropped
/// DRYRUN from the plan.
#[test]
fn dryrun_cycle_short_circuits_inner_adapter() {
    let (stdout, stderr, ok) = run(
        &["op=test {{cycle}}", "cycles=3", "dryrun=cycle"],
        "dryrun-short-circuits",
    );
    assert!(ok, "nmbrs run failed in dryrun=cycle: stderr={stderr}");
    // The inner stdout adapter writes ONE line per cycle when it
    // executes; under dryrun=cycle it MUST NOT fire at all. Any
    // `test <n>` line means DRYRUN failed to short-circuit.
    for line in stdout.lines() {
        assert!(
            !line.starts_with("test "),
            "inner adapter fired in dryrun=cycle (saw `{line}`); \
             DRYRUN wrapper failed to short-circuit. \
             Full stdout: {stdout:?}"
        );
    }
}

/// SRD-77 `dryrun=dispenser`: every op template's dispenser
/// constructs (adapter `map_op` fires, wrapper plan resolves,
/// pull plan seals) but NO cycles run. Distinct from
/// `dryrun=phase` (which doesn't reach run_phase / map_op) and
/// `dryrun=op`/`cycle` (which run cycles with wrapper short-
/// circuit). Stdout MUST be empty (no `test <n>` lines) AND
/// stderr MUST surface the dispenser-construction confirmation
/// so the operator can tell the level apart.
#[test]
fn dryrun_dispenser_builds_dispensers_without_running_cycles() {
    let (stdout, stderr, ok) = run(
        &["op=test {{cycle}}", "cycles=3", "dryrun=dispenser"],
        "dryrun-dispenser",
    );
    assert!(ok, "nmbrs run failed in dryrun=dispenser: stderr={stderr}");
    // No per-cycle output — cycles never ran.
    for line in stdout.lines() {
        assert!(
            !line.starts_with("test "),
            "dryrun=dispenser MUST NOT fire any cycle; \
             saw stdout line `{line}`. Full stdout: {stdout:?}"
        );
    }
    // The construction-confirmation log line from
    // `Activity::run_with_adapters`'s
    // `stop_after_dispenser_init` exit. Operator-visible
    // signal that this level fired correctly.
    assert!(
        stderr.contains("dryrun=dispenser:")
            && stderr.contains("dispenser(s) constructed")
            && stderr.contains("stopping before cycle execution"),
        "dryrun=dispenser MUST log the construction-confirmation \
         line; stderr:\n{stderr}"
    );
}

/// Symmetric check vs `dryrun=phase`: at phase depth NO
/// dispenser is built, so the construction-confirmation log
/// MUST NOT fire. Catches a regression where the gate at
/// `executor.rs`'s structural walker incorrectly lets Phase
/// depth reach `run_phase`.
#[test]
fn dryrun_phase_does_not_build_dispensers() {
    let (_, stderr, ok) = run(
        &["op=test {{cycle}}", "cycles=3", "dryrun=phase"],
        "dryrun-phase-no-dispenser",
    );
    assert!(ok, "nmbrs run failed in dryrun=phase: stderr={stderr}");
    assert!(
        !stderr.contains("dryrun=dispenser:"),
        "dryrun=phase MUST NOT trigger the dispenser-build log; \
         stderr:\n{stderr}"
    );
}

// =====================================================
// 2. Inner wrappers (verify) short-circuit too
// =====================================================

/// A workload op declaring `verify:` with a predicate that
/// CANNOT possibly pass (asserting a body field that doesn't
/// exist) will produce a `validation_failed` error in normal
/// mode — that's the baseline. Under `dryrun=cycle`, the
/// DRYRUN wrapper sits OUTSIDE the validate wrapper, so the
/// short-circuit fires before validate's assertion check ever
/// runs. No validation_failed error.
///
/// This test is the original failure mode that motivated the
/// dryrun-as-outermost-wrapper redesign — verify clauses
/// were firing against the dryrun stand-in's empty body and
/// producing spurious assertion failures.
#[test]
fn dryrun_cycle_skips_failing_verify_clause() {
    let dir = SessionDir::new("dryrun-skips-verify");
    let workload = write_workload(
        &dir.parent,
        r#"
phases:
  always_fail:
    cycles: 1
    concurrency: 1
    ops:
      will_fail_in_normal_mode:
        stmt: "noop"
        verify:
          - field: nonexistent_field_that_should_fail
            eq: "expected_value"
        strict: true
"#,
    );

    let output = nmbrs()
        .args(["run"])
        .arg(format!("workload={}", workload.display()))
        .arg("dryrun=cycle")
        .arg("--session-path")
        .arg(dir.session_path())
        .output()
        .expect("nmbrs run");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Under dryrun=cycle, the verify wrapper sits inside the
    // DRYRUN short-circuit and MUST NOT fire. A
    // `validation_failed` error in the output signals the
    // composition is wrong.
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        !combined.contains("validation_failed"),
        "validate wrapper fired in dryrun=cycle — DRYRUN failed \
         to short-circuit. Combined output:\n{combined}"
    );
}

// =====================================================
// 3. Memo + dryrun coexistence (memo-vs-dryrun ordering bug regression)
// =====================================================

/// A workload op declaring BOTH `memo:` and (implicitly via
/// the session flag) `dryrun:` must resolve cleanly — memo
/// sits inside dryrun in the default ordering. If the topo-
/// sort tiebreak placed dryrun inside memo, the resolver
/// would reject the op at phase-init with
/// "wrapper `memo` was placed outside `dryrun`...".
///
/// This is a regression test for the bug surfaced when the
/// DRYRUN registration's `forbids_outer = […, memo]` first
/// landed without a corresponding `memo` entry in
/// `DEFAULT_ORDER`.
#[test]
fn dryrun_cycle_with_memo_coexists_no_constraint_error() {
    let dir = SessionDir::new("dryrun-memo");
    let workload = write_workload(
        &dir.parent,
        r#"
phases:
  with_memo:
    cycles: 1
    concurrency: 1
    ops:
      memoized_op:
        stmt: "noop"
        memo:
          before: "starting memoized_op"
          after:  "finished memoized_op"
"#,
    );

    let output = nmbrs()
        .args(["run"])
        .arg(format!("workload={}", workload.display()))
        .arg("dryrun=cycle")
        .arg("--session-path")
        .arg(dir.session_path())
        .output()
        .expect("nmbrs run");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{stdout}\n{stderr}");

    assert!(
        output.status.success(),
        "nmbrs failed with memo+dryrun: combined:\n{combined}"
    );
    // The specific error the ordering bug produced; if any
    // future regression places dryrun inside memo this assert
    // catches it.
    assert!(
        !combined.contains("was placed outside `dryrun`"),
        "wrapper resolver rejected memo+dryrun composition — \
         the default-order tiebreaker likely lost `memo`'s \
         entry. Combined output:\n{combined}"
    );
}
