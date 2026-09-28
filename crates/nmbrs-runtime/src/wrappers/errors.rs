// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Error-handler wrapper — the OUTERMOST op-level wrapper (SRD-82 Part 3b).
//!
//! Owns the op shell's terminal-error handling, promoted out of the fiber
//! loop's inline block: it routes the ONE terminal outcome of the stack
//! below it through the op's resolved [`ErrorPolicy`], tallies the
//! result-level error counters, captures the error into the phase's
//! structured buffer, and applies the `stop`/`fail` effects. Every op
//! dispenser carries its own policy — the op-level `errors:` override or
//! the enclosing shell's policy shared by reference — so a lenient op
//! never softens its siblings.
//!
//! **The happy path is one branch.** `Ok` results pass through untouched;
//! the pattern match, verb chain (`count` / `warn` / `ignore` / …), and
//! effects run only in the `Err` arm. The router's verbs stay uniform
//! *inside* the compiled policy (the `nmbrs-errorhandler` `ErrorHandler`
//! chain) rather than as stacked dispenser layers — see SRD-82 Part 3b
//! for why per-verb wrappers were rejected (runtime rule-matched
//! activation, per-rule verb order, and happy-path layer cost).
//!
//! This wrapper also owns the op's PANIC BACKSTOP: a panic anywhere in
//! the stack below (traverse / result / metrics / adapter) is caught
//! here, synthesised into a `panic` op error, and routed through the
//! policy like any other terminal failure — so a panicking layer can
//! never kill the fiber, and the policy decides its disposition. (The
//! innermost `TriesDispenser`, when present, catches per-ATTEMPT panics
//! first; this is the whole-stack backstop.)

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::activity::ActivityMetrics;
use crate::adapter::{AdapterError, ExecutionError, OpDispenser, OpResult, WrappingDispenser};
use crate::error_policy::ErrorPolicy;
use crate::phase_outcome::PhaseErrorDetail;
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

pub const NAME: WrapperName = WrapperName::new("errors");

/// Cap on the phase's structured error-capture buffer so a runaway
/// phase doesn't unbound it. The TRUE total keeps counting through
/// `errors_total`; only the captured detail set is capped.
const PHASE_ERROR_CAPTURE_CAP: usize = 64;

/// Every op has an effective error policy (the session root seeds the
/// `.*:warn,stop` default), so the handler applies to every op.
fn triggers(s: WrapperSubject) -> bool {
    s.op().is_some()
}

/// Show the op-level `errors:` override when one is declared; the
/// inherited default is boilerplate (every op has it) and stays quiet.
fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let op = s.op()?;
    op.params
        .get("errors")
        .and_then(|v| v.as_str())
        .map(|spec| format!("errors: {spec}"))
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        owned_fields: &["errors"],
        triggers,
        requires_inner: &[],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Wraps the whole op stack with terminal-error routing driven by the
/// op's resolved [`ErrorPolicy`]. Constructed OUTERMOST — hand-placed
/// after the plan cascade, mirroring how the retry wrapper is
/// hand-placed innermost — so it observes exactly one terminal outcome
/// per cycle.
pub struct ErrorHandlerDispenser {
    inner: Arc<dyn OpDispenser>,
    /// The op's own resolved policy (op-level `errors:` child or the
    /// enclosing shell's policy by reference). Only the op-error ROUTER
    /// is consulted here; the aggregate rate guard stays at the phase
    /// shell.
    policy: Arc<ErrorPolicy>,
    /// Activity-level metrics — result-level error tallies
    /// (`errors_total`, per-error-type counters).
    metrics: Arc<ActivityMetrics>,
    /// The phase's structured error-capture buffer, rendered by the
    /// `error_readout` phase-end body.
    phase_errors: Arc<Mutex<Vec<PhaseErrorDetail>>>,
    /// The phase's cooperative stop flag — set on a `stop` effect.
    stop_flag: Arc<AtomicBool>,
    /// First stopping error diagnostic — lock-and-set-once.
    stop_reason: Arc<Mutex<Option<String>>>,
    /// Op-template name for diagnostics.
    op_name: String,
    /// SRD-82 Part 3b — when the CONDITIONAL retry wrapper is absent (no
    /// `retry` sigil resolved), this wrapper records the single-attempt
    /// `attempt_*` tallies so the attempt-success display (`att:%`) stays
    /// truthful: for a single-attempt op, attempt == result. When the retry
    /// wrapper IS present it owns the per-attempt counters and this is
    /// `false` (never double-count).
    records_attempts: bool,
}

impl ErrorHandlerDispenser {
    #[allow(clippy::too_many_arguments)]
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        policy: Arc<ErrorPolicy>,
        metrics: Arc<ActivityMetrics>,
        phase_errors: Arc<Mutex<Vec<PhaseErrorDetail>>>,
        stop_flag: Arc<AtomicBool>,
        stop_reason: Arc<Mutex<Option<String>>>,
        op_name: String,
        records_attempts: bool,
    ) -> Arc<dyn OpDispenser> {
        Arc::new(Self {
            inner,
            policy,
            metrics,
            phase_errors,
            stop_flag,
            stop_reason,
            op_name,
            records_attempts,
        })
    }

    /// The `Err`-arm routing: classify the terminal failure through the
    /// policy's router, tally, capture, and apply effects. Runs ONLY on
    /// the error path — never on a successful cycle.
    fn route_terminal_error(
        &self,
        e: &ExecutionError,
        cycle: u64,
        wires: &dyn crate::wires::WireSource,
        service_nanos: u64,
    ) {
        let inner_err = e.error();
        let detail = self.policy.router.handle_error(
            &inner_err.error_name,
            &inner_err.message,
            cycle,
            service_nanos,
        );
        self.metrics.errors_total.inc();
        self.metrics.count_error_type(&detail.name);

        // Capture the terminal error into the phase's structured error
        // buffer so the `error_readout` (default phase-end body alongside
        // `phase_outcome`) can render them as one block.
        if let Ok(mut errs) = self.phase_errors.lock() {
            if errs.len() < PHASE_ERROR_CAPTURE_CAP {
                let op_template = self.inner.describe();
                let op_resolved = self.inner.describe_resolved(wires);
                errs.push(PhaseErrorDetail {
                    class: inner_err.error_name.clone(),
                    message: inner_err.message.clone(),
                    op_name: Some(self.op_name.clone()),
                    cycle: Some(cycle),
                    op_template,
                    op_resolved,
                    at_nanos: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0),
                    retryable: detail.is_retryable(),
                });
            }
        }

        if detail.should_stop {
            self.stop_flag.store(true, Ordering::Relaxed);
            // Capture the first stopping error so the phase-level error
            // surfaces a real diagnostic — op-template name, cycle, and
            // the dispenser's `describe()` (the actual statement /
            // request). Lock-and-set-once; later fibers' errors don't
            // overwrite.
            if let Ok(mut slot) = self.stop_reason.lock()
                && slot.is_none()
            {
                let op_shape = self
                    .inner
                    .describe()
                    .map(|d| format!("\n    op-template: {d}"))
                    .unwrap_or_default();
                let op_resolved = self
                    .inner
                    .describe_resolved(wires)
                    .map(|d| format!("\n    op-resolved: {d}"))
                    .unwrap_or_default();
                // Headline = first line of the message; the full
                // enriched text (multi-line panic diagnostics)
                // was captured verbatim into phase_errors above
                // and renders once in the `errors:` block
                // (SRD-82 §"Panic reporting: one full render").
                let first = inner_err
                    .message
                    .lines()
                    .next()
                    .unwrap_or(&inner_err.message);
                *slot = Some(format!(
                    "[{}] op '{}' at cycle {}: {first}{op_shape}{op_resolved}",
                    inner_err.error_name, self.op_name, cycle,
                ));
            }
        }
    }
}

impl WrappingDispenser for ErrorHandlerDispenser {}

impl OpDispenser for ErrorHandlerDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let service_start = Instant::now();
            // Whole-stack panic backstop: a panic in ANY layer below
            // (outer wrappers included — the retry wrapper's per-attempt
            // catch only covers the adapter call) becomes a synthesised
            // `panic` op error and flows through the same routing as any
            // terminal failure. The fiber above never sees an unwind.
            let outcome: Result<OpResult, ExecutionError> = {
                use futures::FutureExt as _;
                match std::panic::AssertUnwindSafe(self.inner.execute(cycle, ctx))
                    .catch_unwind()
                    .await
                {
                    Ok(r) => r,
                    Err(payload) => {
                        let msg = payload
                            .downcast_ref::<&'static str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "<non-string panic payload>".into());
                        Err(ExecutionError::Op(AdapterError {
                            error_name: "panic".into(),
                            message: msg,
                            retryable: false,
                        }))
                    }
                }
            };
            match outcome {
                // Happy path: one branch (plus the single-attempt tally when
                // no retry wrapper owns the attempt counters), untouched
                // passthrough. A skipped op (`if:` short-circuit, dryrun
                // stand-in) never reached the adapter — no attempt to record,
                // matching the geometry when retry sat inside `if:`.
                Ok(result) => {
                    if self.records_attempts && !result.skipped {
                        let dt = service_start.elapsed().as_nanos() as u64;
                        self.metrics.attempt_total.inc();
                        self.metrics.attempt_success.observe(dt);
                        self.metrics.tries_histogram.record(1);
                    }
                    Ok(result)
                }
                Err(e) => {
                    let service_nanos = service_start.elapsed().as_nanos() as u64;
                    if self.records_attempts {
                        self.metrics.attempt_total.inc();
                        self.metrics.attempt_failure.observe(service_nanos);
                        self.metrics.tries_histogram.record(1);
                    }
                    self.route_terminal_error(&e, cycle, ctx.wires, service_nanos);
                    Err(e)
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
    use super::*;
    use crate::adapter::ResultBody;
    use crate::fixture::{ExecCtx, ResolvedPulls};
    use nmbrs_metrics::labels::Labels;

    /// Inner stub: succeeds or fails with a named error.
    struct FakeInner {
        error: Option<(String, String)>,
        panics: bool,
    }

    impl OpDispenser for FakeInner {
        fn execute<'a>(
            &'a self,
            _cycle: u64,
            _ctx: &'a ExecCtx<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
        > {
            Box::pin(async move {
                if self.panics {
                    panic!("inner blew up");
                }
                match &self.error {
                    Some((name, msg)) => Err(ExecutionError::Op(AdapterError {
                        error_name: name.clone(),
                        message: msg.clone(),
                        retryable: false,
                    })),
                    None => Ok(OpResult {
                        body: None::<Box<dyn ResultBody>>,
                        skipped: false,
                    }),
                }
            })
        }
    }

    struct Harness {
        metrics: Arc<ActivityMetrics>,
        phase_errors: Arc<Mutex<Vec<PhaseErrorDetail>>>,
        stop_flag: Arc<AtomicBool>,
        stop_reason: Arc<Mutex<Option<String>>>,
    }

    fn wrap_with(spec: &str, inner: FakeInner) -> (Arc<dyn OpDispenser>, Harness) {
        let policy = ErrorPolicy::standalone(crate::error_policy::PolicyConfig::new(spec, None));
        let h = Harness {
            metrics: Arc::new(ActivityMetrics::new(&Labels::empty())),
            phase_errors: Arc::new(Mutex::new(Vec::new())),
            stop_flag: Arc::new(AtomicBool::new(false)),
            stop_reason: Arc::new(Mutex::new(None)),
        };
        let d = ErrorHandlerDispenser::wrap(
            Arc::new(inner),
            policy,
            h.metrics.clone(),
            h.phase_errors.clone(),
            h.stop_flag.clone(),
            h.stop_reason.clone(),
            "test_op".into(),
            /* records_attempts */ true,
        );
        (d, h)
    }

    fn empty_ctx() -> (crate::adapter::ResolvedFields, ResolvedPulls) {
        let fields = crate::adapter::ResolvedFields::new(vec![], vec![]);
        let pulls = ResolvedPulls::empty();
        (fields, pulls)
    }

    /// Happy path: an Ok result passes through with NO routing side
    /// effects — no tallies, no capture, no stop.
    #[tokio::test]
    async fn ok_passes_through_untouched() {
        let (d, h) = wrap_with(
            ".*:warn,stop",
            FakeInner {
                error: None,
                panics: false,
            },
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        d.execute(0, &ctx).await.expect("ok");
        assert_eq!(h.metrics.errors_total.get(), 0);
        assert!(h.phase_errors.lock().unwrap().is_empty());
        assert!(!h.stop_flag.load(Ordering::Relaxed));
    }

    /// A `stop` policy routes the terminal error: tally + capture +
    /// stop flag + first-stop diagnostic.
    #[tokio::test]
    async fn stop_policy_sets_flag_and_captures() {
        let (d, h) = wrap_with(
            ".*:warn,stop",
            FakeInner {
                error: Some(("ModelError".into(), "boom".into())),
                panics: false,
            },
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let err = d.execute(7, &ctx).await.expect_err("must propagate");
        assert_eq!(err.error().error_name, "ModelError");
        assert_eq!(h.metrics.errors_total.get(), 1);
        assert!(
            h.stop_flag.load(Ordering::Relaxed),
            "stop verb must set the flag"
        );
        let reason = h
            .stop_reason
            .lock()
            .unwrap()
            .clone()
            .expect("reason captured");
        assert!(
            reason.contains("test_op") && reason.contains("cycle 7"),
            "diagnostic names op + cycle: {reason}"
        );
        let errs = h.phase_errors.lock().unwrap();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].class, "ModelError");
    }

    /// A lenient policy (`warn,counter`) tallies + captures but does
    /// NOT stop.
    #[tokio::test]
    async fn lenient_policy_counts_without_stopping() {
        let (d, h) = wrap_with(
            ".*:warn,counter",
            FakeInner {
                error: Some(("Timeout".into(), "slow".into())),
                panics: false,
            },
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let _ = d.execute(0, &ctx).await.expect_err("must propagate");
        assert_eq!(h.metrics.errors_total.get(), 1);
        assert!(
            !h.stop_flag.load(Ordering::Relaxed),
            "counter/warn must not stop"
        );
        assert!(h.stop_reason.lock().unwrap().is_none());
    }

    /// A panic below the wrapper is caught, synthesised as a `panic`
    /// error, and routed like any terminal failure — the fiber above
    /// never unwinds.
    #[tokio::test]
    async fn panic_below_is_routed_not_unwound() {
        let (d, h) = wrap_with(
            ".*:warn,stop",
            FakeInner {
                error: None,
                panics: true,
            },
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let err = d.execute(0, &ctx).await.expect_err("panic becomes Err");
        assert_eq!(err.error().error_name, "panic");
        assert!(h.stop_flag.load(Ordering::Relaxed));
        assert_eq!(h.phase_errors.lock().unwrap()[0].class, "panic");
    }

    /// The capture buffer caps at PHASE_ERROR_CAPTURE_CAP while the
    /// true total keeps counting.
    #[tokio::test]
    async fn capture_buffer_caps_but_totals_keep_counting() {
        let (d, h) = wrap_with(
            ".*:counter",
            FakeInner {
                error: Some(("E".into(), "m".into())),
                panics: false,
            },
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        for c in 0..(PHASE_ERROR_CAPTURE_CAP as u64 + 10) {
            let _ = d.execute(c, &ctx).await;
        }
        assert_eq!(
            h.phase_errors.lock().unwrap().len(),
            PHASE_ERROR_CAPTURE_CAP
        );
        assert_eq!(
            h.metrics.errors_total.get(),
            PHASE_ERROR_CAPTURE_CAP as u64 + 10
        );
    }
}
