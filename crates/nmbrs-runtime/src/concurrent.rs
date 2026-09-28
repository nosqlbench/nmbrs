// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-88 — the **headless observer + single-run headless helper**.
//!
//! [`HeadlessObserver`] folds an execution's lifecycle + log into an
//! [`ExecutionOutcome`] with no live display surface (SRD-88 §4), and
//! [`run_workload_headless`] runs ONE workload inside a scoped
//! [`ExecutionContext`](crate::execution_context) for that capture.
//!
//! The real CONCURRENT harness — N executions sharing ONE session — is
//! [`crate::runner::run_executions`]: one `SessionHost`, forked under a
//! `ScheduleSpec`/semaphore, each execution deriving its own `exec_id` under
//! the shared session and flushing its metrics to the shared store. The bespoke
//! task-based `run_executions_concurrent` that used to live here was a DUPLICATE
//! concurrency path (SRD-02 One Concurrency Path) and has been retired.

use std::sync::{Arc, Mutex};

use crate::execution_context::{self, ExecutionContext};
use crate::observer::{LogLevel, PhaseProgressUpdate, RunObserver};

/// One phase's terminal record captured by a [`HeadlessObserver`].
#[derive(Clone, Debug, PartialEq)]
pub enum PhaseRecord {
    Completed {
        name: String,
        labels: String,
        duration_secs: f64,
    },
    Failed {
        name: String,
        labels: String,
        error: String,
    },
}

/// What one execution produced, returned by [`run_executions_concurrent`].
#[derive(Clone, Debug)]
pub struct ExecutionOutcome {
    /// The execution's process-unique id (SRD-77 / §A2).
    pub exec_id: u64,
    /// Terminal phase records, in completion order.
    pub phases: Vec<PhaseRecord>,
    /// Diagnostic log lines this execution emitted (`level >= Info` kept;
    /// captured headless rather than displayed).
    pub logs: Vec<String>,
}

/// A headless [`RunObserver`]: captures lifecycle + log into an outcome, draws
/// nothing. Each concurrent execution gets its own, so events route here via
/// the task-local context (`observer::global_observer()` resolves to it) and
/// never to a shared global.
pub struct HeadlessObserver {
    phases: Mutex<Vec<PhaseRecord>>,
    logs: Mutex<Vec<String>>,
}

impl HeadlessObserver {
    pub fn new() -> Self {
        Self {
            phases: Mutex::new(Vec::new()),
            logs: Mutex::new(Vec::new()),
        }
    }

    fn take(&self) -> (Vec<PhaseRecord>, Vec<String>) {
        let p = std::mem::take(&mut *self.phases.lock().unwrap_or_else(|e| e.into_inner()));
        let l = std::mem::take(&mut *self.logs.lock().unwrap_or_else(|e| e.into_inner()));
        (p, l)
    }
}

impl Default for HeadlessObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl RunObserver for HeadlessObserver {
    fn phase_starting(
        &self,
        _scene_node_id: crate::scene_tree::SceneNodeId,
        _name: &str,
        _labels: &str,
        _ops: usize,
        _cycles: u64,
        _conc: usize,
    ) {
    }

    fn phase_completed(
        &self,
        _scene_node_id: crate::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        duration_secs: f64,
    ) {
        self.phases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(PhaseRecord::Completed {
                name: name.to_string(),
                labels: labels.to_string(),
                duration_secs,
            });
    }

    fn phase_failed(
        &self,
        _scene_node_id: crate::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        error: &str,
    ) {
        self.phases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(PhaseRecord::Failed {
                name: name.to_string(),
                labels: labels.to_string(),
                error: error.to_string(),
            });
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

/// Run ONE workload headless inside its own [`ExecutionContext`]: route the
/// run's lifecycle + log through a [`HeadlessObserver`] (no display surface),
/// then return the captured [`ExecutionOutcome`] alongside the run result.
///
/// This is the building block that proves the de-globalized run path works
/// **inside a scoped context** — the run's observer/scene-tree/stop resolve to
/// the execution's own (task-local), and the propagated per-cycle fibers
/// (SRD-88 fiber propagation) carry the context end-to-end.
///
/// Single-execution headless helper: lets `run_with_observer` create its own
/// session. To run N executions CONCURRENTLY against ONE shared session, use
/// [`crate::runner::run_executions`] (the session-tier harness: one
/// `SessionHost`, forked under a `ScheduleSpec`/semaphore, each execution
/// deriving its own `exec_id` under the shared session). The bespoke
/// task-based `run_executions_concurrent` that used to live here was a
/// DUPLICATE concurrency path (SRD-02 One Concurrency Path) and has been
/// retired in favour of `run_executions`.
pub async fn run_workload_headless(args: &[String]) -> (ExecutionOutcome, Result<(), String>) {
    let obs = Arc::new(HeadlessObserver::new());
    let ctx = ExecutionContext::with_observer(obs.clone() as Arc<dyn RunObserver>);
    let exec_id = ctx.exec_id;
    let result = execution_context::scope(
        ctx,
        crate::runner::run_with_observer(args, obs.clone() as Arc<dyn RunObserver>),
    )
    .await;
    let (phases, logs) = obs.take();
    (
        ExecutionOutcome {
            exec_id,
            phases,
            logs,
        },
        result,
    )
}
