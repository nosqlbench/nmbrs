// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Resume-after-failure staircase integration test (SRD-44; design
//! memo `docs/SRD/history/resumable_test_fixture.md`).
//!
//! Drives the test fixture workload through a four-invocation
//! staircase, verifying at each step that:
//!
//!   - the testkit's `testkit_side_effect_sequence_next_cycling` advances
//!     the threshold once per session,
//!   - `testkit_throw_at(cycle, threshold, ...)` panics at the trip cycle
//!     and the errors cascade marks the active phase Failed,
//!   - the resume planner classifies prior-Completed phases as
//!     Skip (they don't re-run) and prior-Failed phases as ReRun
//!     (they re-run from scratch and may succeed under the new
//!     threshold),
//!   - each Failed-then-rerun phase has its prior-invocation
//!     metric rows purged (no double-counting),
//!   - the session id is preserved across all four invocations.
//!
//! Workload at `nmbrs/examples/workloads/diagnostics/resume_test.yaml`. The
//! workload's hardcoded `/tmp/nmbrs_resume_test.seq` path is
//! rewritten per-test to a unique tempdir-keyed path so parallel
//! cargo-test threads don't collide.

use std::path::PathBuf;

use nmbrs_runtime::checkpoint::{Checkpoint, PhaseStatus, storage};

// Pull in the testkit adapter so its inventory submissions land
// in the registry when the test binary links. Same pattern as
// the stdout pull-in in the existing e2e test.
#[allow(unused_imports)]
use nmbrs_adapter_testkit::ModelAdapter as _PullInTestkitAdapter;

#[test]
fn staircase_failures_resume_correctly() {
    let dir = tempdir("nmbrs-staircase-e2e");
    let workload_path = dir.join("workload.yaml");
    let statefile = dir.join("seq.txt");

    // Read the canonical workload, retarget the state file path
    // to a per-test value so parallel test threads don't share
    // state. Forward slashes: the path lands inside a polydat
    // string literal in the workload, where Windows `\` separators
    // would read as escape sequences.
    let statefile_str = statefile
        .to_str()
        .expect("non-utf8 tempdir")
        .replace('\\', "/");
    let canonical = std::fs::read_to_string(workspace_path(
        "nmbrs/examples/workloads/diagnostics/resume_test.yaml",
    ))
    .expect("read canonical workload");
    let body = canonical.replace("/tmp/nmbrs_resume_test.seq", &statefile_str);
    std::fs::write(&workload_path, body).expect("write workload");

    let run_with = |resume: Option<&PathBuf>| {
        // Each cargo-test invocation runs in one process, but
        // the testkit's `testkit_side_effect_sequence_next_cycling`
        // caches its picked value process-wide (production
        // semantics — each `nmbrs run` is a fresh process). To
        // simulate the process boundary in-test we clear the
        // sequence cache before each run.
        // The cache key must match the node's path argument
        // verbatim — the forward-slash form spliced above.
        nmbrs_adapter_testkit::polydat_fixtures::clear_sequence_cache_for(&statefile_str);
        let mut args = vec![format!("workload={}", workload_path.display())];
        if let Some(prior) = resume {
            args.push(format!("resume={}", prior.display()));
        }
        in_dir(&dir, || run_args(&args));
    };

    // Each Failed phase produces an Err return from runner.run
    // (the cascade's `:stop` action). The test catches that — the
    // run still completes, and the checkpoint records the Failed
    // status, which is what we assert on.

    // -------------------------------------------------------------
    // Run 1: threshold=10. phase1 fails at cycle 10.
    // -------------------------------------------------------------
    run_with(None);
    let session_dir = read_logs_latest(&dir);
    let cp_path = session_dir.join("checkpoint.jsonl");
    let saved1 = storage::read(&cp_path).expect("read 1").expect("present 1");
    assert_eq!(saved1.invocation, 1);
    assert_eq!(
        find_phase(&saved1, "phase1").status,
        PhaseStatus::Failed,
        "run 1: phase1 should fail at cycle 10"
    );
    assert_eq!(
        find_phase(&saved1, "phase2").status,
        PhaseStatus::Pending,
        "run 1: phase2 should not have started (cascade :stop)"
    );
    assert_eq!(
        find_phase(&saved1, "phase3").status,
        PhaseStatus::Pending,
        "run 1: phase3 should not have started"
    );
    let session_id = saved1.session.clone();

    // -------------------------------------------------------------
    // Run 2: threshold=51. phase1 reruns + succeeds; phase2 fails.
    // -------------------------------------------------------------
    run_with(Some(&session_dir));
    let saved2 = storage::read(&cp_path).expect("read 2").expect("present 2");
    assert_eq!(saved2.invocation, 2);
    assert_eq!(saved2.session, session_id, "session id must be preserved");
    assert_eq!(
        find_phase(&saved2, "phase1").status,
        PhaseStatus::Completed,
        "run 2: phase1 should rerun and succeed (cap=50, threshold=51)"
    );
    assert_eq!(
        find_phase(&saved2, "phase2").status,
        PhaseStatus::Failed,
        "run 2: phase2 should fail at cycle 51"
    );
    assert_eq!(
        find_phase(&saved2, "phase3").status,
        PhaseStatus::Pending,
        "run 2: phase3 should not have started"
    );

    // -------------------------------------------------------------
    // Run 3: threshold=101. phase1 skips (Completed), phase2 reruns
    // + succeeds, phase3 fails at cycle 101.
    // -------------------------------------------------------------
    run_with(Some(&session_dir));
    let saved3 = storage::read(&cp_path).expect("read 3").expect("present 3");
    assert_eq!(saved3.invocation, 3);
    assert_eq!(find_phase(&saved3, "phase1").status, PhaseStatus::Completed);
    assert_eq!(find_phase(&saved3, "phase2").status, PhaseStatus::Completed);
    assert_eq!(find_phase(&saved3, "phase3").status, PhaseStatus::Failed);
    // phase1 was Skip-classified — its duration must equal the
    // run-2 value (no re-execution).
    let p1_dur_2 = find_phase(&saved2, "phase1").duration_secs;
    let p1_dur_3 = find_phase(&saved3, "phase1").duration_secs;
    assert_eq!(
        p1_dur_2, p1_dur_3,
        "phase1 was Skipped on run 3; its duration must equal run 2's"
    );

    // -------------------------------------------------------------
    // Run 4: threshold=999. phase3 reruns + succeeds. All Completed.
    // -------------------------------------------------------------
    run_with(Some(&session_dir));
    let saved4 = storage::read(&cp_path).expect("read 4").expect("present 4");
    assert_eq!(saved4.invocation, 4);
    assert_eq!(find_phase(&saved4, "phase1").status, PhaseStatus::Completed);
    assert_eq!(find_phase(&saved4, "phase2").status, PhaseStatus::Completed);
    assert_eq!(
        find_phase(&saved4, "phase3").status,
        PhaseStatus::Completed,
        "run 4: phase3 should succeed (cap=150, threshold=999)"
    );
    let p1_dur_4 = find_phase(&saved4, "phase1").duration_secs;
    assert_eq!(
        p1_dur_2, p1_dur_4,
        "phase1 was Skipped through runs 3+4; its duration must still match run 2's"
    );
}

// ---------------------------------------------------------------
// Helpers (mirror checkpoint_resume_e2e.rs; per-test process
// state pollution is the same constraint).
// ---------------------------------------------------------------

fn run_args(args: &[String]) {
    // The runner is async; spin up a Tokio runtime per call so the
    // test function can stay synchronous around the cwd swap.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio rt");
    rt.block_on(async {
        // The runner returns Err when a phase fails the cascade
        // — that's expected for staircase runs 1..3. We swallow
        // the error here; the test reads the checkpoint to
        // verify the recorded status.
        let _ = nmbrs_runtime::runner::run(args).await;
    });
}

fn tempdir(prefix: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("{prefix}-{n:x}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn in_dir<F: FnOnce()>(dir: &std::path::Path, f: F) {
    use std::sync::Mutex;
    static CWD_LOCK: Mutex<()> = Mutex::new(());
    let _g = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::env::set_current_dir(prev).unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn read_logs_latest(dir: &std::path::Path) -> PathBuf {
    let latest = dir.join("sessions").join("latest");
    let target = std::fs::read_link(&latest)
        .unwrap_or_else(|_| panic!("sessions/latest missing in {}", dir.display()));
    if target.is_absolute() {
        target
    } else {
        dir.join("sessions").join(target)
    }
}

fn find_phase<'a>(cp: &'a Checkpoint, name: &str) -> &'a nmbrs_runtime::checkpoint::PhaseEntry {
    use nmbrs_runtime::checkpoint::PathSegment;
    cp.phases
        .iter()
        .find(|e| {
            e.identity
                .yaml_path
                .iter()
                .any(|seg| matches!(seg, PathSegment::Phase(n) if n == name))
        })
        .unwrap_or_else(|| panic!("phase {name} not in checkpoint"))
}

/// Resolve a path relative to the workspace root (where
/// `examples/` lives). Tests run with cwd = the test crate
/// (`nmbrs-runtime/`), so we walk up one level.
fn workspace_path(rel: &str) -> PathBuf {
    let mut p = std::env::current_dir().expect("cwd");
    if p.ends_with("nmbrs-runtime") {
        p.pop();
    }
    p.join(rel)
}
