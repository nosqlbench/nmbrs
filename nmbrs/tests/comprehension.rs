// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-18b / 18c / 18d / 18e comprehension surface coverage.
//!
//! Each test runs one scenario from
//! `nmbrs/examples/workloads/iteration/comprehension_coverage.yaml` and asserts
//! the expected iteration shape: form (single var / multi-clause
//! / array / union), range operator, named generator, set
//! operator, traversal order, parallel-iter, `all(<cursor>)`,
//! `where` filter, or dependent clauses.
//!
//! The workload YAML carries the operator-readable shape
//! vocabulary; tests verify behavior numerically by grepping
//! `cmp/<prefix>` emit lines and counting / comparing them.

use std::path::{Path, PathBuf};
use std::process::Command;

const WORKLOAD: &str = "nmbrs/examples/workloads/iteration/comprehension_coverage.yaml";

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
        let parent =
            std::env::temp_dir().join(format!("nmbrs-comprehension-coverage-{pid}-{nanos}"));
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

fn run_scenario(scenario: &str) -> (String, String, bool) {
    let session = SessionDir::new();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(workspace_root)
        .arg("run")
        .arg("--session-path")
        .arg(&session.path)
        .arg(format!("workload={WORKLOAD}"))
        .arg(format!("scenario={scenario}"));
    let out = cmd.output().expect("run nmbrs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (stdout, stderr, out.status.success())
}

fn lines_with_prefix(stdout: &str, prefix: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|l| l.starts_with(prefix))
        .map(|l| l.to_string())
        .collect()
}

fn sorted_lines_with_prefix(stdout: &str, prefix: &str) -> Vec<String> {
    let mut lines = lines_with_prefix(stdout, prefix);
    lines.sort();
    lines
}

// ─────────────────────────────────────────────────────────────────
// Form shapes
// ─────────────────────────────────────────────────────────────────

#[test]
fn comprehension_form_single_var() {
    let (stdout, stderr, ok) = run_scenario("form_single_var");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/x "),
        vec!["cmp/x x=1", "cmp/x x=2", "cmp/x x=3",]
    );
}

#[test]
fn comprehension_form_for_keyword_synonym() {
    // `for_each:` and `for:` produce identical comprehensions.
    let (stdout, stderr, ok) = run_scenario("form_for_keyword_synonym");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/x "),
        vec!["cmp/x x=1", "cmp/x x=2", "cmp/x x=3",]
    );
}

#[test]
fn comprehension_form_inline_cartesian() {
    // 3 × 2 = 6 tuples.
    let (stdout, stderr, ok) = run_scenario("form_inline_cartesian");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/xy "),
        vec![
            "cmp/xy x=1 y=a",
            "cmp/xy x=1 y=b",
            "cmp/xy x=2 y=a",
            "cmp/xy x=2 y=b",
            "cmp/xy x=3 y=a",
            "cmp/xy x=3 y=b",
        ]
    );
}

#[test]
fn comprehension_form_array_cartesian() {
    // Array form ≡ inline form when names are distinct.
    let (stdout, stderr, ok) = run_scenario("form_array_cartesian");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(sorted_lines_with_prefix(&stdout, "cmp/xy ").len(), 6);
}

#[test]
fn comprehension_form_array_single_entry_multi_clause() {
    // Array with one entry containing both clauses ≡ inline form.
    let (stdout, stderr, ok) = run_scenario("form_array_single_entry_multi_clause");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(sorted_lines_with_prefix(&stdout, "cmp/xy ").len(), 6);
}

#[test]
fn comprehension_form_repeated_var_union() {
    // Repeated `x` → union of 3 single-tuple sub-spaces.
    let (stdout, stderr, ok) = run_scenario("form_repeated_var_union");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/x "),
        vec!["cmp/x x=1", "cmp/x x=2", "cmp/x x=3",]
    );
}

#[test]
fn comprehension_form_subspace_union() {
    // Two multi-clause sub-spaces, joined as union. 6 tuples total —
    // 3 from sub-space 1 (x=10, y={a,b,c}) and 3 from sub-space 2
    // (x=100, y={d,e,f}). Cartesian would have visited 18.
    let (stdout, stderr, ok) = run_scenario("form_subspace_union");
    assert!(ok, "scenario failed: {stderr}");
    let lines = sorted_lines_with_prefix(&stdout, "cmp/xy ");
    assert_eq!(lines.len(), 6);
    for line in &lines {
        let is_subspace1 =
            line.contains("x=10 y=a") || line.contains("x=10 y=b") || line.contains("x=10 y=c");
        let is_subspace2 =
            line.contains("x=100 y=d") || line.contains("x=100 y=e") || line.contains("x=100 y=f");
        assert!(
            is_subspace1 || is_subspace2,
            "line not in either sub-space: {line}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────
// `where` filter
// ─────────────────────────────────────────────────────────────────

#[test]
fn comprehension_where_cartesian_filter() {
    // 2 × 4 = 8 candidates, filtered by k*limit < 1000:
    //   keep: (10,1), (10,10), (10,50), (100,1)
    //   skip: (10,100)=1000, (100,10)=1000, (100,50)=5000, (100,100)=10000
    let (stdout, stderr, ok) = run_scenario("where_cartesian_filter");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/kl "),
        vec![
            "cmp/kl k=10 limit=1",
            "cmp/kl k=10 limit=10",
            "cmp/kl k=10 limit=50",
            "cmp/kl k=100 limit=1",
        ]
    );
}

#[test]
fn comprehension_where_union_filter() {
    // Union 6 candidates, filtered by k > 2:
    //   sub-space 1: (3) — 1, 2 dropped
    //   sub-space 2: (10), (20), (30) — all kept
    let (stdout, stderr, ok) = run_scenario("where_union_filter");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/k "),
        vec!["cmp/k k=10", "cmp/k k=20", "cmp/k k=3", "cmp/k k=30",]
    );
}

// ─────────────────────────────────────────────────────────────────
// Range operator
// ─────────────────────────────────────────────────────────────────

#[test]
fn comprehension_range_half_open() {
    // 1..5 → 1, 2, 3, 4 (4 values).
    let (stdout, stderr, ok) = run_scenario("range_half_open");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/n "),
        vec!["cmp/n n=1", "cmp/n n=2", "cmp/n n=3", "cmp/n n=4",]
    );
}

#[test]
fn comprehension_range_inclusive() {
    // 1..=5 → 1, 2, 3, 4, 5 (5 values).
    let (stdout, stderr, ok) = run_scenario("range_inclusive");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/n "),
        vec![
            "cmp/n n=1",
            "cmp/n n=2",
            "cmp/n n=3",
            "cmp/n n=4",
            "cmp/n n=5",
        ]
    );
}

#[test]
fn comprehension_range_with_step() {
    // 1..10..2 → 1, 3, 5, 7, 9.
    let (stdout, stderr, ok) = run_scenario("range_with_step");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/n "),
        vec![
            "cmp/n n=1",
            "cmp/n n=3",
            "cmp/n n=5",
            "cmp/n n=7",
            "cmp/n n=9",
        ]
    );
}

#[test]
fn comprehension_range_inclusive_with_step() {
    // 1..=10..2 → 1, 3, 5, 7, 9 (10 isn't reached by step from 1).
    let (stdout, stderr, ok) = run_scenario("range_inclusive_with_step");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/n "),
        vec![
            "cmp/n n=1",
            "cmp/n n=3",
            "cmp/n n=5",
            "cmp/n n=7",
            "cmp/n n=9",
        ]
    );
}

// ─────────────────────────────────────────────────────────────────
// Named generators
// ─────────────────────────────────────────────────────────────────

#[test]
fn comprehension_gen_fib_count() {
    // fib(7) → first 7 Fibonacci: 1, 1, 2, 3, 5, 8, 13.
    let (stdout, stderr, ok) = run_scenario("gen_fib_count");
    assert!(ok, "scenario failed: {stderr}");
    let lines = lines_with_prefix(&stdout, "cmp/n ");
    assert_eq!(lines.len(), 7);
    // Sum of first 7 Fibonacci = 1+1+2+3+5+8+13 = 33.
    let sum: u64 = lines
        .iter()
        .filter_map(|l| l.strip_prefix("cmp/n n="))
        .filter_map(|s| s.parse::<u64>().ok())
        .sum();
    assert_eq!(sum, 33);
}

#[test]
fn comprehension_gen_pow2_count() {
    // pow2(6) → 1, 2, 4, 8, 16, 32.
    let (stdout, stderr, ok) = run_scenario("gen_pow2_count");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/n "),
        vec![
            "cmp/n n=1",
            "cmp/n n=16",
            "cmp/n n=2",
            "cmp/n n=32",
            "cmp/n n=4",
            "cmp/n n=8",
        ]
    );
}

#[test]
fn comprehension_gen_geometric() {
    // geometric(1, 2, 5) → 1, 2, 4, 8, 16.
    let (stdout, stderr, ok) = run_scenario("gen_geometric");
    assert!(ok, "scenario failed: {stderr}");
    let lines = lines_with_prefix(&stdout, "cmp/n ");
    assert_eq!(lines.len(), 5, "geometric(1,2,5) should produce 5 terms");
}

#[test]
fn comprehension_gen_linear_starts() {
    // linear_starts(0.0, 1.0, 5) → 0.0, 0.2, 0.4, 0.6, 0.8 (half-open).
    let (stdout, stderr, ok) = run_scenario("gen_linear_starts");
    assert!(ok, "scenario failed: {stderr}");
    let lines = lines_with_prefix(&stdout, "cmp/n ");
    assert_eq!(lines.len(), 5);
}

#[test]
fn comprehension_gen_linear_steps() {
    // linear_steps(0, 100, 5) → 0, 25, 50, 75, 100 (inclusive).
    let (stdout, stderr, ok) = run_scenario("gen_linear_steps");
    assert!(ok, "scenario failed: {stderr}");
    let lines = lines_with_prefix(&stdout, "cmp/n ");
    assert_eq!(lines.len(), 5);
}

// ─────────────────────────────────────────────────────────────────
// Set operators
// ─────────────────────────────────────────────────────────────────

#[test]
fn comprehension_setop_union() {
    // [1,2,3] | [3,4,5] → 1, 2, 3, 4, 5 (5 distinct).
    let (stdout, stderr, ok) = run_scenario("setop_union");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/n "),
        vec![
            "cmp/n n=1",
            "cmp/n n=2",
            "cmp/n n=3",
            "cmp/n n=4",
            "cmp/n n=5",
        ]
    );
}

#[test]
fn comprehension_setop_intersect() {
    // [1,2,3] & [2,3,4] → 2, 3.
    let (stdout, stderr, ok) = run_scenario("setop_intersect");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/n "),
        vec!["cmp/n n=2", "cmp/n n=3",]
    );
}

#[test]
fn comprehension_setop_difference() {
    // [1,2,3,4] - [2,4] → 1, 3.
    let (stdout, stderr, ok) = run_scenario("setop_difference");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/n "),
        vec!["cmp/n n=1", "cmp/n n=3",]
    );
}

// ─────────────────────────────────────────────────────────────────
// Traversal orders
// ─────────────────────────────────────────────────────────────────

#[test]
fn comprehension_order_lex_with_truncation() {
    // `order: "lex/5"` → first 5 in lex order: 1, 2, 3, 4, 5.
    // Runner exits non-zero due to "not run" warning on the
    // 5 truncated phases — truncation is expected, so we
    // assert on the line content rather than exit status.
    let (stdout, _stderr, _ok) = run_scenario("order_lex_with_truncation");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/n "),
        vec![
            "cmp/n n=1",
            "cmp/n n=2",
            "cmp/n n=3",
            "cmp/n n=4",
            "cmp/n n=5",
        ]
    );
}

// ─────────────────────────────────────────────────────────────────
// Tuple LHS / parallel-iter
// ─────────────────────────────────────────────────────────────────

#[test]
fn comprehension_tuple_lhs_zip_strict() {
    // `(a, b) in (1..=3, 10..=30..10)` → strict zip:
    //   (a=1, b=10), (a=2, b=20), (a=3, b=30).
    let (stdout, stderr, ok) = run_scenario("tuple_lhs_zip_strict");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/ab "),
        vec!["cmp/ab a=1 b=10", "cmp/ab a=2 b=20", "cmp/ab a=3 b=30",]
    );
}

// ─────────────────────────────────────────────────────────────────
// `all(<cursor>)` generator
// ─────────────────────────────────────────────────────────────────

#[test]
fn comprehension_cursor_all_full_extent() {
    // Cursor `row = range(0, 50)` → 50 iterations.
    let (stdout, stderr, ok) = run_scenario("cursor_all_full_extent");
    assert!(ok, "scenario failed: {stderr}");
    let lines = lines_with_prefix(&stdout, "cmp/xval ");
    assert_eq!(lines.len(), 50);
}

#[test]
fn comprehension_cursor_all_with_where() {
    // `all(row) where {xval} % 10 == 0` → 5 multiples of 10:
    //   0, 10, 20, 30, 40.
    let (stdout, stderr, ok) = run_scenario("cursor_all_with_where");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/xval "),
        vec![
            "cmp/xval xval=0",
            "cmp/xval xval=10",
            "cmp/xval xval=20",
            "cmp/xval xval=30",
            "cmp/xval xval=40",
        ]
    );
}

#[test]
fn comprehension_cursor_all_with_lex_truncate() {
    // `all(row)` with `order: "lex/5"` → first 5 of cursor's
    // 50 ordinals. Truncation produces a non-zero exit status
    // (warning about un-run phases); we assert on content only.
    let (stdout, _stderr, _ok) = run_scenario("cursor_all_with_lex_truncate");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/xval "),
        vec![
            "cmp/xval xval=0",
            "cmp/xval xval=1",
            "cmp/xval xval=2",
            "cmp/xval xval=3",
            "cmp/xval xval=4",
        ]
    );
}

// ─────────────────────────────────────────────────────────────────
// Dependent clauses
// ─────────────────────────────────────────────────────────────────

#[test]
fn comprehension_dependent_clauses() {
    // `k in {k_values}, limit in {k_{k}_limits}` —
    //   k=1   → limit in k_1_limits   = 1, 2
    //   k=10  → limit in k_10_limits  = 10, 20, 30
    // Total: 5 tuples.
    let (stdout, stderr, ok) = run_scenario("dependent_clauses");
    assert!(ok, "scenario failed: {stderr}");
    assert_eq!(
        sorted_lines_with_prefix(&stdout, "cmp/kl "),
        vec![
            "cmp/kl k=1 limit=1",
            "cmp/kl k=1 limit=2",
            "cmp/kl k=10 limit=10",
            "cmp/kl k=10 limit=20",
            "cmp/kl k=10 limit=30",
        ]
    );
}
