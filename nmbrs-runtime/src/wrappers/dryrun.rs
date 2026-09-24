// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Dryrun short-circuit wrapper.
//!
//! Installed as the outermost wrapper when the runner is in
//! `dryrun=` mode. At execute it returns an empty `OpResult`
//! without calling its inner — wraps the op and does NOT call it,
//! per the design contract. Sits outside every other wrapper so
//! verify / metrics / poll / etc. never observe the empty result
//! and can't fire spurious diagnostics.
//!
//! Under `dryrun=cycle` the real adapter still constructs in
//! full — connecting, preparing statements, gathering metadata —
//! because `dryrun=cycle` means "make the cycle path fully
//! executable, then suppress only the outbound `execute()`." The
//! wrapper handles that suppression at cycle time; no adapter
//! substitution is needed.

use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

/// SRD-32a wrapper name.
pub const NAME: WrapperName = WrapperName::new("dryrun");

/// Trigger: op carries the injected `dryrun:` parameter (a
/// session-originated marker, NOT something workload authors
/// write by hand — see `inject_dryrun_intent`).
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template.params.contains_key("dryrun")
}

/// Reports the mode (`emit` / `silent` / `json`) from the
/// injected `dryrun:` parameter.
fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    let v = template.params.get("dryrun")?;
    let mode = v.as_str().unwrap_or("silent");
    Some(format!("dryrun: short-circuit (mode={mode})"))
}

/// DRYRUN must be the absolute outermost wrapper so its
/// short-circuit happens BEFORE any inner wrapper (verify /
/// metrics / poll / etc.) observes the empty body.
const FORBIDS_OUTER: &[WrapperName] = &[
    super::traverse::NAME,
    super::delay::NAME,
    crate::validation::WRAPPER_NAME,
    super::poll::NAME,
    super::r#if::NAME,
    // `fields` is INTENTIONALLY allowed outer of dryrun — under
    // `dryrun=fields` the fields wrapper's pre-execute render is
    // the surface that produces operator-visible output, so it
    // must run before DRYRUN's short-circuit.
    super::result::NAME,
    super::metrics::NAME,
    super::memo::NAME,
    super::gutter::NAME,
    // `while` loops the entire wrapper stack inner of it; dryrun
    // must short-circuit BEFORE the loop or the dry-run pass
    // would burn cycles iterating a no-op stand-in.
    super::r#while::NAME,
    // `rate` introduces a per-iteration wait; dryrun must
    // short-circuit BEFORE the wait or the dry-run pass would
    // sit on the rate-limiter for nothing.
    super::rate::NAME,
];

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        // `owned_fields = ["dryrun"]` participates in the parse-
        // time misplaced-field guard the same way every other
        // wrapper's owned fields do, so a workload that
        // erroneously declares `dryrun:` on an op surfaces an
        // init-time error pointing here.
        owned_fields: &["dryrun"],
        triggers,
        requires_inner: &[],
        forbids_outer: FORBIDS_OUTER,
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Wraps an inner [`OpDispenser`] and short-circuits per cycle
/// — does nothing more than wrap the op and not call it.
///
/// Per-cycle contract: `execute` returns `Ok(OpResult { body:
/// None, skipped: true })` without touching `ctx.wires`, the
/// inner dispenser, or any field templates. Whatever per-cycle
/// preparation work the real adapter would do happens on demand
/// when its dispenser is called — under dryrun the dispenser
/// isn't called, so no preparation work fires.
///
/// **Not the place for emit/json display.** The historical
/// emit/json modes that printed resolved fields per cycle were
/// extra work this wrapper had no business doing — those belong
/// in a separate display wrapper or the adapter itself. The
/// dryrun wrapper's only job is to suppress the outbound call.
pub struct DryRunWrapper {
    inner: Arc<dyn OpDispenser>,
}

impl DryRunWrapper {
    pub fn wrap(inner: Arc<dyn OpDispenser>) -> Arc<dyn OpDispenser> {
        Arc::new(Self { inner })
    }
}

impl WrappingDispenser for DryRunWrapper {}

impl OpDispenser for DryRunWrapper {
    fn execute<'a>(
        &'a self,
        _cycle: u64,
        _ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // Pure short-circuit. No field resolution, no wires
            // access, no inner call. `skipped: true` so any
            // outer wrapper that observes the result respects
            // the short-circuit per the existing
            // `if result.skipped { return Ok(result); }` pattern.
            Ok(OpResult {
                body: None,
                skipped: true,
            })
        })
    }

    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{ExecutionError, OpResult};
    use crate::fixture::{ExecCtx, ResolvedPulls};

    /// An inner dispenser that fails the test if its `execute` is
    /// ever called. Use as `DryRunWrapper`'s inner to pin the
    /// short-circuit invariant.
    struct PanicIfCalled;
    impl OpDispenser for PanicIfCalled {
        fn execute<'a>(
            &'a self,
            _cycle: u64,
            _ctx: &'a ExecCtx<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
        > {
            Box::pin(async {
                panic!(
                    "DryRunWrapper invariant violated: inner dispenser was called \
                     in dryrun mode. The wrapper must short-circuit BEFORE any \
                     wrapped layer (verify, metrics, poll, …) observes the result."
                );
            })
        }
    }

    #[tokio::test]
    async fn dry_run_wrapper_short_circuits_inner() {
        let inner: Arc<dyn OpDispenser> = Arc::new(PanicIfCalled);
        let wrapper = DryRunWrapper::wrap(inner);

        let mut kernel =
            polydat::dsl::compile::compile_polydat_interpreter("input cycle: u64\n").unwrap();
        let cw = crate::wires::CycleWires::new(&mut kernel);
        let fields = crate::adapter::ResolvedFields::new(vec![], vec![]);
        let pulls = ResolvedPulls::empty();
        let ctx = ExecCtx::with_wires(&fields, &pulls, &cw);

        let result = wrapper
            .execute(0, &ctx)
            .await
            .expect("dryrun should succeed");
        assert!(result.body.is_none(), "dryrun result carries no body");
        assert!(
            result.skipped,
            "dryrun result is marked skipped so any wrapper that DID sit \
             outside us (defensive) honours the existing skip-on-skipped \
             contract"
        );
    }
}
