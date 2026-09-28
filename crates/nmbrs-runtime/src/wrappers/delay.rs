// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Per-cycle delay wrapper. Reads delay values through the
//! cycle's pull plan and sleeps before and/or after delegating
//! to the inner op. u64 → nanoseconds; f64 → milliseconds.
//!
//! Two surface forms (see [`nmbrs_workload::model::DelaySpec`]):
//! - `delay: <name>` — single pre-op delay
//! - `delay: { before: <name>, after: <name> }` — independent
//!   pre-op and/or post-op delays

use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};
use nmbrs_workload::model::DelaySpec;

/// SRD-32a wrapper name.
pub const NAME: WrapperName = WrapperName::new("delay");

/// Trigger: an op declares any `delay:` spec.
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template.delay.is_some()
}

/// One-line assignment summary for init-time diagnostics.
fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    template.delay.as_ref().map(|spec| match spec {
        DelaySpec::Before(name) => {
            let trimmed = crate::wrapper_registrations::trim_braces(name);
            format!("delay: delay binding `{trimmed}`")
        }
        DelaySpec::BeforeAfter { before, after } => {
            let b = before.as_deref().map(|n| {
                let t = crate::wrapper_registrations::trim_braces(n);
                format!("before=`{t}`")
            });
            let a = after.as_deref().map(|n| {
                let t = crate::wrapper_registrations::trim_braces(n);
                format!("after=`{t}`")
            });
            let parts: Vec<String> = [b, a].into_iter().flatten().collect();
            format!("delay: {}", parts.join(", "))
        }
    })
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        owned_fields: &["delay"],
        triggers,
        requires_inner: &[super::traverse::NAME],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Wraps an inner OpDispenser with per-cycle delays.
///
/// Reads delay values via `PullHandle`s from the cycle's
/// `ResolvedPulls`. u64 values are interpreted as nanoseconds;
/// f64 values are interpreted as milliseconds. Delays are
/// invisible to the inner adapter — they're never in
/// `ResolvedFields`.
pub struct DelayDispenser {
    inner: Arc<dyn OpDispenser>,
    /// Pre-op delay handle, when configured.
    before_handle: Option<crate::fixture::PullHandle>,
    /// Post-op delay handle, when configured.
    after_handle: Option<crate::fixture::PullHandle>,
}

impl DelayDispenser {
    /// Wrap an inner dispenser with a single pre-op delay
    /// binding. Backwards-compatible entry point for the bare-
    /// string `delay: <name>` form.
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        delay_field: &str,
        fx: &mut crate::fixture::ScopeFixture,
    ) -> Result<Arc<dyn OpDispenser>, String> {
        let before_handle = Some(
            fx.register_pull(delay_field)
                .map_err(|e| format!("delay: {e}"))?,
        );
        Ok(Arc::new(Self {
            inner,
            before_handle,
            after_handle: None,
        }))
    }

    /// Wrap an inner dispenser with optional pre-op and/or
    /// post-op delays. At least one must be set; the caller is
    /// expected to have validated that.
    pub fn wrap_before_after(
        inner: Arc<dyn OpDispenser>,
        before_name: Option<&str>,
        after_name: Option<&str>,
        fx: &mut crate::fixture::ScopeFixture,
    ) -> Result<Arc<dyn OpDispenser>, String> {
        let before_handle = match before_name {
            Some(name) => Some(
                fx.register_pull(name)
                    .map_err(|e| format!("delay.before: {e}"))?,
            ),
            None => None,
        };
        let after_handle = match after_name {
            Some(name) => Some(
                fx.register_pull(name)
                    .map_err(|e| format!("delay.after: {e}"))?,
            ),
            None => None,
        };
        if before_handle.is_none() && after_handle.is_none() {
            return Err("delay: empty before/after — at least one must be set".into());
        }
        Ok(Arc::new(Self {
            inner,
            before_handle,
            after_handle,
        }))
    }
}

fn value_to_nanos(value: &polydat::ast::Value) -> u64 {
    match value {
        polydat::ast::Value::U64(ns) => *ns,
        polydat::ast::Value::F64(ms) => (*ms * 1_000_000.0) as u64,
        _ => 0,
    }
}

impl WrappingDispenser for DelayDispenser {}

impl OpDispenser for DelayDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if let Some(h) = self.before_handle {
                let nanos = value_to_nanos(ctx.pulls.get(h));
                if nanos > 0 {
                    tokio::time::sleep(std::time::Duration::from_nanos(nanos)).await;
                }
            }
            let result = self.inner.execute(cycle, ctx).await?;
            if let Some(h) = self.after_handle {
                let nanos = value_to_nanos(ctx.pulls.get(h));
                if nanos > 0 {
                    tokio::time::sleep(std::time::Duration::from_nanos(nanos)).await;
                }
            }
            Ok(result)
        })
    }
    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}
