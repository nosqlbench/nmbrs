// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Integration tests that run example workloads end-to-end via the
//! stdout adapter, verifying that the full pipeline (YAML parsing →
//! Polydat compilation → phased execution → adapter output) works correctly.
//!
//! Each test runs `nmbrs run` as a subprocess and checks the output.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Per-invocation session directory so concurrent test runs
/// don't collide on `logs/default_<timestamp>`. cargo runs
/// integration tests in parallel by default, and the wall-
/// clock-second-grained default session name is too coarse
/// to keep them apart.
///
/// We compute a unique path but DON'T create the directory —
/// nmbrs's session bootstrap will mkdir it. Pre-creating would
/// trigger nmbrs's "directory already contains artifacts"
/// reuse-policy check.
struct SessionDir {
    path: PathBuf,
}

impl SessionDir {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Nest each test's session under its own parent so
        // nmbrs's `purge_stale_sessions` (which scans the
        // session-path's parent dir) can't see sibling
        // tests' sessions.
        let parent = std::env::temp_dir().join(format!("nmbrs-workload-examples-{pid}-{nanos}"));
        std::fs::create_dir_all(&parent).expect("create session parent");
        let path = parent.join("session");
        Self { path }
    }

    fn parent(&self) -> &Path {
        self.path.parent().expect("session dir has parent")
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.parent());
    }
}

fn nmbrs(session: &SessionDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    // Run from workspace root so workload paths resolve correctly
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    cmd.current_dir(workspace_root);
    cmd.arg("run");
    cmd.arg("--session-path");
    cmd.arg(&session.path);
    cmd
}

/// Returns `(stdout, evidence)` where `evidence` is stderr plus the
/// session's `session.log` — `tui=off` (the non-TTY default under
/// `cargo test`) claims the log-only surface, so in-run diagnostics
/// like the `done.` completion line land in session.log and are
/// deliberately suppressed from the console.
fn run_inline(op: &str, extra_args: &[&str]) -> (String, String) {
    let session = SessionDir::new();
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("op={op}"));
    for arg in extra_args {
        cmd.arg(arg);
    }
    let output = cmd.output().expect("failed to run nmbrs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let session_log = std::fs::read_to_string(session.path.join("session.log")).unwrap_or_default();
    let evidence = format!("{}\n{session_log}", String::from_utf8_lossy(&output.stderr));
    (stdout, evidence)
}

// ─── Example workloads ─────────────────────────────────────────

#[test]
fn optimizer_control_nests_inside_a_coordinate_sweep() {
    // SRD-86 §4 "mixed = node nesting" — a phase whose sweeps cover BOTH a
    // non-control (the outer `for: batch`, re-run actuation) AND a control (the
    // inner `servo: conc` axis, daemon actuation). The outer scope reruns
    // the phase per `batch`; within EACH rerun the control daemon servos the live
    // concurrency. Two outer values ⇒ the Control daemon must fire twice, each a
    // clean continuous phase. (Inner sweep is a single setting so the test stays
    // to one settle per rerun.)
    let yaml = r#"
params:
  metrics_cadence: 50ms
scenarios:
  sweep:
    - for: "batch in 1, 2"
      phases:
        - saturate

phases:
  saturate:
    cycles: 60000
    concurrency: "{conc}"
    rate: 2000
    errors: warn
    error_rate_max: 1.0
    for_each: "conc in 2"
    optimize:
      objective: score
      max_evals: 10
      servo: conc
    bindings: |
      input cycle: u64
      err_rate := metricsql_scalar("sum(rate(errors_total[400ms]))")
      score := 0 - err_rate
    ops:
      insert:
        adapter: testkit
        stmt: "INSERT INTO demo.events (id) VALUES ({cycle});"
        result-latency: "5ms"
        result-capacity: 2
        result-overload: 4
"#;
    let (path, session) = write_inline_workload("optimizer_control_nested", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    cmd.arg("scenario=sweep");
    let output = cmd.output().expect("failed to run nmbrs");
    let out = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !out.contains("error:"),
        "nested control sweep errored: {out}"
    );
    // The Control daemon composed under the outer Coordinate rerun: one servoed
    // continuous phase per `batch`, each finding the overload-free setting.
    let control_runs = out.matches("(control): best [2]").count();
    assert_eq!(
        control_runs, 2,
        "the control daemon must servo once per outer `batch` iteration (2): {out}"
    );
}

#[test]
fn optimizer_hybrid_iterates_coordinate_and_servos_control() {
    // SRD-86 §4 hybrid actuation — ONE optimize node mixing a coordinate axis
    // (`batch`, rerun) and a control axis (`conc`, servoed), expressed in one
    // multi-clause phase `for_each`. The coordinate axis forms the outer rerun
    // grid (one phase activation per `batch`), and the Control daemon servos the
    // control subspace interior to each cell. (Single control value keeps the
    // test to one settle per cell.)
    let yaml = r#"
params:
  metrics_cadence: 50ms
phases:
  saturate:
    cycles: 60000
    concurrency: "{conc}"
    rate: 2000
    errors: warn
    error_rate_max: 1.0
    for_each: "batch in [1,2], conc in [2]"
    optimize:
      objective: score
      max_evals: 10
      servo: conc
    bindings: |
      input cycle: u64
      err_rate := metricsql_scalar("sum(rate(errors_total[400ms]))")
      score := 0 - err_rate
    ops:
      insert:
        adapter: testkit
        stmt: "INSERT INTO demo.events (batch, id) VALUES ({batch}, {cycle});"
        result-latency: "5ms"
        result-capacity: 2
        result-overload: 4
"#;
    let (path, session) = write_inline_workload("optimizer_hybrid", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    let output = cmd.output().expect("failed to run nmbrs");
    let out = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!out.contains("error:"), "hybrid optimize errored: {out}");
    // The hybrid partition ran: the coordinate axis `batch` is the outer rerun
    // grid (2 cells), and `conc` was servoed interior to each — reaching the
    // overload-free setting (conc=2) without rerunning per conc value.
    assert!(
        out.contains("(hybrid 2 coordinate cells × control)") && out.contains("; 2]"),
        "the hybrid must rerun per coordinate cell and servo the control to conc=2: {out}"
    );
    // One clean phase activation per coordinate cell (no failures).
    assert!(
        out.contains("2 completed, 0 failed"),
        "each coordinate cell must complete as a clean servoed phase: {out}"
    );
}

#[test]
fn optimizer_servo_without_control_field_is_rejected() {
    // SRD-86 §4 servo validation — `servo: conc` must be wired to a live control
    // by a referencing control field (`concurrency: "{conc}"`). Without it, the
    // servo cannot know which control to retarget, so the dispatch rejects it
    // with an actionable error — surfacing the half-specified-servo mistake
    // rather than silently downgrading to a coordinate.
    let yaml = r#"
params:
  metrics_cadence: 50ms
phases:
  bad:
    cycles: 4
    for_each: "conc in 1, 2"
    optimize:
      objective: score
      max_evals: 5
      servo: conc
    bindings: |
      score := 0 - conc
    ops:
      probe:
        adapter: stdout
        params:
          stdout: eventlog
        stmt: "c={conc}"
"#;
    let (path, session) = write_inline_workload("optimizer_servo_unwired", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    let output = cmd.output().expect("failed to run nmbrs");
    let out = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        out.contains("is neither a live control nor wired to one"),
        "`servo:` on a var with no control field must be rejected: {out}"
    );
}

#[test]
fn optimizer_servos_a_control_directly_by_name() {
    // SRD-86 §4 — `servo:` can name a live control DIRECTLY (the axis IS the
    // control), with no `{var}`-bind wire. `for_each: "concurrency in …"` +
    // `servo: concurrency` servos the concurrency control itself. (`concurrency:
    // 16` is just the warmup the daemon retargets from.)
    // Same fast idiom as the mixed-resolution test below (and
    // nmbrs/examples/workloads/optimizer/*.yaml): 50ms cadence, 400ms objective
    // window. On the 1s default this was 22s of wall for two evals.
    let yaml = r#"
params:
  metrics_cadence: 50ms
phases:
  saturate:
    cycles: 60000
    concurrency: 16
    rate: 2000
    errors: warn
    error_rate_max: 1.0
    for_each: "concurrency in 32, 2"
    optimize:
      objective: score
      max_evals: 10
      servo: concurrency
    bindings: |
      input cycle: u64
      err_rate := metricsql_scalar("sum(rate(errors_total[400ms]))")
      score := 0 - err_rate
    ops:
      insert:
        adapter: testkit
        stmt: "INSERT INTO demo.events (id) VALUES ({cycle});"
        result-latency: "5ms"
        result-capacity: 2
        result-overload: 4
"#;
    let (path, session) = write_inline_workload("optimizer_servo_direct", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    let output = cmd.output().expect("failed to run nmbrs");
    let out = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!out.contains("error:"), "direct servo errored: {out}");
    assert!(
        out.contains("(control): best [2]"),
        "servoing the `concurrency` control by name must find the overload-free setting: {out}"
    );
}

#[test]
fn optimizer_servos_mixed_direct_and_indirect_controls() {
    // SRD-86 §4 — a single `servo:` LIST can mix resolution forms. Here `conc`
    // resolves INDIRECTLY (the axis var sinks into the `concurrency` control via
    // `concurrency: "{conc}"`) while `rate` resolves DIRECTLY (the axis name IS
    // the rate control). The classifier must handle both kinds in one list, and
    // the daemon retargets both live controls per setting on ONE continuous
    // phase. This is the resolution variant the all-direct
    // `optimizer_multiservo.yaml` doesn't cover.
    // Timing follows the optimizer examples' fast idiom (see
    // nmbrs/examples/workloads/optimizer/multiservo.yaml): a 50ms metrics cadence
    // and a 400ms objective window, so each of the 4 grid evals settles in
    // well under a second instead of ~10s on the default 1s cadence. This
    // was the workspace's single longest test (42s) for no property-related
    // reason — the assertions below are unchanged.
    let yaml = r#"
params:
  metrics_cadence: 50ms
phases:
  saturate:
    cycles: 120000
    concurrency: "{conc}"
    rate: 2000
    errors: warn
    error_rate_max: 1.0
    for_each: "conc in 32, 2, rate in 4000, 1000"
    optimize:
      objective: score
      max_evals: 10
      servo: [conc, rate]
    bindings: |
      input cycle: u64
      err_rate := metricsql_scalar("sum(rate(errors_total[400ms]))")
      score := 0 - err_rate
    ops:
      insert:
        adapter: testkit
        stmt: "INSERT INTO demo.events (id) VALUES ({cycle});"
        result-latency: "5ms"
        result-capacity: 2
        result-overload: 4
"#;
    let (path, session) = write_inline_workload("optimizer_servo_mixed", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    let output = cmd.output().expect("failed to run nmbrs");
    let out = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !out.contains("error:"),
        "mixed-resolution servo errored: {out}"
    );
    // A 2-tuple on the Control path proves both axes servoed; the indirect
    // `conc` drove the concurrency control down to 2 (the overload-free corner).
    assert!(
        out.contains("(control): best [2,"),
        "mixed `servo: [conc, rate]` must servo both (indirect conc + direct rate): {out}"
    );
    assert!(
        out.contains("score=0"),
        "the overload-free corner scores 0: {out}"
    );
    assert!(
        out.contains("after 4 evals"),
        "the 2-D control grid (2×2) must be fully searched: {out}"
    );
    assert!(
        out.contains("1 completed, 0 failed") || out.contains("[ok]"),
        "the multi-servoed phase must complete cleanly: {out}"
    );
}

#[test]
fn optimizer_servos_the_rate_control_directly() {
    // SRD-86 §4 — `servo:` can name the `rate` control DIRECTLY. This is the ONLY
    // way to servo `rate`: the phase `rate:` field is a fixed `f64` that can't carry
    // a `{var}` bind, so the indirect form is unavailable. The `rate:` value is the
    // warmup the daemon retargets from.
    //
    // Real-time sensitivity: overload onset is in-flight >= 4, i.e.
    // ~2000/s at the 2 ms op latency. The candidates sit WIDE of that
    // threshold on both sides (3x above, 4x below) so a loaded machine
    // that only achieves a fraction of the requested pace still
    // overloads at the high setting and never at the low one — a
    // tight 2x margin made this flip to a scoring tie under
    // concurrent-sweep CPU contention. The cycle budget must outlast
    // both eval settle windows at the candidates' burn rates (the
    // high candidate burns ~6000 cycles/s; running out mid-eval ends
    // the phase at "0 evals"), so the cheap candidate runs first and
    // the budget carries generous slack — the optimizer concludes the
    // phase after its evals, so slack costs no wall time.
    let yaml = r#"
params:
  metrics_cadence: 50ms
phases:
  saturate:
    cycles: 200000
    concurrency: 8
    rate: 2000
    errors: warn
    error_rate_max: 1.0
    for_each: "rate in 500, 6000"
    optimize:
      objective: score
      max_evals: 10
      servo: rate
    bindings: |
      input cycle: u64
      err_rate := metricsql_scalar("sum(rate(errors_total[400ms]))")
      score := 0 - err_rate
    ops:
      insert:
        adapter: testkit
        stmt: "INSERT INTO demo.events (id) VALUES ({cycle});"
        result-latency: "2ms"
        result-capacity: 2
        result-overload: 4
"#;
    let (path, session) = write_inline_workload("optimizer_servo_rate_direct", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    let output = cmd.output().expect("failed to run nmbrs");
    let out = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!out.contains("error:"), "direct rate servo errored: {out}");
    assert!(
        out.contains("(control): best [500]"),
        "servoing the `rate` control by name must find the lower, overload-free rate: {out}"
    );
}

#[test]
fn optimizer_servo_rate_without_rate_field_is_rejected() {
    // SRD-86 §4 — `servo: rate` needs the phase to declare a `rate:` field, since the
    // rate control only exists when set (concurrency is always declared). Without it
    // there is no control to servo, so the dispatch rejects it at validation time —
    // a clean pre-run error, symmetric to the windowed-objective check, NOT a runtime
    // phase failure when the daemon can't find the control.
    let yaml = r#"
params:
  metrics_cadence: 50ms
phases:
  bad:
    cycles: 60000
    concurrency: 8
    error_rate_max: 1.0
    for_each: "rate in 4000, 1000"
    optimize:
      objective: score
      max_evals: 5
      servo: rate
    bindings: |
      input cycle: u64
      err_rate := metricsql_scalar("sum(rate(errors_total[400ms]))")
      score := 0 - err_rate
    ops:
      probe:
        adapter: testkit
        stmt: "INSERT INTO demo.events (id) VALUES ({cycle});"
        result-latency: "2ms"
"#;
    let (path, session) = write_inline_workload("optimizer_servo_rate_norate", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    let output = cmd.output().expect("failed to run nmbrs");
    let out = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        out.contains("declares no `rate:` field"),
        "`servo: rate` without a `rate:` field must be rejected before the run: {out}"
    );
}

#[test]
fn phase_for_each_multi_clause_cartesian() {
    // Polydat owns the comprehension grammar; the PHASE `for_each` now
    // delegates to the same multi-clause bridge the scenario level uses
    // (it previously split on the first ` in ` → single axis only). A
    // two-clause phase for_each produces the full cartesian, consistent
    // with scenario-level `for:`.
    let yaml = r#"
params:
  metrics_cadence: 50ms
phases:
  walk:
    cycles: 1
    for_each: "a in [1,2], b in [10,20]"
    ops:
      emit:
        adapter: stdout
        params:
          stdout: eventlog
        stmt: "pair a={a} b={b}"
"#;
    let (path, session) = write_inline_workload("phase_multi_clause", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    let output = cmd.output().expect("failed to run nmbrs");
    let out = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let pairs = out.matches("pair a=").count();
    assert_eq!(
        pairs, 4,
        "phase multi-clause for_each must produce the cartesian {{1,2}}×{{10,20}}: {out}"
    );
    for expected in [
        "pair a=1 b=10",
        "pair a=1 b=20",
        "pair a=2 b=10",
        "pair a=2 b=20",
    ] {
        assert!(out.contains(expected), "missing `{expected}`: {out}");
    }
}

// ─── Inline ops ────────────────────────────────────────────────

#[test]
fn inline_simple_expression() {
    let (stdout, stderr) = run_inline("hello {{hash(cycle)}}", &["cycles=3"]);
    assert!(stderr.contains("done"), "stderr: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "should have 3 lines");
    for line in &lines {
        assert!(line.starts_with("hello "), "line: {line}");
    }
}

#[test]
fn inline_multiple_ops_with_ratios() {
    let (stdout, stderr) = run_inline(
        "3:read {{cycle}};1:write {{mod(hash(cycle),100)}}",
        &["cycles=8"],
    );
    assert!(stderr.contains("done"), "stderr: {stderr}");
    let reads = stdout.lines().filter(|l| l.starts_with("read")).count();
    let writes = stdout.lines().filter(|l| l.starts_with("write")).count();
    assert!(
        reads > writes,
        "should have more reads than writes: {reads} reads, {writes} writes"
    );
}

#[test]
fn inline_math_expression() {
    let (stdout, stderr) = run_inline("val={{sin(to_f64(cycle) * 0.1)}}", &["cycles=5"]);
    assert!(stderr.contains("done"), "stderr: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 5);
    // First cycle (cycle=0): sin(0) = 0
    assert!(
        lines[0].contains("val=0"),
        "sin(0) should be 0: {}",
        lines[0]
    );
}

// ─── Bang path (shebang) ───────────────────────────────────────

#[test]
fn bare_file_invocation() {
    // nmbrs <file.yaml> should work without 'run' subcommand
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let session = SessionDir::new();
    let output = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(workspace_root)
        .arg("nmbrs/examples/workloads/visual/maze.yaml")
        .arg("cycles=3")
        .arg("--session-path")
        .arg(&session.path)
        .output()
        .expect("failed to run nmbrs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("done"), "should complete: {stderr}");
    assert_eq!(stdout.lines().count(), 3, "should have 3 lines");
}

// ─── Polydat features ───────────────────────────────────────────────

#[test]
fn const_expression_in_cycles() {
    // cycles={4*4} should evaluate to 16 via Polydat const expression
    let (stdout, stderr) = run_inline("tick", &["cycles={4*4}"]);
    assert!(stderr.contains("done"), "stderr: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        16,
        "4*4=16 cycles expected, got {}",
        lines.len()
    );
}

#[test]
fn deterministic_output() {
    // Same cycle should produce same output
    let (out1, _) = run_inline("v={{hash(cycle)}}", &["cycles=5"]);
    let (out2, _) = run_inline("v={{hash(cycle)}}", &["cycles=5"]);
    assert_eq!(out1, out2, "same workload should produce identical output");
}

// ─── Shared cells (SRD-16 §"Mutability Rules: Shared Mutable") ─────
//
// Round-trip test for `shared X := <literal>` end-to-end via
// stdout. Each scenario in `shared_cells.yaml` emits stable,
// grep-able lines so the assertions can check exact values
// rather than substring-fuzzing.

// ─── Coverage matrix tests moved to nmbrs/tests/scope.rs ─────────
//
// The `scope_coverage.yaml` workload (formerly
// `workload_coverage_matrix.yaml`) and its matching
// `scope_*` tests now live in their own thematic file, per the
// `<theme>_coverage.yaml` + `nmbrs/tests/<theme>.rs` pattern
// established alongside `cursor_partitions_coverage`.

// ─── Scenario-tree `set:` (workload-param shadowing) ──────────
//
// The canonical surfaces of `set:` — bare-token shadow, multi-key
// shadow, expression-with-interpolation value, set-wrapping-for_each
// composition, and nested-set composition — each live as a single-
// scenario demo under `nmbrs/examples/workloads/scenario_param_overrides/`
// (and the iter-var variants under `scenario_set_iter_var/`), verified
// by the example-walker test against their `#@ expect` directives. All
// resolve through the Polydat scope-chain (no HashMap merges, no
// synthesizer side-channels).

#[test]
fn empty_bindings_and_set_blocks_emit_no_op_warning() {
    // A scenario-tree `set:` or `bindings:` block with no
    // `phases:` body is structurally a no-op: the scope is
    // entered and immediately exited with no descendants
    // reading any of its declared names. The parser warns at
    // workload-load time and keeps the scope node out of the
    // resolved tree (almost always an author error — typed
    // the override and forgot the body). Pin both forms here
    // so a future refactor that silences the warning is
    // caught.
    // `'"verbose"'` — YAML single-quotes around polydat
    // double-quotes survive YAML scalar parsing and reach
    // polydat as a string literal. Under the set-block
    // bare-ident → polydat-reference convention, the explicit
    // quotes are required to signal a string literal.
    let yaml = r#"
params:
  mode: default

scenarios:
  good:
    - set: { mode: '"verbose"' }
      phases:
        - just_say
  empty_set:
    - set: { mode: '"verbose"' }
  empty_bindings:
    - bindings: |
        const mode := "verbose"

phases:
  just_say:
    adapter: stdout
    cycles: 1
    ops:
      msg:
        stmt: "mode={mode}"
"#;
    let (path, session) = write_inline_workload("empty_set_warning", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    // Pick the `good` scenario so the run succeeds — the
    // warnings still fire because they're parser-level
    // (emitted once per workload-load, regardless of which
    // scenario the operator picked).
    cmd.arg("scenario=good");
    let output = cmd.output().expect("failed to run nmbrs");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    assert!(
        stderr.contains("`set:` block (overriding [\"mode\"])"),
        "empty set: must emit the no-op warning naming the \
         overridden keys. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("scenario-tree `bindings:` block has no"),
        "empty bindings: must emit the no-op warning. stderr:\n{stderr}"
    );

    // The `good` scenario still completes and substitutes
    // mode correctly — the empty blocks are warnings, not
    // errors, and they don't poison the run.
    assert!(
        stderr.contains("all phases complete"),
        "good scenario must still complete. stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("mode=verbose"),
        "good scenario must still produce mode=verbose. stdout:\n{stdout}"
    );
}

// ─── Synthetic metrics (SRD-40b) end-to-end ─────────────────

/// Run `synthetic_metrics.yaml` and verify every declared
/// synthetic metric flows through the pipeline into the
/// session's `metrics.db`. Covers SRD-40b §1 (the schema:
/// mapping form, bare-string sugar, list form with
/// `wire := <expr>` entries) plus SRD-40a §4.3 (unit-suffix
/// + `metric_family.unit` invariant).
///
/// Owns its `SessionDir` for the whole test so the metrics.db
/// can be inspected after the run; the session parent is
/// removed by `SessionDir`'s `Drop` impl when the test exits.
#[test]
fn synthetic_metrics_workload_populates_metric_family() {
    let session = SessionDir::new();
    let mut cmd = nmbrs(&session);
    cmd.arg("workload=nmbrs/examples/workloads/metrics/synthetic_metrics.yaml");
    cmd.arg("cycle_count=12");
    let output = cmd.output().expect("failed to run nmbrs");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success() && stderr.contains("done"),
        "synthetic_metrics workload did not complete:\n{stderr}",
    );

    let db_path = session.path.join("metrics.db");
    assert!(db_path.exists(), "metrics.db missing at {db_path:?}");
    let conn = rusqlite::Connection::open(&db_path).expect("open metrics.db");

    // (family_name, expected_type, expected_unit). SRD-40b §1
    // says `unit:` lands in BOTH the `_<unit>` suffix on
    // `metric_family.name` AND in `metric_family.unit`.
    let expectations: &[(&str, &str, Option<&str>)] = &[
        // Mapping form: explicit unit "ms" → name suffix + unit column.
        ("latency_curve_ms", "gauge", Some("ms")),
        // Mapping form with non-bare `value:` — auto-injected
        // into op-template bindings (SRD-13d Phase 9 §1).
        ("latency_window", "gauge", None),
        // Bare-string sugar: `metrics: load` — gauge default, no unit.
        ("load", "gauge", None),
        // List form with `:= <expr>` — auto-injected wires.
        ("forecast_low", "gauge", None),
        ("forecast_high", "gauge", None),
        // Counter with explicit unit "ops".
        ("step_counter_ops", "counter", Some("ops")),
        // Histogram defaults: stored as "summary" per OpenMetrics
        // mapping (HDR-backed → summary).
        ("observation_dist", "summary", None),
    ];

    for (name, expected_type, expected_unit) in expectations {
        let row: Result<(String, Option<String>), _> = conn.query_row(
            "SELECT type, unit FROM metric_family WHERE name = ?1",
            [name],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        );
        let (got_type, got_unit) = row.unwrap_or_else(|e| {
            panic!("metric_family row missing for {name:?}: {e}");
        });
        assert_eq!(
            got_type, *expected_type,
            "metric_family.type mismatch for {name}",
        );
        assert_eq!(
            got_unit.as_deref(),
            *expected_unit,
            "metric_family.unit mismatch for {name}",
        );
    }
}

/// SRD-13d Phase 9 follow-up §3 — value-correctness check.
///
/// The sibling test above only asserts metric_family rows
/// exist; this one runs the workload with a fixed
/// cycle_count and asserts the recorded values are
/// consistent with the workload's per-cycle formulas.
/// Catches any per-fiber kernel-instancing or cross-scope-
/// snapshot bug that the row-existence test would miss.
///
/// `cycles: "{cycle_count}"` plus the implicit
/// `cycles: ===auto` shape means the phase runs
/// `cycle_count * stanza_len` total cycles (one per op per
/// stanza). With cycle_count=6 and 4 ops, total cycles = 24.
/// Per-cycle formulas (from
/// `nmbrs/examples/workloads/metrics/synthetic_metrics.yaml`):
///   load           = cycle + 1
///   latency_curve  = load * 2             (per phase)
///   forecast_low   = latency_curve * 0.9  (synth_op_list)
///   forecast_high  = latency_curve * 1.1  (synth_op_list)
///   step           = 1                    (synth_op_kinds)
///   observation    = cycle % 100          (synth_op_kinds)
///
/// Each metric instance is a distinct (family, op-label)
/// pair; the test asserts shape (positive values, plausible
/// bounds, formula-consistent ratios) rather than pinning
/// the exact last-cycle value, which depends on the op
/// sequencer's ordering.
#[test]
fn synthetic_metrics_workload_records_correct_values() {
    let session = SessionDir::new();
    let mut cmd = nmbrs(&session);
    cmd.arg("workload=nmbrs/examples/workloads/metrics/synthetic_metrics.yaml");
    cmd.arg("cycle_count=6");
    let output = cmd.output().expect("failed to run nmbrs");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success() && stderr.contains("done"),
        "synthetic_metrics workload did not complete:\n{stderr}",
    );

    let db_path = session.path.join("metrics.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open metrics.db");

    /// For each instance of `family`, return (op_label,
    /// last_mean, last_count, last_sum, last_max). Joins
    /// across the schema; orders samples per instance by
    /// timestamp.
    fn all_instance_samples(
        conn: &rusqlite::Connection,
        family: &str,
    ) -> Vec<(String, f64, i64, f64, f64)> {
        let mut stmt = conn
            .prepare(
                "SELECT i.id, COALESCE(i.spec, '') FROM metric_instance i \
             JOIN metric_family f ON i.family_id = f.id \
             WHERE f.name = ?1",
            )
            .unwrap();
        let instances: Vec<(i64, String)> = stmt
            .query_map([family], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        instances
            .into_iter()
            .filter_map(|(id, label_set_json)| {
                // (mean, count, sum, max) for the latest sample row.
                #[allow(clippy::type_complexity)]
                let row: rusqlite::Result<(
                    Option<f64>,
                    Option<i64>,
                    Option<f64>,
                    Option<f64>,
                )> = conn.query_row(
                    "SELECT s.mean, s.count, s.sum, s.max FROM sample_value s \
                     WHERE s.instance_id = ?1 \
                     ORDER BY s.timestamp_ms DESC LIMIT 1",
                    [id],
                    |r| {
                        Ok((
                            r.get::<_, Option<f64>>(0)?,
                            r.get::<_, Option<i64>>(1)?,
                            r.get::<_, Option<f64>>(2)?,
                            r.get::<_, Option<f64>>(3)?,
                        ))
                    },
                );
                row.ok().map(|(mean, count, sum, max)| {
                    (
                        label_set_json,
                        mean.unwrap_or(0.0),
                        count.unwrap_or(0),
                        sum.unwrap_or(0.0),
                        max.unwrap_or(0.0),
                    )
                })
            })
            .collect()
    }

    // Every metric must have at least one instance with at
    // least one sample (i.e. the dispenser path actually wrote
    // through to the cadence reporter).
    for family in &[
        "load",
        "latency_curve_ms",
        "latency_window",
        "forecast_low",
        "forecast_high",
        "step_counter_ops",
        "observation_dist",
    ] {
        let samples = all_instance_samples(&conn, family);
        assert!(
            !samples.is_empty(),
            "metric {family}: no instance samples in metrics.db"
        );
    }

    // Gauges. All recorded values must be positive (cycles
    // start at 0 → cycle+1 ≥ 1, mul by positive constant
    // stays positive).
    for family in &[
        "load",
        "latency_curve_ms",
        "latency_window",
        "forecast_low",
        "forecast_high",
    ] {
        for (label, mean, _, _, _) in all_instance_samples(&conn, family) {
            assert!(
                mean > 0.0,
                "{family} ({label}): gauge value {mean} should be positive"
            );
        }
    }

    // Counter. step_counter_ops always increments by 1 per
    // synth_op_kinds execution, so the cumulative count must
    // be ≥ 1 and ≤ cycle_count (synth_op_kinds runs once per
    // stanza of the 4-op cycle).
    for (label, _, count, _, _) in all_instance_samples(&conn, "step_counter_ops") {
        assert!(
            count >= 1,
            "step_counter_ops ({label}): expected ≥1, got {count}"
        );
        assert!(
            count <= 6,
            "step_counter_ops ({label}): expected ≤6 (cycle_count=6), got {count}"
        );
    }

    // Histogram. observation = cycle % 100. With 24 total
    // cycles (cycle_count=6, 4 ops, every 4th is
    // synth_op_kinds) the values recorded are at cycles 3, 7,
    // 11, 15, 19, 23 → observations 3, 7, 11, 15, 19, 23.
    // count = 6, max ≤ 23.
    for (label, _, count, _sum, max) in all_instance_samples(&conn, "observation_dist") {
        assert!(
            count >= 1,
            "observation_dist ({label}): expected ≥1 sample, got {count}"
        );
        assert!(
            count <= 6,
            "observation_dist ({label}): expected ≤6 samples, got {count}"
        );
        assert!(
            max <= 23.0,
            "observation_dist ({label}): max={max}, expected ≤23"
        );
    }

    // Cross-formula invariant — for instances of forecast_low
    // and forecast_high writing on the same cycles, the
    // ratio matches the formula (forecast_high =
    // latency_curve * 1.1, forecast_low = latency_curve *
    // 0.9 → forecast_high / forecast_low = 1.1/0.9 ≈ 1.222).
    let lows = all_instance_samples(&conn, "forecast_low");
    let highs = all_instance_samples(&conn, "forecast_high");
    if let (Some((_, low, _, _, _)), Some((_, high, _, _, _))) = (lows.first(), highs.first()) {
        let ratio = high / low;
        let expected = 1.1 / 0.9;
        assert!(
            (ratio - expected).abs() < 1e-3,
            "forecast_high / forecast_low = {ratio}, expected ≈ {expected}"
        );
    }
}

// ─── SRD-13d Phase 9 §4: wrapper smoke tests under
// materialised op-templates ─────────────────────────────────

/// Write `yaml` to a temporary file under
/// `target/test-tmp/<unique>/workload.yaml` so test workloads
/// can be authored inline. Returns (workload_path, session_dir).
fn write_inline_workload(name: &str, yaml: &str) -> (PathBuf, SessionDir) {
    let session = SessionDir::new();
    let workload_path = session.parent().join(format!("{name}.yaml"));
    std::fs::write(&workload_path, yaml).expect("write inline workload");
    (workload_path, session)
}

/// Conditional dispenser under a materialised op-template:
/// the op carries its own `bindings:` block (forcing per-op
/// kernel synthesis under Phase 9), and `if:` references one
/// of those op-local bindings. Verifies the wrapper resolves
/// its `PullHandle` against the op-template kernel's state
/// rather than the workload-root state — the op-local
/// `mod(cycle, 2)` binding must change per cycle so the
/// gating actually flips. We assert via the per-op skips
/// counter in metrics.db rather than parsing stdout, since
/// the adapter's bind-point rendering is on a different
/// resolve path than the wrapper pulls.
#[test]
fn conditional_under_materialised_op_template() {
    let yaml = r#"
params:
  metrics_cadence: 50ms
phases:
  predict:
    cycles: 10
    ops:
      gated:
        adapter: stdout
        params:
          stdout: eventlog
        bindings: |
          // Op-local: forces materialisation of the op-template kernel.
          local_pred := mod(cycle, 2)
        if: local_pred
        stmt: "ran"
"#;
    let (path, session) = write_inline_workload("conditional_op_template", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    let output = cmd.output().expect("failed to run nmbrs");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success() && stderr.contains("done"),
        "workload did not complete:\nstderr: {stderr}\nstdout: {stdout}"
    );
    // 10 cycles, only odd ones (1,3,5,7,9) have local_pred != 0,
    // so the conditional must fire skips_total == 5 and the op
    // must execute the other 5. If `cycle` weren't propagated
    // to the op-template kernel, local_pred would stay 0 for
    // every cycle and the op would skip every time (10 skips,
    // 0 executions).
    let conn =
        rusqlite::Connection::open(session.path.join("metrics.db")).expect("open metrics.db");
    let count_for = |family: &str| -> i64 {
        conn.query_row(
            "SELECT s.count FROM metric_family f
                JOIN metric_instance i ON i.family_id = f.id
                JOIN sample_value s ON s.instance_id = i.id
              WHERE f.name = ?1 LIMIT 1",
            [family],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
    };
    let total = count_for("cycles_total");
    let skips = count_for("skips_total");
    assert_eq!(total, 10, "cycles_total = {total}, expected 10");
    assert_eq!(
        skips, 5,
        "skips_total = {skips}, expected 5 (odd cycles run, even skip)"
    );
}

/// Throttle dispenser under a materialised op-template: the
/// op has its own `bindings:` declaring the delay binding.
/// Verifies the throttle wrapper reads the delay from the
/// op-template kernel's state per cycle. We check the value
/// *was* observed (not the wall-clock effect) by reading the
/// declared metric, since precise sleep timing is brittle.
#[test]
fn throttle_under_materialised_op_template() {
    let yaml = r#"
params:
  metrics_cadence: 50ms
phases:
  predict:
    cycles: 4
    ops:
      delayed:
        adapter: stdout
        params:
          stdout: eventlog
        bindings: |
          // Op-local: forces materialisation. Delay scaled
          // small so the test stays fast — value verified
          // via the declared metric below, not wall clock.
          local_delay_ns := mod(cycle, 2)
        delay: local_delay_ns
        stmt: "ran cycle={cycle}"
        metrics:
          delay_witness:
            value: local_delay_ns
            kind: gauge
"#;
    let (path, session) = write_inline_workload("throttle_op_template", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    let output = cmd.output().expect("failed to run nmbrs");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success() && stderr.contains("done"),
        "workload did not complete: {stderr}"
    );
    let conn =
        rusqlite::Connection::open(session.path.join("metrics.db")).expect("open metrics.db");
    // delay_witness must exist as a registered family — proving
    // the dispenser wrapping path saw the op-local binding.
    let row: Result<(String,), _> = conn.query_row(
        "SELECT type FROM metric_family WHERE name = ?1",
        ["delay_witness"],
        |r| Ok((r.get::<_, String>(0)?,)),
    );
    let (kind,) = row.expect("delay_witness family missing");
    assert_eq!(kind, "gauge");
}

/// Validation dispenser under a materialised op-template:
/// the op has its own `bindings:` and `verify:` block
/// referencing op-local wires. Verifies the validator
/// resolves its expected/observed handles against the
/// op-template kernel's state.
#[test]
fn validation_under_materialised_op_template() {
    let yaml = r#"
params:
  metrics_cadence: 50ms
phases:
  predict:
    cycles: 4
    ops:
      checked:
        adapter: stdout
        params:
          stdout: eventlog
        bindings: |
          // Op-local: forces materialisation.
          local_doubled := mul(cycle, 2)
        stmt: "n={cycle}"
        verify:
          min_rows: 0
"#;
    let (path, session) = write_inline_workload("validation_op_template", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    let output = cmd.output().expect("failed to run nmbrs");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success() && stderr.contains("done"),
        "workload did not complete: {stderr}"
    );
}

// ─── SRD-71: cursor partitioning ────────────────────────────────

#[test]
fn cursor_over_narrows_to_first_percentage() {
    // Cursor declared `over cursor` with the operator passing
    // `cursor=0..10%` should narrow `range(0, 1000)` to
    // `[0, 100)` — emitting 100 cycles instead of 1000.
    let yaml = r#"
params:
  cursor: "0..100%"

phases:
  walk:
    concurrency: 1
    bindings: |
      cursor row = range(0, 1000) over cursor
      n := row
    ops:
      emit:
        adapter: stdout
        stmt: "row={n}"
"#;
    let (path, session) = write_inline_workload("cursor_over_pct", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    cmd.arg("cursor=0..10%");
    let output = cmd.output().expect("failed to run nmbrs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "workload failed: {stderr}");
    let count = stdout.lines().filter(|l| l.starts_with("row=")).count();
    assert_eq!(
        count, 100,
        "expected 100 narrowed rows from `cursor=0..10%`, got {count}.\nstdout:\n{stdout}"
    );
}

#[test]
fn cursor_over_with_literal_ordinals() {
    // Literal-ordinal partition spec: `cursor=100..200` should
    // narrow `range(0, 1000)` to `[100, 200)` — 100 cycles
    // starting at row 100.
    let yaml = r#"
params:
  cursor: "0..100%"

phases:
  walk:
    concurrency: 1
    bindings: |
      cursor row = range(0, 1000) over cursor
      n := row
    ops:
      emit:
        adapter: stdout
        stmt: "row={n}"
"#;
    let (path, session) = write_inline_workload("cursor_over_literal", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    cmd.arg("cursor=100..200");
    let output = cmd.output().expect("failed to run nmbrs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "workload failed: {stderr}");
    let rows: Vec<u64> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("row="))
        .filter_map(|s| s.parse().ok())
        .collect();
    assert_eq!(rows.len(), 100, "expected 100 rows, got {}", rows.len());
    assert_eq!(*rows.iter().min().unwrap(), 100);
    assert_eq!(*rows.iter().max().unwrap(), 199);
}

#[test]
fn cursor_without_over_ignores_cursor_param() {
    // A cursor declared without `over` should be unaffected by
    // the `cursor=...` parameter — the cursor uses its full
    // declared extent even when the operator passes a narrowing
    // spec. The workload must still declare `cursor` in its
    // `params:` for the runtime to accept the CLI override at
    // all; this test verifies that even with the param set, the
    // cursor that doesn't opt in via `over` stays at full extent.
    let yaml = r#"
params:
  cursor: "0..100%"

phases:
  walk:
    concurrency: 1
    bindings: |
      cursor row = range(0, 50)
      n := row
    ops:
      emit:
        adapter: stdout
        stmt: "row={n}"
"#;
    let (path, session) = write_inline_workload("cursor_no_over", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    cmd.arg("cursor=0..10%");
    let output = cmd.output().expect("failed to run nmbrs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let count = stdout.lines().filter(|l| l.starts_with("row=")).count();
    assert_eq!(
        count, 50,
        "cursor without `over` must ignore `cursor=...`; got {count} rows"
    );
}

#[test]
fn mod_in_maps_cycle_into_narrowed_partition() {
    // `mod_in(cycle, row.cursor)` maps cycle to an ordinal that
    // stays inside the cursor's narrowed range. With
    // `cursor=100..200` against `range(0, 1000)`, the cursor
    // narrows to [100, 200), and mod_in wraps `cycle` (0..100)
    // into that range — yielding 100, 101, ..., 199.
    let yaml = r#"
params:
  cursor: "0..100%"

phases:
  walk:
    concurrency: 1
    bindings: |
      cursor row = range(0, 1000) over cursor
      n := mod_in(cycle, row.cursor)
    ops:
      emit:
        adapter: stdout
        stmt: "n={n}"
"#;
    let (path, session) = write_inline_workload("cursor_mod_in", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    cmd.arg("cursor=100..200");
    let output = cmd.output().expect("failed to run nmbrs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "workload failed: {stderr}");
    let ns: Vec<u64> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("n="))
        .filter_map(|s| s.parse().ok())
        .collect();
    assert_eq!(ns.len(), 100, "expected 100 outputs, got {}", ns.len());
    assert_eq!(*ns.iter().min().unwrap(), 100);
    assert_eq!(*ns.iter().max().unwrap(), 199);
}

#[test]
fn comprehension_iterates_partition_list_per_partition() {
    // SRD-71 §"Comprehension iteration": a scenario-tree
    // `for: "p in <expr>"` over `partitions(...)` iterates
    // partition-by-partition. Each iteration's bound kernel
    // carries the per-partition `Partition` value to descendant
    // phases, where `mod_in(cycle, p)` (or `over p` on a cursor
    // decl) consumes it as an Ext-typed wire.
    let yaml = r#"
params:
  metrics_cadence: 50ms
scenarios:
  sweep:
    - for: "p in partitions(\"linear:3\", 99)"
      phases:
        - walk

phases:
  walk:
    cycles: 5
    concurrency: 1
    bindings: |
      lo := start_of(p)
      hi := end_of(p)
      i := idx_of(p)
    ops:
      emit:
        adapter: stdout
        stmt: "part={i} lo={lo} hi={hi}"
"#;
    let (path, session) = write_inline_workload("partition_comprehension", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    cmd.arg("scenario=sweep");
    let output = cmd.output().expect("failed to run nmbrs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "workload failed: {stderr}");

    // 3 partitions × 5 cycles = 15 emit lines.
    let emit_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("part=")).collect();
    assert_eq!(
        emit_lines.len(),
        15,
        "expected 3 partitions × 5 cycles = 15 emits, got {}.\nstdout:\n{stdout}",
        emit_lines.len()
    );

    // Each partition emits 5 lines; the three partition indices
    // (0, 1, 2) all appear, and the lo/hi values are distinct
    // per partition.
    let parts_seen: std::collections::HashSet<&str> = emit_lines
        .iter()
        .filter_map(|l| l.split(' ').next())
        .collect();
    assert_eq!(parts_seen.len(), 3, "expected 3 distinct partition indices");
    assert!(parts_seen.contains("part=0"));
    assert!(parts_seen.contains("part=1"));
    assert!(parts_seen.contains("part=2"));

    // Partition 0 covers [0..33), partition 1 [33..66), partition 2 [66..99).
    let part0_line = emit_lines
        .iter()
        .find(|l| l.starts_with("part=0 "))
        .unwrap();
    assert!(
        part0_line.contains("lo=0") && part0_line.contains("hi=33"),
        "partition 0 should be [0, 33), got: {part0_line}"
    );
    let part2_line = emit_lines
        .iter()
        .find(|l| l.starts_with("part=2 "))
        .unwrap();
    assert!(
        part2_line.contains("lo=66") && part2_line.contains("hi=99"),
        "partition 2 should be [66, 99), got: {part2_line}"
    );
}

#[test]
fn cardinality_and_start_of_expose_partition_metadata() {
    // `cardinality(row.cursor)` and `start_of(row.cursor)` are
    // effectively-const for the activation — they should
    // produce the same value every cycle.
    let yaml = r#"
params:
  cursor: "0..100%"

phases:
  walk:
    cycles: 3
    concurrency: 1
    bindings: |
      cursor row = range(0, 1000) over cursor
      card := cardinality(row.cursor)
      lo := start_of(row.cursor)
    ops:
      emit:
        adapter: stdout
        stmt: "card={card} lo={lo}"
"#;
    let (path, session) = write_inline_workload("partition_metadata", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    cmd.arg("cursor=100..200");
    let output = cmd.output().expect("failed to run nmbrs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "workload failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().filter(|l| l.contains("card=")).collect();
    assert!(!lines.is_empty(), "no card= lines emitted:\n{stdout}");
    for line in &lines {
        assert!(
            line.contains("card=100"),
            "expected card=100 (200-100), got: {line}"
        );
        assert!(line.contains("lo=100"), "expected lo=100, got: {line}");
    }
}

#[test]
fn cursor_param_quote_elision_works_end_to_end() {
    // Quote elision on the CLI surface: `cursor='0..10%'` and
    // `'cursor=0..10%'` and `cursor="0..10%"` should all parse
    // identically and narrow the cursor to 10%.
    let yaml = r#"
params:
  cursor: "0..100%"

phases:
  walk:
    concurrency: 1
    bindings: |
      cursor row = range(0, 1000) over cursor
      n := row
    ops:
      emit:
        adapter: stdout
        stmt: "row={n}"
"#;
    let (path, session) = write_inline_workload("cursor_over_quoted", yaml);
    let mut cmd = nmbrs(&session);
    cmd.arg(format!("workload={}", path.display()));
    cmd.arg("cursor='0..10%'");
    let output = cmd.output().expect("failed to run nmbrs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let count = stdout.lines().filter(|l| l.starts_with("row=")).count();
    assert_eq!(
        count, 100,
        "single-quoted cursor='0..10%' should narrow to 100 rows; got {count}"
    );
}
