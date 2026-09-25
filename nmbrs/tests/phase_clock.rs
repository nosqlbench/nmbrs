// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The phase-scoped clock must be readable BY THE OPS, not only at the phase's
//! completion pull.
//!
//! `phase_start` was declared as an extern on the phase kernel and filled by
//! the executor on a fresh subscope at the completion-time metric pull. That
//! made `time_to_index: now_ms - phase_start` work and everything else lie: an
//! op reading `phase_start` while the phase was still running got the declared
//! default of `0` and silently computed against the Unix epoch. Nothing failed
//! — the number was just wrong, which is the worst way for a clock to break.
//!
//! `phase_start_millis()` / `phase_elapsed_millis()` are the runtime-provided
//! nodes for it, anchored to an origin the executor scopes once per phase in
//! `run_phase`. The session-scoped `elapsed_millis()` is NOT a substitute: in a
//! run that sweeps many phases it answers a different question, and these tests
//! pin the difference.

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
        let path = std::env::temp_dir().join(format!("nmbrs-phase-clock-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&path).expect("create sandbox");
        Self { path }
    }

    fn write(&self, name: &str, body: &str) {
        std::fs::write(self.path.join(name), body).expect("write workload");
    }

    fn run(&self, workload: &str) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
            .args([
                "run",
                &format!("workload={workload}"),
                "adapter=stdout",
                // Never let a test's throwaway sessions retire real ones.
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

/// Pull `key=<digits>` out of the line beginning with `tag`.
fn field(out: &str, tag: &str, key: &str) -> u64 {
    let line = out
        .lines()
        .find(|l| l.trim_start().starts_with(tag))
        .unwrap_or_else(|| panic!("no line starting with {tag:?} in:\n{out}"));
    let needle = format!("{key}=");
    let rest = line
        .split(&needle)
        .nth(1)
        .unwrap_or_else(|| panic!("no {needle:?} on {line:?}"));
    rest.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|e| panic!("non-numeric {key} on {line:?}: {e}"))
}

/// Two phases separated by a 3s delay phase. The phase clock must restart for
/// the second phase while the session clock keeps running — that difference is
/// the entire reason the node exists.
#[test]
fn the_phase_clock_restarts_each_phase_while_the_session_clock_does_not() {
    let sb = Sandbox::new("scoped");
    sb.write(
        "clock.yaml",
        r#"
params:
  adapter: stdout
phases:
  p1:
    cycles: 1
    ops:
      a:
        bindings: |
          volatile ps := phase_start_millis()
          volatile pe := phase_elapsed_millis()
          volatile se := current_epoch_millis() - session_start
        stmt: "P1 phase_start={ps} phase_elapsed={pe} session_elapsed={se}"
  wait:
    cycles: 1
    ops:
      s:
        delay: pause_ms
        bindings: |
          pause_ms := 3000.0
        stmt: "waited"
  p2:
    cycles: 1
    ops:
      b:
        bindings: |
          volatile ps := phase_start_millis()
          volatile pe := phase_elapsed_millis()
          volatile se := current_epoch_millis() - session_start
        stmt: "P2 phase_start={ps} phase_elapsed={pe} session_elapsed={se}"
"#,
    );
    let out = sb.run("clock.yaml");

    let (p1_start, p1_elapsed) = (
        field(&out, "P1", "phase_start"),
        field(&out, "P1", "phase_elapsed"),
    );
    let (p2_start, p2_elapsed) = (
        field(&out, "P2", "phase_start"),
        field(&out, "P2", "phase_elapsed"),
    );
    let p2_session = field(&out, "P2", "session_elapsed");

    // A real wall-clock origin, not the epoch default that the broken extern
    // handed out.
    assert!(
        p1_start > 1_700_000_000_000,
        "phase_start must be a real epoch-ms origin, got {p1_start}:\n{out}"
    );

    // The second phase starts ~3s after the first: the origin follows the phase.
    let origin_gap = p2_start.saturating_sub(p1_start);
    assert!(
        (2_500..=8_000).contains(&origin_gap),
        "the second phase's origin should trail the first by the ~3s delay, got {origin_gap}ms:\n{out}"
    );

    // ...and the phase-relative elapsed does NOT accumulate that delay.
    assert!(
        p1_elapsed < 2_000 && p2_elapsed < 2_000,
        "phase_elapsed must restart per phase, got {p1_elapsed} then {p2_elapsed}:\n{out}"
    );

    // The session clock, by contrast, has absorbed the delay — proving the two
    // are distinct and that this test is not passing on a session-scoped value.
    assert!(
        p2_session >= 2_500,
        "session_elapsed must span the whole run, got {p2_session}:\n{out}"
    );
}

/// The regression itself: an op in a phase that declares `metrics:` reads the
/// synthesized `phase_start` wire, which used to be 0 until the completion pull.
#[test]
fn phase_start_is_populated_for_ops_not_only_at_the_completion_pull() {
    let sb = Sandbox::new("extern");
    sb.write(
        "meta.yaml",
        r#"
params:
  adapter: stdout
phases:
  p:
    cycles: 1
    metrics:
      tti: { kind: gauge, value: "phase_start" }
    ops:
      b:
        bindings: |
          volatile ps := phase_start
        stmt: "op ran"
        memo:
          after: "SEEN phase_start={ps}"
"#,
    );
    let out = sb.run("meta.yaml");

    // The memo is rendered through the wires, which is the path that works
    // regardless of bind-point resolution; it lands in the session log.
    let log =
        std::fs::read_to_string(sb.path.join("sessions/latest/session.log")).expect("session log");
    let seen = field(&log, "[[ SEEN", "phase_start");
    assert!(
        seen > 1_700_000_000_000,
        "an op must see the real phase origin, not the 0 default; got {seen}:\n{out}"
    );
}
