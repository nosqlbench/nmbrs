// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs bench wiring --cones` — the SRD-105 lattice report
//! (catchup C2): cones formed for the production (jit=auto)
//! compile, interpreter residue with per-node lattice headroom.

use std::process::Command;

fn bench(expr: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .args(["bench", "wiring", expr, "--cones", "cycles=100", "iters=1"])
        .output()
        .expect("run nmbrs bench");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn cones_flag_reports_fusion_and_residue() {
    let text = bench("default_or(add(mul(cycle, 3), 7), 9)");
    assert!(
        text.contains("Lattice (jit=auto)"),
        "section header: {text}"
    );
    assert!(
        text.contains("jit_cone[mul+add]"),
        "cone with members: {text}"
    );
    assert!(text.contains("default_or"), "residue listed: {text}");
    assert!(
        text.contains("p3-classifiable") || text.contains("p2-capable") || text.contains("p1-only"),
        "residue carries lattice tiers: {text}"
    );
}

#[test]
fn tier_table_p1_row_stays_interpreter() {
    // The P1 row must be the true interpreter even though auto is
    // the process default — the structural summary shows the
    // unextracted node count (4), not one fused cone.
    let text = bench("hash(mod(add(mul(cycle, 3), 7), 1000))");
    assert!(text.contains("4 nodes"), "structural summary: {text}");
    assert!(
        text.contains("1 cone (4 nodes fused)"),
        "lattice view: {text}"
    );
}

#[test]
fn infix_star_is_an_expression_not_a_glob() {
    // `cycle * 3` used to be eaten by glob expansion ("matched no
    // files"); only `.polydat` path patterns glob now.
    let text = bench("hash((cycle * 3) + 7)");
    assert!(
        !text.contains("matched no files"),
        "infix multiply must not glob: {text}"
    );
    assert!(text.contains("Lattice (jit=auto)"), "expr benched: {text}");
    assert!(text.contains("jit_cone["), "and fused: {text}");
}
