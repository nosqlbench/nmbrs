// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `readout` wrapper (SRD-63) — opt-in per-op status visibility.
//!
//! When an op declares `readout: visible`, this wrapper wraps it so each
//! execution reports an op-level lifecycle (start / complete / fail) to the
//! active observer via [`crate::observer::global_observer`], nested under the
//! op's parent phase ([`crate::execution_context::current_phase_node`]). The
//! TUI renders these as indented op status lines with their own execution
//! timer, so aggregate-few-ops phases (e.g. `finalize_index`: flush, compact,
//! poll) reveal per-op timing.
//!
//! Zero cost by default: absent the `readout:` field the wrapper is never
//! inserted into the op's shell stack (its `triggers` returns false), so the
//! common path pays nothing.

use std::sync::Arc;

use crate::adapter::{ExecutionError, OpDispenser, OpResult, WrappingDispenser};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

/// SRD-32a wrapper name.
pub const NAME: WrapperName = WrapperName::new("readout");

/// Trigger: `readout: visible` on the op template. Any other value (or
/// absence) leaves the wrapper off.
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template
        .params
        .get("readout")
        .and_then(|v| v.as_str())
        .map(|v| v.eq_ignore_ascii_case("visible"))
        .unwrap_or(false)
}

fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    let v = template.params.get("readout")?.as_str()?;
    Some(format!("readout: {v} (op-level status line)"))
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        owned_fields: &["readout"],
        triggers,
        // Sits outside `traverse` like the other op wrappers so it observes
        // the fully-resolved inner execution.
        requires_inner: &[super::traverse::NAME],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Wraps an inner dispenser to report op-level lifecycle events so the op
/// surfaces as its own timed status leaf under its phase. The wrapper times
/// only the inner op's execution; it never alters the result or errors.
pub struct ReadoutDispenser {
    inner: Arc<dyn OpDispenser>,
    op_name: String,
    /// Optional `measure:` template — the op's key measurable, rendered against
    /// the wires at completion. `None` when the op declares none, in which case
    /// the leaf row shows only its duration, exactly as before.
    measure: Option<String>,
}

impl ReadoutDispenser {
    pub fn wrap(inner: Arc<dyn OpDispenser>, op_name: String) -> Arc<dyn OpDispenser> {
        Self::wrap_with_measure(inner, op_name, None)
    }

    pub fn wrap_with_measure(
        inner: Arc<dyn OpDispenser>,
        op_name: String,
        measure: Option<String>,
    ) -> Arc<dyn OpDispenser> {
        Arc::new(Self {
            inner,
            op_name,
            measure,
        })
    }
}

impl WrappingDispenser for ReadoutDispenser {}

impl OpDispenser for ReadoutDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let parent = crate::execution_context::current_phase_node();
            let obs = crate::observer::global_observer();
            if let (Some(p), Some(o)) = (parent, obs.as_ref()) {
                o.op_starting(p, &self.op_name);
            }
            let start = std::time::Instant::now();
            let result = self.inner.execute(cycle, ctx).await;
            let dur = start.elapsed().as_secs_f64();
            if let (Some(p), Some(o)) = (parent, obs.as_ref()) {
                // The op's key measurable, rendered against the wires the op just
                // populated (captures included). Emitted before completion so the
                // display has it when the leaf row settles.
                if let Some(t) = self.measure.as_deref() {
                    match crate::wires::substitute_via_wires(t, ctx.wires) {
                        Ok(text) => o.op_measure(p, &self.op_name, &text),
                        Err(e) => crate::diag!(
                            crate::observer::LogLevel::Debug,
                            "measure: substitution failed for '{t}': {e}"
                        ),
                    }
                }
                match &result {
                    Ok(_) => o.op_completed(p, &self.op_name, dur),
                    Err(e) => o.op_failed(p, &self.op_name, &format!("{e}")),
                }
            }
            result
        })
    }

    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}
