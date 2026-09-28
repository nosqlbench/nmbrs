// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Type-stable shared cells (scope_model.md §"Type stability: a cell
//! keeps ONE type for life"), end-to-end through the real binary.
//!
//! The incident this pins: a result binding wrote an F64
//! (`floor_decade(...)`) into a cell declared `shared measured := 1`
//! (U64), silently flipping the cell's runtime type; a bridge compiled
//! against the declared type later panicked `expected U64, got F64` at
//! a READ tiers from the cause, killing a worker with no context. The
//! contract now rejects the narrowing write AT THE WRITE, with the cell
//! name and the fix in the message — and lossless widening (u64 value
//! into an f64 cell) still heals silently.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

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
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create_dir_all {dir:?}: {e}"));
    dir.push(format!(
        "nmbrs_typestable_{label}_{}.yaml",
        std::process::id()
    ));
    let mut f = std::fs::File::create(&dir).unwrap_or_else(|e| panic!("create {dir:?}: {e}"));
    f.write_all(body.as_bytes())
        .unwrap_or_else(|e| panic!("write {dir:?}: {e}"));
    dir
}

fn run(workload: &std::path::Path, extra: &[&str]) -> (String, String, bool) {
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-typestable-{}-{}-{}",
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
    let _ = std::fs::remove_dir_all(&session_parent);
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

/// The incident shape: an F64 result-binding write into a U64-declared
/// cell. Must FAIL at the write with the type-stable diagnostic — not
/// flip the cell and panic at some later read.
#[test]
fn narrowing_write_into_u64_cell_fails_with_diagnostic() {
    let wl = write_workload(
        "narrow",
        r#"
bindings: |
  shared measured := 1
phases:
  probe:
    adapter: stdout
    cycles: 1
    ops:
      write:
        op: "probe"
        result: |
          measured := 900.5
"#,
    );
    let (_stdout, stderr, ok) = run(&wl, &[]);
    assert!(!ok, "narrowing write must fail the run, stderr={stderr}");
    assert!(
        stderr.contains("type-stable cell violation"),
        "write-site diagnostic expected: {stderr}"
    );
    assert!(
        stderr.contains("measured"),
        "diagnostic names the cell: {stderr}"
    );
    assert!(
        stderr.contains("trunc_u64"),
        "diagnostic points at the explicit cast: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "must be a routed error, never a panic: {stderr}"
    );
}

/// The corrected shape: the cell declared F64 (`1.0`) accepts the F64
/// write; a u64 write into the same f64 cell heals by widening. Both
/// runs complete.
#[test]
fn f64_cell_accepts_f64_and_widened_u64_writes() {
    let wl = write_workload(
        "widen",
        r#"
bindings: |
  shared measured := 1.0
phases:
  write_f64:
    adapter: stdout
    cycles: 1
    ops:
      w1:
        op: "wf"
        result: |
          measured := 900.5
  write_u64:
    adapter: stdout
    cycles: 1
    ops:
      w2:
        op: "wu"
        result: |
          measured := 900
  show:
    adapter: stdout
    cycles: 1
    bindings: |
      m := measured
    ops:
      s:
        op: "m={m}"
"#,
    );
    let (stdout, stderr, ok) = run(&wl, &[]);
    assert!(
        ok,
        "matching + widening writes must succeed, stderr={stderr}"
    );
    assert!(
        stdout.contains("m=900"),
        "the widened u64 write (900 → 900.0) must be the final value: {stdout}"
    );
}

/// The explicit narrowing escape hatch: `trunc_u64(...)` lets an f64
/// expression land in a u64 cell deliberately.
#[test]
fn trunc_u64_is_the_explicit_narrowing_path() {
    let wl = write_workload(
        "cast",
        r#"
bindings: |
  shared measured := 1
phases:
  probe:
    adapter: stdout
    cycles: 1
    ops:
      write:
        op: "probe"
        result: |
          measured := trunc_u64(900.9)
  show:
    adapter: stdout
    cycles: 1
    bindings: |
      m := measured
    ops:
      s:
        op: "m={m}"
"#,
    );
    let (stdout, stderr, ok) = run(&wl, &[]);
    assert!(
        ok,
        "explicit trunc_u64 narrowing must succeed, stderr={stderr}"
    );
    assert!(
        stdout.contains("m=900"),
        "truncated value lands in the u64 cell: {stdout}"
    );
}

/// Piece 4 — the explicit cell type annotation: `shared m: f64 := 1`
/// pins the cell to F64 despite the integer literal (the annotation
/// wins over literal inference), so the F64 write heals nothing and
/// just lands. The `1`-vs-`1.0` subtlety stops being load-bearing.
#[test]
fn type_annotation_pins_the_cell_type() {
    let wl = write_workload(
        "annot",
        r#"
bindings: |
  shared measured: f64 := 1
phases:
  probe:
    adapter: stdout
    cycles: 1
    ops:
      write:
        op: "probe"
        result: |
          measured := 900.5
  show:
    adapter: stdout
    cycles: 1
    bindings: |
      m := measured
    ops:
      s:
        op: "m={m}"
"#,
    );
    let (stdout, stderr, ok) = run(&wl, &[]);
    assert!(
        ok,
        "annotated f64 cell must accept the f64 write, stderr={stderr}"
    );
    assert!(
        stdout.contains("m=900.5"),
        "annotation pinned the cell to f64: {stdout}"
    );
}

/// A mismatched annotation (str cell, integer initializer) is a
/// COMPILE error at the declaration — not a runtime surprise.
#[test]
fn mismatched_annotation_is_a_compile_error() {
    let wl = write_workload(
        "annot_bad",
        r#"
bindings: |
  shared name: str := 42
phases:
  p:
    adapter: stdout
    cycles: 1
    ops:
      o:
        op: "x"
"#,
    );
    let (_stdout, stderr, ok) = run(&wl, &[]);
    assert!(!ok, "mismatched annotation must fail, stderr={stderr}");
    assert!(
        stderr.contains("doesn't match the annotated type") || stderr.contains("shared binding"),
        "compile-time declaration diagnostic expected: {stderr}"
    );
}

mod scratch;
