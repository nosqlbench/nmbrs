// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-88 — prove a real workload runs **headless, inside a scoped
//! `ExecutionContext`**, end-to-end: the run's lifecycle/log route through the
//! execution's own observer (task-local), the de-globalized accessors resolve
//! to the context, and the propagated per-cycle fibers carry it across the
//! `tokio::spawn` boundary. This is the building block the concurrent
//! shared-session harness composes (the run-path factoring that lets N of
//! these share one session is the next increment).
//!
//! In-process, so the adapter inventory must be force-linked (this test binary
//! isn't the `nmbrs` binary). A bare `op=…/adapter=stdout` workload needs only
//! the stdout adapter.

extern crate nmbrs_adapter_stdout;

use std::path::PathBuf;

use nmbrs_runtime::concurrent::{PhaseRecord, run_workload_headless};

/// Tempdir under the project's redirected `TMPDIR` (`target/test-tmp`).
struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("nmbrs-headless-{pid}-{nanos}-{}", scratch::seq()));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[tokio::test]
async fn headless_run_executes_a_workload_in_context_and_captures_outcome() {
    let tmp = TempDir::new();
    let session = tmp.path.join("s");
    let args: Vec<String> = vec![
        "op=id-{cycle}".into(),
        "cycles=5".into(),
        "adapter=stdout".into(),
        "--session-path".into(),
        session.to_string_lossy().into_owned(),
    ];

    let (outcome, result) = run_workload_headless(&args).await;

    assert!(result.is_ok(), "headless run errored: {result:?}");
    // The run's lifecycle routed through the execution's OWN observer (the
    // HeadlessObserver in the scoped context), so the completed phase is
    // captured here — proving the de-globalized run path resolves to the
    // task-local context end-to-end (incl. the spawned per-cycle fibers).
    assert!(
        outcome
            .phases
            .iter()
            .any(|p| matches!(p, PhaseRecord::Completed { .. })),
        "expected a completed phase captured by the scoped observer; got {:?}",
        outcome.phases
    );
}

mod scratch;
