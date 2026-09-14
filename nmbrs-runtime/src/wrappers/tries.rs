// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Tries wrapper — the CONDITIONAL innermost op-level wrapper (SRD-82 Part
//! 3b). `tries:` is its sigil: the TOTAL number of attempts an op may make.
//!
//! - **No `tries` in scope** → the wrapper is not constructed; the op runs
//!   single-attempt (the outermost error-handler wrapper records the
//!   single-attempt tallies).
//! - **`tries: 1`** → identical to no wrapper (single attempt); the cascade
//!   skips construction.
//! - **`tries: 0`** → the op FAILS WITHOUT EXECUTING: every cycle yields a
//!   synthesised `tries_zero` op error, routed through the op's error
//!   policy like any terminal failure. The explicit "never run this" knob.
//! - **`tries: N ≥ 2`** → up to `N` total attempts; a retryable attempt
//!   failure re-runs the inner op until the budget is spent.
//!
//! When constructed it owns the whole ATTEMPT→RESULT boundary: it runs the
//! inner op (adapter) one-or-more times, owns the `attempt_*` counters,
//! catches a per-attempt panic, and returns exactly ONE terminal outcome to
//! the layers above. Everything above it — traversal, result-binding,
//! metrics, the outermost error-handler wrapper — sees a single result,
//! never the retries.
//!
//! Retryability is the ADAPTER's signal: an `ExecutionError::Op` whose
//! `retryable` flag is set (CQL timeouts/overloads are). The `errors:`
//! policy is deliberately NOT consulted here — the two surfaces are
//! orthogonal; the policy's `retry` verb participates only by INJECTING a
//! `tries` budget at dispenser build (the SRD-82 Part 3b bridge), never by
//! steering the loop per-cycle.

use std::sync::Arc;
use std::time::Instant;

use crate::activity::ActivityMetrics;
use crate::adapter::{AdapterError, ExecutionError, OpDispenser, OpResult, WrappingDispenser};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

pub const NAME: WrapperName = WrapperName::new("tries");

/// Registry trigger: the op's own `tries:` field. The full activation set is
/// wider — an in-scope `tries` wire, the inherited phase/root `tries`, or an
/// `errors:` retry-verb injection — but those resolve against the kernel /
/// config at dispenser build (the cascade), which a `ParsedOp` predicate
/// cannot see. The registry entry drives field validation + telemetry; the
/// hand-placed innermost construction is authoritative.
fn triggers(s: WrapperSubject) -> bool {
    let Some(op) = s.op() else {
        return false;
    };
    op.params.contains_key("tries")
}

fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let op = s.op()?;
    op.params.get("tries").map(|v| format!("tries: {v}"))
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        // The wrapper owns its sigil plus the standalone companion
        // knobs consumed at wrapper build: retry pacing and the
        // retry-error exemplar sampler (rate fraction, default 0.0
        // = off; max_hz emission ceiling).
        owned_fields: &["tries", "retry_backoff", "retry_backoff_max",
            "retry_backoff_ratio", "retry_exemplar_rate",
            "retry_exemplar_max_hz", "retry_advisory"],
        triggers,
        requires_inner: &[],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Wraps an inner dispenser with a bounded attempt loop over retryable
/// attempt failures, owning the `attempt_*` metrics and the per-attempt
/// panic catch. Constructed only when a `tries` budget of `0` or `≥ 2`
/// resolves for the op (see the module doc; `1` / absent skip construction).
pub struct TriesDispenser {
    inner: Arc<dyn OpDispenser>,
    /// TOTAL attempts the op may make. `0` = fail without executing.
    /// (`1` never reaches construction — the cascade skips the wrapper.)
    tries: u32,
    /// Activity-level metrics — the attempt tallies live here so the whole
    /// phase shares one attempt counter regardless of which op path ran.
    metrics: Arc<ActivityMetrics>,
    /// Backoff pacing between retryable attempts (compaction-demo
    /// diagnosis: an immediate-`continue` retry loop hammers a dying
    /// server, and `tries: 20 × timeout: 60s` makes it look like a
    /// silent stall). Attempt `k`'s wait is `backoff_base_ms *
    /// backoff_ratio^(k-1)`, capped at `backoff_max_ms`, with
    /// deterministic jitter in [50%, 100%] derived from (cycle, attempt)
    /// so runs stay replayable. `base == 0` disables pacing. Set per op
    /// via the `tries:` map form (`backoff: {ratio, min, max}`) or the
    /// standalone `retry_backoff` / `retry_backoff_max` /
    /// `retry_backoff_ratio` params (defaults 100ms / 10s / 2.0).
    backoff_base_ms: u64,
    backoff_max_ms: u64,
    /// Geometric growth factor per retry. `2.0` doubles each wait; `1.0`
    /// holds it constant at the floor. Clamped to `>= 1.0` at use so a
    /// misconfigured ratio never shrinks the backoff.
    backoff_ratio: f64,
    /// SRD-92 cooperative-stop view (the same aspect handed to the
    /// `while:` wrapper): once any layered stop source fires — session
    /// Ctrl-C, activity fault, walk halt, daemon-group completion —
    /// a retryable failure returns terminally instead of starting
    /// fresh attempts through the drain window. Injected at wrap time
    /// so the wrapper never reaches for globals itself.
    stop: crate::session_signals::StopView,
    /// Op template name — identifies the specimen in exemplar lines.
    op_name: String,
    /// Counter-exemplar sampling for errors caught in the retry loop
    /// (`retry_exemplar_rate` / `retry_exemplar_max_hz` params;
    /// default rate 0.0 = off). The error policy never sees a
    /// retried-then-recovered attempt, so without sampling those
    /// messages are visible only as `attempt_failure` counts.
    exemplars: crate::exec_events::ExemplarSampler,
    /// Default-on per-phase retry advisory (`exec_events`): the first
    /// sighting of each error class in the retry loop emits one
    /// advisory line through the shared per-activity gate — a retry
    /// storm identifies itself without flooding the output. `None`
    /// when the op opted out (`retry_advisory: off`).
    advisory: Option<Arc<crate::exec_events::AdvisoryGate>>,
}

impl crate::exec_events::ExecEventSubscriber for TriesDispenser {}

impl TriesDispenser {
    /// Wrap `inner` with a total-attempts budget, retry pacing, and
    /// optional retry-error exemplar sampling.
    #[allow(clippy::too_many_arguments)]
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        tries: u32,
        metrics: Arc<ActivityMetrics>,
        backoff_base_ms: u64,
        backoff_max_ms: u64,
        backoff_ratio: f64,
        stop: crate::session_signals::StopView,
        op_name: String,
        exemplars: crate::exec_events::ExemplarSampler,
        advisory: Option<Arc<crate::exec_events::AdvisoryGate>>,
    ) -> Arc<dyn OpDispenser> {
        Arc::new(Self {
            inner,
            tries,
            metrics,
            backoff_base_ms,
            backoff_max_ms,
            backoff_ratio,
            stop,
            op_name,
            exemplars,
            advisory,
        })
    }
}

/// Runtime-agnostic async sleep. `tokio::time::sleep` binds its
/// timer to the runtime owning the current thread — fibers run on
/// shared pool threads, so under multiple runtimes (the lib-test
/// harness; any embedder) the timer can land on a runtime that
/// shuts down mid-sleep. A detached sleeper thread + oneshot has
/// no such coupling, and backoff is the degraded path — a
/// short-lived thread is noise next to the ≥50ms wait it serves.
async fn portable_sleep_ms(ms: u64) {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        let _ = tx.send(());
    });
    let _ = rx.await;
}

use crate::exec_events::splitmix64;

/// The jittered wait (ms) before retry `attempt_no` (1-based). Geometric
/// growth `base * ratio^(attempt-1)`, capped at `max`, then deterministic
/// jitter into `[50%, 100%]` of the capped value keyed on `(cycle,
/// attempt)` so a replay reproduces the same schedule. `base == 0` disables
/// pacing (returns 0). `ratio` is clamped to `>= 1.0` so a misconfigured
/// value never shrinks the wait; a huge exponent saturates `powi` to +inf,
/// which the `.min(max)` folds to the cap (no integer overflow).
fn backoff_wait_ms(base_ms: u64, max_ms: u64, ratio: f64, attempt_no: u32, cycle: u64) -> u64 {
    if base_ms == 0 {
        return 0;
    }
    let mult = ratio.max(1.0).powi((attempt_no.saturating_sub(1)) as i32);
    let raw = base_ms as f64 * mult;
    let capped = raw.min(max_ms as f64).max(1.0) as u64;
    let h = splitmix64(cycle ^ ((attempt_no as u64) << 48));
    capped / 2 + (h % (capped / 2 + 1))
}

impl WrappingDispenser for TriesDispenser {}

impl OpDispenser for TriesDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // `tries: 0` — the op is configured to fail without executing.
            // Accounted as one failed (zero-length) attempt so the att:%
            // display stays truthful, and routed through the error policy by
            // the outermost error-handler wrapper like any terminal failure.
            if self.tries == 0 {
                self.metrics.attempt_total.inc();
                self.metrics.attempt_failure.observe(0);
                self.metrics.tries_histogram.record(0);
                return Err(ExecutionError::Op(AdapterError {
                    error_name: "tries_zero".into(),
                    message: "tries: 0 — op is configured to fail without executing".into(),
                    retryable: false,
                }));
            }
            // `attempt_no` counts attempts (1-based); total attempts run is
            // at most `tries`. It is also the value recorded into
            // `tries_histogram` per op.
            let mut attempt_no: u32 = 0;
            loop {
                attempt_no += 1;
                let attempt_start = Instant::now();

                // Per-attempt panic catch: an adapter that unwinds out of
                // `execute` (an FFI driver on a broken connection) becomes a
                // synthesised `panic` op error rather than killing the fiber.
                // It is NOT adapter-retryable, so it counts as a failed attempt
                // and (absent a retryable classification) terminates without
                // spinning — the fiber survives regardless.
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
                let dt = attempt_start.elapsed().as_nanos() as u64;

                match outcome {
                    // Attempt instruments count at RESOLUTION, same
                    // discipline as the result instruments: an attempt
                    // exists in the tallies only once it has returned,
                    // so `attempt_total == attempt_success +
                    // attempt_failure` holds at every read and a rate
                    // over `attempt_total` carries no in-flight skew.
                    Ok(result) => {
                        self.metrics.attempt_total.inc();
                        self.metrics.attempt_success.observe(dt);
                        self.metrics.tries_histogram.record(attempt_no as u64);
                        return Ok(result);
                    }
                    Err(e) => {
                        self.metrics.attempt_total.inc();
                        self.metrics.attempt_failure.observe(dt);
                        // Retry only an adapter-retryable OP error, within
                        // the total-attempts budget. Adapter-level errors are
                        // never retried here (they are connection-level, not
                        // per-op).
                        let retryable = matches!(&e, ExecutionError::Op(ad) if ad.retryable);
                        if retryable && attempt_no < self.tries {
                            // Session shutdown: the cooperative drain
                            // waits for the CURRENT attempt only — a
                            // fresh retry is new work, and against a
                            // struggling server it spins through the
                            // whole grace window. Checked again after
                            // the backoff so a Ctrl-C during the wait
                            // also lands.
                            if self.stop.stopped() {
                                self.metrics.tries_histogram.record(attempt_no as u64);
                                return Err(e);
                            }
                            // Default-on advisory: the FIRST sighting of
                            // each error class in this phase announces
                            // itself once — the retry loop is otherwise
                            // silent about what it absorbs.
                            if let Some(gate) = &self.advisory
                                && let ExecutionError::Op(ad) = &e
                                && gate.first_sighting(&ad.error_name)
                            {
                                use crate::exec_events::ExecEventSubscriber as _;
                                self.submit_advisory(
                                    &self.op_name,
                                    cycle,
                                    self.tries,
                                    &ad.error_name,
                                    &ad.message,
                                );
                            }
                            // Counter-exemplar sampling: this error will be
                            // retried, so the error policy never sees it —
                            // a sampled specimen goes to the structured
                            // sink instead (see `exec_events`).
                            if self.exemplars.enabled()
                                && let ExecutionError::Op(ad) = &e
                                && let Some(squelched) = self.exemplars.admit(cycle, attempt_no)
                            {
                                use crate::exec_events::ExecEventSubscriber as _;
                                self.submit_exemplar(&crate::exec_events::ExecExemplar {
                                    op_name: &self.op_name,
                                    cycle,
                                    attempt_no,
                                    tries_budget: self.tries,
                                    error_class: &ad.error_name,
                                    message: &ad.message,
                                    will_retry: true,
                                    squelched_since_last: squelched,
                                });
                            }
                            let wait = backoff_wait_ms(
                                self.backoff_base_ms,
                                self.backoff_max_ms,
                                self.backoff_ratio,
                                attempt_no,
                                cycle,
                            );
                            if wait > 0 {
                                portable_sleep_ms(wait).await;
                            }
                            if self.stop.stopped() {
                                self.metrics.tries_histogram.record(attempt_no as u64);
                                return Err(e);
                            }
                            continue;
                        }
                        // Terminal: hand the failure up to the result level.
                        self.metrics.tries_histogram.record(attempt_no as u64);
                        return Err(e);
                    }
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
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Inner stub: fails with a retryable error until `fail_first` attempts
    /// have been consumed, then succeeds. Counts invocations.
    struct FlakyInner {
        fail_first: u32,
        calls: AtomicU32,
    }

    impl OpDispenser for FlakyInner {
        fn execute<'a>(
            &'a self,
            _cycle: u64,
            _ctx: &'a ExecCtx<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
        > {
            Box::pin(async move {
                let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= self.fail_first {
                    Err(ExecutionError::Op(AdapterError {
                        error_name: "Timeout".into(),
                        message: "flaky".into(),
                        retryable: true,
                    }))
                } else {
                    Ok(OpResult {
                        body: None::<Box<dyn ResultBody>>,
                        skipped: false,
                    })
                }
            })
        }
    }

    fn empty_ctx() -> (crate::adapter::ResolvedFields, ResolvedPulls) {
        let fields = crate::adapter::ResolvedFields::new(vec![], vec![]);
        let pulls = ResolvedPulls::empty();
        (fields, pulls)
    }

    /// The backoff schedule grows geometrically by `ratio`, stays within
    /// `[50%, 100%]` of the capped value, honours the `max` cap, holds
    /// constant at `ratio == 1.0`, and disables at `base == 0`.
    #[test]
    fn backoff_is_geometric_capped_and_jittered() {
        // ratio 2.0, base 100, max 10s: the CAP for attempt k is
        // 100 * 2^(k-1) until it hits 10_000, and the wait lands in
        // [cap/2, cap].
        let caps = [100u64, 200, 400, 800, 1600, 3200, 6400, 10_000, 10_000];
        for (i, &cap) in caps.iter().enumerate() {
            let attempt = (i + 1) as u32;
            let w = backoff_wait_ms(100, 10_000, 2.0, attempt, 42);
            assert!(
                w >= cap / 2 && w <= cap,
                "attempt {attempt}: wait {w} out of [{},{cap}]",
                cap / 2
            );
        }
        // ratio 1.0 holds the wait at the floor every attempt.
        for attempt in 1..=5u32 {
            let w = backoff_wait_ms(200, 10_000, 1.0, attempt, 7);
            assert!(w >= 100 && w <= 200, "constant backoff drifted: {w}");
        }
        // base 0 disables pacing entirely.
        assert_eq!(backoff_wait_ms(0, 10_000, 2.0, 3, 1), 0);
        // Deterministic: same (cycle, attempt) → same wait (replayable).
        assert_eq!(
            backoff_wait_ms(100, 10_000, 2.0, 4, 99),
            backoff_wait_ms(100, 10_000, 2.0, 4, 99)
        );
        // A misconfigured ratio < 1.0 is clamped, never shrinks the wait.
        let w = backoff_wait_ms(100, 10_000, 0.1, 5, 3);
        assert!(
            w >= 50 && w <= 100,
            "sub-1.0 ratio should hold at floor: {w}"
        );
    }

    /// `tries: 0` fails WITHOUT invoking the inner op.
    #[tokio::test]
    async fn tries_zero_fails_without_executing() {
        let _guard = crate::session_signals::STOP_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::session_signals::clear_session_stop_for_test();
        let inner = Arc::new(FlakyInner {
            fail_first: 0,
            calls: AtomicU32::new(0),
        });
        let metrics = Arc::new(ActivityMetrics::new(&Labels::empty()));
        let d = TriesDispenser::wrap(
            inner.clone(),
            0,
            metrics,
            0,
            0,
            2.0,
            crate::session_signals::StopView::default(),
            "test_op".to_string(),
            crate::exec_events::ExemplarSampler::pinned(0.0, 0.0),
            None,
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let err = d.execute(0, &ctx).await.expect_err("tries:0 must fail");
        assert_eq!(err.error().error_name, "tries_zero");
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            0,
            "inner must never be invoked at tries:0"
        );
    }

    /// Session shutdown ends the retry loop: a retryable failure with
    /// budget remaining returns terminally instead of spinning through
    /// the cooperative-drain window on a struggling server.
    #[tokio::test]
    async fn shutdown_stops_retries_immediately() {
        let _guard = crate::session_signals::STOP_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::session_signals::clear_session_stop_for_test();
        // Would fail retryably 50 times — but stop is raised, so the
        // very first failure must be terminal.
        let inner = Arc::new(FlakyInner {
            fail_first: 50,
            calls: AtomicU32::new(0),
        });
        let metrics = Arc::new(ActivityMetrics::new(&Labels::empty()));
        let d = TriesDispenser::wrap(
            inner.clone(),
            100,
            metrics,
            0,
            0,
            2.0,
            crate::session_signals::StopView::default(),
            "test_op".to_string(),
            crate::exec_events::ExemplarSampler::pinned(0.0, 0.0),
            None,
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        crate::session_signals::request_stop();
        let res = d.execute(0, &ctx).await;
        crate::session_signals::clear_session_stop_for_test();
        res.expect_err("failure under shutdown must be terminal");
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            1,
            "no fresh attempts once the session is stopping"
        );
    }

    /// `tries: 3` retries a retryable failure up to 3 TOTAL attempts and
    /// succeeds when the third works.
    #[tokio::test]
    async fn tries_is_a_total_attempt_budget() {
        let _guard = crate::session_signals::STOP_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::session_signals::clear_session_stop_for_test();
        let inner = Arc::new(FlakyInner {
            fail_first: 2,
            calls: AtomicU32::new(0),
        });
        let metrics = Arc::new(ActivityMetrics::new(&Labels::empty()));
        let d = TriesDispenser::wrap(
            inner.clone(),
            3,
            metrics.clone(),
            0,
            0,
            2.0,
            crate::session_signals::StopView::default(),
            "test_op".to_string(),
            crate::exec_events::ExemplarSampler::pinned(0.0, 0.0),
            None,
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        d.execute(0, &ctx).await.expect("third attempt succeeds");
        assert_eq!(inner.calls.load(Ordering::Relaxed), 3);
        assert_eq!(metrics.attempt_total.get(), 3);
    }

    /// The budget is TOTAL attempts: `tries: 2` against an op that needs a
    /// third attempt fails after exactly 2 invocations.
    #[tokio::test]
    async fn budget_exhaustion_is_terminal() {
        let _guard = crate::session_signals::STOP_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::session_signals::clear_session_stop_for_test();
        let inner = Arc::new(FlakyInner {
            fail_first: 5,
            calls: AtomicU32::new(0),
        });
        let metrics = Arc::new(ActivityMetrics::new(&Labels::empty()));
        let d = TriesDispenser::wrap(
            inner.clone(),
            2,
            metrics,
            0,
            0,
            2.0,
            crate::session_signals::StopView::default(),
            "test_op".to_string(),
            crate::exec_events::ExemplarSampler::pinned(0.0, 0.0),
            None,
        );
        let (fields, pulls) = empty_ctx();
        let ctx = ExecCtx::new(&fields, &pulls);
        let err = d.execute(0, &ctx).await.expect_err("budget spent");
        assert_eq!(err.error().error_name, "Timeout");
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            2,
            "tries:2 = exactly two total attempts"
        );
    }
}
