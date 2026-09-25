// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Proves the full "measure → shared cell → const freeze → later phase"
//! chain WITHOUT a live backend, using the testkit adapter:
//!
//!   1. The workload-root scope declares `shared measured := 1`.
//!   2. Phase A (testkit op) runs a RESULT-BINDING
//!      `measured := floor_decade(count)` where `count` is the op's
//!      `element_count()` (SRD-66 magic extern). This writes THROUGH
//!      (SRD-67 Phase 5 "Rule 2") to the parent `shared` cell.
//!   3. A later scenario-tree `bindings:` node declares
//!      `const stride := measured` — an extern-no-default freeze of the
//!      shared cell's value AT SCOPE ACTIVATION.
//!   4. Phase B reads `{stride}` and emits it.
//!
//! The testkit body is a JSON array of 42 elements, so `count` == 42 and
//! `floor_decade(42)` == 40. The assertion proves phase B sees 40 (the
//! measured-and-floored value phase A produced) and NOT 1 (the shared
//! cell's initial literal) — i.e. the whole chain persisted across
//! phases and froze the RUNTIME value, not the pre-map value.
//!
//! In-process, so the adapter inventory is force-linked.

extern crate nmbrs_adapter_stdout;
extern crate nmbrs_adapter_testkit;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nmbrs_runtime::observer::{LogLevel, PhaseProgressUpdate, RunObserver};
use nmbrs_runtime::output_channel::{CaptureChannel, OutputChannel};
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
        let path = std::env::temp_dir().join(format!(
            "nmbrs-measure-freeze-{pid}-{nanos}-{}",
            scratch::seq()
        ));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Minimal observer that just records log lines (and swallows the rest).
struct LogObserver {
    logs: Mutex<Vec<String>>,
}
impl LogObserver {
    fn new() -> Self {
        Self {
            logs: Mutex::new(Vec::new()),
        }
    }
}
impl RunObserver for LogObserver {
    fn phase_starting(
        &self,
        _scene_node_id: nmbrs_runtime::scene_tree::SceneNodeId,
        _name: &str,
        _labels: &str,
        _op_templates: usize,
        _total_cycles: u64,
        _concurrency: usize,
    ) {
    }
    fn phase_completed(
        &self,
        _scene_node_id: nmbrs_runtime::scene_tree::SceneNodeId,
        _name: &str,
        _labels: &str,
        _duration_secs: f64,
    ) {
    }
    fn phase_failed(
        &self,
        _scene_node_id: nmbrs_runtime::scene_tree::SceneNodeId,
        _name: &str,
        _labels: &str,
        error: &str,
    ) {
        self.logs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("PHASE_FAILED: {error}"));
    }
    fn phase_progress(&self, _update: &PhaseProgressUpdate) {}
    fn run_finished(&self) {}
    fn log(&self, _level: LogLevel, message: &str) {
        self.logs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(message.to_string());
    }
}

/// Build the workload text: 42-element testkit body → count == 42 →
/// floor_decade(42) == 40.
fn workload_yaml() -> String {
    // 42 array elements (content irrelevant — element_count == len).
    let body_elems = std::iter::repeat("0")
        .take(42)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"
bindings: |
  input cycle: u64
  # Parent shared cell, literal-initialized to 1.
  shared measured := 1

scenarios:
  default:
    # Phase A: measure and write-through to the parent shared cell.
    - measure
    # Later scope: freeze the (now-updated) shared cell into a const.
    - bindings: |
        const stride := measured
      phases:
        - use_stride

phases:
  measure:
    adapter: testkit
    cycles: 1
    concurrency: 1
    ops:
      probe:
        stmt: "measure cycle={{cycle}}"
        # Synthetic body of 42 elements → element_count() == 42.
        result-body: [ {body} ]
        # SRD-66 result-binding: write floor_decade(count) THROUGH to
        # the parent `shared measured` cell (SRD-67 Phase 5 "Rule 2").
        # `trunc_u64` — floor_decade returns f64, the cell is U64, and
        # narrowing is never automatic (type-stable cells): the
        # explicit cast is the author's act, per scope_model.md.
        result: |
          measured := trunc_u64(floor_decade(count))

  use_stride:
    adapter: testkit
    cycles: 1
    concurrency: 1
    ops:
      show:
        stmt: "phaseB stride={{stride}}"
"#,
        body = body_elems,
    )
}

#[test]
fn measure_writes_shared_cell_then_const_freezes_it_for_later_phase() {
    let tmp = TempDir::new();
    let session = tmp.path.join("session");
    let workload_path = tmp.path.join("measure_freeze.yaml");
    std::fs::write(&workload_path, workload_yaml()).expect("write workload");

    let cap = Arc::new(CaptureChannel::new());
    let obs = Arc::new(LogObserver::new());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let result = rt.block_on(async {
        let session_args: Vec<String> = vec![
            "--session-path".into(),
            session.to_string_lossy().into_owned(),
        ];
        let session_obs: Arc<dyn RunObserver> =
            Arc::new(nmbrs_runtime::concurrent::HeadlessObserver::new());
        let specs = vec![ExecutionSpec {
            args: vec![format!("workload={}", workload_path.to_string_lossy())],
            observer: obs.clone() as Arc<dyn RunObserver>,
            channel: Some(cap.clone() as Arc<dyn OutputChannel>),
        }];
        run_executions(&session_args, session_obs, specs, 1).await
    });

    let results = result.expect("run_executions: session setup");
    assert_eq!(results.len(), 1, "one execution expected");
    let exec_result = &results[0];

    let op_lines = cap.op_lines();
    let logs = obs.logs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let diag = format!("exec_result={exec_result:?}\nop_lines={op_lines:#?}\nlogs={logs:#?}",);

    assert!(exec_result.is_ok(), "execution errored:\n{diag}");

    // Phase B's line.
    let stride_line = op_lines
        .iter()
        .find(|l| l.contains("phaseB stride="))
        .unwrap_or_else(|| panic!("no phaseB line found.\n{diag}"));

    // The chain worked iff phase B saw the floored measurement (40),
    // NOT the shared cell's initial literal (1).
    assert!(
        stride_line.contains("stride=40"),
        "phase B did not see the measured-and-floored value.\n\
         expected 'stride=40', got: {stride_line:?}\n{diag}",
    );
    assert!(
        !stride_line.contains("stride=1"),
        "phase B saw the initial literal, not the write-through value.\n{diag}",
    );
}

mod scratch;
