// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-71 cursor partitioning — end-to-end coverage tests.
//!
//! Each test runs one scenario from
//! `nmbrs/examples/workloads/cursors/cursor_partitions_coverage.yaml` and
//! asserts the expected behavior of one shape:
//!
//! - Form 1 single sub-range (number forms + bracket tolerance)
//! - Form 2 contiguous delta lists (pct / fraction / literal / mixed)
//! - Form 3 pre-baked recipes
//! - `over` clause shapes (workload-param, iter-var, cross-cursor)
//! - Stdlib partition functions
//! - Reified custom-named cursor parameters
//! - Cursor without `over` ignoring the parameter
//!
//! The workload YAML carries the operator-readable shape
//! vocabulary; the tests verify the behavior numerically.

use std::path::{Path, PathBuf};
use std::process::Command;

const WORKLOAD: &str = "nmbrs/examples/workloads/cursors/cursor_partitions_coverage.yaml";

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
        let parent = std::env::temp_dir().join(format!("nmbrs-cursor-partitions-{pid}-{nanos}"));
        std::fs::create_dir_all(&parent).expect("create session parent");
        Self {
            path: parent.join("session"),
        }
    }
    fn parent(&self) -> &Path {
        self.path.parent().unwrap()
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.parent());
    }
}

fn run_scenario(scenario: &str, extra_args: &[&str]) -> (String, String, bool) {
    let session = SessionDir::new();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(workspace_root)
        .arg("run")
        .arg("--session-path")
        .arg(&session.path)
        .arg(format!("workload={WORKLOAD}"))
        .arg(format!("scenario={scenario}"));
    for a in extra_args {
        cmd.arg(a);
    }
    let out = cmd.output().expect("run nmbrs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (stdout, stderr, out.status.success())
}

/// Collect lines that start with the given prefix. Useful for
/// filtering out informational stdout and counting / asserting
/// against the `cp/...` emits the workload phases produce.
fn lines_with_prefix(stdout: &str, prefix: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|l| l.starts_with(prefix))
        .map(|l| l.to_string())
        .collect()
}

/// Distinct lines with the given prefix. Form 1 scenarios emit
/// the same `lo=X hi=Y` repeatedly (one per cycle in the
/// narrowed range); the test asserts on the distinct content.
fn distinct_lines_with_prefix(stdout: &str, prefix: &str) -> Vec<String> {
    let mut lines: Vec<String> = lines_with_prefix(stdout, prefix);
    lines.sort();
    lines.dedup();
    lines
}

// ─────────────────────────────────────────────────────────────────
// Form 1: single sub-range
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_form1_percentage_end() {
    // `over "0..53%"` against range(0, 1000) → cursor [0, 530).
    let (stdout, stderr, ok) = run_scenario("form1_percentage_end", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=0 hi=530".to_string()]
    );
}

#[test]
fn cursor_partitions_form1_fraction_end() {
    // `over "0..0.53"` (fraction form) === `over "0..53%"`.
    let (stdout, stderr, ok) = run_scenario("form1_fraction_end", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=0 hi=530".to_string()]
    );
}

#[test]
fn cursor_partitions_form1_literal_ordinals() {
    // `over "100..500"` (bare integers) → cursor [100, 500).
    let (stdout, stderr, ok) = run_scenario("form1_literal_ordinals", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=100 hi=500".to_string()]
    );
}

#[test]
fn cursor_partitions_form1_mixed_literal_then_pct() {
    // `over "100..50%"` — literal start, percentage end.
    let (stdout, stderr, ok) = run_scenario("form1_mixed_literal_then_pct", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=100 hi=500".to_string()]
    );
}

#[test]
fn cursor_partitions_form1_mixed_frac_then_literal() {
    // `over "0.10..800"` — fraction start, literal end.
    let (stdout, stderr, ok) = run_scenario("form1_mixed_frac_then_literal", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=100 hi=800".to_string()]
    );
}

#[test]
fn cursor_partitions_form1_brackets_tolerated() {
    // `over "[0..53%)"` — bracket / closure markers stripped.
    let (stdout, stderr, ok) = run_scenario("form1_brackets_tolerated", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=0 hi=530".to_string()]
    );
}

// ─────────────────────────────────────────────────────────────────
// Form 2: delta lists
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_form2_pct_with_star() {
    // `partitions("2%,10%,*%")` against default extent 100 →
    //   [0..2), [2..12), [12..100).
    let (stdout, stderr, ok) = run_scenario("form2_pct_with_star", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=2",
            "cp/iter idx=1 lo=2 hi=12",
            "cp/iter idx=2 lo=12 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_form2_fraction_with_star() {
    // `partitions("0.02,0.10,*")` equivalent to percentage form.
    let (stdout, stderr, ok) = run_scenario("form2_fraction_with_star", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=2",
            "cp/iter idx=1 lo=2 hi=12",
            "cp/iter idx=2 lo=12 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_form2_literal_with_star() {
    // `partitions("1000,5000,*", 10000)` — literal ordinal deltas.
    let (stdout, stderr, ok) = run_scenario("form2_literal_with_star", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=1000",
            "cp/iter idx=1 lo=1000 hi=6000",
            "cp/iter idx=2 lo=6000 hi=10000",
        ]
    );
}

#[test]
fn cursor_partitions_form2_mixed_literal_pct_with_star() {
    // `partitions("1000,10%,*", 10000)` — first delta literal,
    // second a percentage of the extent (10% of 10000 = 1000),
    // third the remainder (8000).
    let (stdout, stderr, ok) = run_scenario("form2_mixed_literal_pct_with_star", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=1000",
            "cp/iter idx=1 lo=1000 hi=2000",
            "cp/iter idx=2 lo=2000 hi=10000",
        ]
    );
}

#[test]
fn cursor_partitions_form2_short_list_drops_gap() {
    // `partitions("20%,30%")` — no `*`, sum < 100% → trailing
    // 50% gap is dropped, only 2 partitions emitted.
    let (stdout, stderr, ok) = run_scenario("form2_short_list_drops_gap", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec!["cp/iter idx=0 lo=0 hi=20", "cp/iter idx=1 lo=20 hi=50",]
    );
}

// ─────────────────────────────────────────────────────────────────
// Form 2 tail tokens: `...` fill and `*/N` split
// ─────────────────────────────────────────────────────────────────

/// The expected partitions for "first 90%, then the rest in ten
/// 1%-of-the-whole chunks" against the default extent 100 —
/// shared by the fill and split spellings, which must coincide
/// at head=90%.
fn ninety_ten_by_one() -> Vec<String> {
    let mut expected = vec!["cp/iter idx=0 lo=0 hi=90".to_string()];
    for i in 0..10u64 {
        expected.push(format!("cp/iter idx={} lo={} hi={}", i + 1, 90 + i, 91 + i));
    }
    expected.sort();
    expected
}

#[test]
fn cursor_partitions_form2_fill() {
    // `partitions("90%,1%,...")` — the fill token repeats the
    // 1% delta until the extent is used up → 11 partitions.
    let (stdout, stderr, ok) = run_scenario("form2_fill", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        ninety_ten_by_one()
    );
}

#[test]
fn cursor_partitions_form2_star_split() {
    // `partitions("90%,*/10")` — the split token divides the
    // remainder into 10 → identical partitions to the fill
    // spelling at head=90%.
    let (stdout, stderr, ok) = run_scenario("form2_star_split", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        ninety_ten_by_one()
    );
}

#[test]
fn cursor_partitions_form2_star_split_whole() {
    // `partitions("*/4")` — no head, so the remainder is the
    // whole extent: resolves exactly like `linear:4`.
    let (stdout, stderr, ok) = run_scenario("form2_star_split_whole", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=25",
            "cp/iter idx=1 lo=25 hi=50",
            "cp/iter idx=2 lo=50 hi=75",
            "cp/iter idx=3 lo=75 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_form2_fill_truncates() {
    // `partitions("3,2,...", 10)` — the final fill chunk
    // truncates at the extent (1 ordinal instead of 2); "until
    // used up" emits the short tail rather than dropping it.
    let (stdout, stderr, ok) = run_scenario("form2_fill_truncates", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=3",
            "cp/iter idx=1 lo=3 hi=5",
            "cp/iter idx=2 lo=5 hi=7",
            "cp/iter idx=3 lo=7 hi=9",
            "cp/iter idx=4 lo=9 hi=10",
        ]
    );
}

#[test]
fn cursor_partitions_form2_repetition() {
    // `partitions("90%,1%x10")` — finite repetition, the fourth
    // spelling of the 90/10 family: identical partitions to the
    // fill and split spellings.
    let (stdout, stderr, ok) = run_scenario("form2_repetition", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        ninety_ten_by_one()
    );
}

#[test]
fn cursor_partitions_form2_gap() {
    // `partitions("10%,~80%,10%")` — the gap consumes 80% of
    // the extent without emitting; idx counts emitted
    // partitions only.
    let (stdout, stderr, ok) = run_scenario("form2_gap", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec!["cp/iter idx=0 lo=0 hi=10", "cp/iter idx=1 lo=90 hi=100",]
    );
}

#[test]
fn cursor_partitions_form2_star_shaped() {
    // `partitions("50%,*/ratios:1,3", 1000)` — the remainder
    // (500 ordinals) shaped 25%/75% by the recipe weights.
    let (stdout, stderr, ok) = run_scenario("form2_star_shaped", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=500",
            "cp/iter idx=1 lo=500 hi=625",
            "cp/iter idx=2 lo=625 hi=1000",
        ]
    );
}

#[test]
fn cursor_partitions_windowed_chunking() {
    // `partitions("linear:4 in 20%..100%", 1000)` — the
    // chunking resolves against the window [200, 1000), so the
    // four chunks are 200 ordinals each starting at 200.
    let (stdout, stderr, ok) = run_scenario("windowed_chunking", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=200 hi=400",
            "cp/iter idx=1 lo=400 hi=600",
            "cp/iter idx=2 lo=600 hi=800",
            "cp/iter idx=3 lo=800 hi=1000",
        ]
    );
}

#[test]
fn cursor_partitions_order_largest_first() {
    // `partitions("fib:5 largest_first")` — iteration runs the
    // biggest partition first; idx keeps identifying the
    // generation position, so the emitted idx sequence is 4..0.
    // Order matters here, so this uses the order-preserving
    // helper.
    let (stdout, stderr, ok) = run_scenario("order_largest_first", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=4 lo=58 hi=100",
            "cp/iter idx=3 lo=32 hi=58",
            "cp/iter idx=2 lo=16 hi=32",
            "cp/iter idx=1 lo=5 hi=16",
            "cp/iter idx=0 lo=0 hi=5",
        ]
    );
}

#[test]
fn cursor_partitions_order_random_is_deterministic() {
    // `partitions("linear:4 random")` — a deterministic
    // shuffle: same spec, same order, every run.
    let (stdout, stderr, ok) = run_scenario("order_random", &[]);
    assert!(ok, "scenario failed: {stderr}");
    let first = lines_with_prefix(&stdout, "cp/iter ");
    assert_eq!(first.len(), 4, "four partitions: {first:?}");
    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            "cp/iter idx=0 lo=0 hi=25",
            "cp/iter idx=1 lo=25 hi=50",
            "cp/iter idx=2 lo=50 hi=75",
            "cp/iter idx=3 lo=75 hi=100",
        ],
        "shuffle is a permutation of the same partitions"
    );
    let (stdout2, stderr2, ok2) = run_scenario("order_random", &[]);
    assert!(ok2, "second run failed: {stderr2}");
    assert_eq!(
        lines_with_prefix(&stdout2, "cp/iter "),
        first,
        "random order must be deterministic across runs"
    );
}

// ─────────────────────────────────────────────────────────────────
// Form 3: pre-baked recipes
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_recipe_linear() {
    // `linear:4` → 4 uniform quarter-partitions [0..25), [25..50),
    // [50..75), [75..100).
    let (stdout, stderr, ok) = run_scenario("recipe_linear", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=25",
            "cp/iter idx=1 lo=25 hi=50",
            "cp/iter idx=2 lo=50 hi=75",
            "cp/iter idx=3 lo=75 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_recipe_ratios() {
    // `ratios:1,1,2` (sum 4) → 25%, 25%, 50%.
    let (stdout, stderr, ok) = run_scenario("recipe_ratios", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=25",
            "cp/iter idx=1 lo=25 hi=50",
            "cp/iter idx=2 lo=50 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_recipe_bin() {
    // `bin:5` → C(4, 0..4) = [1, 4, 6, 4, 1], sum 16 →
    // [6.25%, 25%, 37.5%, 25%, 6.25%].
    let (stdout, stderr, ok) = run_scenario("recipe_bin", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=6",
            "cp/iter idx=1 lo=6 hi=31",
            "cp/iter idx=2 lo=31 hi=69",
            "cp/iter idx=3 lo=69 hi=94",
            "cp/iter idx=4 lo=94 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_recipe_fib() {
    // `fib:5` → distinct fib (skipping leading 1,1) = [1, 2, 3, 5, 8].
    let (stdout, stderr, ok) = run_scenario("recipe_fib", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=5",
            "cp/iter idx=1 lo=5 hi=16",
            "cp/iter idx=2 lo=16 hi=32",
            "cp/iter idx=3 lo=32 hi=58",
            "cp/iter idx=4 lo=58 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_recipe_ln() {
    // `ln:5` → monotonically increasing weights from ln(2)..ln(6).
    let (stdout, stderr, ok) = run_scenario("recipe_ln", &[]);
    assert!(ok, "scenario failed: {stderr}");
    let lines = distinct_lines_with_prefix(&stdout, "cp/iter ");
    assert_eq!(lines.len(), 5, "ln:5 should produce 5 partitions");
    // Last partition ends at 100% (no trailing gap because
    // sum-of-deltas equals extent exactly).
    assert!(
        lines.last().unwrap().contains("hi=100"),
        "last partition should reach 100, got: {lines:?}"
    );
}

#[test]
fn cursor_partitions_recipe_mul_decay() {
    // `mul:0.5` (decay) terminates when current < start*0.001 —
    // 11 terms (1, 0.5, 0.25, …, 1/1024 ≈ 0.00098).
    let (stdout, stderr, ok) = run_scenario("recipe_mul_decay", &[]);
    assert!(ok, "scenario failed: {stderr}");
    let lines = distinct_lines_with_prefix(&stdout, "cp/count ");
    assert_eq!(
        lines.len(),
        11,
        "mul:0.5 decay should produce 11 partitions"
    );
}

#[test]
fn cursor_partitions_recipe_mul_with_start() {
    // `mul:5,0.5` — same termination rule, different starting weight.
    let (stdout, stderr, ok) = run_scenario("recipe_mul_with_start", &[]);
    assert!(ok, "scenario failed: {stderr}");
    let lines = distinct_lines_with_prefix(&stdout, "cp/count ");
    assert_eq!(lines.len(), 11);
}

#[test]
fn cursor_partitions_recipe_geom() {
    // `geom:4,2` → fixed 4 terms: 1, 2, 4, 8 (sum 15).
    let (stdout, stderr, ok) = run_scenario("recipe_geom", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=7",
            "cp/iter idx=1 lo=7 hi=20",
            "cp/iter idx=2 lo=20 hi=47",
            "cp/iter idx=3 lo=47 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_recipe_zipf() {
    // `zipf:1,4` → Zipfian weights 1/1, 1/2, 1/3, 1/4. Test
    // asserts only on partition count; exact boundaries are
    // numerically sensitive across libm versions.
    let (stdout, stderr, ok) = run_scenario("recipe_zipf", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(distinct_lines_with_prefix(&stdout, "cp/count ").len(), 4);
}

#[test]
fn cursor_partitions_recipe_pareto() {
    // `pareto:1,4` → Pareto-distributed weights (1/n)^1 for n=1..4.
    let (stdout, stderr, ok) = run_scenario("recipe_pareto", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(distinct_lines_with_prefix(&stdout, "cp/count ").len(), 4);
}

#[test]
fn cursor_partitions_recipe_front_heavy() {
    // `front_heavy:4` → 4, 3, 2, 1 (sum 10). 40%, 30%, 20%, 10%.
    let (stdout, stderr, ok) = run_scenario("recipe_front_heavy", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=40",
            "cp/iter idx=1 lo=40 hi=70",
            "cp/iter idx=2 lo=70 hi=90",
            "cp/iter idx=3 lo=90 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_recipe_back_heavy() {
    // `back_heavy:4` → 1, 2, 3, 4 (sum 10). 10%, 20%, 30%, 40%.
    let (stdout, stderr, ok) = run_scenario("recipe_back_heavy", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=10",
            "cp/iter idx=1 lo=10 hi=30",
            "cp/iter idx=2 lo=30 hi=60",
            "cp/iter idx=3 lo=60 hi=100",
        ]
    );
}

// ─────────────────────────────────────────────────────────────────
// `over` clause shapes
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_over_workload_param() {
    // `cursor q = range(0, 1000) over cursor` + CLI cursor=0..50%
    // narrows to [0, 500).
    let (stdout, stderr, ok) = run_scenario("over_workload_param", &["cursor=0..50%"]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/over_cursor "),
        vec!["cp/over_cursor lo=0 hi=500".to_string()]
    );
}

#[test]
fn cursor_partitions_phase_scoped_override_exact() {
    // SRD 71 P3: `<phase>.cursor=<spec>` overrides the param for
    // that phase only, beating the workload-wide `cursor=`.
    let (stdout, stderr, ok) = run_scenario(
        "over_workload_param",
        &["cursor=0..50%", "over_cursor_phase.cursor=0..25%"],
    );
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/over_cursor "),
        vec!["cp/over_cursor lo=0 hi=250".to_string()]
    );
}

#[test]
fn cursor_partitions_phase_scoped_override_glob() {
    // Glob form: `over_*.cursor=` matches `over_cursor_phase`.
    let (stdout, stderr, ok) = run_scenario("over_workload_param", &["over_*.cursor=0..10%"]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/over_cursor "),
        vec!["cp/over_cursor lo=0 hi=100".to_string()]
    );
}

#[test]
fn cursor_partitions_phase_override_never_matching_is_startup_error() {
    // A pattern that matches no phase is a typo, not a no-op.
    let (stdout, stderr, ok) = run_scenario("over_workload_param", &["nosuch_*.cursor=0..10%"]);
    assert!(!ok, "never-matching override pattern must fail at startup");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("matches no phase"),
        "diagnostic: {combined}"
    );
}

#[test]
fn cursor_partitions_phase_override_ambiguous_globs_fatal() {
    // Two distinct globs matching the same phase for the same
    // param is ambiguous.
    let (stdout, stderr, ok) = run_scenario(
        "over_workload_param",
        &["over_*.cursor=0..10%", "*_phase.cursor=0..20%"],
    );
    assert!(!ok, "ambiguous globs must fail");
    let combined = format!("{stdout}\n{stderr}");
    assert!(combined.contains("ambiguous"), "diagnostic: {combined}");
}

#[test]
fn cursor_partitions_over_param_multi_partition_is_startup_error() {
    // SRD 71 §"Single-partition / no-iteration form": a cursor
    // consuming a multi-partition spec directly (no enclosing
    // `for:` iteration) is a startup error naming the missing
    // iteration — never a silent partition-0 run.
    let (stdout, stderr, ok) = run_scenario("over_workload_param", &["cursor=2%,10%,*"]);
    assert!(!ok, "multi-partition spec on a direct `over` must fail");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("for:"),
        "diagnostic should point at the `for:` iteration form: {combined}"
    );
    assert!(
        combined.contains("partitions"),
        "diagnostic should name the partition list: {combined}"
    );
}

#[test]
fn cursor_partitions_over_iter_var() {
    // `over p` where `p` is bound by an outer for-clause.
    // Three partitions (linear:3 against extent 1000) → three
    // per-iteration cursor narrowings.
    let (stdout, stderr, ok) = run_scenario("over_iter_var", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/over_p "),
        vec![
            "cp/over_p idx=0 lo=0 hi=333",
            "cp/over_p idx=1 lo=333 hi=667",
            "cp/over_p idx=2 lo=667 hi=1000",
        ]
    );
}

#[test]
fn cursor_partitions_over_cross_cursor() {
    // `cursor q2 = range(...) over q1.cursor` — q2 reads q1's
    // resolved partition. Both cursors narrow identically.
    let (stdout, stderr, ok) = run_scenario("over_cross_cursor", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/cross "),
        vec![
            "cp/cross idx=0 q1=[0..500) q2=[0..500)",
            "cp/cross idx=1 q1=[500..1000) q2=[500..1000)",
        ]
    );
}

// ─────────────────────────────────────────────────────────────────
// Param-sweep flow — `for: "p in <param>.partitions"`
//
// These exercise the partition PROJECTION WIRE: the workload param
// (default `sweep_cursor`, overridable on the CLI) is resolved to
// its spec string and iterated via the `.partitions` sibling wire —
// NOT a hardcoded `partitions("literal")`. The `cursor=...`-style
// override threads through `run_scenario`'s `extra_args`, and the
// test asserts the projected partition list RE-RESOLVES.
// SRD-71 §"Driving an iteration".
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_param_sweep_default() {
    // `for: "p in sweep_cursor.partitions"` with the workload
    // default `sweep_cursor="linear:4"` → four equal quarter-
    // partitions against the default extent 100. Exercises the
    // param projection-wire sweep (NOT a hardcoded literal).
    let (stdout, stderr, ok) = run_scenario("param_sweep", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/sweep "),
        vec![
            "cp/sweep idx=0 lo=0 hi=25",
            "cp/sweep idx=1 lo=25 hi=50",
            "cp/sweep idx=2 lo=50 hi=75",
            "cp/sweep idx=3 lo=75 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_param_sweep_cli_override_linear() {
    // `sweep_cursor=linear:2` overrides the workload default → the
    // same projection wire now yields two half-partitions. Verifies
    // the CLI param override re-resolves the partition list.
    let (stdout, stderr, ok) = run_scenario("param_sweep", &["sweep_cursor=linear:2"]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/sweep "),
        vec!["cp/sweep idx=0 lo=0 hi=50", "cp/sweep idx=1 lo=50 hi=100",]
    );
}

#[test]
fn cursor_partitions_param_sweep_cli_override_pct_list() {
    // `sweep_cursor=2%,10%,*` — a Form-2 percentage delta list driven
    // through the projection wire. `*` absorbs the remainder, so on
    // extent 100: [0,2) [2,12) [12,100).
    let (stdout, stderr, ok) = run_scenario("param_sweep", &["sweep_cursor=2%,10%,*"]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/sweep "),
        vec![
            "cp/sweep idx=0 lo=0 hi=2",
            "cp/sweep idx=1 lo=2 hi=12",
            "cp/sweep idx=2 lo=12 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_param_sweep_cli_override_recipe() {
    // `sweep_cursor=fib:5` — a Form-3 recipe driven through the
    // projection wire. Fibonacci weights (1,2,3,5,8 → sum 19) against
    // extent 100 give the cumulative boundaries 5/16/32/58/100.
    let (stdout, stderr, ok) = run_scenario("param_sweep", &["sweep_cursor=fib:5"]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/sweep "),
        vec![
            "cp/sweep idx=0 lo=0 hi=5",
            "cp/sweep idx=1 lo=5 hi=16",
            "cp/sweep idx=2 lo=16 hi=32",
            "cp/sweep idx=3 lo=32 hi=58",
            "cp/sweep idx=4 lo=58 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_param_sweep_cli_override_single_range() {
    // `sweep_cursor=0..50%` — a Form-1 single sub-range driven
    // through the projection wire collapses the sweep to one
    // partition [0,50) on extent 100.
    let (stdout, stderr, ok) = run_scenario("param_sweep", &["sweep_cursor=0..50%"]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/sweep "),
        vec!["cp/sweep idx=0 lo=0 hi=50".to_string()]
    );
}

#[test]
fn cursor_partitions_reify_sweep_default() {
    // Reified custom-named param `warmup_sweep` carries the same
    // `.partitions` projection surface as the built-in `cursor`.
    // Default `warmup_sweep="linear:3"` → three thirds of extent 100.
    let (stdout, stderr, ok) = run_scenario("reify_sweep", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/reify "),
        vec![
            "cp/reify idx=0 lo=0 hi=33",
            "cp/reify idx=1 lo=33 hi=67",
            "cp/reify idx=2 lo=67 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_reify_sweep_cli_override() {
    // `warmup_sweep=ratios:1,1,2` overrides the reified param →
    // weights 1,1,2 (sum 4) against extent 100 give [0,25) [25,50)
    // [50,100). Confirms the custom-named param's projection wire
    // re-resolves under a CLI override.
    let (stdout, stderr, ok) = run_scenario("reify_sweep", &["warmup_sweep=ratios:1,1,2"]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/reify "),
        vec![
            "cp/reify idx=0 lo=0 hi=25",
            "cp/reify idx=1 lo=25 hi=50",
            "cp/reify idx=2 lo=50 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_source_over_comprehension() {
    // `for: "p in partitions("linear:4")"` used directly as a
    // comprehension SOURCE (distinct from the param-projection sweep
    // and from the hardcoded `over "literal"` Form-1 usage). The
    // phase narrows a `range(0, 1000)` cursor `over p`, so the bounds
    // re-scale to the cursor's own extent → quarters of 1000.
    let (stdout, stderr, ok) = run_scenario("partitions_source_over", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/source "),
        vec![
            "cp/source idx=0 lo=0 hi=250",
            "cp/source idx=1 lo=250 hi=500",
            "cp/source idx=2 lo=500 hi=750",
            "cp/source idx=3 lo=750 hi=1000",
        ]
    );
}

// ─────────────────────────────────────────────────────────────────
// Stdlib partition functions
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_fn_cardinality() {
    // `linear:3` against extent 300 → each partition has cardinality 100.
    let (stdout, stderr, ok) = run_scenario("fn_cardinality", &[]);
    assert!(ok, "scenario failed: {stderr}");
    let lines = distinct_lines_with_prefix(&stdout, "cp/fn cardinality ");
    assert_eq!(lines.len(), 3);
    for line in &lines {
        assert!(
            line.contains("n=100"),
            "expected cardinality 100, got: {line}"
        );
    }
}

#[test]
fn cursor_partitions_fn_idx_and_bounds() {
    // `linear:4` against default extent 100 → idx 0..3, bounds in 25-step quarters.
    let (stdout, stderr, ok) = run_scenario("fn_idx_and_bounds", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/fn bounds "),
        vec![
            "cp/fn bounds idx=0 lo=0 hi=25",
            "cp/fn bounds idx=1 lo=25 hi=50",
            "cp/fn bounds idx=2 lo=50 hi=75",
            "cp/fn bounds idx=3 lo=75 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_fn_mod_in() {
    // `mod_in(cycle, p)` where p is the sole linear:1 partition
    // [0..100). cycle 0..4 maps to 0, 1, 2, 3, 4 (no wrap).
    let (stdout, stderr, ok) = run_scenario("fn_mod_in", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/fn mod_in "),
        vec![
            "cp/fn mod_in v=0",
            "cp/fn mod_in v=1",
            "cp/fn mod_in v=2",
            "cp/fn mod_in v=3",
            "cp/fn mod_in v=4",
        ]
    );
}

#[test]
fn cursor_partitions_fn_at() {
    // `at(p, cycle)` where p is [0..100). cycle 0..2 → 0, 1, 2.
    let (stdout, stderr, ok) = run_scenario("fn_at", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/fn at "),
        vec!["cp/fn at v=0", "cp/fn at v=1", "cp/fn at v=2",]
    );
}

#[test]
fn cursor_partitions_fn_clamp_in() {
    // `clamp_in(cycle, p)` where p is [0..100). cycle 0..4 are
    // all inside the range → no saturation, just pass-through.
    let (stdout, stderr, ok) = run_scenario("fn_clamp_in", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/fn clamp_in "),
        vec![
            "cp/fn clamp_in v=0",
            "cp/fn clamp_in v=1",
            "cp/fn clamp_in v=2",
            "cp/fn clamp_in v=3",
            "cp/fn clamp_in v=4",
        ]
    );
}

#[test]
fn cursor_partitions_fn_random_in() {
    // `random_in(p, cycle)` where p is [0..100): every value
    // lands inside the partition, and the mapping is a hash —
    // a second run reproduces the same sequence.
    let (stdout, stderr, ok) = run_scenario("fn_random_in", &[]);
    assert!(ok, "scenario failed: {stderr}");
    let lines = lines_with_prefix(&stdout, "cp/fn random_in ");
    assert_eq!(lines.len(), 5, "five cycles → five values: {lines:?}");
    for line in &lines {
        let v: u64 = line
            .rsplit("v=")
            .next()
            .unwrap()
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("unparseable value in `{line}`: {e}"));
        assert!(v < 100, "random_in escaped the partition: {line}");
    }
    let (stdout2, stderr2, ok2) = run_scenario("fn_random_in", &[]);
    assert!(ok2, "second run failed: {stderr2}");
    assert_eq!(
        lines_with_prefix(&stdout2, "cp/fn random_in "),
        lines,
        "random_in must be deterministic per seed"
    );
}

#[test]
fn cursor_partitions_dotted_scalar_projections() {
    // SRD 71 §"Cursor metadata wires": `q.cursor.idx`,
    // `.partition_count`, `.start_ordinal`, `.end_ordinal`
    // resolve as typed scalar wires in bindings AND in `{...}`
    // text interpolation (the `ord=` field reads the dotted
    // name directly in the op template).
    let (stdout, stderr, ok) = run_scenario("dotted_projections", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/dotted "),
        vec![
            "cp/dotted i=0 n=3 lo=0 hi=333 ord=333",
            "cp/dotted i=1 n=3 lo=333 hi=667 ord=667",
            "cp/dotted i=2 n=3 lo=667 hi=1000 ord=1000",
        ]
    );
}

#[test]
fn cursor_partitions_fn_subdivide() {
    // `subdivide(p, 4)` over [0..100) → a PartitionList of four
    // quarter-partitions, boundary-identical to the `*/4` spec
    // token.
    let (stdout, stderr, ok) = run_scenario("fn_subdivide", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/fn subdivide "),
        vec!["cp/fn subdivide subs=PartitionList[4]=[0..25),[25..50),[50..75),[75..100)",]
    );
}

// ─────────────────────────────────────────────────────────────────
// Partition-bound open-extent cursors
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_open_extent_capped_by_partition() {
    // `until_passes(10, 5)` over partition [0, 25): the policy
    // wants 5 passes (50 cycles), the partition caps it at 25 —
    // the source terminates the moment the partition is
    // exhausted, per SRD 71 §"Interaction with existing cursor
    // surface".
    let (stdout, stderr, ok) = run_scenario("open_extent_over", &[]);
    assert!(ok, "scenario failed: {stderr}");
    let lines = lines_with_prefix(&stdout, "cp/open_extent ");
    assert_eq!(
        lines.len(),
        25,
        "exactly the partition's cardinality: {lines:?}"
    );
    // Ordinals stay inside the partition.
    for line in &lines {
        let n: u64 = line
            .rsplit("n=")
            .next()
            .unwrap()
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("unparseable `{line}`: {e}"));
        assert!(n < 25, "ordinal escaped the partition: {line}");
    }
}

// ─────────────────────────────────────────────────────────────────
// Status banner — `partition i/n [lo..hi)`
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_status_banner_on_iteration() {
    // SRD 71 §"Status / report integration": each iteration of a
    // multi-partition sweep announces its active partition
    // (1-based for display) with the condensed effective range.
    let (stdout, stderr, ok) = run_scenario("over_iter_var", &[]);
    assert!(ok, "scenario failed: {stderr}");
    let combined = format!("{stdout}\n{stderr}");
    for needle in [
        "partition 1/3 [0..333)",
        "partition 2/3 [333..667)",
        "partition 3/3 [667..1000)",
    ] {
        assert!(
            combined.contains(needle),
            "missing banner `{needle}` in output:\n{combined}"
        );
    }
}

#[test]
fn cursor_partitions_status_banner_suppressed_for_single() {
    // A single-partition spec (no iteration) stays silent — no
    // `partition 1/1` noise.
    let (stdout, stderr, ok) = run_scenario("over_workload_param", &["cursor=0..50%"]);
    assert!(ok, "scenario failed: {stderr}");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        !combined.contains("partition 1/1"),
        "single-partition runs must not emit the banner:\n{combined}"
    );
}

// ─────────────────────────────────────────────────────────────────
// Nested subdivision — `subdivide(outer, n)` as a source
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_nested_subdivide_source() {
    // Outer `partitions("50%,*", 1000)` → two windows; each
    // subdivides into 2 → four leaf runs with contiguous
    // 250-ordinal bounds. The inner clause resolves `outer`
    // through the kernel's scope chain.
    let (stdout, stderr, ok) = run_scenario("hierarchical_sweep", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/nested "),
        vec![
            "cp/nested outer=0 inner=0 lo=0 hi=250",
            "cp/nested outer=0 inner=1 lo=250 hi=500",
            "cp/nested outer=1 inner=0 lo=500 hi=750",
            "cp/nested outer=1 inner=1 lo=750 hi=1000",
        ]
    );
}

// ─────────────────────────────────────────────────────────────────
// Reified custom-named cursor parameters
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_reified_warmup_steady() {
    // Workload declares `warmup_cursor: "0..10%"` and
    // `steady_cursor: "10%..100%"`. Each phase's cursor names
    // its corresponding parameter; the two are independently
    // controlled. Operator sees `warmup_cursor=` / `steady_cursor=`
    // as the public surface.
    let (stdout, stderr, ok) = run_scenario("reified_warmup_steady", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/warmup "),
        vec!["cp/warmup lo=0 hi=100".to_string()]
    );
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/steady "),
        vec!["cp/steady lo=100 hi=1000".to_string()]
    );
}

#[test]
fn cursor_partitions_reified_warmup_steady_with_cli_override() {
    // Operator overrides `warmup_cursor=0..1%` — cursor narrows
    // to [0, 10). `steady_cursor` keeps its workload default.
    let (stdout, stderr, ok) = run_scenario("reified_warmup_steady", &["warmup_cursor=0..1%"]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/warmup "),
        vec!["cp/warmup lo=0 hi=10".to_string()]
    );
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/steady "),
        vec!["cp/steady lo=100 hi=1000".to_string()]
    );
}

// ─────────────────────────────────────────────────────────────────
// Negative: cursor without `over` ignores cursor=...
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_no_over_ignores_cursor_param() {
    // Phase's cursor declared without `over` should keep its
    // full extent (50) regardless of the CLI `cursor=` value.
    let (stdout, stderr, ok) = run_scenario("no_over_ignores_cursor_param", &["cursor=0..10%"]);
    assert!(ok, "scenario failed: {stderr}");
    // 50 emits because the cursor's full extent is 50 ordinals.
    assert_eq!(
        lines_with_prefix(&stdout, "cp/no_over ").len(),
        50,
        "expected 50 emits (cursor's full extent), got {}",
        lines_with_prefix(&stdout, "cp/no_over ").len()
    );
}

// ─────────────────────────────────────────────────────────────────
// Clean rejection delivery (SRD-71 §"Rejection diagnostics")
//
// A malformed partition spec in comprehension position must surface
// the VERBATIM polydat diagnostic as the PRIMARY error: the run
// fails, the named fragment appears in stderr, and neither a panic
// backtrace nor a downstream "type mismatch: cannot connect String
// output to ext input" leak is present. The scenarios all point at
// the `noop_emit` phase, which ignores `p` so the spec diagnostic is
// never masked by a String→ext type mismatch.
//
// These scenarios are NAMED-ONLY — never the walked scenario — so the
// example walker (which runs only `windowed_chunking`) is unaffected.
// ─────────────────────────────────────────────────────────────────

/// Assert the four-part rejection contract for a scenario run.
fn assert_clean_rejection(stdout: &str, stderr: &str, ok: bool, fragment: &str) {
    assert!(!ok, "bad spec must fail the run; stderr: {stderr}");
    assert!(
        stderr.contains(fragment),
        "verbatim diagnostic fragment `{fragment}` missing from stderr: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "rejection must not panic: {stderr}"
    );
    assert!(
        !stderr.contains("type mismatch"),
        "spec diagnostic must not be masked by a type mismatch: {stderr}"
    );
    let _ = stdout;
}

#[test]
fn cursor_partitions_reject_delta_overshoot_pct() {
    // `60%,60%` sums to 120% of the default extent 100 → the delta
    // list overshoots the cursor's extent. SRD 71 §"Over-sum lists".
    let (stdout, stderr, ok) = run_scenario("reject_delta_overshoot_pct", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "delta list sums to 120 ordinals, exceeding the cursor's extent 100; \
         trim the list or use a `*` remainder to absorb the overflow",
    );
}

#[test]
fn cursor_partitions_reject_delta_overshoot_literal() {
    // `10000,5000` against extent 10000 → sums to 15000 ordinals,
    // overshooting the explicit extent.
    let (stdout, stderr, ok) = run_scenario("reject_delta_overshoot_literal", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "delta list sums to 15000 ordinals, exceeding the cursor's extent 10000",
    );
}

#[test]
fn cursor_partitions_reject_unknown_recipe() {
    // A bad recipe name surfaces the verbatim diagnostic with the
    // supported-recipe list. SRD 71 §"Form 3 pre-baked recipes".
    let (stdout, stderr, ok) = run_scenario("reject_unknown_recipe", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "unknown recipe `frobnicate` — supported: linear, ratios, mul, bin, \
         fib, ln, geom, zipf, pareto, front_heavy, back_heavy",
    );
}

#[test]
fn cursor_partitions_reject_unknown_order() {
    // A bad order suffix surfaces the supported-order list verbatim.
    // SRD 71 §"Ordering suffix".
    let (stdout, stderr, ok) = run_scenario("reject_unknown_order", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "unknown order `sideways` — supported: unchanged, smallest_first, \
         largest_first, random",
    );
}

#[test]
fn cursor_partitions_reject_bad_window() {
    // `linear:4 in 50%` — the `in` window must be a `start..end`
    // range, not a bare sized value. SRD 71 §"Windowed chunking".
    let (stdout, stderr, ok) = run_scenario("reject_bad_window", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "the window after `in` must be a `start..end` range; got `50%`",
    );
}

#[test]
fn cursor_partitions_reject_tail_pct_divisor() {
    // `*/1%` — the divisor after `*/` is a chunk COUNT and must be a
    // bare integer; a percentage divisor is rejected with a teaching
    // hint pointing at the fill form. SRD 71 §"Tail tokens".
    let (stdout, stderr, ok) = run_scenario("reject_tail_pct_divisor", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "the divisor after `*/` is a chunk count and must be a bare integer",
    );
}

#[test]
fn cursor_partitions_reject_tail_linear_split() {
    // `*/linear:4` — an equal-count remainder split spelled as a
    // recipe; the diagnostic redirects to the canonical `*/4` form.
    let (stdout, stderr, ok) = run_scenario("reject_tail_linear_split", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "spell an equal-count remainder split as `*/4` — `*/N` is the \
         canonical form",
    );
}

#[test]
fn cursor_partitions_reject_double_star() {
    // `*,*` — two remainder tokens; at most one is allowed.
    let (stdout, stderr, ok) = run_scenario("reject_double_star", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "at most one remainder token (`*`, `...`, `*/N`, or `*/recipe`) is \
         allowed in a delta list; got 2 in `*,*`",
    );
}

#[test]
fn cursor_partitions_reject_double_tail() {
    // `90%,...,*` — two distinct tail tokens; only one is allowed.
    let (stdout, stderr, ok) = run_scenario("reject_double_tail", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "at most one remainder token (`*`, `...`, `*/N`, or `*/recipe`) is \
         allowed in a delta list; got 2 in `90%,...,*`",
    );
}

#[test]
fn cursor_partitions_reject_x0() {
    // `1%x0` — a zero repetition count; must be >= 1.
    let (stdout, stderr, ok) = run_scenario("reject_x0", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "`1%x0`: the repetition count must be >= 1",
    );
}

#[test]
fn cursor_partitions_reject_repeated_tail() {
    // `*/4,...` — entries trailing a star tail; the tail must be the
    // last entry in the delta list.
    let (stdout, stderr, ok) = run_scenario("reject_repeated_tail", &[]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "`*/4` consumes the rest of the extent and must be the last entry \
         in the delta list; got trailing entries after it",
    );
}

#[test]
fn cursor_partitions_reject_param_bad_spec() {
    // Param-flow rejection (① + ②): the `for:` iterates the
    // `reject_cursor.partitions` projection wire and the test drives a
    // bad spec via the CLI `reject_cursor=` override. The diagnostic
    // must still surface cleanly through the param-resolution path —
    // no panic, no type mismatch.
    let (stdout, stderr, ok) =
        run_scenario("reject_param_bad_spec", &["reject_cursor=frobnicate:3"]);
    assert_clean_rejection(
        &stdout,
        &stderr,
        ok,
        "unknown recipe `frobnicate` — supported: linear, ratios, mul, bin, \
         fib, ln, geom, zipf, pareto, front_heavy, back_heavy",
    );
}

// ─────────────────────────────────────────────────────────────────
// forms-a: uncovered Form 1 range spellings
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_form1_both_pct() {
    // `over "0%..53%"` — both endpoints percentage — over
    // range(0, 1000) → [0, 530).
    let (stdout, stderr, ok) = run_scenario("form1_both_pct", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=0 hi=530".to_string()]
    );
}

#[test]
fn cursor_partitions_form1_bracket_trailing_pct() {
    // `over "[0..53)%"` — `%` trails the closing bracket and applies
    // to the whole range → [0, 530).
    let (stdout, stderr, ok) = run_scenario("form1_bracket_trailing_pct", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=0 hi=530".to_string()]
    );
}

#[test]
fn cursor_partitions_form1_bracket_square_close() {
    // `over "[0..53%]"` — square-bracket close marker is advisory;
    // closure is always [start, end) → [0, 530).
    let (stdout, stderr, ok) = run_scenario("form1_bracket_square_close", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=0 hi=530".to_string()]
    );
}

#[test]
fn cursor_partitions_form1_both_fraction() {
    // `over "0.05..0.5"` — both endpoints fraction (5% to 50%) over
    // range(0, 1000) → [50, 500).
    let (stdout, stderr, ok) = run_scenario("form1_both_fraction", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=50 hi=500".to_string()]
    );
}

#[test]
fn cursor_partitions_form1_literal_both() {
    // `over "100..1000"` — both endpoints literal ordinals over
    // range(0, 10000) → [100, 1000). The second literal is the
    // ABSOLUTE end ordinal (per-endpoint type), not a count.
    let (stdout, stderr, ok) = run_scenario("form1_literal_both", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=100 hi=1000".to_string()]
    );
}

#[test]
fn cursor_partitions_form1_literal_end() {
    // `over "0..1000"` — literal end over range(0, 10000) →
    // [0, 1000) (the first 1000 rows).
    let (stdout, stderr, ok) = run_scenario("form1_literal_end", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/form1 "),
        vec!["cp/form1 lo=0 hi=1000".to_string()]
    );
}

// ─────────────────────────────────────────────────────────────────
// forms-a: Form 2 under-100% drops-remainder (literal / mixed)
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_form2_literal_drops_remainder() {
    // `partitions("1000,5000", 10000)` — literal deltas, no `*`.
    // Sum 6000 < extent 10000 → trailing 4000-ordinal gap dropped.
    let (stdout, stderr, ok) = run_scenario("form2_literal_drops_remainder", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=1000",
            "cp/iter idx=1 lo=1000 hi=6000",
        ]
    );
}

#[test]
fn cursor_partitions_form2_mixed_drops_remainder() {
    // `partitions("1000,10%", 10000)` — mixed literal + percentage
    // deltas, no `*`. First delta 1000 ords, second 10% of 10000 =
    // 1000; sum 2000 < extent → trailing 8000-ordinal gap dropped.
    let (stdout, stderr, ok) = run_scenario("form2_mixed_drops_remainder", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=1000",
            "cp/iter idx=1 lo=1000 hi=2000",
        ]
    );
}

// ─────────────────────────────────────────────────────────────────
// Form 2 tail tokens — uncovered spellings (group `tails`)
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_form2_star_midlist() {
    // `partitions("2%,*,10%")` — `*` is position-independent and
    // may sit MID-LIST, absorbing the middle remainder.
    let (stdout, stderr, ok) = run_scenario("form2_star_midlist", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=2",
            "cp/iter idx=1 lo=2 hi=90",
            "cp/iter idx=2 lo=90 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_form2_head_star() {
    // `partitions("90%,*")` — head plus a single-remainder `*`:
    // exactly two partitions.
    let (stdout, stderr, ok) = run_scenario("form2_head_star", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec!["cp/iter idx=0 lo=0 hi=90", "cp/iter idx=1 lo=90 hi=100",]
    );
}

#[test]
fn cursor_partitions_form2_plain_pct_star() {
    // `partitions("2%,10%,*")` — bare `*` (no decorative `%`) is
    // interchangeable with the `*%` spelling.
    let (stdout, stderr, ok) = run_scenario("form2_plain_pct_star", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=2",
            "cp/iter idx=1 lo=2 hi=12",
            "cp/iter idx=2 lo=12 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_form2_star_split_16() {
    // `partitions("*/16")` — no head, so `*/N` divides the whole
    // extent into sixteen near-equal partitions.
    let (stdout, stderr, ok) = run_scenario("form2_star_split_16", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=6",
            "cp/iter idx=1 lo=6 hi=13",
            "cp/iter idx=10 lo=63 hi=69",
            "cp/iter idx=11 lo=69 hi=75",
            "cp/iter idx=12 lo=75 hi=81",
            "cp/iter idx=13 lo=81 hi=88",
            "cp/iter idx=14 lo=88 hi=94",
            "cp/iter idx=15 lo=94 hi=100",
            "cp/iter idx=2 lo=13 hi=19",
            "cp/iter idx=3 lo=19 hi=25",
            "cp/iter idx=4 lo=25 hi=31",
            "cp/iter idx=5 lo=31 hi=38",
            "cp/iter idx=6 lo=38 hi=44",
            "cp/iter idx=7 lo=44 hi=50",
            "cp/iter idx=8 lo=50 hi=56",
            "cp/iter idx=9 lo=56 hi=63",
        ]
    );
}

#[test]
fn cursor_partitions_star_split_equals_linear() {
    // `*/16` (whole-extent split) and `linear:16` (whole-extent
    // recipe) are DISTINCT specs that produce byte-identical
    // boundaries — SRD 71 §"Tail tokens" `*/N` ≡ `linear:N`.
    let (split_out, split_err, split_ok) = run_scenario("form2_star_split_16", &[]);
    let (lin_out, lin_err, lin_ok) = run_scenario("form2_linear_16", &[]);
    assert!(split_ok, "*/16 scenario failed: {split_err}");
    assert!(lin_ok, "linear:16 scenario failed: {lin_err}");
    assert_eq!(
        distinct_lines_with_prefix(&split_out, "cp/iter "),
        distinct_lines_with_prefix(&lin_out, "cp/iter "),
        "*/16 and linear:16 must produce identical partitions",
    );
}

#[test]
fn cursor_partitions_form2_star_shaped_fib() {
    // `partitions("90%,*/fib:5", 1000)` — `*/recipe` shapes the
    // 100-ordinal remainder by the fib weights [1,2,3,5,8].
    let (stdout, stderr, ok) = run_scenario("form2_star_shaped_fib", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=900",
            "cp/iter idx=1 lo=900 hi=905",
            "cp/iter idx=2 lo=905 hi=916",
            "cp/iter idx=3 lo=916 hi=932",
            "cp/iter idx=4 lo=932 hi=958",
            "cp/iter idx=5 lo=958 hi=1000",
        ]
    );
}

#[test]
fn cursor_partitions_form2_star_shaped_nohead() {
    // `partitions("*/ratios:1,3", 1000)` — `*/recipe` with no
    // head: the remainder is the whole extent, shaped 25%/75%.
    let (stdout, stderr, ok) = run_scenario("form2_star_shaped_nohead", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec!["cp/iter idx=0 lo=0 hi=250", "cp/iter idx=1 lo=250 hi=1000",]
    );
}

// ─────────────────────────────────────────────────────────────────
// recipes group: uncovered recipe + order forms
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_order_smallest_first() {
    // `partitions("front_heavy:4 smallest_first")` — front_heavy
    // generates largest-first, so the size-ascending sort reverses
    // the schedule: idx sequence 3, 2, 1, 0.
    let (stdout, stderr, ok) = run_scenario("order_smallest_first", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=3 lo=90 hi=100",
            "cp/iter idx=2 lo=70 hi=90",
            "cp/iter idx=1 lo=40 hi=70",
            "cp/iter idx=0 lo=0 hi=40",
        ]
    );
}

#[test]
fn cursor_partitions_order_unchanged() {
    // `partitions("front_heavy:4 unchanged")` — the explicit
    // self-documenting keyword for generation order: idx 0,1,2,3.
    let (stdout, stderr, ok) = run_scenario("order_unchanged", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=40",
            "cp/iter idx=1 lo=40 hi=70",
            "cp/iter idx=2 lo=70 hi=90",
            "cp/iter idx=3 lo=90 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_order_random_front_heavy() {
    // `partitions("front_heavy:4 random")` — a deterministic
    // shuffle over a NON-uniform recipe. Assert it is a permutation
    // of the four partitions and that two runs are byte-identical.
    let (stdout, stderr, ok) = run_scenario("order_random_front_heavy", &[]);
    assert!(ok, "scenario failed: {stderr}");
    let first = lines_with_prefix(&stdout, "cp/iter ");
    assert_eq!(first.len(), 4, "four partitions: {first:?}");
    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            "cp/iter idx=0 lo=0 hi=40",
            "cp/iter idx=1 lo=40 hi=70",
            "cp/iter idx=2 lo=70 hi=90",
            "cp/iter idx=3 lo=90 hi=100",
        ],
        "shuffle is a permutation of the same partitions"
    );
    let (stdout2, stderr2, ok2) = run_scenario("order_random_front_heavy", &[]);
    assert!(ok2, "second run failed: {stderr2}");
    assert_eq!(
        lines_with_prefix(&stdout2, "cp/iter "),
        first,
        "random order must be deterministic across runs"
    );
}

#[test]
fn cursor_partitions_recipe_geom_decay() {
    // `geom:4,0.5` (R < 1) → front-loaded decay. Weights
    // [1, 0.5, 0.25, 0.125] → [0,53), [53,80), [80,93), [93,100).
    let (stdout, stderr, ok) = run_scenario("recipe_geom_decay", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=53",
            "cp/iter idx=1 lo=53 hi=80",
            "cp/iter idx=2 lo=80 hi=93",
            "cp/iter idx=3 lo=93 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_recipe_ratios_head() {
    // `ratios:3,1,1,1` (sum 6) heavy-head → 50%, ~16.7% x3.
    let (stdout, stderr, ok) = run_scenario("recipe_ratios_head", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=50",
            "cp/iter idx=1 lo=50 hi=67",
            "cp/iter idx=2 lo=67 hi=83",
            "cp/iter idx=3 lo=83 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_recipe_ratios_quadratic() {
    // `ratios:1,4,9,16` (squares, sum 30) → quadratic growth.
    let (stdout, stderr, ok) = run_scenario("recipe_ratios_quadratic", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=3",
            "cp/iter idx=1 lo=3 hi=17",
            "cp/iter idx=2 lo=17 hi=47",
            "cp/iter idx=3 lo=47 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_recipe_fib7() {
    // `fib:7` worked SRD example: weights [1,2,3,5,8,13,21] sum 53
    // → [0,2),[2,6),[6,11),[11,21),[21,36),[36,60),[60,100).
    let (stdout, stderr, ok) = run_scenario("recipe_fib7", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=2",
            "cp/iter idx=1 lo=2 hi=6",
            "cp/iter idx=2 lo=6 hi=11",
            "cp/iter idx=3 lo=11 hi=21",
            "cp/iter idx=4 lo=21 hi=36",
            "cp/iter idx=5 lo=36 hi=60",
            "cp/iter idx=6 lo=60 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_recipe_mul_growth() {
    // `mul:2.3` GROWTH case — multiplies by 2.3 until each term's
    // contribution drops below 0.1% of the running total → 10
    // terms. Asserts count only (boundary arithmetic is float-
    // sensitive).
    let (stdout, stderr, ok) = run_scenario("recipe_mul_growth", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/count ").len(),
        10,
        "mul:2.3 growth should produce 10 partitions"
    );
}

#[test]
fn cursor_partitions_recipe_linear_equivalences() {
    // SRD: `linear:N ≡ */N ≡ ratios:1…1 ≡ geom:N,1`. Assert that
    // ratios:1,1,1,1 and geom:4,1 both produce the same four
    // uniform quarter-partitions as linear:4.
    let expected = vec![
        "cp/iter idx=0 lo=0 hi=25".to_string(),
        "cp/iter idx=1 lo=25 hi=50".to_string(),
        "cp/iter idx=2 lo=50 hi=75".to_string(),
        "cp/iter idx=3 lo=75 hi=100".to_string(),
    ];
    let (ratios_out, ratios_err, ratios_ok) = run_scenario("recipe_ratios_uniform", &[]);
    assert!(ratios_ok, "ratios scenario failed: {ratios_err}");
    assert_eq!(
        distinct_lines_with_prefix(&ratios_out, "cp/iter "),
        expected,
        "ratios:1,1,1,1 must equal linear:4"
    );
    let (geom_out, geom_err, geom_ok) = run_scenario("recipe_geom_unit", &[]);
    assert!(geom_ok, "geom scenario failed: {geom_err}");
    assert_eq!(
        distinct_lines_with_prefix(&geom_out, "cp/iter "),
        expected,
        "geom:4,1 must equal linear:4"
    );
}

#[test]
fn cursor_partitions_recipe_zipf_pareto_identity() {
    // SRD: `zipf:s,N` and `pareto:alpha,N` are the same math
    // (1/k^s ≡ (1/k)^s). Assert the two recipes produce
    // byte-identical partition bounds.
    let (zipf_out, zipf_err, zipf_ok) = run_scenario("recipe_zipf_bounds", &[]);
    assert!(zipf_ok, "zipf scenario failed: {zipf_err}");
    let (pareto_out, pareto_err, pareto_ok) = run_scenario("recipe_pareto_bounds", &[]);
    assert!(pareto_ok, "pareto scenario failed: {pareto_err}");
    let zipf = distinct_lines_with_prefix(&zipf_out, "cp/iter ");
    assert_eq!(
        zipf,
        vec![
            "cp/iter idx=0 lo=0 hi=48",
            "cp/iter idx=1 lo=48 hi=72",
            "cp/iter idx=2 lo=72 hi=88",
            "cp/iter idx=3 lo=88 hi=100",
        ]
    );
    assert_eq!(
        distinct_lines_with_prefix(&pareto_out, "cp/iter "),
        zipf,
        "pareto:1,4 must produce the same bounds as zipf:1,4"
    );
}

#[test]
fn cursor_partitions_recipe_zipf_heavy_head() {
    // `zipf:2,4` — heavier head than s=1. Asserts count only
    // (float-sensitive); complements recipe_zipf (s=1).
    let (stdout, stderr, ok) = run_scenario("recipe_zipf_heavy_head", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(distinct_lines_with_prefix(&stdout, "cp/count ").len(), 4);
}

// ─────────────────────────────────────────────────────────────────
// modwin group: modifier/window interactions
// ─────────────────────────────────────────────────────────────────

#[test]
fn cursor_partitions_repetition_with_open_tail() {
    // `partitions("1%x5,*")` — finite `xN` repetition (five 1%
    // chunks) composed with a `*` open tail that absorbs the whole
    // remainder as one partition.
    let (stdout, stderr, ok) = run_scenario("rep_open_tail", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=0 hi=1",
            "cp/iter idx=1 lo=1 hi=2",
            "cp/iter idx=2 lo=2 hi=3",
            "cp/iter idx=3 lo=3 hi=4",
            "cp/iter idx=4 lo=4 hi=5",
            "cp/iter idx=5 lo=5 hi=100",
        ]
    );
}

#[test]
fn cursor_partitions_gap_with_star_tail() {
    // `partitions("10%,~40%,*")` — a `~` gap between a sized delta
    // and a `*` tail. The gap consumes [10,50) without emitting; it
    // counts toward the allocated total, so `*` absorbs only the
    // remaining [50,100).
    let (stdout, stderr, ok) = run_scenario("gap_with_star", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec!["cp/iter idx=0 lo=0 hi=10", "cp/iter idx=1 lo=50 hi=100",]
    );
}

#[test]
fn cursor_partitions_windowed_fib() {
    // `partitions("fib:5 in 10%..90%", 1000)` — a non-linear recipe
    // composed with a window. The window is [100,900) (800 ordinals)
    // and the fib:5 weights (1,2,3,5,8 sum 19) chunk it WINDOW-
    // relative with cumulative rounding, landing exactly on 900.
    let (stdout, stderr, ok) = run_scenario("windowed_fib", &[]);
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        distinct_lines_with_prefix(&stdout, "cp/iter "),
        vec![
            "cp/iter idx=0 lo=100 hi=142",
            "cp/iter idx=1 lo=142 hi=226",
            "cp/iter idx=2 lo=226 hi=353",
            "cp/iter idx=3 lo=353 hi=563",
            "cp/iter idx=4 lo=563 hi=900",
        ]
    );
}
