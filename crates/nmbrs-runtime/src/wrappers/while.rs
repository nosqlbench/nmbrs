// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Loop-while wrapper. Iterates the inner op as long as the
//! op-template's `while:` Polydat expression evaluates truthy. On
//! each iteration the wrapper:
//!
//! 1. Checks the activity-global stop flag. If set, the loop
//!    returns [`OpResult::skipped`] (no further iterations).
//!    Used to drain non-daemon while loops cleanly on session
//!    stop, and as a belt-and-braces secondary signal for
//!    daemon while loops (the daemon body's tokio::select!
//!    cancels the future independently).
//! 2. Reads the synthesised `__while` binding via the pull
//!    plan. The op-kernel synthesis appends
//!    `__while := <expr>` to the kernel's result bindings —
//!    same path metrics' value expressions take — so referenced
//!    wires are auto-externed and read from the right cells.
//! 3. If the condition is falsy, returns [`OpResult::skipped`]
//!    (loop exit). Otherwise calls the inner dispenser; an Err
//!    propagates and breaks the loop, an Ok continues.
//!
//! Composition: `while:` sits outer of `result`/`metrics` /
//! `traverse` (so the inner's result captures land before the
//! next iteration's predicate check) and inner of `if:` /
//! `memo` (so the conditional short-circuit fires before the
//! loop starts, and the memo emits after the loop concludes).

use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

/// Wrapper name.
pub const NAME: WrapperName = WrapperName::new("while");

/// Internal kernel-output name the wrapper pulls. The op-
/// kernel synthesiser appends `__while := <expr>` to the
/// kernel's result-bindings source so the expression's free
/// identifiers get auto-externed (same path metrics take).
pub const BINDING_NAME: &str = "__while";

/// Trigger: op declares a `while:` expression.
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template.while_cond.is_some()
}

fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    template
        .while_cond
        .as_ref()
        .map(|expr| format!("while: {}", expr.trim()))
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        owned_fields: &["while"],
        triggers,
        requires_inner: &[],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Wraps an inner OpDispenser with a loop driven by a GK
/// boolean expression. See module docs.
pub struct WhileWrapper {
    inner: Arc<dyn OpDispenser>,
    /// Handle for the synthesised `__while` binding registered
    /// into the scope fixture at init.
    cond_handle: crate::fixture::PullHandle,
    /// SRD-92 Step 0 — the cooperative-stop view (activity `stop_flag` +
    /// session stop + SRD-83 `walk_stop` + SRD-82 P6 `daemon_stop`). Each
    /// loop iteration checks it and exits cleanly when ANY arm is set.
    /// Before Step 0 this held only the activity `stop_flag`, so a `while:`
    /// op did NOT abort on a scenario walk-halt or a daemon-group stop.
    stop_view: crate::session_signals::StopView,
    /// Hard ceiling on iterations per outer dispatch. Defends
    /// against runaway loops (a `while:` predicate that
    /// never flips falsy) hanging the activity. The default
    /// is intentionally large; daemons that need genuine long-
    /// running loops are governed by the daemon stop-flag and
    /// the activity-stop flag, not this counter.
    iteration_ceiling: u64,
}

impl WhileWrapper {
    /// Default iteration ceiling. Sized to allow days-long
    /// daemon loops at modest tick rates (e.g. 100k/sec for
    /// 11+ days) while still terminating a buggy infinite loop
    /// in finite time during dev. Raise via
    /// [`Self::with_iteration_ceiling`] if a legitimate workload
    /// needs more.
    pub const DEFAULT_ITERATION_CEILING: u64 = 100_000_000_000;

    /// Wrap an inner dispenser. Registers the synthesised
    /// `__while` binding into `fx` so the per-cycle read goes
    /// through the canonical PullPlan path.
    ///
    /// Errors if the kernel doesn't know `__while` — that
    /// means the op-kernel synthesiser didn't inject the
    /// binding (registry bug) or the expression failed to
    /// compile and the kernel was rebuilt without it.
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        stop_view: crate::session_signals::StopView,
        fx: &mut crate::fixture::ScopeFixture,
    ) -> Result<Arc<dyn OpDispenser>, String> {
        let cond_handle = fx.register_pull(BINDING_NAME).map_err(|e| {
            format!(
                "while: {e} (expected synthesised binding `{BINDING_NAME}` \
                     on the op-template kernel — synthesis bug?)"
            )
        })?;
        Ok(Arc::new(Self {
            inner,
            cond_handle,
            stop_view,
            iteration_ceiling: Self::DEFAULT_ITERATION_CEILING,
        }))
    }

    /// Test-only constructor with a custom iteration ceiling.
    #[cfg(test)]
    pub fn with_iteration_ceiling(
        inner: Arc<dyn OpDispenser>,
        stop_view: crate::session_signals::StopView,
        fx: &mut crate::fixture::ScopeFixture,
        ceiling: u64,
    ) -> Result<Arc<dyn OpDispenser>, String> {
        let cond_handle = fx
            .register_pull(BINDING_NAME)
            .map_err(|e| format!("while: {e}"))?;
        Ok(Arc::new(Self {
            inner,
            cond_handle,
            stop_view,
            iteration_ceiling: ceiling,
        }))
    }
}

/// Truthy test for the predicate value. Mirrors
/// `ConditionalDispenser::is_truthy` — the two wrappers
/// share a semantic: 0/false/empty-string/None is falsy,
/// everything else is truthy.

impl WrappingDispenser for WhileWrapper {}

impl OpDispenser for WhileWrapper {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut iterations: u64 = 0;
            let mut last_result: Option<OpResult> = None;
            loop {
                if self.stop_view.stopped() {
                    // Any cooperative stop (activity / session / SRD-83
                    // walk / SRD-82 P6 daemon-group). Treat as a clean
                    // exit; any iteration metrics already landed through
                    // the inner's wrappers.
                    return Ok(last_result.unwrap_or_else(OpResult::skipped));
                }
                if iterations >= self.iteration_ceiling {
                    // Defensive ceiling. Surfacing as a skip
                    // (not an error) keeps the phase outcome
                    // identical to a natural predicate-flip
                    // exit; the activity log records the
                    // ceiling hit at iteration time below.
                    crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "while: iteration ceiling {} hit; exiting loop",
                        self.iteration_ceiling
                    );
                    return Ok(last_result.unwrap_or_else(OpResult::skipped));
                }
                let cond = ctx.pulls.get(self.cond_handle);
                if !crate::wrappers::condition::is_truthy(cond) {
                    return Ok(last_result.unwrap_or_else(OpResult::skipped));
                }
                match self.inner.execute(cycle, ctx).await {
                    Ok(r) => {
                        last_result = Some(r);
                        iterations += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        })
    }
    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}

#[cfg(test)]
mod tests {

    // Truthiness itself is not tested here — it lives in
    // `super::condition` and is tested there. Duplicating it was how the two
    // implementations drifted apart in the first place.

    // The `while` loop halts: given a ceiling and a stop flag, the loop exits
    // after at most `ceiling` iterations. Models the wrapper's
    // loop bookkeeping synchronously (no tokio) since the
    // arithmetic is what we're exercising.
    mod proptests {
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]
            #[test]
            fn loop_terminates_at_ceiling(
                ceiling in 1u64..=10_000,
                always_true in any::<bool>(),
                stop_after in 0u64..=15_000,
            ) {
                // Simulate the loop: every iteration checks
                // (a) stop flag, (b) ceiling, (c) predicate.
                // Returns the iteration count actually run.
                let mut iterations: u64 = 0;
                let stop_flag = std::sync::atomic::AtomicBool::new(false);
                loop {
                    if stop_flag.load(std::sync::atomic::Ordering::Acquire) { break; }
                    if iterations >= ceiling { break; }
                    if !always_true { break; }
                    iterations += 1;
                    if iterations == stop_after {
                        stop_flag.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
                // Two invariants:
                // (1) Loop runs at most `ceiling` iterations.
                prop_assert!(iterations <= ceiling,
                    "iterations {} exceeded ceiling {}", iterations, ceiling);
                // (2) If the predicate is always-false, zero
                // iterations run regardless of ceiling.
                if !always_true {
                    prop_assert_eq!(iterations, 0,
                        "predicate=false must yield zero iterations");
                }
            }
        }
    }
}
