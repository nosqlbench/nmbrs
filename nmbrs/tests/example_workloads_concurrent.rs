// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-88 — the motivating use case: **real example workloads run as
//! concurrent IN-PROCESS executions sharing ONE session** (≤N at a
//! time), via `runner::run_executions`. No subprocess fan-out: one
//! `SessionHost`, N executions, each deriving its own `exec_id` under
//! the shared session, metrics landing separably in the one
//! `metrics.db`.
//!
//! In-process, so the stdout adapter is force-linked (this binary
//! isn't `nmbrs`).

extern crate nmbrs_adapter_stdout;

use std::path::PathBuf;
use std::sync::Arc;

use nmbrs_runtime::concurrent::HeadlessObserver;
use nmbrs_runtime::observer::RunObserver;
use nmbrs_runtime::runner::{ExecutionSpec, run_executions};

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
        let path = std::env::temp_dir().join(format!("nmbrs-examples-{pid}-{nanos}"));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `<workspace>/nmbrs/examples/workloads/<rel>` as an absolute path.
fn example(rel: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("nmbrs/examples/workloads")
        .join(rel)
        .to_string_lossy()
        .into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn example_workloads_run_as_concurrent_in_process_executions_in_one_session() {
    let tmp = TempDir::new();
    let session = tmp.path.join("s");
    let session_args: Vec<String> = vec![
        "--session-path".into(),
        session.to_string_lossy().into_owned(),
    ];
    let session_obs: Arc<dyn RunObserver> = Arc::new(HeadlessObserver::new());

    // A handful of bare-runnable stdout example workloads — run them ALL
    // concurrently against ONE shared session, ≤3 at a time.
    let workloads = [
        "getting_started/inline_ops.yaml",
        "getting_started/op_inline_forms.yaml",
        "getting_started/basic_workload.yaml",
    ];
    let specs: Vec<ExecutionSpec> = workloads
        .iter()
        .map(|w| ExecutionSpec {
            args: vec![format!("workload={}", example(w))],
            observer: Arc::new(HeadlessObserver::new()) as Arc<dyn RunObserver>,
            channel: None,
        })
        .collect();
    let n = specs.len() as i64;

    let results = run_executions(&session_args, session_obs, specs, 3)
        .await
        .expect("run_executions: session setup");

    assert_eq!(results.len() as i64, n);
    for (w, r) in workloads.iter().zip(&results) {
        assert!(r.is_ok(), "example {w} failed in-process: {r:?}");
    }

    // ONE shared session: N separable executions in one metrics.db.
    let db = session.join("metrics.db");
    assert!(db.exists(), "shared session metrics.db missing at {db:?}");
    let conn = rusqlite::Connection::open(&db).expect("open shared metrics.db");
    let n_exec: i64 = conn
        .query_row("SELECT COUNT(*) FROM executions", [], |r| r.get(0))
        .expect("count executions");
    assert_eq!(
        n_exec, n,
        "expected {n} executions rows in the ONE shared session"
    );
    let distinct: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT exec_id) FROM metric_instance",
            [],
            |r| r.get(0),
        )
        .expect("count distinct exec_id");
    assert_eq!(
        distinct, n,
        "each example execution's metrics must be separable by its own exec_id"
    );
}
