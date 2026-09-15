// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-13c / 13d / 13f scope-model coverage tests.
//!
//! Each test runs one scenario from
//! `nmbrs/examples/workloads/scope/scope_coverage.yaml` and asserts the
//! expected behavior of one construct:
//!
//! - All four shared-cell scalar types (u64, f64, str, bool)
//! - `const NAME := <literal>` modifier
//! - Derived binding consuming a shared cell value
//! - `for_each` chain at three scope levels
//! - Nested `for_each` (inner spec references outer iter-var)
//! - `do_while` chain
//! - Conditional op gated by a shared bool (`if:` field)
//! - Multi-cell op template (multiple shared cells in placeholders)
//! - The default scenario chaining every other scenario
//!
//! The workload YAML carries the operator-readable shape
//! vocabulary; tests verify behavior numerically by counting
//! and comparing `cm/<scenario> ...` emit lines.

use std::path::{Path, PathBuf};
use std::process::Command;

const WORKLOAD: &str = "nmbrs/examples/workloads/scope/scope_coverage.yaml";

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
        let parent = std::env::temp_dir().join(format!("nmbrs-scope-coverage-{pid}-{nanos}"));
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
        .arg(format!("workload={WORKLOAD}"));
    if !scenario.is_empty() {
        cmd.arg(format!("scenario={scenario}"));
    }
    let out = cmd.output().expect("run nmbrs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (stdout, stderr, out.status.success())
}

// ─────────────────────────────────────────────────────────────────
// Shared cells: all four scalar types with edge values
// ─────────────────────────────────────────────────────────────────

#[test]
fn scope_shared_types_all_four_with_edge_values() {
    let (stdout, stderr, ok) = run_scenario("shared_types");
    assert!(ok, "scenario failed: {stderr}");
    assert!(stderr.contains("all phases complete"), "stderr: {stderr}");

    let expected_lines = [
        "cm/shared_types kind=u64 sub=zero value=0",
        "cm/shared_types kind=u64 sub=big value=1000",
        "cm/shared_types kind=f64 sub=zero value=0",
        "cm/shared_types kind=f64 sub=pi value=3.14159",
        "cm/shared_types kind=str sub=empty value=[]",
        "cm/shared_types kind=str sub=text value=matrix",
        "cm/shared_types kind=bool sub=on value=true",
        "cm/shared_types kind=bool sub=off value=false",
    ];
    for expected in expected_lines {
        assert!(
            stdout.contains(expected),
            "missing line `{expected}` in:\n{stdout}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────
// `const NAME := <literal>` modifier
// ─────────────────────────────────────────────────────────────────

#[test]
fn scope_const_modifier_round_trips() {
    let (stdout, stderr, ok) = run_scenario("const_modifier");
    assert!(ok, "scenario failed: {stderr}");
    assert!(stderr.contains("all phases complete"), "stderr: {stderr}");
    assert!(
        stdout.contains("cm/const base_dim=256"),
        "const-modifier value missing:\n{stdout}"
    );
}

// ─────────────────────────────────────────────────────────────────
// Derived binding consuming a shared cell
// ─────────────────────────────────────────────────────────────────

#[test]
fn scope_derived_binding_consumes_shared_cell() {
    // `doubled_count := mul(count_big, 2)` reads the cell value
    // (1000) through standard Polydat wiring. Output must show the
    // multiplied result, proving the cell value flows downstream
    // (not just visible via `lookup`).
    let (stdout, stderr, ok) = run_scenario("derived_from_shared");
    assert!(ok, "scenario failed: {stderr}");
    assert!(stderr.contains("all phases complete"), "stderr: {stderr}");
    assert!(
        stdout.contains("cm/derived count_big=1000 doubled=2000"),
        "derived binding output missing:\n{stdout}"
    );
}

// ─────────────────────────────────────────────────────────────────
// `for_each` chain — three scope levels
// ─────────────────────────────────────────────────────────────────

#[test]
fn scope_for_each_chain_three_levels() {
    // 3-level chain: workload → for_each → phase. The phase
    // sees both the for_each iter var (0,1,2 from the CSV) and
    // the workload-scope shared values via the bind_outer_scope
    // chain.
    let (stdout, stderr, ok) = run_scenario("for_each_chain");
    assert!(ok, "scenario failed: {stderr}");
    assert!(stderr.contains("all phases complete"), "stderr: {stderr}");

    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("cm/for_each_chain "))
        .collect();
    assert_eq!(lines.len(), 3, "expected 3 iterations:\n{stdout}");
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(
            line,
            &format!("cm/for_each_chain iter={i} count_big=1000 label=matrix"),
            "line {i} shape mismatch: {line}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────
// `do_while` chain — three scope levels
// ─────────────────────────────────────────────────────────────────

#[test]
fn scope_do_while_chain_three_levels() {
    let (stdout, stderr, ok) = run_scenario("do_while_chain");
    assert!(ok, "scenario failed: {stderr}");
    assert!(stderr.contains("all phases complete"), "stderr: {stderr}");

    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("cm/do_while_chain "))
        .collect();
    assert_eq!(lines.len(), 3, "expected 3 do-while iterations:\n{stdout}");
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(
            line,
            &format!("cm/do_while_chain i={i} count_big=1000 label=matrix"),
            "line {i} shape mismatch: {line}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────
// Conditional op gated by a shared bool
// ─────────────────────────────────────────────────────────────────

#[test]
fn scope_conditional_op_gated_by_shared_bool() {
    let (stdout, stderr, ok) = run_scenario("conditional_op");
    assert!(ok, "scenario failed: {stderr}");
    assert!(stderr.contains("all phases complete"), "stderr: {stderr}");

    // gated_on (if: flag_on=true) fires, gated_off (if:
    // flag_off=false) is suppressed, always fires.
    assert!(
        stdout.contains("cm/conditional gated=on result=fired"),
        "gated_on (true) must fire:\n{stdout}"
    );
    assert!(
        stdout.contains("cm/conditional gated=always result=fired"),
        "always must fire:\n{stdout}"
    );
    assert!(
        !stdout.contains("cm/conditional gated=off"),
        "gated_off (false) must be suppressed:\n{stdout}"
    );
}

// ─────────────────────────────────────────────────────────────────
// Multi-cell op template (no cross-name interference)
// ─────────────────────────────────────────────────────────────────

#[test]
fn scope_multi_cell_no_cross_name_interference() {
    let (stdout, stderr, ok) = run_scenario("multi_cell");
    assert!(ok, "scenario failed: {stderr}");
    assert!(stderr.contains("all phases complete"), "stderr: {stderr}");
    assert!(
        stdout.contains("cm/multi_cell zero=0 big=1000 pi=3.14159 text=matrix on=true off=false"),
        "multi-cell output missing or malformed:\n{stdout}"
    );
}

// ─────────────────────────────────────────────────────────────────
// Nested for_each (inner spec references outer iter-var)
// ─────────────────────────────────────────────────────────────────

#[test]
fn scope_nested_for_each_inner_sees_outer_iter_var() {
    // Inner for_each spec `inner in pre_{outer}` references the
    // outer for_each's iter var. The fix routes the inner's
    // dispatcher to the outer's *live execution* kernel (via
    // `effective_parent_kernel` preferring `ctx.current_parent_kernel`
    // over the scope-tree canonical ancestor), so spec
    // interpolation resolves `{outer}` to the current iteration's
    // value rather than the canonical default.
    let (stdout, stderr, ok) = run_scenario("nested_for_each");
    assert!(ok, "scenario failed: {stderr}");
    assert!(stderr.contains("all phases complete"), "stderr: {stderr}");

    // Outer iterates over a,b,c — three iterations. Each
    // inner_for_each enumerates a single value (`pre_<outer>`),
    // so the leaf phase fires three times total.
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("cm/nested "))
        .collect();
    assert_eq!(
        lines.len(),
        3,
        "expected 3 nested-iteration leaf calls:\n{stdout}"
    );
    for (outer, expected) in [("a", "pre_a"), ("b", "pre_b"), ("c", "pre_c")] {
        let target = format!("cm/nested outer={outer} inner={expected}");
        assert!(
            lines.contains(&target.as_str()),
            "missing nested line `{target}`:\n{stdout}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────
// Default scenario — chains all of the above
// ─────────────────────────────────────────────────────────────────

#[test]
fn scope_default_runs_full_matrix() {
    // Round-trip: every other scenario's identifying line
    // appears at least once in the default-scenario combined
    // output.
    let (stdout, stderr, ok) = run_scenario("");
    assert!(ok, "scenario failed: {stderr}");
    assert!(stderr.contains("all phases complete"), "stderr: {stderr}");

    let prefixes = [
        "cm/shared_types ",
        "cm/const ",
        "cm/derived ",
        "cm/for_each_chain ",
        "cm/do_while_chain ",
        "cm/conditional ",
        "cm/multi_cell ",
    ];
    for prefix in prefixes {
        assert!(
            stdout.lines().any(|l| l.starts_with(prefix)),
            "default scenario missing prefix `{prefix}`:\n{stdout}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────
// Documented-but-not-yet-exercised tests
// ─────────────────────────────────────────────────────────────────
//
// Each `#[test]` below pairs with a commented-out scenario at
// the bottom of `scope_coverage.yaml`. When the corresponding
// construct lands, uncomment both halves together. Keeping them
// here makes the gap visible in the test binary's symbol table
// without breaking compilation.

// #[test]
// fn scope_concurrent_shared_writes_lwwins() {
//     // Concurrent for_each branches all decrement the same
//     // shared cell. Last-write-wins baseline (SRD-16
//     // §"Concurrent semantics: last-write-wins") makes the
//     // exact final count non-deterministic, so this test is
//     // gated on the templated patterns shipping
//     // (`shared(atomic)` / `shared(sum)`). The kernel-level
//     // test `shared_last_write_wins_under_concurrent_writers`
//     // covers the Mutex serialization API surface today.
// }

// #[test]
// fn scope_state_driven_do_while_termination() {
//     // do_while: "{count_big} > 0" with a child phase that
//     // decrements `count_big`. Requires per-cycle propagation
//     // from the leaf phase's per-cycle kernel back to the
//     // loop scope's SharedCell, which isn't yet wired (no GK
//     // binding syntax targets an outer shared cell, and the
//     // dispatcher only writes the counter, not arbitrary
//     // shared names). Tracked in SRD-18b §"Open: per-cycle
//     // propagation".
// }

// #[test]
// fn scope_cursor_driven_phase() {
//     // `cycles: ===auto` with a cursor-bound source. Needs a
//     // mock source the adapter-testkit doesn't currently
//     // provide. Vectordata integration tests cover this with
//     // real datasets; the synthetic single-file demo is gated
//     // on a mock source crate.
// }

// #[test]
// fn scope_delay_field_reads_shared_cell() {
//     // `delay: rate_pi` resolves the cell value per cycle.
//     // Mechanically should work today, but verifying delay
//     // application requires timing measurement that stdout
//     // grepping can't perform. Gated on a metric-based
//     // assertion path that compares wall-clock to expected.
// }
