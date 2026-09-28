// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Conditional execution wrapper. Reads a per-cycle truthy/falsy
//! value through the pull plan; if falsy, the op is skipped
//! (no inner execution, no adapter call) and `skips_total` is
//! incremented on the activity metrics.

use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

/// SRD-32a wrapper name.
pub const NAME: WrapperName = WrapperName::new("if");

/// Trigger: op declares an `if:` condition.
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template.condition.is_some()
}

fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    template.condition.as_ref().map(|cond| {
        let trimmed = crate::wrapper_registrations::trim_braces(cond);
        format!("if: {trimmed}")
    })
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        owned_fields: &["if"],
        triggers,
        requires_inner: &[],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Wraps an inner OpDispenser with a conditional check.
///
/// Evaluates a named field in `ResolvedFields` before executing.
/// If the field is falsy (0, 0.0, false, empty string, None), the
/// op is skipped — no inner execution, no adapter call. Returns
/// `OpResult::skipped()`.
///
/// The condition Polydat name is resolved at init time via
/// `ScopeFixture::register_pull` and read at cycle time through
/// the stored `PullHandle` against `ExecCtx::pulls`.
pub struct ConditionalDispenser {
    inner: Arc<dyn OpDispenser>,
    /// Memoized handle for the condition Polydat name registered into
    /// the scope fixture at init.
    condition_handle: crate::fixture::PullHandle,
    /// Metrics reference for counting skips.
    metrics: Arc<crate::activity::ActivityMetrics>,
}

impl ConditionalDispenser {
    /// Wrap an inner dispenser with a condition check, registering
    /// `condition_field` into the supplied scope fixture so the
    /// per-cycle read goes through the canonical PullPlan path
    /// (SRD 32 §"Init-Time Fixture and Consumer Self-Registration").
    ///
    /// Errors if the kernel doesn't know `condition_field`.
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        condition_field: &str,
        metrics: Arc<crate::activity::ActivityMetrics>,
        fx: &mut crate::fixture::ScopeFixture,
    ) -> Result<Arc<dyn OpDispenser>, String> {
        let condition_handle = fx
            .register_pull(condition_field)
            .map_err(|e| format!("conditional `if`: {e}"))?;
        Ok(Arc::new(Self {
            inner,
            condition_handle,
            metrics,
        }))
    }
}

/// Test whether a resolved field value is truthy.

impl WrappingDispenser for ConditionalDispenser {}

impl OpDispenser for ConditionalDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Single read path: condition value via handle from
            // the cycle's ResolvedPulls. Adapter never sees the
            // condition — it's not in fields, so no strip step.
            let value = ctx.pulls.get(self.condition_handle);
            if !crate::wrappers::condition::is_truthy(value) {
                self.metrics.skips_total.inc();
                return Ok(OpResult::skipped());
            }
            self.inner.execute(cycle, ctx).await
        })
    }
    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}
