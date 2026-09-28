// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Regression: a daemon op (SRD-79) that carries an `if:` condition
//! must resolve its wrapper pull plan, just like a cycle-pool op.
//!
//! The bug: `activity::daemon_dispatch` built the daemon's `ExecCtx`
//! with `ResolvedPulls::empty()`, so the IF_COND wrapper's registered
//! pull handle indexed into an empty vec and panicked
//! ("index out of bounds: the len is 0 but the index is 0",
//! `fixture.rs`). The daemon pool catches the panic → sets the stop
//! flag → the phase fails. This surfaced on the live `finalize_index`
//! phase, whose `trigger_compact` daemon gates compaction on
//! `if: sstables > 1`.
//!
//! This drives the *full* synthesis path (so the `if:` is compiled
//! into the op-template kernel as `__if := <expr>`) via the in-process
//! runner against the stdout adapter — no database needed. Before the
//! fix the run returns `Err` (phase failed); after, it completes.

use std::path::PathBuf;

// Force-link the stdout adapter so its `inventory::submit!`
// registration lands in this test binary — otherwise the runner
// sees an empty adapter registry ("unknown adapter 'stdout'").
#[allow(unused_imports)]
use nmbrs_adapter_stdout::StdoutAdapter as _PullInStdoutAdapter;

fn run_args(args: &[String]) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio rt");
    rt.block_on(async {
        nmbrs_runtime::runner::run(args)
            .await
            .expect("runner.run returned Err — daemon `if:` pull resolution regressed")
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

/// Serialize cwd swaps + runner invocations (the runner installs a
/// process-wide singleton scene tree). Same pattern as
/// `checkpoint_resume_e2e.rs`.
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

/// A phase whose only op is a daemon gated by an `if:` condition
/// (`cycle >= 0` is always true for u64, so the daemon runs). The
/// `if:` is what makes the IF_COND wrapper register a pull —
/// exercising the daemon pull-resolution path. Mirrors the
/// `finalize_index` `trigger_compact` shape (daemon + `if:`) minus
/// the jolokia specifics.
const DAEMON_IF_WORKLOAD: &str = r#"
phases:
  drive:
    cycles: 1
    concurrency: 1
    ops:
      trigger:
        daemon: true
        if: "cycle >= 0"
        stmt: "daemon fired"
scenarios:
  default: [drive]
"#;

#[test]
fn daemon_op_with_if_condition_completes() {
    let dir = tempdir("nmbrs-daemon-if-e2e");
    let workload_path = dir.join("workload.yaml");
    std::fs::write(&workload_path, DAEMON_IF_WORKLOAD).expect("write workload");

    in_dir(&dir, || {
        run_args(&[
            format!("workload={}", workload_path.display()),
            "driver=stdout".into(),
        ])
    });
}
