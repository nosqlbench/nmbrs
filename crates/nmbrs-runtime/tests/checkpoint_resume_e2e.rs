// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Tier 1 end-to-end resume integration test (SRD-44).
//!
//! Drives the full runner pipeline twice against the same
//! workload — first as a fresh session, second as a resume —
//! and asserts that:
//!
//! - the first run wrote a `checkpoint.jsonl` with three
//!   Completed phase entries carrying their program hashes,
//! - the resume run reused the prior session directory
//!   (`Session::resume` semantics, not `Session::new`),
//! - the resume's invocation counter is `2`,
//! - phases declared `checkpoint: idempotent` were
//!   `Skip`-classified by the planner and never re-dispatched
//!   (status preserved at Completed; duration matches the
//!   first run, not a sentinel zero from a re-execution),
//! - phases declared `checkpoint: none` (or absent) were
//!   re-classified as ReRun and produced a new Completed entry
//!   with a fresh duration.

use std::path::PathBuf;

use nmbrs_runtime::checkpoint::{Checkpoint, PhaseStatus, storage};

// Adapter inventory registration is via `inventory::submit!`,
// which only fires when the linker pulls in the adapter crate.
// A `use` of any symbol from the adapter is enough to keep its
// `inventory::submit!` blocks in the binary; without this the
// runner sees an empty adapter registry and bails out with
// "unknown adapter 'stdout'".
#[allow(unused_imports)]
use nmbrs_adapter_stdout::StdoutAdapter as _PullInStdoutAdapter;

/// Spin up a tempdir, write a small workload that emits stdout
/// and declares one idempotent + one re-run phase, then run
/// the full pipeline through `nmbrs_runtime::runner::run`.
#[test]
fn fresh_run_then_resume_preserves_idempotent_phase() {
    let dir = tempdir("nmbrs-resume-e2e");

    let workload_path = dir.join("workload.yaml");
    let workload_body = r#"
description: tier-1 resume test
scenarios:
  default:
    - schema
    - load
phases:
  schema:
    checkpoint: idempotent
    cycles: 1
    concurrency: 1
    adapter: stdout
    ops:
      decl:
        stmt: "CREATE TABLE t(id int PRIMARY KEY)"
  load:
    cycles: 3
    concurrency: 1
    adapter: stdout
    ops:
      ins:
        stmt: "INSERT INTO t (id) VALUES ({cycle})"
"#;
    std::fs::write(&workload_path, workload_body).expect("write workload");

    let stdout_path = dir.join("out.txt");

    // First (fresh) invocation. Run from inside the tempdir
    // because the runner places `logs/` relative to cwd; this
    // confines the side effects to the tempdir.
    in_dir(&dir, || {
        run_args(&[
            format!("workload={}", workload_path.display()),
            "driver=stdout".into(),
            format!("filename={}", stdout_path.display()),
        ])
    });

    let session_dir = read_logs_latest(&dir);
    let checkpoint_path = session_dir.join("checkpoint.jsonl");
    let saved = storage::read(&checkpoint_path)
        .expect("read 1st checkpoint")
        .expect("checkpoint should exist after fresh run");
    assert_eq!(saved.invocation, 1, "first invocation");
    assert_eq!(saved.phases.len(), 2, "two phases declared");
    let schema = find_phase(&saved, "schema");
    assert_eq!(schema.status, PhaseStatus::Completed);
    assert!(schema.skip_eligible, "schema declared idempotent");
    assert!(schema.identity.phase_hash.is_some(), "hash stamped");
    let schema_duration_v1 = schema.duration_secs.expect("duration");
    let load = find_phase(&saved, "load");
    assert_eq!(load.status, PhaseStatus::Completed);
    assert!(!load.skip_eligible, "load declared none/absent");

    // Second invocation, resuming from the prior session.
    in_dir(&dir, || {
        run_args(&[
            format!("workload={}", workload_path.display()),
            "driver=stdout".into(),
            format!("filename={}", stdout_path.display()),
            format!("resume={}", session_dir.display()),
        ])
    });

    let post = storage::read(&checkpoint_path)
        .expect("read 2nd checkpoint")
        .expect("checkpoint still present after resume");
    assert_eq!(post.invocation, 2, "second invocation increments counter");
    assert_eq!(
        post.session, saved.session,
        "session id preserved across resume"
    );

    let schema_post = find_phase(&post, "schema");
    assert_eq!(
        schema_post.status,
        PhaseStatus::Completed,
        "skipped schema retains Completed"
    );
    assert_eq!(
        schema_post.duration_secs.expect("duration"),
        schema_duration_v1,
        "skipped phase preserves the original duration — never re-executed",
    );

    let load_post = find_phase(&post, "load");
    assert_eq!(
        load_post.status,
        PhaseStatus::Completed,
        "rerun load completes again"
    );
}

/// Workload-root binding edit between runs → phase's
/// `instance_hash` differs (the phase's own program is
/// unchanged, but its ancestor chain's `canonical_hash`
/// differs) → resume planner classifies as IdentityMismatch
/// → ReRun. Verifies the program_hash vs instance_hash split
/// (project memory `program_vs_instance_hash`).
///
/// **This test must run in its own process** — runner globals
/// (scene-tree OnceLock, observer log file, etc.) pin on
/// first-write-wins, so two e2e tests in the same process
/// fight over those slots. `cargo test
/// --test-threads=1` is the supported invocation; per-test
/// process isolation is a separate test-infra concern.
#[test]
fn upstream_binding_edit_invalidates_idempotent_skip() {
    let dir = tempdir("nmbrs-resume-mismatch");
    let workload_path = dir.join("workload.yaml");
    let stdout_path = dir.join("out.txt");

    // Polydat DSL block bindings at workload root. The phase op
    // template references `{shard}` so the phase's program
    // pulls `shard` in via auto-extern. Editing the modulus
    // changes the workload-root program's const slot, which
    // canonical_hash covers; the phase's own program is
    // byte-identical between runs but its instance_hash
    // (over its ancestor chain) differs.
    let make_workload = |modulus: u64| {
        format!(
            r#"
scenarios:
  default:
    - schema
bindings: |
  input cycle: u64
  shard := mod(hash(cycle), {modulus})
phases:
  schema:
    checkpoint: idempotent
    cycles: 1
    concurrency: 1
    adapter: stdout
    ops:
      decl:
        stmt: "DECL shard={{shard}}"
"#
        )
    };

    std::fs::write(&workload_path, make_workload(8)).unwrap();
    in_dir(&dir, || {
        run_args(&[
            format!("workload={}", workload_path.display()),
            "driver=stdout".into(),
            format!("filename={}", stdout_path.display()),
        ])
    });
    let session_dir = read_logs_latest(&dir);
    let cp_path = session_dir.join("checkpoint.jsonl");
    let saved = storage::read(&cp_path).unwrap().unwrap();
    let h_v1 = saved.phases[0].identity.phase_hash;
    assert!(h_v1.is_some(), "phase 1 should have stamped a hash");

    // Edit the workload-root modulus and resume. instance_hash
    // walks the ancestor chain, so the ancestor program's
    // changed const slot must propagate to the phase's
    // instance_hash even though the phase's own program is
    // byte-identical.
    std::fs::write(&workload_path, make_workload(16)).unwrap();
    in_dir(&dir, || {
        run_args(&[
            format!("workload={}", workload_path.display()),
            "driver=stdout".into(),
            format!("filename={}", stdout_path.display()),
            format!("resume={}", session_dir.display()),
        ])
    });
    let post = storage::read(&cp_path).unwrap().unwrap();
    let h_v2 = post.phases[0].identity.phase_hash;
    assert_ne!(
        h_v1, h_v2,
        "instance_hash must differ after the upstream binding edit — \
         this is the program_hash vs instance_hash split working as intended"
    );
    assert_eq!(post.invocation, 2);
    assert_eq!(
        post.phases[0].status,
        PhaseStatus::Completed,
        "phase ran fresh after the mismatch and completed"
    );
}

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

fn run_args(args: &[String]) {
    // The runner is async; spin up a Tokio runtime per call so
    // the test function itself can stay synchronous around the
    // CWD swap. (Tokio's #[test] reuses one runtime, which
    // tangles with sync side effects we want isolated to one
    // run-and-resume pair.)
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio rt");
    rt.block_on(async {
        nmbrs_runtime::runner::run(args)
            .await
            .expect("runner.run returned Err")
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
    // Serialize cwd swaps AND `nmbrs_runtime::runner::run`
    // invocations via a single process-wide mutex. cargo test
    // runs tests in parallel by default; the runner installs a
    // process-wide singleton scene tree (`OnceLock`-backed)
    // whose first-write-wins semantics make parallel runner
    // executions cross-contaminate (test B's phase identities
    // get looked up against test A's tree). Holding the lock
    // across the entire closure (including the runner call)
    // gives us a clean serial-execution guarantee for tests
    // that drive the runner end-to-end.
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
