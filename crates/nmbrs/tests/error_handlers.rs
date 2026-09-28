// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Integration examples for the error handler surface.
//!
//! Each test drives the `testkit` adapter to inject errors in a
//! known way (deterministic rate via `result-error-rate`, or
//! stateful capacity overload via `result-capacity`/`result-overload`)
//! and runs `nmbrs run errors=<spec>` to verify the router and
//! each built-in handler behave as documented.
//!
//! These double as copy-pasteable recipes for operators: the
//! `errors=` string in every test is a valid CLI argument on its
//! own.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

// ─── Harness ──────────────────────────────────────────────────────

fn nmbrs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    // Run from workspace root so relative workload paths resolve.
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .expect("workspace root");
    cmd.current_dir(workspace_root);
    cmd
}

/// Write a temporary workload YAML and return its path. The file
/// survives for the duration of the process; individual tests run
/// in parallel so each gets its own.
fn write_workload(label: &str, body: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    // `.cargo/config.toml` redirects TMPDIR to `target/test-tmp/`,
    // which cargo does not create for us — the first test to write
    // there would fail otherwise. Create-on-demand.
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create_dir_all {dir:?}: {e}"));
    dir.push(format!(
        "nmbrs_errhandler_{label}_{}.yaml",
        std::process::id(),
    ));
    let mut f = std::fs::File::create(&dir).unwrap_or_else(|e| panic!("create {dir:?}: {e}"));
    f.write_all(body.as_bytes())
        .unwrap_or_else(|e| panic!("write {dir:?}: {e}"));
    dir
}

/// Run `nmbrs run workload=... <extra>` and return
/// `(stdout, evidence, success)`, where `evidence` is stderr plus the
/// session's `session.log`. The handler/readout output the tests
/// assert on moved off the console: `tui=off` claims the log-only
/// surface for the whole run (the always-a-sink invariant — no
/// workload thread may block on an adapter-owned terminal), so in-run
/// diagnostics land in the ring + session.log and are deliberately
/// suppressed from stderr; only post-run reports still print there.
/// Uses a per-invocation `--session-path` so cargo's parallel
/// test execution doesn't race on `logs/default_<timestamp>`.
fn run(workload: &std::path::Path, extra: &[&str]) -> (String, String, bool) {
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-errh-{}-{}-{}",
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

// ─── Workload shapes used across tests ────────────────────────────

/// Deterministic-error workload: each cycle has a fixed probability
/// of a `ModelError` based on a per-cycle hash, so results are
/// reproducible across runs. Ops print to stdout *before* the error
/// injection, so stdout always has one line per cycle even when
/// errors fire.
const DETERMINISTIC_ERROR_YAML: &str = r#"
blocks:
  main:
    ops:
      insert:
        stmt: "op"
        result-error-rate: 0.5
        result-error-name: "ModelError"
"#;

/// Overload workload: narrow capacity + tight overload threshold so
/// real concurrency above the threshold produces retryable errors.
/// Latency is long enough (10 ms) that the queue actually forms.
const OVERLOAD_YAML: &str = r#"
blocks:
  main:
    ops:
      insert:
        stmt: "op"
        result-latency: "10ms"
        result-capacity: 2
        result-overload: 4
"#;

/// Two-class workload: `flaky` emits `ModelError`, `heavy` emits
/// retryable `Overload` on saturation. Lets us verify pattern-based
/// routing that treats each error class differently.
const TWO_CLASS_YAML: &str = r#"
blocks:
  main:
    ops:
      flaky:
        stmt: "flaky"
        result-error-rate: 0.5
        result-error-name: "ModelError"
      heavy:
        stmt: "heavy"
        result-latency: "10ms"
        result-capacity: 1
        result-overload: 2
"#;

// ─── Tests ────────────────────────────────────────────────────────

/// Default / `errors=stop`: a single error aborts the run.
///
/// With 100% error rate, cycle 0 errors immediately. The process
/// should exit non-zero and must not log "done." in stderr (that
/// message is only printed on successful completion).
#[test]
fn stop_aborts_on_first_error() {
    let wl = write_workload(
        "stop",
        r#"
blocks:
  main:
    ops:
      insert:
        stmt: "op"
        result-error-rate: 1.0
"#,
    );
    let (_stdout, stderr, ok) = run(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            "errors=stop",
        ],
    );
    assert!(!ok, "run should have failed, stderr={stderr}");
    // Line-based: the completion marker is a bare `done.` on stderr or
    // a `… INF done.` session.log line — either ends the line with it.
    assert!(
        !stderr.lines().any(|l| l.trim_end().ends_with("done.")),
        "run should not have completed, stderr={stderr}"
    );
}

/// `errors=warn`: log each error to stderr, keep running. Every
/// cycle is processed (stdout has one line per cycle, regardless
/// of whether the cycle errored — the adapter prints the op text
/// before running error injection).
#[test]
fn warn_logs_and_continues() {
    let wl = write_workload("warn", DETERMINISTIC_ERROR_YAML);
    let (stdout, stderr, ok) = run(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            "errors=warn",
        ],
    );
    assert!(ok, "run should have succeeded, stderr={stderr}");
    assert!(
        stderr.contains("done"),
        "stderr should log 'done': {stderr}"
    );

    let stdout_lines = stdout.lines().filter(|l| l.trim() == "op").count();
    assert_eq!(
        stdout_lines, 20,
        "all 20 cycles should have printed before error injection, got {stdout_lines}"
    );

    // Per-cycle WARN spam was deferred into the structured
    // `error_readout` block at PhaseEnd (the normative phase
    // line renders first; errors are appended below it).
    // Assert the new shape: the readout block lists the first
    // ModelError with the `(+N more)` extra-count tail.
    assert!(
        stderr.contains("errors:"),
        "expected error_readout block in stderr: {stderr}"
    );
    assert!(
        stderr.contains("[ModelError]"),
        "expected ModelError class in error_readout block: {stderr}"
    );
    assert!(
        stderr.contains("more"),
        "expected `(+N more)` extra-count tail when several errors fire: {stderr}"
    );
}

/// `errors=ignore`: errors pass through silently. Every cycle runs
/// to completion with no stderr noise from the handler.
#[test]
fn ignore_suppresses_output() {
    let wl = write_workload("ignore", DETERMINISTIC_ERROR_YAML);
    let (stdout, stderr, ok) = run(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            "errors=ignore",
        ],
    );
    assert!(ok, "run should have succeeded, stderr={stderr}");

    let stdout_lines = stdout.lines().filter(|l| l.trim() == "op").count();
    assert_eq!(
        stdout_lines, 20,
        "all 20 cycles should have printed, got {stdout_lines}"
    );

    assert!(
        !stderr.contains("WARN error"),
        "ignore handler should emit no WARN output: {stderr}"
    );
    assert!(
        !stderr.contains("ERROR at cycle"),
        "ignore handler should emit no ERROR output: {stderr}"
    );
}

/// `errors=.*:warn,counter`: the counter handler tallies error
/// occurrences by type. Here we just verify the chain runs through
/// and the run still completes (the count itself is exposed via
/// metrics, not stderr — this test covers the "chain doesn't abort"
/// property).
#[test]
fn warn_then_counter_chain() {
    let wl = write_workload("warncount", DETERMINISTIC_ERROR_YAML);
    let (_stdout, stderr, ok) = run(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            // Full router syntax: match all error names, run warn then
            // counter. Handlers are comma-separated and executed in
            // order; either alone would also work.
            "errors=.*:warn,counter",
        ],
    );
    assert!(ok, "run should have succeeded, stderr={stderr}");
    // The `warn` half of the chain no longer floods stderr with
    // per-cycle lines (deferred into error_readout); the test
    // verifies the chain ran through to completion by checking
    // the structured error block surfaces the class.
    assert!(
        stderr.contains("[ModelError]"),
        "warn-then-counter chain should still surface errors via error_readout: {stderr}"
    );
}

/// Pattern routing: different handlers per error class. `Overload`
/// is treated as a soft retry/warn (retryable backpressure), while
/// any other error type (`ModelError` in particular) trips the
/// `stop` handler. This is the canonical "fail-fast on bugs,
/// tolerate transient backpressure" setup.
///
/// Our workload only produces `ModelError`, so `stop` fires on the
/// first error — demonstrating that patterns further down the spec
/// really do claim their names and the catch-all doesn't swallow
/// everything.
#[test]
fn pattern_routing_distinguishes_error_classes() {
    let wl = write_workload("pattern", DETERMINISTIC_ERROR_YAML);
    let (_stdout, stderr, ok) = run(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            // Rules are semicolon-separated. Overload errors go to
            // retry,warn; anything else hits stop.
            "errors=Overload:retry,warn;.*:stop",
        ],
    );
    assert!(
        !ok,
        "ModelError should have hit the stop rule, stderr={stderr}"
    );
}

/// Pattern routing on the saturation path: `Overload` → warn (soft
/// backpressure), everything else → stop. The run should complete
/// because only Overload errors fire under this load.
#[test]
fn overload_warned_other_errors_stopped() {
    let wl = write_workload("overload_warn", OVERLOAD_YAML);
    let (_stdout, stderr, ok) = run(
        &wl,
        &[
            "adapter=testkit",
            "cycles=40",
            "concurrency=8",
            "errors=Overload:warn;.*:stop",
        ],
    );
    assert!(
        ok,
        "Overload warnings should not abort the run, stderr={stderr}"
    );
    // Per-cycle WARN lines are now collected into the
    // error_readout block at PhaseEnd. Assert the structured
    // surface mentions Overload.
    assert!(
        stderr.contains("[Overload]"),
        "at least one Overload error expected in error_readout: {stderr}"
    );
}

/// SRD-82 op shell — an op-template's own `errors:` overrides the
/// enclosing scope's policy for THAT op. The global policy is
/// `errors=stop` (a single error aborts), but the `lenient` op pins
/// `.*:warn,counter`, so its 100%-error stream is logged + counted and
/// the run completes. This is the per-op handler pinned to the op
/// dispenser: the terminal error is routed through it, not the phase
/// policy.
#[test]
fn op_level_errors_overrides_scope_policy() {
    let wl = write_workload(
        "op_lenient",
        r#"
blocks:
  main:
    ops:
      lenient:
        stmt: "lenient"
        result-error-rate: 1.0
        result-error-name: "ModelError"
        errors: ".*:warn,counter"
"#,
    );
    let (_stdout, stderr, ok) = run(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            // Global policy says stop-on-first-error; the op overrides it.
            "errors=stop",
        ],
    );
    assert!(
        ok,
        "op-scoped `.*:warn,counter` should tolerate the errors despite the \
         global `errors=stop`, stderr={stderr}"
    );
    assert!(
        stderr.contains("[ModelError]"),
        "the op DID error (and was tolerated) — its class should surface in \
         the error_readout block: {stderr}"
    );
}

/// SRD-82 op shell — one op's leniency does NOT leak to its siblings.
/// `lenient` pins `.*:warn,counter`; `strict` declares no `errors:` and
/// so inherits the global `errors=stop`. `strict`'s error must still
/// abort the run — proving each dispenser routes through its OWN handler,
/// not a shared phase-wide one softened by a neighbor.
#[test]
fn op_level_errors_does_not_leak_to_siblings() {
    let wl = write_workload(
        "op_sibling",
        r#"
blocks:
  main:
    ops:
      lenient:
        stmt: "lenient"
        result-error-rate: 1.0
        result-error-name: "ModelError"
        errors: ".*:warn,counter"
      strict:
        stmt: "strict"
        result-error-rate: 1.0
        result-error-name: "ModelError"
"#,
    );
    let (_stdout, stderr, ok) = run(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            "errors=stop",
        ],
    );
    assert!(
        !ok,
        "the `strict` sibling inherits the global `errors=stop` and must \
         abort — `lenient`'s override must not soften it, stderr={stderr}"
    );
}

/// Two-class routing: `flaky` errors (ModelError) get ignored,
/// `heavy` errors (Overload) get warned. Run completes. Used to
/// prove the router actually classifies by `error-name`, not by
/// op name or block.
#[test]
fn two_class_routing() {
    let wl = write_workload("twoclass", TWO_CLASS_YAML);
    let (_stdout, stderr, ok) = run(
        &wl,
        &[
            "adapter=testkit",
            "cycles=40",
            "concurrency=6",
            "errors=Overload:warn;ModelError:ignore",
        ],
    );
    assert!(ok, "both classes should be tolerated, stderr={stderr}");
    assert!(
        !stderr
            .lines()
            .any(|l| l.contains("WARN error") && l.contains("ModelError")),
        "ModelError should have been ignored, not warned: {stderr}"
    );
    // We can't assert Overload actually fired (it depends on timing
    // + scheduling), but if it did, it must have been warned about.
    let model_warns = stderr
        .lines()
        .filter(|l| l.contains("WARN error") && l.contains("ModelError"))
        .count();
    assert_eq!(model_warns, 0);
}

/// A per-cycle binding panic (predicate violation) must surface as
/// a first-class failure: the enriched diagnostic — node name,
/// original panic location, inputs — plus a `[panic]` headline,
/// with NO raw un-enriched hook print drowning it out.
#[test]
fn binding_panic_surfaces_enriched_headline() {
    let wl = write_workload(
        "panic_headline",
        r#"
phases:
  probe:
    adapter: stdout
    cycles: 1
    bindings: |
      checked := is_positive(mul(cycle, 0))
    ops:
      o:
        op: "c={checked}"
"#,
    );
    let (_stdout, stderr, ok) = run(&wl, &[]);
    assert!(
        !ok,
        "a panicking binding must fail the run, stderr={stderr}"
    );
    assert!(
        stderr.contains("is_positive"),
        "original panic body names the violated predicate: {stderr}"
    );
    assert!(
        stderr.contains("in node"),
        "enriched context (node name) expected: {stderr}"
    );
    assert!(
        stderr.contains("[panic]"),
        "headline stop_reason expected: {stderr}"
    );
    // The suppression hook must have eaten the raw pre-enrichment
    // print. A raw hook line reads `thread '…' panicked at
    // <crate source>/src/library/param_helpers.rs:…` — pointing
    // at the ORIGINAL panic site with no node context.
    assert!(
        !stderr
            .lines()
            .any(|l| l.contains("thread '") && l.contains("panicked at")),
        "raw un-enriched hook print leaked through: {stderr}"
    );
    // SRD-82 §"Panic reporting: one full render" (catchup C3):
    // the runtime declares downstream reporting, so the hook
    // prints ONE short first-line notice; headlines (stop_reason,
    // footer) are first-line short form; the `errors:` block owns
    // the full multi-line render.
    assert!(
        stderr.contains("op eval panic (detail in phase errors)"),
        "hook prints the short notice: {stderr}"
    );
    assert!(
        !stderr
            .lines()
            .any(|l| l.contains("stopped by error handler") && l.contains("in node")),
        "headline must be first-line short form: {stderr}"
    );
    assert!(
        stderr.contains("errors:"),
        "the errors block is present: {stderr}"
    );
}

/// Attempt-level stop wires (compaction-demo diagnosis: the result-
/// level error_rate is blind behind retries). `attempt_count` /
/// `attempt_error_rate` are declarable stop_when wires; with a 100%
/// deterministic error rate the guard trips at the 50-attempt floor.
#[test]
fn attempt_level_guard_is_declarable() {
    // `rate:` paces the phase so it stays alive across
    // stop-condition evaluation ticks — instant failing ops would
    // otherwise drain all cycles before the first tick. (testkit
    // injects the error before its latency await, so
    // result-latency can't provide the pacing.)
    let wl = write_workload(
        "attempt_guard",
        r#"
phases:
  probe:
    adapter: testkit
    cycles: 2000
    rate: 500
    stop_when:
      - when: "attempt_total >= 50 && to_f64(attempt_failure) > (to_f64(attempt_total) * 0.5)"
    ops:
      insert:
        stmt: "op"
        result-error-rate: 1.0
"#,
    );
    let (_stdout, stderr, ok) = run(&wl, &["errors=warn", "concurrency=4"]);
    assert!(!ok, "attempt guard must fail the phase: {stderr}");
    assert!(
        stderr.contains("attempt_failure"),
        "trip names the attempt instrument: {stderr}"
    );
    assert!(
        stderr.contains("attempt_failure_fraction="),
        "derived attempt fraction reported in the actuals: {stderr}"
    );
}

mod scratch;
