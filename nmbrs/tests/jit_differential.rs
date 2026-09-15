// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-105 Push 2 — workload-level differential battery.
//!
//! Every scenario of the stdlib coverage workload (plus the
//! expression examples) runs twice through the real binary — once
//! with `jit=off` (interpreter baseline) and once with `jit=force`
//! (every eligible node fused) — and the emitted op output must be
//! byte-identical. Polydat's determinism makes the comparison
//! exact; any divergence is a cone-extraction or marshalling bug,
//! never acceptable noise. This complements the in-crate battery
//! (polydat's `function_coverage` suite compiles every expression
//! on both engines) by exercising the full workload path:
//! op-template synthesis, scope chains, capture wires, and the
//! stdout dispenser.
//!
//! PARKED on polydat 0.3: the interpreter entry points fix the JIT
//! mode at `auto` and expose no per-compile override for programs
//! with traversals, so the runner refuses `jit=off`/`jit=force`
//! (see the SRD-105 note). The three differential tests are
//! `#[ignore]`d with that reason; `unknown_jit_value_is_rejected`
//! still runs.

use std::path::{Path, PathBuf};
use std::process::Command;

const STDLIB_WORKLOAD: &str = "examples/workloads/expressions/stdlib_coverage.yaml";

/// Scenario names from the stdlib coverage workload — the same set
/// nmbrs/tests/stdlib.rs pins exact output lines for.
const STDLIB_SCENARIOS: &[&str] = &[
    "arithmetic_u64",
    "arithmetic_f64",
    "arithmetic_named_nodes",
    "bitwise_ops",
    "comparison_ops",
    "conversion_to_f64_to_u64",
    "format_u64_bases",
    "hash_chain",
    "string_interpolation",
    "encoding_hex",
    "encoding_base64",
    "encoding_url",
    "digest_sha256",
    "digest_md5",
    "probability_unit_interval",
    "distribution_uniform",
    "fair_coin_flip",
    "weighted_strings_pick",
    "weighted_u64_pick",
    "pick_select_blend",
    "lerp_and_clamp",
    "noise_perlin_2d",
    "date_components",
    "json_round_trip",
    "regex_match_and_replace",
];

struct SessionDir {
    path: PathBuf,
}

impl SessionDir {
    fn new(label: &str) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!("nmbrs-jit-diff-{label}-{pid}-{nanos}"));
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

/// Run one (workload, scenario) through the binary with the given
/// jit mode, returning the op output (stdout).
fn run_with_mode(workload: &str, scenario: Option<&str>, jit: &str) -> String {
    let session = SessionDir::new(jit);
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(workspace_root)
        .arg("run")
        .arg("--session-path")
        .arg(&session.path)
        .arg(format!("workload={workload}"))
        .arg(format!("jit={jit}"))
        .arg("tui=off");
    if let Some(sc) = scenario {
        cmd.arg(format!("scenario={sc}"));
    }
    let out = cmd.output().expect("run nmbrs");
    assert!(
        out.status.success(),
        "workload={workload} scenario={scenario:?} jit={jit} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Assert byte-identical op output between the interpreter baseline
/// and forced cone extraction.
fn assert_differential(workload: &str, scenario: Option<&str>) {
    let off = run_with_mode(workload, scenario, "off");
    let force = run_with_mode(workload, scenario, "force");
    assert_eq!(
        off, force,
        "SRD-105 differential: jit=force diverged from jit=off for \
         workload={workload} scenario={scenario:?}"
    );
}

#[test]
#[ignore = "polydat 0.3 fixes the interpreter kernel's JIT mode at auto and exposes no per-compile override; jit=off/force are refused by the runner (SRD-105 note) — re-enable when CompileOptions carries a jit_mode"]
fn stdlib_coverage_is_differential() {
    for scenario in STDLIB_SCENARIOS {
        assert_differential(STDLIB_WORKLOAD, Some(scenario));
    }
}

#[test]
#[ignore = "polydat 0.3 fixes the interpreter kernel's JIT mode at auto and exposes no per-compile override; jit=off/force are refused by the runner (SRD-105 note) — re-enable when CompileOptions carries a jit_mode"]
fn expression_examples_are_differential() {
    assert_differential("examples/workloads/expressions/math_and_bitwise.yaml", None);
}

#[test]
#[ignore = "polydat 0.3 fixes the interpreter kernel's JIT mode at auto and exposes no per-compile override; jit=off/force are refused by the runner (SRD-105 note) — re-enable when CompileOptions carries a jit_mode"]
fn getting_started_bindings_are_differential() {
    assert_differential(
        "examples/workloads/getting_started/polydat_bindings.yaml",
        None,
    );
}

/// An unknown jit value is a routed configuration error, not a
/// silent fallback.
#[test]
fn unknown_jit_value_is_rejected() {
    let session = SessionDir::new("badval");
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(workspace_root)
        .arg("run")
        .arg("--session-path")
        .arg(&session.path)
        .arg(format!("workload={STDLIB_WORKLOAD}"))
        .arg("scenario=hash_chain")
        .arg("jit=fast")
        .arg("tui=off")
        .output()
        .expect("run nmbrs");
    assert!(!out.status.success(), "jit=fast must be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown jit value"),
        "diagnostic names the bad value: {stderr}"
    );
}
