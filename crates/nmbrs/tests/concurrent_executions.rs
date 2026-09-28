// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-88 — prove N executions run **concurrently in one process,
//! sharing ONE session**, each with a distinct `exec_id`, and that
//! their metrics land **separably** in the one shared `metrics.db`.
//! This is the payoff of the run-path factoring (`SessionHost::setup`
//! + `run_execution`) + the per-execution `quiesce` flush-to-store:
//! the session (dir / stores / cadence + scheduler) is set up once and
//! torn down once, while each execution derives its own `Execution`
//! tier under the shared session component.
//!
//! In-process, so the adapter inventory must be force-linked (this
//! test binary isn't the `nmbrs` binary). A bare `op=…/adapter=stdout`
//! workload needs only the stdout adapter.

extern crate nmbrs_adapter_stdout;

use std::path::PathBuf;
use std::sync::Arc;

use nmbrs_runtime::concurrent::HeadlessObserver;
use nmbrs_runtime::observer::RunObserver;
use nmbrs_runtime::runner::{ExecutionSpec, run_executions};

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
            std::env::temp_dir().join(format!("nmbrs-concurrent-{pid}-{nanos}-{}", scratch::seq()));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n_executions_share_one_session_with_separable_exec_ids() {
    let tmp = TempDir::new();
    let session = tmp.path.join("s");
    // Session-tier args — just where the shared session lives.
    let session_args: Vec<String> = vec![
        "--session-path".into(),
        session.to_string_lossy().into_owned(),
    ];
    let session_obs: Arc<dyn RunObserver> = Arc::new(HeadlessObserver::new());

    // Three executions, each its own workload, run concurrently (≤3).
    let specs: Vec<ExecutionSpec> = (0..3)
        .map(|i| ExecutionSpec {
            args: vec![
                format!("op=ex{i}-{{cycle}}"),
                "cycles=3".into(),
                "adapter=stdout".into(),
            ],
            observer: Arc::new(HeadlessObserver::new()) as Arc<dyn RunObserver>,
            channel: None,
        })
        .collect();

    let results = run_executions(&session_args, session_obs, specs, 3)
        .await
        .expect("run_executions: session setup");

    assert_eq!(results.len(), 3, "one result per execution");
    for (i, r) in results.iter().enumerate() {
        assert!(r.is_ok(), "execution {i} errored: {r:?}");
    }

    // ONE shared session directory + db.
    let db = session.join("metrics.db");
    assert!(
        db.exists(),
        "the shared session metrics.db is missing at {db:?}"
    );
    let conn = rusqlite::Connection::open(&db).expect("open shared metrics.db");

    // SRD-77/88 — three separable executions recorded in the ONE session.
    let n_exec: i64 = conn
        .query_row("SELECT COUNT(*) FROM executions", [], |r| r.get(0))
        .expect("count executions");
    assert_eq!(
        n_exec, 3,
        "expected 3 executions rows in the shared session"
    );

    // Each execution's metrics are separable by its distinct exec_id.
    let distinct_exec_ids: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT exec_id) FROM metric_instance",
            [],
            |r| r.get(0),
        )
        .expect("count distinct exec_id");
    assert_eq!(
        distinct_exec_ids, 3,
        "expected 3 distinct exec_ids in metric_instance — concurrent \
         executions must be separable in the shared store"
    );

    // And every execution actually produced its own metric rows.
    let min_rows_per_exec: i64 = conn
        .query_row(
            "SELECT MIN(c) FROM (SELECT exec_id, COUNT(*) c FROM metric_instance GROUP BY exec_id)",
            [],
            |r| r.get(0),
        )
        .expect("min rows per exec");
    assert!(
        min_rows_per_exec > 0,
        "every execution should have written metric instances under its exec_id"
    );
}

mod scratch;
