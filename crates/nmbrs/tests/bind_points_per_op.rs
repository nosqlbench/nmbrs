// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! A bind point must be validated against the program that owns it.
//!
//! `validate_bind_points` checked every op template against the ACTIVITY-wide
//! Polydat program. An op that owns its own kernel — because it declares
//! `bindings:`, or because its adapter materialises one — keeps its bindings in
//! that op-template program and nowhere else, so its own binding was reported
//! as unresolvable.
//!
//! The trigger was indirect enough to look like a scoping rule: whether an op
//! got its own kernel depended on the PHASE, so `{x}` in `stmt:`/`raw:` worked
//! in a bare phase and failed once the phase declared `bindings:` or
//! `metrics:`. It failed at runtime, not at load, and the same name kept
//! rendering correctly in `memo:`/`gutter:` (those read the live wires rather
//! than this check) — so the op looked half-wired rather than mis-validated.

use std::path::PathBuf;
use std::process::Command;

struct Sandbox {
    path: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("nmbrs-bindpoints-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&path).expect("create sandbox");
        Self { path }
    }

    fn run(&self, name: &str, body: &str) -> String {
        std::fs::write(self.path.join(name), body).expect("write workload");
        let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
            .args([
                "run",
                &format!("workload={name}"),
                "adapter=stdout",
                "--session-keep=1000",
            ])
            .current_dir(&self.path)
            .output()
            .expect("spawn nmbrs");
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// An op's own binding, interpolated into `stmt:`, in a phase that declares
/// `bindings:`. This is the case that failed.
#[test]
fn an_op_binding_resolves_in_stmt_when_the_phase_also_has_bindings() {
    let sb = Sandbox::new("phase-bindings");
    let out = sb.run(
        "w.yaml",
        r#"
params:
  adapter: stdout
phases:
  t:
    cycles: 1
    bindings: |
      const window_ms := 86400000.0
    ops:
      probe:
        bindings: |
          computed := round_u64(current_epoch_millis() - window_ms)
        stmt: "since={computed}"
"#,
    );
    assert!(
        !out.contains("unresolved bind point"),
        "an op's own binding must validate against its own program:\n{out}"
    );
    // Not vacuous: the value must actually render, and as a real epoch.
    let line = out
        .lines()
        .find(|l| l.trim_start().starts_with("since="))
        .unwrap_or_else(|| panic!("no rendered stmt in:\n{out}"));
    let n: u64 = line
        .trim()
        .trim_start_matches("since=")
        .parse()
        .unwrap_or_else(|e| panic!("non-numeric render {line:?}: {e}"));
    assert!(
        n > 1_700_000_000_000,
        "expected an epoch-ms render, got {n}"
    );
}

/// The same, with `metrics:` as the phase-level declaration — the shape the
/// compaction workload actually uses, and the one that blocked bounding a
/// query by the phase clock.
#[test]
fn an_op_binding_resolves_in_stmt_when_the_phase_has_metrics() {
    let sb = Sandbox::new("phase-metrics");
    let out = sb.run(
        "w.yaml",
        r#"
params:
  adapter: stdout
phases:
  t:
    cycles: 1
    metrics:
      started: { kind: gauge, value: "phase_start" }
    ops:
      probe:
        bindings: |
          volatile since_phase := current_epoch_millis() - phase_start
        stmt: "elapsed={since_phase}"
"#,
    );
    assert!(
        !out.contains("unresolved bind point"),
        "an op in a phase with metrics must resolve its own binding:\n{out}"
    );
    assert!(
        out.lines().any(|l| l.trim_start().starts_with("elapsed=")),
        "the op must render its bind point:\n{out}"
    );
}

/// The guard the fix must not remove: a genuinely undeclared name still fails.
/// Validating per-op could otherwise have been "fixed" by validating against
/// nothing at all.
#[test]
fn a_truly_undeclared_bind_point_still_fails() {
    let sb = Sandbox::new("still-fails");
    let out = sb.run(
        "w.yaml",
        r#"
params:
  adapter: stdout
phases:
  t:
    cycles: 1
    bindings: |
      const window_ms := 1.0
    ops:
      probe:
        bindings: |
          computed := 1.0
        stmt: "value={nonexistent_name}"
"#,
    );
    // Either rejection is fine — the earlier placeholder check usually gets
    // there first with better wording. What matters is that it IS rejected.
    assert!(
        out.contains("unresolved bind point") || out.contains("undeclared placeholder"),
        "an undeclared name must still be rejected:\n{out}"
    );
}
