// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Polling / await wrapper. Re-executes the inner op until its
//! row count (optionally projected through a JSON-Pointer path)
//! falls into the configured `[min_rows, max_rows]` window, or
//! the timeout fires. Used for waiting on backend state to
//! settle: SAI index build, compactions, etc.

use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{AdapterError, ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

/// SRD-32a wrapper name.
pub const NAME: WrapperName = WrapperName::new("poll");

/// Trigger: `poll:` may be a bare string (mode only, defaults
/// for everything else) or a map carrying the full config —
/// either form turns the wrapper on.
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template
        .params
        .get("poll")
        .map(|v| v.is_string() || v.is_object())
        .unwrap_or(false)
}

fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    let poll_val = template.params.get("poll")?;
    let (mode, interval, timeout): (String, u64, u64) = match poll_val {
        v if v.is_string() => (v.as_str().unwrap().to_string(), 1000, 300_000),
        v if v.is_object() => {
            let m = v.as_object().unwrap();
            let mode = m
                .get("mode")
                .and_then(|x| x.as_str())
                .unwrap_or("await_empty")
                .to_string();
            let interval = m
                .get("interval_ms")
                .and_then(crate::wrapper_registrations::json_to_u64)
                .unwrap_or(1000);
            let timeout = m
                .get("timeout_ms")
                .and_then(crate::wrapper_registrations::json_to_u64)
                .unwrap_or(300_000);
            (mode, interval, timeout)
        }
        _ => return None,
    };
    Some(format!(
        "poll: every {}ms, timeout {}ms, on `{mode}`",
        interval, timeout
    ))
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        owned_fields: &[
            // `poll:` is the single discriminant for the poll
            // wrapper; every knob (interval_ms, timeout_ms,
            // min_rows, max_rows, json_path, metric_name,
            // max_error_retries) lives under it as a map. The
            // flat `poll_*`-prefix surface was retired.
            "poll",
        ],
        triggers,
        requires_inner: &[super::traverse::NAME],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Wraps an inner dispenser and re-executes it until the result
/// body is empty (zero rows). Used for awaiting conditions like
/// SAI index compaction completing.
///
/// Configured via op params:
/// - `poll_interval_ms`: delay between polls (default: 1000)
/// - `timeout_ms`: maximum total wait (default: 300000 = 5 min)
/// - `poll_condition`: when to stop: "empty" (default) = stop when 0 rows
/// - `poll_max_error_retries`: how many retryable errors to swallow
///   before propagating (default: 0 — strict: any inner error fails
///   the poll immediately, per SRD-03 §"Status-Determination
///   Invariant")
///
/// Per SRD-03 §"Status-Determination Invariant", this wrapper
/// short-circuits on every non-positive case:
///
/// - **Positive case**: inner op returns `OpResult` with an empty
///   body → poll succeeds, this dispenser returns success.
/// - **Any other case**: inner op returns a non-retryable
///   `ExecutionError`, OR a retryable error past the retry limit,
///   OR the timeout fires while non-empty bodies are still coming
///   back → this dispenser returns the error, the activity error
///   router sees it, and (under default `errors:` policy) the
///   phase + the run stop. Errors are never swallowed behind the
///   poll.
pub struct PollingDispenser {
    inner: Arc<dyn OpDispenser>,
    poll_interval: std::time::Duration,
    timeout: std::time::Duration,
    /// Cap on consecutive retryable inner-op errors before the
    /// wrapper propagates upstream. `0` means strict: any
    /// inner-op error fails the poll immediately.
    max_error_retries: u32,
    /// SRD-92 cooperative-stop view (same aspect as the `while:` and
    /// `tries` wrappers). Checked at the top of every poll iteration
    /// so a session/walk/daemon stop abandons the poll instead of
    /// waiting out the cadence or timeout. Injected at wrap time.
    stop: crate::session_signals::StopView,
    /// Named metric for the poll elapsed time (e.g., "index_build_time").
    metric_name: Option<String>,
    /// Threshold for "done": the poll is considered satisfied
    /// when the inner op's row-count is in `[min_rows, max_rows]`.
    /// Default `max_rows=0, min_rows=0` reproduces the historical
    /// `await_empty` semantics (zero rows = done). Use
    /// `min_rows=1, max_rows=1` for "settled to a single row"
    /// cases such as SAI's `sai_sstable_count == 1` after
    /// memtable flush + compaction (without the lower bound the
    /// poll would exit too early at count=0, before the
    /// memtable has flushed).
    min_rows: u64,
    max_rows: u64,
    /// Optional JSON-Pointer path (RFC 6901, e.g. `/value`) that
    /// drills into the result body before computing the count
    /// for the `[min_rows, max_rows]` check. Use this when the
    /// op's body wraps the meaningful payload in an envelope —
    /// notably Jolokia, whose every response is
    /// `{request, value, status, timestamp}` and the actual
    /// answer lives under `.value`. When the addressed sub-tree
    /// is an array, count is its length; a number maps directly
    /// to count; an object or null maps to 1 / 0. Default `None`
    /// uses `body.element_count()` as-is.
    json_path: Option<String>,
    /// `poll.memo` — optional template re-rendered after EVERY poll
    /// iteration (against the wires, so this iteration's captures are
    /// visible) and published to the activity memo. Without it, a
    /// long await shows only the memo wrapper's static `before:` text
    /// while the measured values sit unrendered in wires — the memo
    /// is where the operator is looking, so the measurement belongs
    /// there. When absent but `memo_state` is wired, a generic
    /// `<base memo> — measured N row(s) …` suffix is published
    /// instead, so every poll surfaces its live measurement without
    /// workload changes.
    each_memo: Option<String>,
    /// The activity's memo slot (same ArcSwap the memo wrapper
    /// writes). `None` in tests / callers that don't surface memos.
    memo_state: Option<Arc<arc_swap::ArcSwap<String>>>,
    /// `poll.progress` — optional template whose rendered value is
    /// parsed as an `f64` completion fraction in `[0.0, 1.0]` and
    /// published to the activity's derived-progress override each
    /// iteration (e.g. `"{completion_ratio}"`). Drives the phase
    /// completion bar for phases whose one long op measures its own
    /// progress; cleared when the poll completes so the cycle-based
    /// accounting takes back over.
    progress_template: Option<String>,
    /// Metrics of the owning activity — target of the
    /// derived-progress override. `None` in bare tests.
    /// The op's `gutter:` DURING form, re-published per poll
    /// iteration against that poll's wires — a single long drain op
    /// (`await_empty` over an hours-long compaction) keeps a live
    /// cell instead of one publish at op end. The final-form/`final:`
    /// semantics are untouched (the activity epilogue owns those).
    each_gutter: Option<(crate::wrappers::gutter::GutterKind, String)>,
    /// The activity's shared gutter slot; `None` in bare tests.
    gutter_state: Option<Arc<arc_swap::ArcSwapOption<crate::wrappers::gutter::GutterSpec>>>,
    /// The op's compiled `metrics:` GAUGE slots, re-published per
    /// poll iteration (see `metrics::publish_gauges_lenient`).
    /// Filled by the metrics wrapper's cascade arm AFTER this
    /// dispenser is built (metrics wraps outside poll), hence the
    /// late-bound swap; empty/`None` when the op has no metrics.
    iteration_gauges:
        Option<Arc<arc_swap::ArcSwapOption<Vec<crate::wrappers::metrics::MetricSlot>>>>,
    /// When set, the poll is DONE the first time this predicate reads truthy,
    /// and the row-count window is not consulted.
    ///
    /// The predicate itself lives in the executing node's kernel under
    /// [`crate::wrappers::condition::UNTIL_BINDING`], put there by scope
    /// synthesis. This wrapper never learns whether that kernel belongs to an
    /// op template or a phase — it reads one wire through `ctx.wires`, and
    /// scoping decides which binding answers.
    until: bool,
    /// Values written to the wires on the TERMINATING poll only,
    /// as `(wire_name, expression)` pairs from `poll.on_done:`.
    ///
    /// A poll that watches remote work usually cannot observe that
    /// work in a completed state: `system_views.compactions` (and
    /// every view like it) lists what is RUNNING, so the observation
    /// that means "done" is an empty result — the finished tier's
    /// attributes are gone with it. The terminal sample is therefore
    /// real ("nothing is running") but says nothing about the thing
    /// that finished, and a `max(completion_ratio)` over the series
    /// keeps reporting the last in-flight fraction for work that has
    /// been done for an hour.
    ///
    /// `on_done` proxies the measurement the remote view cannot give
    /// us: at termination these expressions are written to the wires
    /// before the final publish, so the gauges fed by them record the
    /// completed state as if it had been observed.
    on_done: Vec<(String, String)>,
    activity_metrics: Option<Arc<crate::activity::ActivityMetrics>>,
    /// Externally visible metrics for the polling operation.
    pub metrics: Arc<PollingMetrics>,
}

/// Metrics surfaced by the polling wrapper.
pub struct PollingMetrics {
    /// Total polls executed across all invocations.
    pub polls_total: std::sync::atomic::AtomicU64,
    /// Total time spent polling (milliseconds).
    pub poll_elapsed_ms: std::sync::atomic::AtomicU64,
    /// Whether the condition has been met (0 = waiting, 1 = done).
    pub condition_met: std::sync::atomic::AtomicU64,
    /// The last observed value from the poll condition (e.g., number of
    /// remaining tasks). This is the metric that determines completion.
    pub poll_metric: std::sync::atomic::AtomicU64,
}

impl PollingMetrics {
    fn new() -> Self {
        Self {
            polls_total: std::sync::atomic::AtomicU64::new(0),
            poll_elapsed_ms: std::sync::atomic::AtomicU64::new(0),
            condition_met: std::sync::atomic::AtomicU64::new(0),
            poll_metric: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl PollingDispenser {
    /// Wrap an inner dispenser with polling behavior.
    /// Returns the wrapped dispenser and a handle to the metrics.
    ///
    /// `metric_name`: if set, the elapsed poll time is captured as a named
    /// gauge (in seconds) for the summary report.
    /// `max_error_retries`: cap on consecutive retryable inner errors
    /// (default 0 = strict).
    // reason: cohesive wrapper constructor — each argument is a distinct poll
    // policy knob (interval, timeout, retry cap, metric name, row bounds);
    // bundling them into a struct would only relocate the same fields.
    #[allow(clippy::too_many_arguments)]
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        poll_interval_ms: u64,
        timeout_ms: u64,
        max_error_retries: u32,
        metric_name: Option<String>,
        min_rows: u64,
        max_rows: u64,
        json_path: Option<String>,
    ) -> (Arc<dyn OpDispenser>, Arc<PollingMetrics>) {
        Self::wrap_with_status(
            inner,
            poll_interval_ms,
            timeout_ms,
            max_error_retries,
            metric_name,
            min_rows,
            max_rows,
            json_path,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            false,
            crate::session_signals::StopView::default(),
        )
    }

    /// As [`Self::wrap`], plus the live-status handles: the
    /// per-iteration memo template + activity memo slot, and the
    /// derived-progress template + activity metrics. See the field
    /// docs (`each_memo`, `progress_template`) for semantics.
    // pub(crate), not pub: the returned handles include `MetricSlot`, which is crate-private, and
    // every caller (activity.rs wiring, `wrap` above) is in-crate. Declaring it `pub` leaked a
    // private type through a public signature.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn wrap_with_status(
        inner: Arc<dyn OpDispenser>,
        poll_interval_ms: u64,
        timeout_ms: u64,
        max_error_retries: u32,
        metric_name: Option<String>,
        min_rows: u64,
        max_rows: u64,
        json_path: Option<String>,
        each_memo: Option<String>,
        memo_state: Option<Arc<arc_swap::ArcSwap<String>>>,
        progress_template: Option<String>,
        activity_metrics: Option<Arc<crate::activity::ActivityMetrics>>,
        each_gutter: Option<(crate::wrappers::gutter::GutterKind, String)>,
        gutter_state: Option<Arc<arc_swap::ArcSwapOption<crate::wrappers::gutter::GutterSpec>>>,
        iteration_gauges: Option<
            Arc<arc_swap::ArcSwapOption<Vec<crate::wrappers::metrics::MetricSlot>>>,
        >,
        on_done: Vec<(String, String)>,
        until: bool,
        stop: crate::session_signals::StopView,
    ) -> (Arc<dyn OpDispenser>, Arc<PollingMetrics>) {
        let metrics = Arc::new(PollingMetrics::new());
        let dispenser = Arc::new(Self {
            inner,
            poll_interval: std::time::Duration::from_millis(poll_interval_ms),
            timeout: std::time::Duration::from_millis(timeout_ms),
            max_error_retries,
            metric_name,
            min_rows,
            max_rows,
            json_path,
            each_memo,
            memo_state,
            progress_template,
            activity_metrics,
            each_gutter,
            gutter_state,
            iteration_gauges,
            on_done,
            until,
            stop,
            metrics: metrics.clone(),
        });
        (dispenser, metrics)
    }

    /// Per-iteration status publish: memo + derived progress.
    ///
    /// Runs after EVERY poll — including the terminating one, whose
    /// observation is the one that detects completion and is no less
    /// a measurement than the ones before it. Skipping it left every
    /// series ending on the last incomplete sample, so a finished
    /// unit of work read as 93% done forever.
    ///
    /// Idempotent by construction: every publish here is a set, not
    /// an accumulate — `publish_gauges_lenient` handles gauges only,
    /// and the memo / gutter / progress-override slots are
    /// last-write-wins. Publishing the same iteration twice therefore
    /// lands the same state; only the sample timestamp moves.
    ///
    /// Captures for this iteration are already on the wires.
    /// Substitution failures degrade to a debug log — status must
    /// never fail the poll.
    fn publish_iteration_status(
        &self,
        wires: &dyn crate::wires::WireSource,
        base_memo: &str,
        row_count: u64,
        polls: u64,
        elapsed_secs: f64,
    ) {
        if let Some(memo) = &self.memo_state {
            let rendered = self.each_memo.as_deref().and_then(|t| {
                match crate::wires::substitute_via_wires(t, wires) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        crate::diag!(
                            crate::observer::LogLevel::Debug,
                            "poll.memo: substitution failed for '{t}': {e}"
                        );
                        None
                    }
                }
            });
            // Default (no template / render failure): keep the memo the
            // operator already sees and append the measurement to it.
            let text = rendered.unwrap_or_else(|| if self.until {
                // A declared `until:` REPLACED the row-count window, so quoting
                // that window here would describe a gate that is not in effect —
                // and worse, "measured 0 row(s) [target 0..=0]" reads as ALREADY
                // SATISFIED while the poll keeps waiting, which is the most
                // confusing thing a status line can say. Report the condition
                // that is actually holding it, and the observation feeding it.
                format!(
                    "{base_memo} — waiting on `until:` (not yet satisfied) · \
                     {row_count} row(s) observed · poll {polls}, {elapsed_secs:.0}s")
            } else {
                format!(
                    "{base_memo} — measured {row_count} row(s) [target {}..={}] · poll {polls}, {elapsed_secs:.0}s",
                    self.min_rows, self.max_rows)
            });
            memo.store(Arc::new(text));
        }
        // Gauges first: the gutter/memo templates may read metricsql
        // over the very series these samples feed.
        if let Some(handle) = &self.iteration_gauges {
            if let Some(slots) = handle.load_full() {
                crate::wrappers::metrics::publish_gauges_lenient(&slots, wires);
            }
        }
        if let (Some(state), Some((kind, template))) = (&self.gutter_state, &self.each_gutter) {
            if let Some(spec) = crate::wrappers::gutter::render_spec(*kind, template, wires) {
                state.store(Some(Arc::new(spec)));
            }
        }
        if let (Some(metrics), Some(t)) =
            (&self.activity_metrics, self.progress_template.as_deref())
        {
            match crate::wires::substitute_via_wires(t, wires) {
                Ok(s) => match s.trim().parse::<f64>() {
                    // Elapsed rides along so the display can derive the
                    // measured-basis ETA (`elapsed × (1−f)/f`) — the
                    // cycle-based ETA stands still for one long measured op.
                    Ok(f) => metrics.set_progress_override_with_elapsed(f, elapsed_secs),
                    Err(_) => {
                        crate::diag!(
                            crate::observer::LogLevel::Debug,
                            "poll.progress: '{t}' rendered to non-numeric '{s}'"
                        );
                    }
                },
                Err(e) => {
                    crate::diag!(
                        crate::observer::LogLevel::Debug,
                        "poll.progress: substitution failed for '{t}': {e}"
                    );
                }
            }
        }
    }
}

/// Drop guard clearing the derived-progress override when the poll
/// future ends — by completion, timeout, error, OR cancellation (a
/// daemon drop mid-await). Without it a finished/cancelled poll's
/// stale fraction would keep driving the phase bar.
struct ProgressOverrideClear<'m>(Option<&'m crate::activity::ActivityMetrics>);

impl Drop for ProgressOverrideClear<'_> {
    fn drop(&mut self) {
        if let Some(m) = self.0 {
            m.set_progress_override(None);
        }
    }
}

impl WrappingDispenser for PollingDispenser {}

impl OpDispenser for PollingDispenser {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let start = std::time::Instant::now();
            let mut polls = 0u64;
            let mut retryable_errors_consumed: u32 = 0;
            // Memo text as of poll start — the memo wrapper is OUTER,
            // so its `before:` template is already published; the
            // default per-iteration publish appends the measurement
            // to this base rather than compounding onto itself.
            //
            // Stripping a previously-appended suffix is what makes that
            // hold ACROSS executions too: within one poll the base is
            // captured once, but a second execution of the same op would
            // otherwise read back its own decorated text as the base and
            // grow the memo without bound.
            let base_memo: String = self
                .memo_state
                .as_ref()
                .map(|m| strip_measurement_suffix(m.load().as_str()).to_string())
                .unwrap_or_default();
            let _progress_clear = ProgressOverrideClear(self.activity_metrics.as_deref());

            loop {
                // Session shutdown: abandon the poll. The cooperative
                // drain waits for in-flight WORK, not for a poll
                // cadence that may have minutes of timeout left —
                // without this check a Ctrl-C leaves the drain stuck
                // behind the next interval/retry sleep.
                if self.stop.stopped() {
                    return Err(ExecutionError::Op(AdapterError {
                        error_name: "shutdown_cancelled".into(),
                        message: format!(
                            "session stop requested — poll abandoned after {} poll(s), {:.1}s",
                            polls,
                            start.elapsed().as_secs_f64()
                        ),
                        retryable: false,
                    }));
                }
                // SRD-03 §"Status-Determination Invariant":
                // every inner-op outcome other than the
                // specific positive case (empty body, signalling
                // "no remaining build tasks") short-circuits
                // upstream as an error. Retryable errors get a
                // bounded retry budget (`max_error_retries`);
                // non-retryable errors never get retried — they
                // propagate on the first occurrence.
                let result = match self.inner.execute(cycle, ctx).await {
                    Ok(r) => r,
                    Err(e) => {
                        let retryable = match &e {
                            ExecutionError::Op(ad) => ad.retryable,
                            ExecutionError::Adapter(_) => false,
                        };
                        if !retryable {
                            return Err(e);
                        }
                        if retryable_errors_consumed >= self.max_error_retries {
                            return Err(e);
                        }
                        retryable_errors_consumed += 1;
                        let indent = crate::scene_tree::running_phase_indent();
                        let color = crate::observer::use_color();
                        let yellow = if color { "\x1b[33m" } else { "" };
                        let reset = if color { "\x1b[0m" } else { "" };
                        crate::diag!(
                            crate::observer::LogLevel::Warn,
                            "{indent}{yellow}poll retry {retryable_errors_consumed}/{}{reset} after retryable error: {}",
                            self.max_error_retries,
                            match &e {
                                ExecutionError::Op(ad) => &ad.message,
                                ExecutionError::Adapter(ad) => &ad.message,
                            },
                        );
                        // Backoff before retry — same as the
                        // between-polls cadence so a flapping
                        // backend doesn't burn the retry budget
                        // in a tight loop.
                        tokio::time::sleep(self.poll_interval).await;
                        continue;
                    }
                };
                polls += 1;

                // Check condition: row count in [min_rows, max_rows] = done.
                // Default `min_rows=0, max_rows=0` reproduces the legacy
                // `await_empty` (exactly 0 = done) semantics.
                //
                // When `json_path` is set, the count comes from the
                // addressed sub-tree of the body's JSON projection
                // — array length, raw number, or 1 (object) / 0
                // (null/missing). This is the Jolokia-poll path:
                // `getCompactions` returns `{value: [...], status: 200, ...}`
                // and we want the length of `.value`, not 1 (the
                // envelope object).
                let row_count = match (&result.body, self.json_path.as_deref()) {
                    (Some(body), Some(path)) => {
                        let json = body.to_json();
                        count_from_json_pointer(&json, path)
                    }
                    (Some(body), None) => body.element_count(),
                    (None, _) => 0,
                };
                // A declared `until:` REPLACES the row-count window: the
                // workload stated the completion condition, so counting rows
                // would be second-guessing it. An unresolved predicate is a
                // wiring fault, not a false one — failing loudly beats
                // spinning to the timeout with no explanation.
                // Publish the poll's own progress as wires BEFORE evaluating the
                // predicate, so an `until:` can bound its own patience. Without
                // these a condition can only describe the observed world, never
                // "and give up waiting for it" — which turns any never-satisfied
                // predicate into a wait to `timeout_ms`. `poll_elapsed_ms` is
                // the time spent polling THIS invocation; `poll_count` is the
                // number of iterations completed.
                //
                // Written through the same `WireSource::write` path captures
                // use, so the names resolve exactly like any other wire and an
                // `extern poll_elapsed_ms: u64 = 0` declaration picks them up.
                // A workload that never mentions them has no slot, the write
                // is a no-op, and nothing changes.
                let elapsed_ms = start.elapsed().as_millis() as u64;
                ctx.wires
                    .write("poll_elapsed_ms", polydat::ast::Value::U64(elapsed_ms));
                ctx.wires
                    .write("poll_count", polydat::ast::Value::U64(polls));
                let is_done = if self.until {
                    match crate::wrappers::condition::holds(
                        ctx.wires,
                        crate::wrappers::condition::UNTIL_BINDING,
                    ) {
                        Some(done) => done,
                        None => {
                            return Err(ExecutionError::Op(crate::adapter::AdapterError {
                                error_name: "poll_until_unresolved".into(),
                                message: format!(
                                    "poll `until:` predicate did not resolve through \
                                     ctx.wires (binding '{}') — scope synthesis should \
                                     have lowered it into this node's kernel",
                                    crate::wrappers::condition::UNTIL_BINDING
                                ),
                                retryable: false,
                            }));
                        }
                    }
                } else {
                    row_count >= self.min_rows && row_count <= self.max_rows
                };

                self.metrics
                    .poll_metric
                    .store(row_count, std::sync::atomic::Ordering::Relaxed);

                if !is_done {
                    // Per-poll progress goes to the durable
                    // session log at Debug — direct `eprint!`
                    // here would clobber the TUI's render
                    // surface. The TUI surfaces poll progress
                    // via the `poll_metric` gauge (live row
                    // count) which is already updated above.
                    let indent = crate::scene_tree::running_phase_indent();
                    crate::diag!(
                        crate::observer::LogLevel::Debug,
                        "{indent}awaiting: {row_count} row(s), need [{}..={}] ({:.0}s elapsed)",
                        self.min_rows,
                        self.max_rows,
                        start.elapsed().as_secs_f64()
                    );
                    // Live status: this iteration's captures are on the
                    // wires; expose the poll's own counters alongside
                    // them (slot-absent writes no-op) and publish the
                    // measured values to the memo + the derived
                    // phase-progress override.
                    let _ = ctx
                        .wires
                        .write("poll_count", polydat::ast::Value::U64(polls));
                    let _ = ctx.wires.write(
                        "poll_elapsed_ms",
                        polydat::ast::Value::U64(start.elapsed().as_millis() as u64),
                    );
                    self.publish_iteration_status(
                        ctx.wires,
                        &base_memo,
                        row_count,
                        polls,
                        start.elapsed().as_secs_f64(),
                    );
                }
                if is_done {
                    // The terminating observation is a measurement too, and it
                    // is the only one that carries the completion time. It used
                    // to be dropped: the loop published on `!is_done` only, so
                    // every series ended on the last INCOMPLETE sample and
                    // finished work read as partially done forever.
                    //
                    // `on_done` first, then the publish: the wires it writes are
                    // what the gauges read. This is the proxy for a completed
                    // state a remote view cannot show (see the field docs) — the
                    // terminal captures describe an empty result set, not the
                    // work that just finished.
                    let _ = ctx
                        .wires
                        .write("poll_count", polydat::ast::Value::U64(polls));
                    let _ = ctx.wires.write(
                        "poll_elapsed_ms",
                        polydat::ast::Value::U64(start.elapsed().as_millis() as u64),
                    );
                    for (name, expr) in &self.on_done {
                        match crate::wires::substitute_via_wires(expr, ctx.wires) {
                            Ok(rendered) => match rendered.trim().parse::<f64>() {
                                Ok(v) => {
                                    let _ = ctx.wires.write(name, polydat::ast::Value::F64(v));
                                }
                                Err(_) => crate::diag!(
                                    crate::observer::LogLevel::Debug,
                                    "poll.on_done: '{name}: {expr}' rendered to \
                                     non-numeric '{rendered}'"
                                ),
                            },
                            Err(e) => crate::diag!(
                                crate::observer::LogLevel::Debug,
                                "poll.on_done: substitution failed for \
                                 '{name}: {expr}': {e}"
                            ),
                        }
                    }
                    self.publish_iteration_status(
                        ctx.wires,
                        &base_memo,
                        row_count,
                        polls,
                        start.elapsed().as_secs_f64(),
                    );
                }

                if is_done {
                    let elapsed = start.elapsed();
                    let elapsed_secs = elapsed.as_secs_f64();
                    self.metrics
                        .polls_total
                        .fetch_add(polls, std::sync::atomic::Ordering::Relaxed);
                    self.metrics.poll_elapsed_ms.store(
                        elapsed.as_millis() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    self.metrics
                        .condition_met
                        .store(1, std::sync::atomic::Ordering::Relaxed);
                    let indent = crate::scene_tree::running_phase_indent();
                    let color = crate::observer::use_color();
                    let dim = if color { "\x1b[2m" } else { "" };
                    let green = if color { "\x1b[32m" } else { "" };
                    let reset = if color { "\x1b[0m" } else { "" };
                    crate::observer::log(
                        crate::observer::LogLevel::Info,
                        &format!(
                            "{indent}{green}poll complete{reset}: {polls} polls {dim}in {elapsed_secs:.1}s{reset}"
                        ),
                    );
                    // Captures land on the per-fiber kernel directly
                    // via ctx.wires.write — wrappers above this layer
                    // see the values through wires.get on the same
                    // cycle. Slot-absent writes silently no-op
                    // (closure-binding economy).
                    let _ = ctx
                        .wires
                        .write("poll_count", polydat::ast::Value::U64(polls));
                    let _ = ctx.wires.write(
                        "poll_elapsed_ms",
                        polydat::ast::Value::U64(elapsed.as_millis() as u64),
                    );
                    // Emit named metric. The recorded value is the
                    // elapsed wait duration; if `metric_name` carries
                    // a recognized unit suffix (`_ns` / `_us` / `_ms`
                    // / `_s` / `_m` / `_h`), the seconds are
                    // converted so the metric reads in the unit its
                    // name advertises. Names without a recognized
                    // suffix fall through as seconds (legacy
                    // behaviour, used by e.g. `index_build_time`).
                    if let Some(ref name) = self.metric_name {
                        let value = duration_value_for_metric_name(name, elapsed_secs);
                        let _ = ctx.wires.write(name, polydat::ast::Value::F64(value));
                    }
                    return Ok(OpResult {
                        body: None,
                        skipped: false,
                    });
                }

                // Check timeout
                if start.elapsed() > self.timeout {
                    return Err(ExecutionError::Op(AdapterError {
                        error_name: "poll_timeout".into(),
                        message: format!(
                            "polling timed out after {:.1}s ({} polls). Last result had rows.",
                            start.elapsed().as_secs_f64(),
                            polls
                        ),
                        retryable: false,
                    }));
                }

                // Wait before next poll
                tokio::time::sleep(self.poll_interval).await;
            }
        })
    }
    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}

/// Map a named metric's elapsed-time value to the numeric value the
/// metric name advertises. The metric name suffix selects the unit
/// (`_ns`, `_us`, `_ms`, `_s`, `_m`, `_h`); the elapsed seconds are
/// scaled accordingly so a metric called `index_build_time_ms`
/// reads in milliseconds.
///
/// Names without a recognised suffix fall through as seconds
/// (preserves the historical contract — e.g. `index_build_time`
/// used to be emitted raw in seconds and still is).
///
/// Longest suffixes are tested first so `_ms` doesn't tail-bind
/// to a more-permissive `_s` rule by accident.
pub(crate) fn duration_value_for_metric_name(name: &str, elapsed_secs: f64) -> f64 {
    if name.ends_with("_ns") {
        elapsed_secs * 1e9
    } else if name.ends_with("_us") {
        elapsed_secs * 1e6
    } else if name.ends_with("_ms") {
        elapsed_secs * 1e3
    } else if name.ends_with("_s") {
        elapsed_secs
    } else if name.ends_with("_m") {
        elapsed_secs / 60.0
    } else if name.ends_with("_h") {
        elapsed_secs / 3600.0
    } else {
        elapsed_secs
    }
}

/// Drill into a JSON tree via JSON-Pointer path (RFC 6901, e.g.
/// `/value`, `/value/results/0`) and reduce the addressed
/// sub-tree to a u64 count for the polling threshold:
///
/// - Array → `len()` (use case: "list of running jobs is empty").
/// - Number → the integer value (use case: a numeric counter
///   like `Compaction.PendingTasks.Value` reaches zero).
/// - Object → 1 (the addressed payload exists; for "wait until
///   *something* is present" patterns).
/// - Null / missing path → 0 (treat as "nothing there").
///
/// An empty path string addresses the root, matching
/// `serde_json::Value::pointer("")`'s contract.
pub(crate) fn count_from_json_pointer(json: &serde_json::Value, path: &str) -> u64 {
    let Some(v) = json.pointer(path) else {
        return 0;
    };
    match v {
        serde_json::Value::Array(a) => a.len() as u64,
        serde_json::Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().map(|i| i.max(0) as u64))
            .or_else(|| n.as_f64().map(|f| f.max(0.0) as u64))
            .unwrap_or(0),
        serde_json::Value::Object(_) => 1,
        serde_json::Value::Bool(b) => {
            if *b {
                1
            } else {
                0
            }
        }
        serde_json::Value::String(s) if s.is_empty() => 0,
        serde_json::Value::String(_) => 1,
        serde_json::Value::Null => 0,
    }
}

/// Read `poll.on_done:` — the wire values a terminating poll writes before
/// its final publish. Values may be numbers or strings; strings are kept
/// verbatim so they can be wire expressions (`"total"`) rather than only
/// literals.
///
/// Ordered by key so the writes are deterministic across runs — a map
/// iteration order that varied would make one `on_done` entry able to
/// shadow another differently from run to run.
pub(crate) fn parse_on_done(
    cfg: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Vec<(String, String)> {
    let Some(map) = cfg
        .and_then(|m| m.get("on_done"))
        .and_then(|v| v.as_object())
    else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = map
        .iter()
        .map(|(k, v)| {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), text)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Drop the default per-iteration measurement suffix this wrapper appends,
/// so re-deriving the base from a published memo is a fixed point.
/// Matched on both halves of the default shape — a memo that merely
/// contains an em-dash keeps it.
fn strip_measurement_suffix(memo: &str) -> &str {
    match memo.find(" — measured ") {
        Some(i) if memo[i..].contains(" row(s) [target ") => &memo[..i],
        _ => memo,
    }
}

#[cfg(test)]
mod on_done_config_tests {
    use super::parse_on_done;

    fn cfg(json: &str) -> serde_json::Map<String, serde_json::Value> {
        match serde_json::from_str(json).expect("test json") {
            serde_json::Value::Object(m) => m,
            _ => panic!("expected an object"),
        }
    }

    #[test]
    fn absent_or_empty_yields_nothing() {
        assert!(parse_on_done(None).is_empty());
        assert!(parse_on_done(Some(&cfg(r#"{"mode":"await_empty"}"#))).is_empty());
        assert!(parse_on_done(Some(&cfg(r#"{"on_done":{}}"#))).is_empty());
    }

    /// A YAML `completion_ratio: 1.0` arrives as a NUMBER, not a string — the
    /// spelling an operator actually writes must not be silently dropped.
    #[test]
    fn numeric_and_string_values_both_read() {
        let got = parse_on_done(Some(&cfg(
            r#"{"on_done":{"completion_ratio":1.0,"progress":"total"}}"#,
        )));
        assert_eq!(
            got,
            vec![
                ("completion_ratio".to_string(), "1.0".to_string()),
                ("progress".to_string(), "total".to_string()),
            ]
        );
    }

    /// Deterministic order: two entries must be applied the same way on every
    /// run, so a later write shadowing an earlier one is reproducible.
    #[test]
    fn entries_are_ordered_by_key() {
        let got = parse_on_done(Some(&cfg(r#"{"on_done":{"z":1,"a":2,"m":3}}"#)));
        let keys: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["a", "m", "z"]);
    }
}

#[cfg(test)]
mod status_publish_tests {
    use super::*;
    use std::sync::Arc;

    /// Minimal WireSource: one f64 wire named `completion_ratio`.
    struct RatioWire(f64);
    impl crate::wires::WireSource for RatioWire {
        fn get(&self, name: &str) -> Option<polydat::ast::Value> {
            (name == "completion_ratio").then(|| polydat::ast::Value::F64(self.0))
        }
        fn names(&self) -> Box<dyn Iterator<Item = String> + '_> {
            Box::new(std::iter::once("completion_ratio".to_string()))
        }
    }

    fn dispenser_with_status(
        each_memo: Option<&str>,
        progress: Option<&str>,
        memo: &Arc<arc_swap::ArcSwap<String>>,
        metrics: &Arc<crate::activity::ActivityMetrics>,
    ) -> PollingDispenser {
        struct NoopInner;
        impl OpDispenser for NoopInner {
            fn execute<'a>(
                &'a self,
                _cycle: u64,
                _ctx: &'a crate::fixture::ExecCtx<'a>,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
            > {
                Box::pin(async move {
                    Ok(OpResult {
                        body: None,
                        skipped: false,
                    })
                })
            }
        }
        PollingDispenser {
            inner: Arc::new(NoopInner),
            poll_interval: std::time::Duration::from_millis(1),
            timeout: std::time::Duration::from_millis(10),
            max_error_retries: 0,
            metric_name: None,
            min_rows: 0,
            max_rows: 0,
            json_path: None,
            each_memo: each_memo.map(String::from),
            memo_state: Some(memo.clone()),
            progress_template: progress.map(String::from),
            activity_metrics: Some(metrics.clone()),
            each_gutter: None,
            gutter_state: None,
            iteration_gauges: None,
            on_done: Vec::new(),
            until: false,
            stop: crate::session_signals::StopView::default(),
            metrics: Arc::new(PollingMetrics::new()),
        }
    }

    /// A writable wire bag — the terminating-publish tests need
    /// `write` to actually land, which `NullWireSource` refuses.
    struct MapWires(std::sync::Mutex<std::collections::HashMap<String, polydat::ast::Value>>);
    impl MapWires {
        fn new(seed: &[(&str, f64)]) -> Self {
            Self(std::sync::Mutex::new(
                seed.iter()
                    .map(|(k, v)| (k.to_string(), polydat::ast::Value::F64(*v)))
                    .collect(),
            ))
        }
    }
    impl crate::wires::WireSource for MapWires {
        fn get(&self, name: &str) -> Option<polydat::ast::Value> {
            self.0.lock().unwrap().get(name).cloned()
        }
        fn names(&self) -> Box<dyn Iterator<Item = String> + '_> {
            let v: Vec<String> = self.0.lock().unwrap().keys().cloned().collect();
            Box::new(v.into_iter())
        }
        fn write(&self, name: &str, value: polydat::ast::Value) -> crate::wires::WriteOutcome {
            self.0.lock().unwrap().insert(name.to_string(), value);
            crate::wires::WriteOutcome::Stored
        }
    }

    /// Run a poll to completion against `wires`. `NoopInner` returns an
    /// empty body, so the FIRST poll is the terminating one — exactly the
    /// iteration whose measurement used to be dropped.
    fn run_to_done(d: &PollingDispenser, wires: &dyn crate::wires::WireSource) {
        // The poll abandons on a session stop, and the stop flag is
        // process-global: hold it clear against sibling tests that set it.
        let _guard = crate::session_signals::STOP_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::session_signals::clear_session_stop_for_test();
        let fields = crate::adapter::ResolvedFields::new(Vec::new(), Vec::new());
        let pulls = crate::fixture::ResolvedPulls::empty();
        let ctx = crate::fixture::ExecCtx::with_wires(&fields, &pulls, wires);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        rt.block_on(d.execute(0, &ctx)).expect("poll completes");
    }

    /// The measurement that DETECTS completion must reach metrics like every
    /// incomplete one before it. Without this the last sample published is the
    /// final not-yet-done poll, so a finished unit of work reads as partially
    /// done for as long as the series is kept.
    #[test]
    fn the_terminating_poll_publishes_its_measurement() {
        let memo = Arc::new(arc_swap::ArcSwap::from_pointee(String::from("base")));
        let metrics = test_metrics();
        let mut d = dispenser_with_status(None, None, &memo, &metrics);
        let (slot, gauge) =
            crate::wrappers::metrics::test_gauge_slot("completion", "completion_ratio");
        d.iteration_gauges = Some(Arc::new(arc_swap::ArcSwapOption::from_pointee(vec![slot])));
        let wires = MapWires::new(&[("completion_ratio", 0.42)]);

        run_to_done(&d, &wires);

        assert_eq!(
            gauge.get(),
            0.42,
            "the terminating poll's measurement must reach the gauge"
        );
    }

    /// A remote view of in-flight work cannot show the finished item — it is
    /// gone from the view, which is *why* the poll ended. `on_done` proxies
    /// that unobservable completed state onto the final sample.
    #[test]
    fn on_done_proxies_the_completion_a_remote_view_cannot_show() {
        let memo = Arc::new(arc_swap::ArcSwap::from_pointee(String::from("base")));
        let metrics = test_metrics();
        let mut d = dispenser_with_status(None, None, &memo, &metrics);
        let (slot, gauge) =
            crate::wrappers::metrics::test_gauge_slot("completion", "completion_ratio");
        d.iteration_gauges = Some(Arc::new(arc_swap::ArcSwapOption::from_pointee(vec![slot])));
        d.on_done = vec![("completion_ratio".into(), "1.0".into())];
        // The last value the view ever showed: 42% done, then it vanished.
        let wires = MapWires::new(&[("completion_ratio", 0.42)]);

        run_to_done(&d, &wires);

        assert_eq!(
            gauge.get(),
            1.0,
            "on_done must override the stale in-flight value"
        );
    }

    /// Idempotence: these publishes are sets, not accumulates, so the same
    /// terminating observation applied twice lands the same state. That is what
    /// makes it safe to publish the final measurement in addition to whatever
    /// the cadence already emitted for the same moment.
    #[test]
    fn republishing_the_terminating_measurement_is_idempotent() {
        let memo = Arc::new(arc_swap::ArcSwap::from_pointee(String::from("base")));
        let metrics = test_metrics();
        let mut d = dispenser_with_status(None, None, &memo, &metrics);
        let (slot, gauge) =
            crate::wrappers::metrics::test_gauge_slot("completion", "completion_ratio");
        d.iteration_gauges = Some(Arc::new(arc_swap::ArcSwapOption::from_pointee(vec![slot])));
        d.on_done = vec![("completion_ratio".into(), "1.0".into())];
        let wires = MapWires::new(&[("completion_ratio", 0.42)]);

        run_to_done(&d, &wires);
        let first = gauge.get();
        let memo_after_first = memo.load_full();
        run_to_done(&d, &wires);
        let second = gauge.get();

        assert_eq!(
            first, second,
            "a repeated terminal publish must not shift the value"
        );
        assert_eq!(
            *memo_after_first,
            *memo.load_full(),
            "a repeated terminal publish must not accumulate into the memo"
        );
    }

    fn test_metrics() -> Arc<crate::activity::ActivityMetrics> {
        Arc::new(crate::activity::ActivityMetrics::new(
            &nmbrs_metrics::labels::Labels::default(),
        ))
    }

    #[test]
    fn progress_template_publishes_override_and_memo_renders() {
        let memo = Arc::new(arc_swap::ArcSwap::from_pointee(String::from("base")));
        let metrics = test_metrics();
        let d = dispenser_with_status(
            Some("ratio {completion_ratio}"),
            Some("{completion_ratio}"),
            &memo,
            &metrics,
        );
        let wires = RatioWire(0.42);
        d.publish_iteration_status(&wires, "base", 1, 3, 15.0);
        assert_eq!(
            metrics.progress_override(),
            Some(0.42),
            "progress template must publish the derived override"
        );
        assert_eq!(memo.load().as_str(), "ratio 0.42");
    }

    #[test]
    fn default_memo_suffix_without_template() {
        let memo = Arc::new(arc_swap::ArcSwap::from_pointee(String::from("waiting")));
        let metrics = test_metrics();
        let d = dispenser_with_status(None, None, &memo, &metrics);
        let wires = RatioWire(0.9);
        d.publish_iteration_status(&wires, "waiting", 4, 7, 33.0);
        assert!(
            memo.load().contains("measured 4 row(s)"),
            "default memo must carry the measurement: {}",
            memo.load()
        );
        assert_eq!(
            metrics.progress_override(),
            None,
            "no progress template -> no override"
        );
    }

    #[test]
    fn iteration_status_republishes_gutter_during_form() {
        // SRD-92: an op with a `gutter:` DURING form inside a poll
        // drain refreshes its cell per poll iteration — the
        // GutterDispenser alone publishes only once, at the end of
        // the (potentially hours-long) drain op. Substitution runs
        // against the iteration's wires, so the cell tracks the
        // measured state exactly like the poll memo.
        let memo = Arc::new(arc_swap::ArcSwap::from_pointee(String::new()));
        let metrics = test_metrics();
        let gutter_state: Arc<arc_swap::ArcSwapOption<crate::wrappers::gutter::GutterSpec>> =
            Arc::new(arc_swap::ArcSwapOption::empty());
        let mut d = dispenser_with_status(None, None, &memo, &metrics);
        d.each_gutter = Some((
            crate::wrappers::gutter::GutterKind::Text,
            "ratio {completion_ratio}".into(),
        ));
        d.gutter_state = Some(gutter_state.clone());

        d.publish_iteration_status(&RatioWire(0.25), "waiting", 4, 1, 5.0);
        assert_eq!(
            gutter_state.load().as_deref(),
            Some(&crate::wrappers::gutter::GutterSpec::Text(
                "ratio 0.25".into()
            )),
            "first iteration publishes the rendered during form"
        );

        // Next iteration's wires supersede the cell.
        d.publish_iteration_status(&RatioWire(0.75), "waiting", 2, 2, 10.0);
        assert_eq!(
            gutter_state.load().as_deref(),
            Some(&crate::wrappers::gutter::GutterSpec::Text(
                "ratio 0.75".into()
            )),
            "each iteration refreshes the cell"
        );

        // A failed substitution must leave the last good value.
        d.each_gutter = Some((
            crate::wrappers::gutter::GutterKind::Spark,
            "{no_such_wire}".into(),
        ));
        d.publish_iteration_status(&RatioWire(0.9), "waiting", 1, 3, 15.0);
        assert_eq!(
            gutter_state.load().as_deref(),
            Some(&crate::wrappers::gutter::GutterSpec::Text(
                "ratio 0.75".into()
            )),
            "render failure must not clobber the cell"
        );
    }
}

#[cfg(test)]
mod poll_wire_tests {
    /// With a declared `until:`, the memo must NOT quote the row-count window.
    /// "measured 0 row(s) [target 0..=0]" reads as satisfied while the poll is
    /// still waiting — the single most misleading thing a status line can say,
    /// and exactly what an operator saw during a 1m28s drain.
    #[test]
    fn until_memo_does_not_quote_the_unused_row_window() {
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/wrappers/poll.rs"))
                .expect("read own source");
        let until_arm = src
            .find("waiting on `until:` (not yet satisfied)")
            .expect("the until: memo arm must exist");
        let window_arm = src
            .find("measured {row_count} row(s) [target")
            .expect("the row-window memo arm must exist");
        assert!(
            until_arm < window_arm,
            "the until: arm must be the FIRST branch, so a declared condition \
             never falls through to the row-window text"
        );
        // And the two must be distinct branches of one `if self.until`.
        assert!(
            src.contains("if self.until {"),
            "the memo must branch on whether an until: was declared"
        );
    }

    /// `poll_elapsed_ms` and `poll_count` must be WRITTEN before the predicate
    /// is evaluated, not after. A condition that bounds its own patience
    /// (`... || poll_elapsed_ms >= 60000`) is the only way to assert something
    /// that may never be observed without risking a wait to `timeout_ms` —
    /// which in the compaction drain is 48 hours. Publishing them a moment too
    /// late would leave the first evaluation reading 0 forever.
    #[test]
    fn poll_progress_wires_are_published_before_the_predicate() {
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/wrappers/poll.rs"))
                .expect("read own source");
        // Needle tolerates rustfmt splitting the receiver from the call —
        // `ctx.wires\n    .write(...)` — which is exactly what broke the
        // original `ctx.wires.write("poll_elapsed_ms"` form.
        let write_at = src
            .find(".write(\"poll_elapsed_ms\"")
            .expect("poll_elapsed_ms must be published");
        let predicate_at = src
            .find("let is_done = if self.until {")
            .expect("predicate evaluation site");
        assert!(
            write_at < predicate_at,
            "the poll-progress wires must be written BEFORE the until: predicate \
             is evaluated, or a self-bounding condition reads a stale 0"
        );
        assert!(
            src.contains(".write(\"poll_count\""),
            "poll_count must be published alongside poll_elapsed_ms"
        );
    }
}
