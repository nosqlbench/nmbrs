// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-100 — a phase-level `for_each` (a `for_each` lifted onto the phase
//! itself) must materialize each cell as a **distinct** scene-tree node, the
//! same as scenario-level `for_each`. `SceneTree::push` is idempotent by
//! `(parent, kind, name)` (labels ignored), so pushing every same-name cell
//! under one scope would collapse them to a single node — which would defeat
//! P1c's dispatch-time-id lifecycle routing (a concurrent sweep's cells would
//! all alias one node and last-writer-wins). The dispatcher therefore wraps
//! each cell in its OWN per-iter scope; this test pins that end-to-end through
//! the real pre-map walk, not a hand-built tree.
//!
//! In-process, so the adapter inventory is force-linked.

extern crate nmbrs_adapter_stdout;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nmbrs_runtime::observer::RunObserver;
use nmbrs_runtime::runner::{ExecutionSpec, run_executions};
use nmbrs_runtime::scene_tree::{NodeKind, SceneTree};

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
        let path = std::env::temp_dir().join(format!("nmbrs-pfe-topology-{pid}-{nanos}"));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Captures the fully pre-mapped scene tree handed to `scenario_pre_mapped`,
/// and no-ops everything else.
struct TreeCapturingObserver {
    tree: Mutex<Option<SceneTree>>,
}
impl RunObserver for TreeCapturingObserver {
    fn scenario_pre_mapped(&self, tree: &SceneTree) {
        *self.tree.lock().unwrap_or_else(|e| e.into_inner()) = Some(tree.clone());
    }
    fn phase_starting(
        &self,
        _: nmbrs_runtime::scene_tree::SceneNodeId,
        _: &str,
        _: &str,
        _: usize,
        _: u64,
        _: usize,
    ) {
    }
    fn phase_completed(&self, _: nmbrs_runtime::scene_tree::SceneNodeId, _: &str, _: &str, _: f64) {
    }
    fn phase_failed(&self, _: nmbrs_runtime::scene_tree::SceneNodeId, _: &str, _: &str, _: &str) {}
    fn phase_progress(&self, _: &nmbrs_runtime::observer::PhaseProgressUpdate) {}
    fn run_finished(&self) {}
    fn log(&self, _: nmbrs_runtime::observer::LogLevel, _: &str) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase_level_for_each_materializes_distinct_nodes_per_cell() {
    let workload = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../nmbrs/examples/workloads/iteration/control_flow.yaml")
        .canonicalize()
        .expect("control_flow.yaml present");

    let tmp = TempDir::new();
    let session = tmp.path.join("s");
    let session_args: Vec<String> = vec![
        "--session-path".into(),
        session.to_string_lossy().into_owned(),
    ];

    let capture = Arc::new(TreeCapturingObserver {
        tree: Mutex::new(None),
    });
    let session_obs: Arc<dyn RunObserver> = capture.clone();

    // `test_phase_for_each` is `- show_each_animal`, a phase whose own
    // `for_each` sweeps animals = "cat,dog" — two cells.
    let specs = vec![ExecutionSpec {
        args: vec![
            format!("workload={}", workload.to_string_lossy()),
            "scenario=test_phase_for_each".into(),
        ],
        observer: capture.clone() as Arc<dyn RunObserver>,
        channel: None,
    }];

    let results = run_executions(&session_args, session_obs, specs, 1)
        .await
        .expect("run_executions: session setup");
    assert!(results[0].is_ok(), "execution errored: {:?}", results[0]);

    let tree = capture
        .tree
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("scenario_pre_mapped delivered the tree");

    // The two cells must be DISTINCT phase nodes (not one aliased node).
    let cells: Vec<_> = tree
        .dfs_phases()
        .filter(|n| n.name == "show_each_animal")
        .collect();
    assert_eq!(
        cells.len(),
        2,
        "phase-level for_each over [cat,dog] must materialize 2 distinct \
         phase nodes, got {} — same-name cells collapsed to one node",
        cells.len()
    );

    // Distinct ids, distinct labels, distinct parents (the per-iter scopes).
    assert_ne!(cells[0].id, cells[1].id, "cells must be distinct node ids");
    assert_ne!(
        cells[0].labels, cells[1].labels,
        "each cell carries its own iteration coordinate"
    );
    assert_ne!(
        cells[0].parent, cells[1].parent,
        "each cell hangs under its OWN per-iter scope (distinctness via parent, \
         not by label-keying the phase — the §4 invariant)"
    );
    // Each parent is a Scope (the per-iter wrapper), not the for_each header
    // shared across cells.
    for c in &cells {
        let parent = c
            .parent
            .and_then(|p| tree.nodes.get(p))
            .expect("cell has a parent");
        assert_eq!(
            parent.kind,
            NodeKind::Scope,
            "cell parent is a per-iter scope"
        );
    }
}
