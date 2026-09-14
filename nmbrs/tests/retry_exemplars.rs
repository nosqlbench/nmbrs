// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Retry-error counter-exemplars, end to end (`exec_events`).
//!
//! The tries wrapper samples errors caught in its retry loop onto the
//! structured event sink at `retry_exemplar_rate` (default 0.0 = off),
//! capped at `retry_exemplar_max_hz`. The testkit synthetic-overload
//! model (`result-load` > `result-overload` → retryable rejection on
//! EVERY attempt) makes the retry storm deterministic: with `tries: 3`,
//! every cycle burns exactly two retried attempts (the third is
//! terminal, which the error policy already surfaces — exemplars cover
//! only the otherwise-invisible retried ones).

use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nmbrs-exemplars-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }
    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Three storm phases, identical except for the exemplar knobs:
/// off (default), full rate uncapped, full rate frequency-capped.
/// `retry_backoff: "20ms"` keeps retries fast but distinct in time.
const WORKLOAD: &str = r#"description: retry exemplar e2e storms

scenarios:
  default:
    - exemplars_off
    - exemplars_full
    - exemplars_capped
    - exemplars_dynamic
    - advisory_off

phases:
  exemplars_off:
    adapter: testkit
    cycles: 4
    concurrency: 1
    errors: "count"
    ops:
      off_op:
        stmt: "x"
        tries: 3
        retry_backoff: "20ms"
        result-load: 9
        result-overload: 4

  exemplars_full:
    adapter: testkit
    cycles: 4
    concurrency: 1
    errors: "count"
    ops:
      full_op:
        stmt: "x"
        tries: 3
        retry_backoff: "20ms"
        retry_exemplar_rate: "1.0"
        retry_exemplar_max_hz: "1000000"
        result-load: 9
        result-overload: 4

  exemplars_capped:
    adapter: testkit
    cycles: 4
    concurrency: 1
    errors: "count"
    ops:
      capped_op:
        stmt: "x"
        tries: 3
        retry_backoff: "20ms"
        retry_exemplar_rate: "1.0"
        retry_exemplar_max_hz: "0.1"
        result-load: 9
        result-overload: 4

  # The push-on-set loop: no pinned exemplar params, so this op
  # samples through the activity's SHARED cell; the bindings arm it
  # live via the dynamic controls (one atomic store each) before the
  # first attempt, exactly as a TUI `e` edit or web POST would
  # mid-storm.
  exemplars_dynamic:
    adapter: testkit
    cycles: 4
    concurrency: 1
    errors: "count"
    bindings: |
      volatile armed := control_set("retry_exemplar_rate", 1.0)
      volatile uncapped := control_set("retry_exemplar_max_hz", 0.0)
    ops:
      dyn_op:
        stmt: "x {armed} {uncapped}"
        tries: 3
        retry_backoff: "20ms"
        result-load: 9
        result-overload: 4

  # Advisory opt-out: the default first-sighting advisory line is
  # silenced for this op.
  advisory_off:
    adapter: testkit
    cycles: 2
    concurrency: 1
    errors: "count"
    ops:
      muted_op:
        stmt: "x"
        tries: 3
        retry_backoff: "20ms"
        retry_advisory: "off"
        result-load: 9
        result-overload: 4
"#;

#[test]
fn retry_exemplars_sample_squelch_and_default_off() {
    let sb = Sandbox::new("e2e");
    let wl = sb.path().join("storm.yaml");
    std::fs::write(&wl, WORKLOAD).expect("write workload");
    let session = sb.path().join("session");

    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(sb.path())
        .arg("run")
        .arg("--session-path")
        .arg(&session)
        .args(["workload=./storm.yaml", "tui=off"])
        .output()
        .expect("run nmbrs");
    assert!(
        out.status.success(),
        "storm run must complete (errors counted, not fatal):\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let log = std::fs::read_to_string(session.join("session.log")).expect("session log");
    let exemplar_lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("retry exemplar:"))
        .collect();

    // Default off: the phase without the rate param emits nothing.
    assert!(
        !exemplar_lines.iter().any(|l| l.contains("op 'off_op'")),
        "rate defaults to 0.0 — no exemplars without opting in:\n{exemplar_lines:#?}"
    );

    // Full rate, uncapped: every retried attempt is an exemplar —
    // 4 cycles × 2 retried attempts (the 3rd attempt is terminal).
    let full: Vec<&&str> = exemplar_lines
        .iter()
        .filter(|l| l.contains("op 'full_op'"))
        .collect();
    assert_eq!(
        full.len(),
        8,
        "rate 1.0 samples every retried attempt:\n{exemplar_lines:#?}"
    );
    assert!(
        full.iter()
            .all(|l| l.contains("[Overload]") && l.contains("(retrying)")),
        "exemplars carry the error class and retry disposition:\n{full:#?}"
    );

    // Frequency-capped: one every 10s means exactly one admission
    // in a fast run; the burst is squelched, never fatal.
    let capped = exemplar_lines
        .iter()
        .filter(|l| l.contains("op 'capped_op'"))
        .count();
    assert_eq!(
        capped, 1,
        "max_hz 0.1 admits one exemplar and squelches the burst:\n{exemplar_lines:#?}"
    );

    // Default-on advisory: every phase's FIRST retryable sighting of
    // a class announces itself once — one line per phase here (all
    // four default-advisory phases hit exactly one class, Overload).
    let advisories: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("retry advisory:"))
        .collect();
    for op in ["off_op", "full_op", "capped_op", "dyn_op"] {
        assert_eq!(
            advisories
                .iter()
                .filter(|l| l.contains(&format!("op '{op}'")))
                .count(),
            1,
            "exactly one first-sighting advisory per phase for {op}:\n{advisories:#?}"
        );
    }
    assert!(
        !advisories.iter().any(|l| l.contains("muted_op")),
        "retry_advisory: off must silence the advisory:\n{advisories:#?}"
    );

    // Dynamic controls, push-on-set: the op pinned nothing, and the
    // workload arms the shared cell live via `control_set`
    // (volatile — the const prepass would otherwise swallow the
    // side-channel dispatch, and `control_set` commits its write
    // asynchronously). Cycle 0's two retried attempts may race the
    // commit; cycles 1-3 must all be sampled.
    let dynamic = exemplar_lines
        .iter()
        .filter(|l| l.contains("op 'dyn_op'"))
        .count();
    assert!(
        (6..=8).contains(&dynamic),
        "control_set must arm the shared sampler live (got {dynamic}):\n{exemplar_lines:#?}"
    );
}
