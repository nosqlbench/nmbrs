// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Phase-level `metrics:` end-to-end coverage.
//!
//! Verifies the `time_to_index` mechanism: a phase declares a
//! phase-level `metrics:` block whose value is
//! `phase_elapsed(phase_start)`, the executor injects the phase's
//! chronological start into `phase_start` and pulls the metric once
//! at phase completion, and the resulting gauge equals the phase's
//! real wall-clock duration.
//!
//! The probe in `polydat::kernel::program` showed why this cannot be
//! expressed as two bracketing volatile clock reads (CSE collapses
//! identical reads; R1.v contagion re-evaluates the "start" on every
//! `set_input`; `const`-over-volatile is rejected by the init
//! contract). The working shape is a single `phase_elapsed` node
//! reading the clock once and subtracting an injected origin — this
//! test pins that it is accurate end-to-end through the real phase
//! lifecycle.
//!
//! Uses the testkit adapter with `result-latency` so the phase has a
//! non-trivial, controllable duration. The explicit `phases:` +
//! `scenarios:` form is required — the inline-block shorthand bypasses
//! `run_phase`, which owns both the `phase_outcomes` row and the
//! phase-metric emission.

use std::io::Write;
use std::path::{Path, PathBuf};
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
        "nmbrs_phase_metrics_{label}_{}.yaml",
        std::process::id()
    ));
    let mut f = std::fs::File::create(&dir).unwrap_or_else(|e| panic!("create {dir:?}: {e}"));
    f.write_all(body.as_bytes())
        .unwrap_or_else(|e| panic!("write {dir:?}: {e}"));
    dir
}

/// Run `nmbrs run workload=... <extra>` against a fresh session dir
/// under TMPDIR (sandbox cwd is the workspace root; `--session-path`
/// pins the metrics.db location).
fn run_with_session(workload: &Path, extra: &[&str]) -> (PathBuf, String, String, bool) {
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-phase-metrics-{}-{}-{}",
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
    (
        session_path,
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

/// Read the latest recorded value of a gauge metric family from a
/// run's metrics.db. Gauges land in `sample_value.mean`
/// (see `nmbrs_metrics::reporters::sqlite::insert_metric`).
fn read_gauge(db: &Path, family: &str) -> Option<f64> {
    let conn = rusqlite::Connection::open(db).expect("open metrics.db");
    conn.query_row(
        "SELECT sv.mean \
         FROM sample_value sv \
         JOIN metric_instance mi ON sv.instance_id = mi.id \
         JOIN metric_family mf   ON mi.family_id = mf.id \
         WHERE mf.name = ?1 AND mf.type = 'gauge' \
         ORDER BY sv.timestamp_ms DESC LIMIT 1",
        [family],
        |row| row.get::<_, f64>(0),
    )
    .ok()
}

/// Single phase that does ~200ms of testkit "work" (5 cycles ×
/// 40ms) and emits `time_to_index = phase_elapsed(phase_start)` in
/// milliseconds at phase completion.
const TIME_TO_INDEX: &str = r#"
phases:
  build_index:
    cycles: 5
    concurrency: 1
    bindings: |
      volatile now_ms := current_epoch_millis()
    metrics:
      time_to_index: { value: "now_ms - phase_start" }
    ops:
      work:
        stmt: "op"
        result-latency: "40ms"
scenarios:
  default: [build_index]
"#;

#[test]
fn phase_metric_time_to_index_matches_phase_duration() {
    let wl = write_workload("ttidx", TIME_TO_INDEX);
    let (session_path, _stdout, stderr, ok) = run_with_session(&wl, &["adapter=testkit"]);
    assert!(ok, "run should succeed; stderr:\n{stderr}");

    let db_path = session_path.join("metrics.db");
    assert!(
        db_path.exists(),
        "metrics.db not produced at {}; stderr:\n{stderr}",
        db_path.display()
    );

    // Ground truth: the phase's wall-clock duration recorded by the
    // executor (monotonic Instant) in the phase_outcomes table.
    let reporter =
        nmbrs_metrics::reporters::sqlite::SqliteReporter::new(&db_path).expect("open metrics.db");
    let outcomes = reporter.read_phase_outcomes(None);
    let build = outcomes
        .iter()
        .find(|o| o.phase_name == "build_index")
        .unwrap_or_else(|| {
            panic!(
                "no build_index phase outcome; rows: {:?}\nstderr:\n{stderr}",
                outcomes
            )
        });
    let duration_ms = build.duration_secs * 1000.0;

    // The phase did real work — sanity-check the ground truth so a
    // broken testkit-latency path can't make the comparison vacuous.
    assert!(
        duration_ms > 120.0,
        "phase should take >120ms (5×40ms work) but duration_secs={} ; stderr:\n{stderr}",
        build.duration_secs
    );

    // The phase-level metric, emitted from `phase_elapsed(phase_start)`.
    let ttidx = read_gauge(&db_path, "time_to_index").unwrap_or_else(|| {
        panic!("time_to_index gauge not found in metrics.db; stderr:\n{stderr}")
    });

    // Accurate: the clock-read metric (epoch millis) must track the
    // executor's monotonic duration. The metric is pulled just before
    // the duration is computed, so it lands a hair under; allow a
    // generous absolute slack for scheduler jitter.
    assert!(
        ttidx > 100.0,
        "time_to_index should reflect the ~200ms of phase work, got {ttidx}ms; stderr:\n{stderr}"
    );
    let diff = (ttidx - duration_ms).abs();
    eprintln!(
        "[ttidx] time_to_index={ttidx:.1}ms  phase_duration={duration_ms:.1}ms  diff={diff:.1}ms"
    );
    assert!(
        diff < 75.0,
        "time_to_index ({ttidx}ms) must match the phase duration ({duration_ms}ms) \
         within 75ms; diff={diff}ms; stderr:\n{stderr}"
    );
}

mod scratch;
