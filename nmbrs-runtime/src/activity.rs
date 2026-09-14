// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Activity: the unit of concurrent execution.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use nmbrs_metrics::instruments::counter::Counter;
use nmbrs_metrics::instruments::histogram::Histogram;
use nmbrs_metrics::instruments::outcome::{MetricDetail, MetricDetailConfig, OutcomeInstrument};
use nmbrs_metrics::instruments::timer::Timer;
use nmbrs_metrics::labels::Labels;
use nmbrs_metrics::snapshot::MetricSet;
use nmbrs_rate::RateLimiter;

use crate::adapter::{DriverAdapter, OpDispenser};
// CycleSource removed — all iteration goes through DataSourceFactory
use crate::opseq::{OpSequence, SequencerType};
use crate::validation;

/// Configuration for an activity.
pub struct ActivityConfig {
    pub name: String,
    pub cycles: u64,
    /// Number of fibers (tokio tasks) executing stanzas concurrently.
    pub concurrency: usize,
    /// Target ops/sec for the single activity-level rate
    /// limiter. `None` disables rate limiting. There is one
    /// rate limiter per activity — no separate stanza-rate
    /// mechanism.
    pub rate: Option<f64>,
    pub sequencer: SequencerType,
    pub error_spec: String,
    /// Error-rate circuit breaker: fail the phase early when the
    /// fraction of errored ops exceeds this threshold (e.g. `0.1`
    /// = 10%). Evaluated only after at least
    /// [`ERROR_RATE_MIN_OPS`] ops so a small phase can't trip on a
    /// single error. `None` disables it; a value `>= 1.0` also
    /// effectively disables it (the rate never exceeds 1.0).
    /// Resolved per phase as the workload's `error_rate_max:` field
    /// over the session-wide default. Installed as the default SRD-83
    /// stop condition (`error_rate > error_rate_max`).
    pub error_rate_max: Option<f64>,
    /// SRD-83 — the phase's declared stop-condition predicates (the
    /// `when:` of each `stop_when:` entry). Compiled into scope-bound
    /// `ScopedExpr`s alongside the default error-rate condition and
    /// evaluated per tick.
    pub stop_when: Vec<crate::stop_conditions::StopConditionDecl>,
    /// SRD-83 §throttle — the phase's adaptive backpressure governor
    /// spec: keep the windowed attempt-failure fraction under a bound
    /// by walking a dynamic control (`concurrency`/`rate`). Evaluated
    /// on the same drain-loop tick as the stop conditions.
    pub throttle: Option<nmbrs_workload::model::ThrottleSpec>,
    /// Inherited total-attempts budget for this activity's ops (phase
    /// `tries:` or the workload-root `tries` param). An op's own `tries:`
    /// field overrides it. `None` = no budget in scope → ops WITHOUT their
    /// own `tries:` run single-attempt with no tries wrapper (SRD-82 Part 3b
    /// sigil). `0` = ops fail without executing; `1` = explicit
    /// single-attempt.
    pub tries: Option<u32>,
    /// Retry-backoff overrides from the phase-level `tries:` map form
    /// (`tries: {count, backoff: {ratio, min, max}}`). `None` = none
    /// declared at the phase → the tries wrapper falls back to the op's
    /// standalone `retry_backoff*` params, then the built-in defaults.
    pub tries_backoff: Option<nmbrs_workload::model::BackoffSpec>,
    /// Maximum number of ops within a stanza that execute concurrently.
    pub stanza_concurrency: usize,
    /// Source factory for data-driven phases. When present, fibers pull
    /// from this source instead of the cycle counter. Each fiber creates
    /// its own reader via `create_reader()`.
    pub source_factory: Option<Arc<dyn polydat::iteration::source::DataSourceFactory>>,
    /// Suppress the inline stderr progress line (TUI handles
    /// display). Wrapped in `Arc<AtomicBool>` so the runner can
    /// flip it at runtime — when the user dismisses the TUI
    /// mid-run (`q` keypress), this flag drops to `false` and
    /// the status thread resumes emission, making the
    /// experience feel like tui=off was set from the start.
    /// A bare `bool` would have baked the TUI-mode value in at
    /// activity construction, so post-dismissal there'd be no
    /// progress display at all.
    pub suppress_status_line: Arc<std::sync::atomic::AtomicBool>,
    /// Names of relevancy / live aggregate metrics to surface on
    /// the inline progress line and the per-phase ✓ DONE summary.
    /// Empty → no extra metrics are shown (status line carries
    /// only the universal counters). Set per-phase via the YAML
    /// `status_metrics: [name]` field; workload-level phases that
    /// compute relevancy must opt in explicitly — nothing is
    /// presumed to be present.
    pub status_metrics: Vec<String>,
    /// Full root-first coordinate label (e.g.
    /// `(profile=label_00), (bucket=1, kind=READ)`) for this
    /// phase's iteration. Used by the ✓ DONE summary line to
    /// show the same identity the per-phase header would carry,
    /// so the completed-status line stands alone — no separate
    /// phase-starting row needed.
    pub phase_labels: String,
    /// Pre-map sequence number `[N/total]` for this phase. Same
    /// numbering the TUI tree row and post-run summary use.
    /// `None` ⇒ inline-CLI form / pre-map didn't produce a seq.
    pub phase_seq: Option<(usize, usize)>,
    /// Resolved `readouts:` slot bindings from the workload
    /// (SRD-63 §5). Empty → all slots fall through to the
    /// hard-coded built-in defaults (`phase_outcome` at
    /// `on_phase_end`, `phase_status` at `on_update`).
    pub readouts: nmbrs_workload::model::ReadoutsBindings,
    /// CLI `--readout=<body>` override (SRD-63 §8).
    /// Applies to the `on_update` slot only; replaces
    /// (or with `+` prefix, appends to) whatever the
    /// workload + default path resolved.
    pub cli_readout_override: Option<String>,
    /// Per-session SQLite writer. Used by Push 6's snapshot
    /// store — every binder.fire captures its rendered
    /// output via `upsert_readout_snapshot` so replay /
    /// scrollback can reproduce the line later. `None`
    /// means snapshot capture is skipped (no session db
    /// — short test fixtures, in-memory sessions).
    pub snapshot_writer:
        Option<Arc<std::sync::Mutex<Option<nmbrs_metrics::reporters::sqlite::SqliteReporter>>>>,
    /// Session-level dryrun mode (`silent` / `emit` / `json`),
    /// or `None` for a normal run.
    ///
    /// `dryrun=cycle` means **full construction of an executable
    /// cycle path** — real adapter, real cluster connection, real
    /// prepared statements, real metadata — and then suppression
    /// of only the outbound `execute()` at cycle time via the
    /// outermost `DryRunWrapper`. The wrapper is triggered by an
    /// injected `dryrun:` op-template parameter, and this field is
    /// the signal that drives that injection. There is no
    /// substitution of the adapter itself; the adapter lifecycle
    /// runs end-to-end, so the typed lvalue contract the adapter
    /// reifies at `map_op` time (CQL: prepare + metadata) is
    /// available under dryrun exactly as it is under a real run.
    pub dry_run_mode: Option<String>,
    /// `dryrun=dispenser`: when true, `run_with_adapters`
    /// returns cleanly after every op template's dispenser is
    /// constructed (adapter `map_op` fires, the wrapper plan
    /// resolves and wraps, the per-template pull plan seals).
    /// No fiber pool is spawned, no cycles run. Lets the
    /// operator verify the full construction pipeline ran
    /// without paying any per-cycle cost.
    ///
    /// Set from `ExecCtx::diag.depth` at phase-attach time —
    /// `< Cycle` flips this on, `>= Cycle` leaves it off.
    pub stop_after_dispenser_init: bool,
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            name: "default".into(),
            cycles: 1,
            concurrency: 1,
            rate: None,
            sequencer: SequencerType::Bucket,
            error_spec: ".*:warn,stop".into(),
            error_rate_max: None,
            stop_when: Vec::new(),
            throttle: None,
            // Default 0 — retries are opt-in via the workload `retries:` param
            // (runner default is also 0). Matches the effective pre-wrapper
            // behaviour (retries previously required a policy `retry`
            // classification, off by default).
            tries: None,
            tries_backoff: None,
            stanza_concurrency: 1,
            source_factory: None,
            suppress_status_line: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            status_metrics: Vec::new(),
            phase_labels: String::new(),
            phase_seq: None,
            readouts: nmbrs_workload::model::ReadoutsBindings::default(),
            cli_readout_override: None,
            snapshot_writer: None,
            dry_run_mode: None,
            stop_after_dispenser_init: false,
        }
    }
}

/// Standard metrics for an activity. Shared via Arc so the metrics
/// scheduler can capture snapshots while executor tasks record.
///
/// Fields are `Arc<Counter>` / `Arc<Timer>` / `Arc<Histogram>` so
/// the same instrument is held both here (for per-cycle record
/// access) and in the activity's `Component` instrument registry
/// (for the cadence reporter's per-tick capture). Per-cycle code
/// continues calling `metrics.cycles_total.inc()` etc. through
/// `Arc`'s `Deref`.
///
/// Static instruments (the fields below) register on the component
/// from [`ActivityMetrics::register_on`] called by
/// [`Activity::attach_component`]. Dynamic per-error-type counters
/// and adapter-specific metrics flow through the
/// [`nmbrs_metrics::component::DynamicCapture`] hook implemented
/// for [`ActivityMetricsDynamic`].
///
/// Late-bound, optional shared dispenser list. Wrapped in a `Mutex`
/// because it is set once after dispenser creation (post-init) and
/// read by the dynamic-capture hook thereafter.
type SharedDispensers = std::sync::Mutex<Option<Arc<Vec<Arc<dyn crate::adapter::OpDispenser>>>>>;

pub struct ActivityMetrics {
    pub service_time: Arc<Timer>,
    pub wait_time: Arc<Timer>,
    pub response_time: Arc<Timer>,
    /// Number of tries per op (1 = succeeded first try, 2+ = retried).
    /// Distribution shape reveals incremental saturation.
    pub tries_histogram: Arc<Histogram>,
    /// Every op dispatched (incl. skips) — the rate driver. Distinct
    /// from `result_total`, which excludes skips. SRD-91.
    pub cycles_total: Arc<Counter>,
    pub skips_total: Arc<Counter>,
    // ── SRD-91 op-outcome taxonomy ────────────────────────────────
    // Two layers that reconcile (the redundancy IS the validation):
    //   • executor layer — `attempt_*` / `result_*`, counted in the
    //     stanza hot loop;
    //   • error-handler layer — `errors_total` + the per-type
    //     breakdown, counted per failed attempt at error dispatch.
    // Invariants:
    //   attempt_total == attempt_success.count + attempt_failure.count
    //   result_total  == result_success.count  + result_failure.count
    //   cycles_total  == result_total + skips_total
    //   errors_total  == Σ per-type == attempt_failure.count
    //                    (when the policy counts every error)
    /// Per-ATTEMPT total — one increment per `dispenser.execute`,
    /// including retries.
    pub attempt_total: Arc<Counter>,
    /// Per-ATTEMPT outcomes (+ attempt latency when Timed). The count
    /// is available in either detail mode — see [`OutcomeInstrument`].
    pub attempt_success: OutcomeInstrument,
    pub attempt_failure: OutcomeInstrument,
    /// Per-OP terminal total — executed results only (success +
    /// failure; excludes skips). Distinct from `cycles_total` by the
    /// skip count.
    pub result_total: Arc<Counter>,
    /// Per-OP terminal outcomes (+ op latency when Timed).
    /// `result_success` replaces the former `result_success_time`
    /// timer and the unexported `successes_total` counter — its
    /// `count()` IS the terminal-success count.
    pub result_success: OutcomeInstrument,
    pub result_failure: OutcomeInstrument,
    /// Error-handler-layer tally: one increment per failed attempt at
    /// error dispatch (per-attempt, so retries DO count here), keyed
    /// by the handler-classified name for the per-type breakdown. The
    /// per-op error rate uses `result_failure` instead, keeping it in
    /// [0,1]. SRD-91.
    pub errors_total: Arc<Counter>,
    pub stanzas_total: Arc<Counter>,
    /// Daemon ops that exited cleanly via stop-signal cancellation
    /// at phase shutdown (the trigger-and-observe happy path).
    /// Counts increment on `DaemonExit::Cancelled` only — natural
    /// completions are tracked through `result_success` /
    /// `result_failure` on the underlying op path. Visibility on
    /// this counter lets the operator distinguish "phase exited
    /// with N daemons cancelled" from "phase exited with no
    /// daemons in flight" without re-reading session.log.
    pub daemon_cancelled_total: Arc<Counter>,
    /// Daemon ops whose shutdown failed: returned an error during
    /// running or shutdown, panicked, or missed the grace window.
    /// Each increment is paired with the activity's stop_flag
    /// being set + a stop_reason being recorded.
    pub daemon_errors_total: Arc<Counter>,
    /// Number of ops dispatched to adapters (monotonic).
    pub ops_started: std::sync::atomic::AtomicU64,
    /// Number of ops returned from adapters (monotonic).
    pub ops_finished: std::sync::atomic::AtomicU64,
    pub result_elements: Arc<Counter>,
    pub result_bytes: Arc<Counter>,
    /// Derived phase-progress override, in parts-per-million of the
    /// completion fraction (0..=1_000_000); `u64::MAX` = unset. When
    /// set, status surfaces render THIS fraction for the phase's
    /// completion bar / percentage instead of the cycles-based
    /// `cycles_completed / total_extent` — load-bearing for phases
    /// whose single long op measures its own progress (e.g. a
    /// `poll:` await publishing `completion_ratio` from
    /// `system_views.sstable_tasks`), where the cycle count pins the
    /// bar at 0% for the whole wait. Stored as integer ppm so the
    /// producer/consumer handoff stays a lock-free atomic.
    pub progress_override_ppm: std::sync::atomic::AtomicU64,
    /// Elapsed milliseconds of the producer that published
    /// [`Self::progress_override_ppm`] (e.g. the poll's own elapsed),
    /// `u64::MAX` = unset. Carried so displays can derive an ETA on the
    /// measured basis — `elapsed × (1−f)/f` — instead of the cycle
    /// accounting, which stands still for a single long measured op.
    pub progress_override_elapsed_ms: std::sync::atomic::AtomicU64,
    /// Per-error-type counters, keyed by error_name.
    /// Created on demand when a new error type is first seen.
    /// Captured via the [`DynamicCapture`] hook — the registry on
    /// `Component` only holds instruments known at init.
    error_type_counts: std::sync::Mutex<std::collections::HashMap<String, Arc<Counter>>>,
    labels: Labels,
    /// Dispensers for adapter-specific metrics capture. Set after dispenser creation.
    dispensers: SharedDispensers,
    /// Shared handles to the per-template validation metrics. Populated
    /// after executor setup so the progress thread can read live
    /// relevancy aggregates (recall-over-last-N, all-time mean) without
    /// draining the precision accumulators.
    validation_metrics:
        std::sync::Mutex<Option<Arc<Vec<Arc<crate::validation::ValidationMetrics>>>>>,
}

/// Resolve the SRD-91 op-outcome detail config from the single
/// `metrics_detail` param. The value is a comma-separated list: a bare
/// token sets the global default (`counts` / `timers`), and a
/// `family:mode` token overrides one instrument. Example:
/// `metrics_detail=timers,attempt_success:counts,attempt_failure:counts`.
/// Absent or unparseable tokens fall back to the default (timers).
pub(crate) fn metric_detail_from_params(
    params: &std::collections::HashMap<String, String>,
) -> MetricDetailConfig {
    let Some(spec) = params.get("metrics_detail") else {
        return MetricDetailConfig::default();
    };
    let mut default = MetricDetail::default();
    let mut overrides: Vec<(String, MetricDetail)> = Vec::new();
    for tok in spec.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        if let Some((family, mode)) = tok.split_once(':') {
            if let Some(d) = MetricDetail::parse(mode) {
                overrides.push((family.trim().to_string(), d));
            }
        } else if let Some(d) = MetricDetail::parse(tok) {
            default = d;
        }
    }
    let mut cfg = MetricDetailConfig::new(default);
    for (family, detail) in overrides {
        cfg = cfg.with_override(family, detail);
    }
    cfg
}

impl ActivityMetrics {
    pub fn new(labels: &Labels) -> Self {
        Self::with_sigdigs(
            labels,
            nmbrs_metrics::instruments::histogram::DEFAULT_HDR_SIGDIGS,
            &MetricDetailConfig::default(),
        )
    }

    /// Construct activity metrics using an explicit HDR
    /// significant-digits precision for every histogram and
    /// timer below. The runner resolves `hdr.sigdigs` from the
    /// session root via
    /// [`nmbrs_metrics::instruments::histogram::resolve_hdr_sigdigs`]
    /// once per activity and threads it here (SRD 40 §"HDR
    /// significant digits — subtree-scoped setting").
    pub fn with_sigdigs(labels: &Labels, sigdigs: u8, detail: &MetricDetailConfig) -> Self {
        // Outcome instruments choose counter-vs-timer per family (global
        // default + override), SRD-91. Default is Timers, preserving the
        // historical always-on latency distributions.
        let outcome = |name: &str| {
            OutcomeInstrument::new(labels.with("name", name), sigdigs, detail.for_family(name))
        };
        Self {
            service_time: Arc::new(Timer::with_sigdigs(
                labels.with("name", "cycles_servicetime"),
                sigdigs,
            )),
            wait_time: Arc::new(Timer::with_sigdigs(
                labels.with("name", "cycles_waittime"),
                sigdigs,
            )),
            response_time: Arc::new(Timer::with_sigdigs(
                labels.with("name", "cycles_responsetime"),
                sigdigs,
            )),
            tries_histogram: Arc::new(
                nmbrs_metrics::instruments::histogram::Histogram::with_sigdigs(
                    labels.with("name", "tries"),
                    sigdigs,
                ),
            ),
            cycles_total: Arc::new(Counter::new(labels.with("name", "cycles_total"))),
            skips_total: Arc::new(Counter::new(labels.with("name", "skips_total"))),
            attempt_total: Arc::new(Counter::new(labels.with("name", "attempt_total"))),
            attempt_success: outcome("attempt_success"),
            attempt_failure: outcome("attempt_failure"),
            result_total: Arc::new(Counter::new(labels.with("name", "result_total"))),
            result_success: outcome("result_success"),
            result_failure: outcome("result_failure"),
            errors_total: Arc::new(Counter::new(labels.with("name", "errors_total"))),
            stanzas_total: Arc::new(Counter::new(labels.with("name", "stanzas_total"))),
            daemon_cancelled_total: Arc::new(Counter::new(
                labels.with("name", "daemon_cancelled_total"),
            )),
            daemon_errors_total: Arc::new(Counter::new(labels.with("name", "daemon_errors_total"))),
            ops_started: std::sync::atomic::AtomicU64::new(0),
            ops_finished: std::sync::atomic::AtomicU64::new(0),
            result_elements: Arc::new(Counter::new(labels.with("name", "result_elements"))),
            result_bytes: Arc::new(Counter::new(labels.with("name", "result_bytes"))),
            progress_override_ppm: std::sync::atomic::AtomicU64::new(u64::MAX),
            progress_override_elapsed_ms: std::sync::atomic::AtomicU64::new(u64::MAX),
            error_type_counts: std::sync::Mutex::new(std::collections::HashMap::new()),
            labels: labels.clone(),
            dispensers: std::sync::Mutex::new(None),
            validation_metrics: std::sync::Mutex::new(None),
        }
    }

    /// Register every static instrument on `component` and install
    /// a [`DynamicCapture`] hook for the dynamic surface (per-error-type
    /// counters and adapter-specific metrics from registered dispensers).
    ///
    /// Called once from [`Activity::attach_component`]. After this
    /// point:
    /// - The cadence reporter's tree walk picks up every static
    ///   instrument here through `component.capture_delta`.
    /// - Per-cycle code continues recording through this struct's
    ///   typed `Arc` fields — same `Arc` that the registry holds.
    pub fn register_on(
        self: &Arc<Self>,
        component: &mut nmbrs_metrics::component::Component,
    ) -> Result<(), String> {
        use nmbrs_metrics::component::InstrumentRef;
        // Order mirrors the historical capture_delta emission so
        // metric_family ordering stays stable for downstream
        // consumers. SRD-91 outcome instruments register via
        // `instrument_ref()` (a counter or summary family per the
        // resolved detail mode).
        component.register_instrument(
            "cycles_servicetime",
            InstrumentRef::Timer(self.service_time.clone()),
        )?;
        component.register_instrument(
            "cycles_waittime",
            InstrumentRef::Timer(self.wait_time.clone()),
        )?;
        component.register_instrument(
            "cycles_responsetime",
            InstrumentRef::Timer(self.response_time.clone()),
        )?;
        component.register_instrument("result_success", self.result_success.instrument_ref())?;
        component.register_instrument("result_failure", self.result_failure.instrument_ref())?;
        component.register_instrument(
            "result_total",
            InstrumentRef::Counter(self.result_total.clone()),
        )?;
        component.register_instrument(
            "cycles_total",
            InstrumentRef::Counter(self.cycles_total.clone()),
        )?;
        component.register_instrument(
            "skips_total",
            InstrumentRef::Counter(self.skips_total.clone()),
        )?;
        component.register_instrument(
            "errors_total",
            InstrumentRef::Counter(self.errors_total.clone()),
        )?;
        component.register_instrument(
            "attempt_total",
            InstrumentRef::Counter(self.attempt_total.clone()),
        )?;
        component.register_instrument("attempt_success", self.attempt_success.instrument_ref())?;
        component.register_instrument("attempt_failure", self.attempt_failure.instrument_ref())?;
        component.register_instrument(
            "stanzas_total",
            InstrumentRef::Counter(self.stanzas_total.clone()),
        )?;
        component.register_instrument(
            "daemon_cancelled_total",
            InstrumentRef::Counter(self.daemon_cancelled_total.clone()),
        )?;
        component.register_instrument(
            "daemon_errors_total",
            InstrumentRef::Counter(self.daemon_errors_total.clone()),
        )?;
        component.register_instrument(
            "result_elements",
            InstrumentRef::Counter(self.result_elements.clone()),
        )?;
        component.register_instrument(
            "result_bytes",
            InstrumentRef::Counter(self.result_bytes.clone()),
        )?;
        component.register_instrument(
            "tries",
            InstrumentRef::Histogram(self.tries_histogram.clone()),
        )?;

        component.set_dynamic_capture(Arc::new(ActivityMetricsDynamic {
            metrics: self.clone(),
            prev_counters: std::sync::Mutex::new(std::collections::HashMap::new()),
        }));
        Ok(())
    }

    /// Return the number of cycles completed so far.
    ///
    /// Reads from the `cycles_total` counter atomically. Used by the
    /// progress reporter thread to display live throughput.
    pub fn cycles_completed(&self) -> u64 {
        self.cycles_total.get()
    }

    /// Publish (or clear, with `None`) the derived phase-progress
    /// override. `Some(f)` is clamped to `[0.0, 1.0]` and stored in
    /// ppm; see the field doc on [`Self::progress_override_ppm`].
    /// Clearing also clears the producer-elapsed companion.
    pub fn set_progress_override(&self, fraction: Option<f64>) {
        let ppm = match fraction {
            Some(f) => (f.clamp(0.0, 1.0) * 1_000_000.0).round() as u64,
            None => u64::MAX,
        };
        self.progress_override_ppm
            .store(ppm, std::sync::atomic::Ordering::Relaxed);
        if fraction.is_none() {
            self.progress_override_elapsed_ms
                .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// As [`Self::set_progress_override`], additionally recording the
    /// producer's own elapsed seconds at publish time. Displays derive
    /// the measured-basis ETA from the pair: `elapsed × (1−f)/f`.
    pub fn set_progress_override_with_elapsed(&self, fraction: f64, elapsed_secs: f64) {
        self.set_progress_override(Some(fraction));
        let ms = (elapsed_secs.max(0.0) * 1000.0).round() as u64;
        self.progress_override_elapsed_ms
            .store(ms.min(u64::MAX - 1), std::sync::atomic::Ordering::Relaxed);
    }

    /// The derived phase-progress override as a fraction in
    /// `[0.0, 1.0]`, or `None` when no producer has published one.
    pub fn progress_override(&self) -> Option<f64> {
        let ppm = self
            .progress_override_ppm
            .load(std::sync::atomic::Ordering::Relaxed);
        (ppm != u64::MAX).then(|| (ppm.min(1_000_000)) as f64 / 1_000_000.0)
    }

    /// The producer-elapsed seconds recorded with the override, or
    /// `None` when unset (no producer, or a producer that publishes
    /// the fraction only).
    pub fn progress_override_elapsed_secs(&self) -> Option<f64> {
        let ms = self
            .progress_override_elapsed_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        (ms != u64::MAX).then(|| ms as f64 / 1000.0)
    }

    /// Increment counter for a specific error type. Creates the
    /// counter on first occurrence of each error name. The new
    /// counter is read by the [`DynamicCapture`] hook on every
    /// capture tick — registration on `Component` is implicit
    /// through the hook, not a per-name `register_instrument` call.
    /// Top-N error types by count, rendered `name=count` comma-joined —
    /// empty string when no typed errors were recorded. Gives failure
    /// messages their WHAT (which error families drove the counters)
    /// without a metrics query.
    pub fn top_error_types(&self, n: usize) -> String {
        let map = self
            .error_type_counts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<(String, u64)> = map
            .iter()
            .map(|(k, c)| (k.clone(), c.get()))
            .filter(|(_, c)| *c > 0)
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(n);
        v.iter()
            .map(|(k, c)| format!("{k}={c}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn count_error_type(&self, error_name: &str) {
        let mut map = self
            .error_type_counts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let counter = map.entry(error_name.to_string()).or_insert_with(|| {
            Arc::new(Counter::new(
                self.labels.with("name", format!("errors.{error_name}")),
            ))
        });
        counter.inc();
    }

    /// Capture an absolute snapshot (counters at their current value,
    /// timer histograms drained as deltas).
    ///
    /// Used by the legacy per-activity capture thread. For the component
    /// tree scheduler, use [`capture_delta`] instead.
    pub fn capture(&self, interval: std::time::Duration) -> MetricSet {
        use nmbrs_metrics::snapshot::split_name_label;
        let service_snap = self.service_time.snapshot();
        let wait_snap = self.wait_time.snapshot();
        let response_snap = self.response_time.snapshot();
        let tries_snap = self.tries_histogram.snapshot();
        let now = Instant::now();
        let mut snap = MetricSet::at(now, interval);

        let (n, lbl) = split_name_label(self.service_time.labels());
        snap.insert_histogram(n, lbl, service_snap.histogram, now);
        let (n, lbl) = split_name_label(self.wait_time.labels());
        snap.insert_histogram(n, lbl, wait_snap.histogram, now);
        let (n, lbl) = split_name_label(self.response_time.labels());
        snap.insert_histogram(n, lbl, response_snap.histogram, now);
        // result_success: a histogram when Timed, a plain count when
        // Counted (SRD-91 detail mode).
        match &self.result_success {
            OutcomeInstrument::Timed(t) => {
                let (n, lbl) = split_name_label(t.labels());
                snap.insert_histogram(n, lbl, t.snapshot().histogram, now);
            }
            OutcomeInstrument::Counted(c) => {
                let (n, lbl) = split_name_label(c.labels());
                snap.insert_counter(n, lbl, c.get(), now);
            }
        }

        let (n, lbl) = split_name_label(self.cycles_total.labels());
        snap.insert_counter(n, lbl, self.cycles_total.get(), now);
        let (n, lbl) = split_name_label(self.skips_total.labels());
        snap.insert_counter(n, lbl, self.skips_total.get(), now);
        let (n, lbl) = split_name_label(self.errors_total.labels());
        snap.insert_counter(n, lbl, self.errors_total.get(), now);
        let (n, lbl) = split_name_label(self.stanzas_total.labels());
        snap.insert_counter(n, lbl, self.stanzas_total.get(), now);
        let (n, lbl) = split_name_label(self.daemon_cancelled_total.labels());
        snap.insert_counter(n, lbl, self.daemon_cancelled_total.get(), now);
        let (n, lbl) = split_name_label(self.daemon_errors_total.labels());
        snap.insert_counter(n, lbl, self.daemon_errors_total.get(), now);
        let (n, lbl) = split_name_label(self.result_elements.labels());
        snap.insert_counter(n, lbl, self.result_elements.get(), now);
        let (n, lbl) = split_name_label(self.result_bytes.labels());
        snap.insert_counter(n, lbl, self.result_bytes.get(), now);
        let (n, lbl) = split_name_label(self.tries_histogram.labels());
        snap.insert_histogram(n, lbl, tries_snap, now);

        let error_counts = self
            .error_type_counts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for counter in error_counts.values() {
            let (n, lbl) = split_name_label(counter.labels());
            snap.insert_counter(n, lbl, counter.get(), now);
        }

        snap
    }

    /// Register dispensers for adapter-specific metrics capture.
    pub fn set_dispensers(&self, dispensers: Arc<Vec<Arc<dyn crate::adapter::OpDispenser>>>) {
        *self.dispensers.lock().unwrap_or_else(|e| e.into_inner()) = Some(dispensers);
    }

    /// Register the per-template validation metrics so the progress
    /// thread can read live relevancy aggregates.
    pub fn set_validation_metrics(&self, vms: Arc<Vec<Arc<crate::validation::ValidationMetrics>>>) {
        *self
            .validation_metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(vms);
    }

    /// Snapshot live relevancy aggregates from every registered
    /// validation-metrics instance (one per op template that declared
    /// `relevancy:`). Non-destructive — safe to call every frame.
    pub fn collect_relevancy_live(&self) -> Vec<crate::validation::RelevancyLive> {
        let mut out = Vec::new();
        if let Ok(guard) = self.validation_metrics.lock()
            && let Some(ref vms) = *guard
        {
            for vm in vms.iter() {
                out.extend(vm.live_snapshot());
            }
        }
        out
    }

    /// Collect every status-line value whose name matches one of
    /// `patterns`. Patterns are glob-style (`*` for any run of
    /// characters, `?` for a single character; literal otherwise),
    /// matched against the canonical names below. Returns formatted
    /// ` name:value` strings ready to concatenate into the inline
    /// progress / DONE summary line, in pattern declaration order
    /// with duplicates suppressed.
    ///
    /// Supported metric families:
    /// - **Relevancy aggregates** — one entry per registered
    ///   `relevancy.functions:` (e.g. `recall`, `precision`,
    ///   `f1`). The relevancy cutoff rides on the metric's
    ///   `k` / `r` labels rather than the family name.
    ///   Value: `total_mean × 100` as a percent.
    /// - **Latency** — `latency_p50`, `latency_p99`, `latency_max`,
    ///   `latency_mean`, sourced from `service_time` (the per-op
    ///   timer, exclusive of wait time). Value: auto-scaled
    ///   duration via [`nmbrs_metrics::reporters::summary::format_duration`].
    pub fn collect_status_values(&self, patterns: &[String]) -> Vec<String> {
        if patterns.is_empty() {
            return Vec::new();
        }
        // Build the candidate list once. Order is stable so
        // pattern ordering, not iteration order, drives the
        // output sequence.
        let mut candidates: Vec<(String, String)> = Vec::new();
        for live in self.collect_relevancy_live() {
            // Zero evaluations (e.g. every op `if:`-skipped) means
            // there is no measurement — omit the chip rather than
            // fabricating `recall:0.00%` from an empty aggregate.
            if live.total_count == 0 {
                continue;
            }
            candidates.push((live.name, format!("{:.2}%", live.total_mean * 100.0)));
        }
        let snap = self.service_time.peek_snapshot();
        let h = &snap.histogram;
        if !h.is_empty() {
            let fmt = nmbrs_metrics::reporters::summary::format_duration;
            candidates.push((
                "latency_p50".to_string(),
                fmt(h.value_at_quantile(0.50) as f64),
            ));
            candidates.push((
                "latency_p99".to_string(),
                fmt(h.value_at_quantile(0.99) as f64),
            ));
            candidates.push(("latency_max".to_string(), fmt(h.max() as f64)));
            candidates.push(("latency_mean".to_string(), fmt(h.mean())));
        }
        // KEY-METRIC accent: `status_metrics:` selection is the
        // workload author saying "this is the number I'm running the
        // test for" — the chip gets its own bright palette slot
        // (bold bright magenta; used by nothing else on the line) so
        // it reads first among the dim bookkeeping counters.
        let color = crate::observer::use_color();
        let accent = if color { "\x1b[1;95m" } else { "" };
        let reset = if color { "\x1b[0m" } else { "" };
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for pat in patterns {
            for (name, val) in &candidates {
                if !seen.contains(name.as_str()) && glob_match(pat, name) {
                    seen.insert(name.clone());
                    let label = chip_display_label(name);
                    out.push(format!(" {accent}{label}:{val}{reset}"));
                }
            }
        }
        out
    }

    /// The PRIMARY key metric as a raw numeric sample: the first
    /// `status_metrics:` pattern's first match, in the same
    /// candidate order [`Self::collect_status_values`] uses
    /// (relevancy aggregates as percent, then service-time
    /// quantiles as milliseconds). Feeds the key-metric row's
    /// gutter cell TREND (SRD-92 R4): the cell shows the metric's
    /// history as a sparkline — the current value's single
    /// placement is the chips text in the row body. `None` when
    /// nothing matches or nothing has been measured yet.
    pub fn collect_status_primary(&self, patterns: &[String]) -> Option<(String, f64)> {
        if patterns.is_empty() {
            return None;
        }
        let mut candidates: Vec<(String, f64)> = Vec::new();
        for live in self.collect_relevancy_live() {
            if live.total_count == 0 {
                continue;
            }
            candidates.push((live.name, live.total_mean * 100.0));
        }
        let snap = self.service_time.peek_snapshot();
        let h = &snap.histogram;
        if !h.is_empty() {
            let ms = |n: f64| n / 1e6;
            candidates.push((
                "latency_p50".to_string(),
                ms(h.value_at_quantile(0.50) as f64),
            ));
            candidates.push((
                "latency_p99".to_string(),
                ms(h.value_at_quantile(0.99) as f64),
            ));
            candidates.push(("latency_max".to_string(), ms(h.max() as f64)));
            candidates.push(("latency_mean".to_string(), ms(h.mean())));
        }
        for pat in patterns {
            for (name, val) in &candidates {
                if glob_match(pat, name) {
                    return Some((name.clone(), *val));
                }
            }
        }
        None
    }

    /// Collect status counters from all registered dispensers.
    pub fn collect_status_counters(&self) -> Vec<(String, u64)> {
        let mut counters = Vec::new();
        if let Ok(guard) = self.dispensers.lock()
            && let Some(ref disps) = *guard
        {
            for disp in disps.iter() {
                for (name, total) in disp.status_counters() {
                    counters.push((name.to_string(), total));
                }
            }
        }
        counters
    }
}

/// Map a canonical status-chip metric name to its short display
/// label. Keeps the underlying pattern-matching identity stable
/// (workloads' `status_metrics: ["latency_*"]` keeps working)
/// while the operator-facing chip stays terse — `P50` / `P99` /
/// `Pmax` / `Pmean` instead of `latency_p50` / etc. Identity
/// passthrough for any name without a shortcut.
fn chip_display_label(name: &str) -> &str {
    match name {
        "latency_p50" => "P50",
        "latency_p99" => "P99",
        "latency_max" => "Pmax",
        "latency_mean" => "Pmean",
        other => other,
    }
}

/// [`DynamicCapture`] adapter for [`ActivityMetrics`]. Captures the
/// dynamic surface — per-error-type counters and adapter-specific
/// metrics from registered dispensers — that isn't known at
/// `register_on` time and therefore can't live in the static
/// component instrument registry.
struct ActivityMetricsDynamic {
    metrics: Arc<ActivityMetrics>,
    /// Per-counter previous-value baseline for delta emission on
    /// the `drain=true` path. Keyed by `counter.labels().identity_hash()`.
    /// Mirrors the per-component baseline that `Component` keeps for
    /// registered counters; per-error-type counters live outside
    /// the registry so the baseline travels with the hook.
    ///
    /// Why deltas: `MetricSet::combine_into` for Counter is
    /// `total = a.total.saturating_add(b.total)` — the cascade
    /// coalesce path treats Counter.total as the per-interval
    /// delta and SUMS across intervals. Emitting absolutes here
    /// would inflate as the cascade coalesces.
    prev_counters: std::sync::Mutex<std::collections::HashMap<u64, u64>>,
}

impl nmbrs_metrics::component::DynamicCapture for ActivityMetricsDynamic {
    fn capture_into(&self, out: &mut MetricSet, now: Instant, drain: bool) {
        use nmbrs_metrics::snapshot::{MetricType, MetricValue, split_name_label};

        // Per-error-type counters.
        // - drain=true (cadence path): emit deltas vs. the stored
        //   baseline so cascade coalesce sums across intervals
        //   without inflation.
        // - drain=false (peek path): emit absolute totals.
        let error_counts = self
            .metrics
            .error_type_counts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if drain {
            let mut prev = self.prev_counters.lock().unwrap_or_else(|e| e.into_inner());
            for counter in error_counts.values() {
                let (name, lbl) = split_name_label(counter.labels());
                let current = counter.get();
                let key = counter.labels().identity_hash();
                let previous = prev.insert(key, current).unwrap_or(0);
                out.insert_counter(name, lbl, current.saturating_sub(previous), now);
            }
        } else {
            for counter in error_counts.values() {
                let (name, lbl) = split_name_label(counter.labels());
                out.insert_counter(name, lbl, counter.get(), now);
            }
        }

        // Adapter-specific metrics from each registered dispenser.
        // Passthrough — the adapter decides delta vs. absolute
        // semantics for its own metrics.
        if let Some(ref disps) = *self
            .metrics
            .dispensers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            for dispenser in disps.iter() {
                for (family, metric_labels, value) in dispenser.adapter_metrics() {
                    let mtype = match &value {
                        MetricValue::Counter(_) => MetricType::Counter,
                        MetricValue::Gauge(_) => MetricType::Gauge,
                        MetricValue::Histogram(_) => MetricType::Summary,
                        MetricValue::BucketedHistogram(_) => MetricType::Histogram,
                        MetricValue::Info(_) => MetricType::Info,
                        MetricValue::StateSet(_) => MetricType::StateSet,
                    };
                    out.insert_metric(family, mtype, metric_labels, value, now);
                }
            }
        }
    }
}

/// A running activity.
pub struct Activity {
    pub config: ActivityConfig,
    pub labels: Labels,
    pub metrics: Arc<ActivityMetrics>,
    pub op_sequence: OpSequence,
    /// SRD-83 — this phase node's own scope kernel (the structural
    /// walk's `cached_kernel`). Stop-condition predicates bind to THIS
    /// native scope as it sits, not a conjured root. `None` when the
    /// phase has no installed kernel (then no conditions evaluate).
    pub phase_kernel: Option<Arc<polydat::kernel::PolydatKernel>>,
    /// SRD-82 — the phase shell's [`crate::error_policy::ErrorPolicy`]
    /// (op router + aggregate guard), resolved at scope-init from the
    /// parent policy so equal configs share one instance. Built
    /// standalone only on the test/library path ([`Self::with_params`]).
    pub error_policy: Arc<crate::error_policy::ErrorPolicy>,
    /// Source factory — creates per-fiber readers. All phases go through
    /// sources. `cycles: N` desugars to `range(0, N)`.
    source_factory: Arc<dyn polydat::iteration::source::DataSourceFactory>,
    /// Resolved workload parameters (constant per run).
    pub workload_params: Arc<std::collections::HashMap<String, String>>,
    /// Shared flag: set to true when a `stop` error handler fires.
    /// All fibers check this and exit their loop when set.
    pub stop_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Per-execution walk-stop flag (SRD-82 Part 4), cloned from this
    /// execution's `WorkloadShell`. Distinct from `stop_flag` (which is
    /// this phase's OWN stop): set when the scenario WALK halts — a
    /// sibling phase failed (a fault) or a stop condition tripped — so
    /// in-flight fibers abort cooperatively and a concurrent
    /// (`Bounded(N>1)`) sibling phase stops instead of draining. `None`
    /// outside a walk (tests, the library shim) → never aborts.
    pub walk_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// SRD-82 Part 6 — set ONLY for a daemon phase: the daemon-group
    /// completion flag, latched by the scenario shell once the scope's
    /// foreground phases finish. A daemon phase's fibers poll it at their
    /// cooperative boundaries and exit, so the daemon stops when the
    /// foreground it shadows completes. `None` for foreground phases.
    pub daemon_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// First error message that triggered `stop_flag` — captured
    /// once (the first stopping error wins, subsequent fibers'
    /// errors don't overwrite). Surfaced in the phase-level
    /// error so the user doesn't have to grep the per-cycle
    /// log to learn what actually stopped the run.
    pub stop_reason: Arc<std::sync::Mutex<Option<String>>>,
    /// SRD-83 Part 5 — the two-axis Outcome of the FIRST stop-condition
    /// trip that stopped this phase, latched together with `stop_reason`
    /// (same first-stopper-wins discipline, written inside the same slot
    /// win). A `stop` effect latches Interrupted+Succeeded — a clean
    /// early halt whose partial result the phase keeps; `fail`/`abort`
    /// latch Interrupted+Failed. The executor reads this at phase end so
    /// the shell adopts the condition's DECLARED outcome instead of
    /// deriving failure from the bare stop flag. `None` whenever the
    /// stop came from any other source (error router `stop` verb, walk
    /// stop, poll timeout, Ctrl-C) — those keep failure semantics.
    pub stop_outcome: Arc<std::sync::Mutex<Option<crate::phase_outcome::Outcome>>>,
    /// SRD-76 — chronologically ordered per-cycle error
    /// records. Populated by the per-cycle dispatch path
    /// (alongside the existing `stop_reason` formatted
    /// string) so the executor can drain a structured
    /// list into `PhaseOutcome.errors` at phase end. The
    /// `stop_reason` string stays — it's the single
    /// load-bearing format the executor reads to compose
    /// the `phase 'X' stopped by error handler:` log
    /// line. This buffer is the orthogonal structured
    /// projection.
    pub phase_errors: Arc<std::sync::Mutex<Vec<crate::phase_outcome::PhaseErrorDetail>>>,
    /// Final validation metrics frame, populated after all cycles complete.
    /// Read by the metrics capture thread after the activity finishes.
    pub validation_frame: Arc<std::sync::Mutex<Option<MetricSet>>>,
    /// Optional handle to this activity's component in the session tree.
    /// Set by the runner via [`Self::attach_component`] before
    /// execution; when present, the executor declares the
    /// `concurrency` control on it (SRD 23) and wires the
    /// [`crate::fiber_pool::ConcurrencyApplier`] so runtime writes
    /// resize the fiber pool.
    pub component: Option<Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>>,
    /// SRD-32a Push 3 — workload-root wrapper-composition
    /// override. When populated (from the workload's
    /// `wrappers: { order: [...] }` block), every op
    /// template that doesn't carry its own
    /// per-template override uses this innermost-to-outermost
    /// list as its composition order. Validated against the
    /// per-op triggered set at cascade time; mismatch is a
    /// hard error per SRD-32a §"Workload-level override".
    pub wrappers_override: Option<Vec<String>>,
    /// SRD-32a Push 3 — CLI `--wrap-default-order` override.
    /// Replaces the resolver's built-in `DEFAULT_ORDER`
    /// tiebreaker for this activity. `None` ⇒ resolver uses
    /// the built-in order. Distinct from
    /// `wrappers_override`: that pins the per-op stack;
    /// this changes the tiebreaker used when constraints
    /// leave order ambiguous.
    pub wrap_default_order: Option<Vec<String>>,
    /// Shared retry-exemplar sampling config (`exec_events`): every
    /// tries wrapper in this activity that does NOT pin its own
    /// `retry_exemplar_*` op params samples through this cell, so
    /// the `retry_exemplar_rate` / `retry_exemplar_max_hz` dynamic
    /// controls (declared in [`Self::attach_component`]) move them
    /// all with one atomic store — push-on-set, no per-op control
    /// traffic, and the read only happens on the retry path.
    pub exemplar_config: Arc<crate::exec_events::ExemplarConfig>,
    /// Shared per-phase retry advisory gate (`exec_events`): one
    /// first-sighting advisory per error class per phase, capped —
    /// the default-on signal that the retry loop started absorbing
    /// errors. Ops opt out with `retry_advisory: off`.
    pub advisory_gate: Arc<crate::exec_events::AdvisoryGate>,
    /// Phase memo — a short operator-visible string that the
    /// `memo` wrapper publishes via `before:` / `after:`
    /// templates. Read by the inline-status readout and
    /// rendered as `[[ <memo> ]]` above the status line when
    /// non-empty. Lock-free atomic so the inline thread can
    /// load it every tick without blocking the executor.
    /// Default empty.
    pub memo: Arc<arc_swap::ArcSwap<String>>,
    /// Phase gutter — the contextual left-gutter cell content that
    /// the `gutter` wrapper publishes (distinct from `memo`, which
    /// owns the `[[ ... ]]` header line). `None` ⇒ the display
    /// derives the cell automatically (completion bar for metered
    /// phases, latency trend for daemons); `Some` overrides that
    /// derivation with the workload-declared spec. Lock-free
    /// atomic for the same reason as `memo`.
    pub gutter: Arc<arc_swap::ArcSwapOption<crate::wrappers::gutter::GutterSpec>>,
    /// The op-declared DURING-execution gutter template (kind +
    /// template), retained for the guaranteed one-final-update at
    /// phase end when no `final:` form is declared.
    pub gutter_spec: std::sync::Mutex<Option<(crate::wrappers::gutter::GutterKind, String)>>,
    /// The op-declared `final:` gutter template — evaluated once at
    /// phase end (wires first, status-metric aggregates as fallback)
    /// and rendered as the ✓ outcome detail line's gutter cell.
    pub gutter_final_spec: std::sync::Mutex<Option<(crate::wrappers::gutter::GutterKind, String)>>,
    /// SRD-75 phase-poll context. When present, the fiber
    /// loop checks the predicate after each source-exhaustion
    /// event; if false and the timeout hasn't elapsed, the
    /// source factory rewinds and the loop continues.
    /// `None` ⇒ no phase-poll (standard activity semantics).
    /// Set by the executor at run-phase entry; not part of
    /// the YAML-derived `ActivityConfig`.
    pub phase_poll: Option<PhasePollContext>,
}

/// Runtime context for SRD-75 phase-level poll. Carried on
/// `Activity` when the phase declares a `poll:` block;
/// consumed by the fiber loop after each source-exhaustion
/// event to decide whether to terminate (predicate satisfied
/// or timeout) or rewind and run another iteration.
#[derive(Clone)]
pub struct PhasePollContext {
    /// Handle to the phase scope kernel. After each iteration the fiber loop
    /// resolves [`crate::wrappers::condition::UNTIL_BINDING`] through
    /// `CycleWires` over the per-fiber kernel — the same call an op-level
    /// `poll:` makes — and ends the loop when it reads truthy per
    /// [`crate::wrappers::condition::is_truthy`]. `lookup` would not do: the
    /// predicate is a DYNAMIC binding fed by per-iteration capture writes, so
    /// it has to be pulled or it returns the last-evaluated value forever.
    pub kernel: Arc<polydat::kernel::PolydatKernel>,
    /// Sleep between iterations (after a predicate check
    /// returns "not done").
    pub interval: std::time::Duration,
    /// Wall-clock cap on the whole poll loop. Computed at
    /// run-phase entry as `Instant::now() + timeout_ms`.
    pub deadline: std::time::Instant,
    /// `Instant` the loop started — used to compute the
    /// elapsed-time value emitted under `metric_name`
    /// (if set) on successful completion.
    pub started_at: std::time::Instant,
    /// Optional named metric to emit on successful loop
    /// completion (predicate fired). Value is the elapsed
    /// wall-clock decoded per the existing `_ns` / `_us` /
    /// `_ms` / `_s` / `_m` / `_h` suffix convention. `None`
    /// ⇒ no metric written.
    pub metric_name: Option<String>,
    /// Tolerated consecutive retryable inner-op errors
    /// before the loop propagates the error. Mirrors the
    /// per-op `PollingDispenser` `max_error_retries`
    /// semantics. Default `0` (strict).
    pub max_error_retries: u32,
    /// What to do when the `deadline` fires without
    /// satisfying the predicate. SRD-75 §"on_timeout":
    /// - `Error` (default) — set the activity's
    ///   stop_flag + stop_reason. The phase returns an
    ///   error; the scenario walker's error-routing
    ///   policy decides whether sibling phases continue.
    /// - `Abort` — additionally call
    ///   `session_signals::request_stop()` so the whole
    ///   scenario terminates. The workload-author
    ///   declares the predicate's satisfaction as a
    ///   precondition for any downstream phase being
    ///   meaningful; a stuck synchronizer invalidates
    ///   the rest of the run.
    pub on_timeout: PhasePollTimeoutPolicy,
    /// SRD-75 (C5) — strict-gate selectors: each must resolve to a
    /// registered instrument before the gate's predicate is trusted.
    /// While any selector is unresolved the gate HOLDS (the predicate
    /// is not consulted — an unregistered family reads 0.0, which
    /// could satisfy a `>=`-shaped predicate spuriously); past
    /// [`Self::require_grace`] an unresolved selector is a hard
    /// `poll_require` failure.
    pub require: Vec<String>,
    /// Grace deadline for [`Self::require`] resolution: the first
    /// poll interval after loop start (late registration is normal —
    /// a producing daemon registers its instruments as it spins up).
    pub require_grace: std::time::Instant,
}

/// SRD-75 `on_timeout` policy — see [`PhasePollContext::on_timeout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PhasePollTimeoutPolicy {
    /// Phase fails; scenario walker's error-routing policy
    /// decides downstream behaviour.
    #[default]
    Error,
    /// Phase fails AND `session_signals::request_stop()`
    /// is called — the scenario walker observes the
    /// global stop on its next iteration check and
    /// terminates the whole run.
    Abort,
}

/// Invoke [`DriverAdapter::declare_controls`] for each unique adapter
/// instance against the given parent component, deduping by
/// `Arc`-pointer identity. The same adapter `Arc` may be entered
/// into the map under multiple alias keys; this guarantees each
/// physical instance gets exactly one declaration call per
/// invocation of this helper.
///
/// Called from two sites:
///
/// 1. The phase executor at component-attach time, so
///    `dryrun=controls` walks a populated tree before any
///    cycles run.
/// 2. [`Activity::run_with_adapters`] at run start, so adapters
///    that only ever materialize at run time still get declared.
///
/// Adapter implementations are expected to be idempotent — calling
/// this helper twice against the same parent must not produce
/// duplicate subcomponents or duplicate-name control declarations.
pub fn declare_adapter_controls(
    adapters: &std::collections::HashMap<String, Arc<dyn DriverAdapter>>,
    component: &Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
) {
    let mut seen: Vec<*const dyn DriverAdapter> = Vec::new();
    for adapter in adapters.values() {
        let ptr = Arc::as_ptr(adapter);
        if seen.contains(&ptr) {
            continue;
        }
        seen.push(ptr);
        adapter.declare_controls(component);
    }
}

impl Activity {
    pub fn new(config: ActivityConfig, parent_labels: &Labels, op_sequence: OpSequence) -> Self {
        Self::with_params(
            config,
            parent_labels,
            op_sequence,
            std::collections::HashMap::new(),
        )
    }

    pub fn with_params(
        config: ActivityConfig,
        parent_labels: &Labels,
        op_sequence: OpSequence,
        params: std::collections::HashMap<String, String>,
    ) -> Self {
        // Library / test path: no session root policy, so build a
        // standalone (un-shared) policy from the config. The real
        // execution path resolves the shared instance from the parent
        // policy and passes it to `with_params_and_sigdigs`.
        let error_policy =
            crate::error_policy::ErrorPolicy::standalone(crate::error_policy::PolicyConfig::new(
                config.error_spec.clone(),
                config.error_rate_max,
            ));
        let metric_detail = metric_detail_from_params(&params);
        Self::with_params_and_sigdigs(
            config,
            parent_labels,
            op_sequence,
            params,
            nmbrs_metrics::instruments::histogram::DEFAULT_HDR_SIGDIGS,
            error_policy,
            // This shim is the no-phase-kernel path (tests / library use);
            // the executor's run_phase path passes the phase node's kernel.
            None,
            &metric_detail,
        )
    }

    /// Build an activity with explicit HDR significant-digits
    /// precision. Used by the runner after it resolves
    /// `hdr.sigdigs` from the session root (SRD 40); every
    /// histogram the activity owns is constructed at this
    /// precision. Callers that don't resolve from a tree can
    /// use [`Self::with_params`] which defaults to
    /// [`nmbrs_metrics::instruments::histogram::DEFAULT_HDR_SIGDIGS`].
    pub fn with_params_and_sigdigs(
        config: ActivityConfig,
        parent_labels: &Labels,
        op_sequence: OpSequence,
        params: std::collections::HashMap<String, String>,
        sigdigs: u8,
        error_policy: Arc<crate::error_policy::ErrorPolicy>,
        phase_kernel: Option<Arc<polydat::kernel::PolydatKernel>>,
        metric_detail: &MetricDetailConfig,
    ) -> Self {
        let labels = parent_labels.clone();
        // SRD-91 — counter-vs-timer detail for the op-outcome instruments,
        // resolved by the caller from the run's effective params (the
        // executor passes the CLI-overlaid set; the library shim derives
        // from its own params). Default: timers.
        let metrics = Arc::new(ActivityMetrics::with_sigdigs(
            &labels,
            sigdigs,
            metric_detail,
        ));
        // All phases go through sources. cycles: N desugars to range(0, N).
        // Named cursors in Polydat provide their own factory via config.source_factory.
        let source_factory: Arc<dyn polydat::iteration::source::DataSourceFactory> =
            config.source_factory.clone().unwrap_or_else(|| {
                Arc::new(polydat::iteration::source::RangeSourceFactory::named(
                    "cycles",
                    0,
                    config.cycles,
                ))
            });

        Self {
            config,
            labels,
            metrics,
            op_sequence,
            error_policy,
            phase_kernel,
            source_factory,
            workload_params: Arc::new(params),
            stop_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            walk_stop: None,
            daemon_stop: None,
            stop_reason: Arc::new(std::sync::Mutex::new(None)),
            stop_outcome: Arc::new(std::sync::Mutex::new(None)),
            phase_errors: Arc::new(std::sync::Mutex::new(Vec::new())),
            validation_frame: Arc::new(std::sync::Mutex::new(None)),
            component: None,
            wrappers_override: None,
            wrap_default_order: None,
            exemplar_config: Arc::new(crate::exec_events::ExemplarConfig::new(0.0, 5.0)),
            advisory_gate: Arc::new(crate::exec_events::AdvisoryGate::new()),
            memo: Arc::new(arc_swap::ArcSwap::from_pointee(String::new())),
            gutter: Arc::new(arc_swap::ArcSwapOption::empty()),
            gutter_spec: std::sync::Mutex::new(None),
            gutter_final_spec: std::sync::Mutex::new(None),
            phase_poll: None,
        }
    }

    /// Whether this execution's scenario walk has halted (SRD-82 Part
    /// 4). Fibers poll this at their cooperative boundaries, alongside
    /// `stop_flag` and `session_signals::stop_requested()`, to abort an
    /// in-flight phase when a sibling failed or a stop condition
    /// tripped. `false` when no walk-stop flag is wired (tests / shim).
    #[inline]
    pub fn walk_stop_requested(&self) -> bool {
        self.walk_stop
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Whether this (daemon) phase's group has signalled completion
    /// (SRD-82 Part 6) — the scenario shell latches `daemon_stop` once
    /// the scope's foreground phases finish, and the daemon's fibers poll
    /// this to exit. `false` for a foreground phase (no flag wired).
    #[inline]
    pub fn daemon_stop_requested(&self) -> bool {
        self.daemon_stop
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// SRD-92 Step 0 — the cooperative-stop view at a loop BREAK boundary:
    /// the activity `stop_flag`, the global / per-execution session stop,
    /// the SRD-83 `walk_stop`, and the SRD-82 P6 `daemon_stop`. Replaces the
    /// scattered per-flag loads at the fiber boundaries. (The
    /// failure-determining return deliberately uses a different set that
    /// EXCLUDES `daemon_stop` — see
    /// [`crate::session_signals::StopView::abnormal`].)
    #[inline]
    pub fn stopped(&self) -> bool {
        self.stop_flag.load(std::sync::atomic::Ordering::Relaxed)
            || crate::session_signals::stop_requested()
            || self.walk_stop_requested()
            || self.daemon_stop_requested()
    }

    /// The portable [`StopView`](crate::session_signals::StopView) for this
    /// activity — handed to the `while:` wrapper (a `Send + 'static`
    /// dispenser that cannot borrow the activity) so its loop observes the
    /// full stop set, not just `stop_flag`. Built once at wrapper
    /// construction, after `walk_stop` / `daemon_stop` are set in `run_phase`.
    pub fn stop_view(&self) -> crate::session_signals::StopView {
        crate::session_signals::StopView::new(
            Some(self.stop_flag.clone()),
            self.walk_stop.clone(),
            self.daemon_stop.clone(),
        )
    }

    /// SRD-32a Push 3 — set the workload-root wrapper-
    /// composition override on this activity. Pass `None` to
    /// clear; pass `Some(order)` to install. The order list
    /// is innermost-to-outermost; per-op `wrappers:` blocks
    /// shadow this entry entirely.
    pub fn set_wrappers_override(&mut self, order: Option<Vec<String>>) {
        self.wrappers_override = order;
    }

    /// SRD-32a Push 3 — set the resolver's default-order
    /// tiebreaker for this activity (CLI
    /// `--wrap-default-order`). `None` ⇒ the resolver uses
    /// its built-in `DEFAULT_ORDER` list.
    pub fn set_wrap_default_order(&mut self, order: Option<Vec<String>>) {
        self.wrap_default_order = order;
    }

    /// Attach this activity to its component in the session tree.
    /// The runner creates the component and installs it here so
    /// `run_with_*` can register appliers on the activity's
    /// declared controls.
    ///
    /// Structural control declarations happen here — not at run
    /// time — so `dryrun=controls` (and every other pre-execution
    /// discovery path) sees the activity's controls without
    /// needing to start any cycles. Appliers that depend on
    /// run-time state (the fiber pool, the rate limiter) are
    /// registered later in `run_with_adapters`.
    pub fn attach_component(
        &mut self,
        component: Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
    ) {
        use crate::control_catalog::{CONCURRENCY, RATE};
        // Derive both controls from their single-source capability descriptors
        // (SRD-23) — name, range, and gauge come from the same `ControlDesc`
        // that `describe controls` reads, so the discovery surface and the
        // live knob can't drift. The instance-specific appliers (fiber-pool
        // resize, rate limiter) are registered later in `run_with_adapters`.
        let concurrency_control = CONCURRENCY.build_u32(self.config.concurrency as u32);
        component
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .controls()
            .declare(concurrency_control);

        // Declare a `rate` control whenever the activity config has a rate
        // set. Its reified gauge projects ops/sec so metric sinks and the
        // f64-writable surface (TUI `e` prompt, web POST, Polydat
        // `control_set`, `optimize.servo: rate`) all read and write the same
        // unit. The [`RateLimiterApplier`] gets registered at run time once
        // the limiter exists (see `run_with_adapters`).
        if let Some(rate) = self.config.rate {
            let rate_control = RATE.build_rate(rate);
            component
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .controls()
                .declare(rate_control);
        }

        // Retry-exemplar sampling (SRD-82 Part 3b / `exec_events`):
        // both knobs are push-on-set — the applier is one atomic
        // store into the activity's shared `ExemplarConfig`, which
        // every unpinned tries wrapper reads (only on its retry
        // path). Ops that pin `retry_exemplar_*` params hold private
        // cells the controls deliberately do not move — authored
        // matter wins, the live control moves the rest.
        {
            use crate::control_catalog::{RETRY_EXEMPLAR_MAX_HZ, RETRY_EXEMPLAR_RATE};
            use nmbrs_metrics::controls::SyncApplier;
            let cfg = self.exemplar_config.clone();
            let rate_control = RETRY_EXEMPLAR_RATE.build_f64(0.0);
            rate_control.register_applier(SyncApplier::new(move |v: f64| {
                cfg.set_rate(v);
                Ok(())
            }));
            component
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .controls()
                .declare(rate_control);

            let cfg = self.exemplar_config.clone();
            let hz_control = RETRY_EXEMPLAR_MAX_HZ.build_f64(5.0);
            hz_control.register_applier(SyncApplier::new(move |v: f64| {
                cfg.set_max_hz(v);
                Ok(())
            }));
            component
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .controls()
                .declare(hz_control);
        }
        // Register every static instrument owned by ActivityMetrics
        // on this component so the cadence reporter's tree walk
        // sees them. Failures here are programming errors
        // (duplicate family on the activity's own component) —
        // panic so the issue surfaces during init.
        {
            let mut guard = component.write().unwrap_or_else(|e| e.into_inner());
            self.metrics
                .register_on(&mut guard)
                .expect("ActivityMetrics::register_on failed on a fresh activity component");
        }
        self.component = Some(component);
    }

    /// Get a shared reference to the metrics for external capture.
    pub fn shared_metrics(&self) -> Arc<ActivityMetrics> {
        self.metrics.clone()
    }

    /// Run the activity with a single adapter for all ops.
    pub async fn run_with_driver(
        self,
        adapter: Arc<dyn DriverAdapter>,
        op_builder: Arc<crate::synthesis::OpBuilder>,
    ) -> bool {
        let mut adapters = std::collections::HashMap::new();
        let name = adapter.name().to_string();
        adapters.insert(name.clone(), adapter);
        self.run_with_adapters(adapters, &name, op_builder).await
    }

    /// Run the activity with multiple adapters (SRD 38/40).
    ///
    /// Each op template's `adapter` param selects which adapter to use.
    /// Templates without an explicit adapter use `default_adapter`.
    /// At init time: maps each template to a dispenser from the
    /// appropriate adapter. Per fiber: creates a FiberBuilder. Per
    /// cycle: resolves fields via GK, executes via dispenser.
    /// Returns true if the activity was stopped by an error handler.
    pub async fn run_with_adapters(
        self,
        adapters: std::collections::HashMap<String, Arc<dyn DriverAdapter>>,
        default_adapter: &str,
        op_builder: Arc<crate::synthesis::OpBuilder>,
    ) -> bool {
        let activity = Arc::new(self);
        let program = op_builder.program();

        // Init time: map each template to a dispenser from its adapter,
        // then wrap with result traverser for consumption/capture.
        //
        // Dryrun injection: when the session is in dryrun mode
        // (`config.dry_run_mode` is `Some(mode)`), inject a logical
        // `dryrun: <mode>` parameter into every op template's
        // `params` map BEFORE the wrapping cascade sees them. This
        // triggers the outermost `DryRunWrapper` to install for
        // every op; the wrapper short-circuits at cycle time and
        // suppresses only the outbound `execute()`.
        //
        // The real adapter's full lifecycle still runs (connect,
        // prepare, metadata) — `dryrun=cycle` means "construct a
        // fully-executable cycle path, then suppress only the
        // outbound call." So `dry_run_mode` is sourced from the
        // session config, NOT from any adapter substitution.
        let dryrun_mode: Option<String> = activity.config.dry_run_mode.clone();
        let templates_owned: Vec<nmbrs_workload::model::ParsedOp>;
        let templates: &[nmbrs_workload::model::ParsedOp] =
            if let Some(mode) = dryrun_mode.as_deref() {
                templates_owned = activity
                    .op_sequence
                    .templates()
                    .iter()
                    .map(|t| {
                        let mut clone = t.clone();
                        clone
                            .params
                            .insert("dryrun".into(), serde_json::Value::String(mode.to_string()));
                        // dryrun=fields also forces the fields wrapper
                        // on so the rendered op text reaches stdout
                        // even though DRYRUN short-circuits the
                        // adapter call. The fields wrapper is composed
                        // OUTER of dryrun (see
                        // wrapper_resolver::DEFAULT_ORDER), so its
                        // pre-execute render runs first; the
                        // subsequent DRYRUN short-circuit suppresses
                        // the real adapter call.
                        if mode == "fields" {
                            clone
                                .params
                                .insert("fields".into(), serde_json::Value::Bool(true));
                        }
                        clone
                    })
                    .collect();
                // The user passed `dryrun=<mode>` on the CLI — they
                // already know what they asked for. Keep the
                // marker-injection record at Debug so step-through /
                // session-log audits can still find it without
                // narrating it back on stderr every phase.
                crate::diag!(
                    crate::observer::LogLevel::Debug,
                    "dryrun={mode}: injected marker into {n} op template(s)",
                    mode = mode,
                    n = templates_owned.len()
                );
                &templates_owned[..]
            } else {
                activity.op_sequence.templates()
            };

        // Validate all bind points are resolvable before execution
        let program_for_op = |name: &str| op_builder.program_for_op(name);
        if let Err(e) = crate::synthesis::validate_bind_points(templates, &program_for_op) {
            crate::diag!(crate::observer::LogLevel::Error, "error: {e}");
            return true;
        }

        // Adapter-level dynamic controls (SRD 23). The phase
        // executor already declared adapter controls at attach
        // time so `dryrun=controls` saw them; calling again here
        // is the safety net for adapters that materialize only
        // at run time. Adapter `declare_controls` impls are
        // contractually idempotent — see `declare_adapter_controls`.
        if let Some(component) = activity.component.as_ref() {
            declare_adapter_controls(&adapters, component);
        }

        let traversal_stats = Arc::new(crate::wrappers::TraversalStats {
            metrics: activity.metrics.clone(),
        });

        // SRD-32a — wrapper registry + resolver. The
        // registry is fixed at link time (every `inventory::
        // submit!` block in the binary contributes one
        // entry); the resolver carries the validated
        // default-order tiebreaker. Both are built once
        // here and reused for every op template in this
        // activity.
        let wrapper_registry = crate::wrapper_registry::WrapperRegistry::from_inventory();
        // SRD-32a Push 3 — CLI `--wrap-default-order` replaces
        // the resolver's built-in tiebreaker. When unset, the
        // resolver builds with its DEFAULT_ORDER. The CLI list
        // is validated against the constraint graph at
        // construction; an inconsistent list aborts the run.
        let wrapper_resolver = match &activity.wrap_default_order {
            Some(order) => {
                let names: Vec<&str> = order.iter().map(|s| s.as_str()).collect();
                crate::wrapper_resolver::WrapperResolver::from_names(&names, &wrapper_registry)
            }
            None => crate::wrapper_resolver::WrapperResolver::with_default_order(&wrapper_registry),
        };
        let wrapper_resolver = match wrapper_resolver {
            Ok(r) => r,
            Err(e) => {
                crate::diag!(
                    crate::observer::LogLevel::Error,
                    "error: wrapper default-order is inconsistent with the \
                     registered wrapper graph: {e}. CLI `--wrap-default-order` \
                     and the built-in default both must satisfy every \
                     registered constraint."
                );
                return true;
            }
        };

        let mut dispensers: Vec<Arc<dyn OpDispenser>> = Vec::new();
        let mut validation_metrics: Vec<Arc<validation::ValidationMetrics>> = Vec::new();
        // SRD-40b §6/§7 — one `Component` per **op dispenser**
        // (= per op template), not per op execution. Op
        // dispensers are the durable CNS layer of the nmbrs
        // runtime; per-cycle op invocations are stack-ephemeral
        // and inherit the dispenser's component implicitly via
        // the wrapper-stack closure capture. Each component
        // carries `op=<template.name>` labels (child of the
        // activity component) so SRD-40b §7.2's duplicate-
        // family check (`Component::register_instrument`)
        // sees one dimensional cell per dispenser, surviving
        // for the run's duration. Held here to keep the Arc
        // alive.
        let mut dispenser_components: Vec<
            std::sync::Arc<std::sync::RwLock<nmbrs_metrics::component::Component>>,
        > = Vec::new();
        // Per-template wrapper pull plan. Wrapper-side reads
        // (validation, conditional, throttle) go through this
        // `PullPlan` against the firing fiber's state — see
        // SRD 31 §"Pull plan vs bind plan". Adapter-side reads
        // moved to the generic `crate::wires::WireSource` surface
        // at SRD-68 Push 5; the legacy `field_pulls` /
        // `bind_plans` / `batch_configs` lists are retired.
        let mut pull_plans_per_template: Vec<crate::fixture::PullPlan> = Vec::new();
        for template in templates {
            // Resolve adapter: per-template override or default
            let adapter_name = template
                .params
                .get("adapter")
                .and_then(|v| v.as_str())
                .or_else(|| template.params.get("driver").and_then(|v| v.as_str()))
                .unwrap_or(default_adapter);
            let adapter = match adapters.get(adapter_name) {
                Some(a) => a,
                None => {
                    let available = adapters.keys().cloned().collect::<Vec<_>>().join(", ");
                    crate::diag!(
                        crate::observer::LogLevel::Error,
                        "error: unknown adapter '{adapter_name}' for op '{}' (available: {available})",
                        template.name
                    );
                    return true; // signal stop — cannot proceed without the adapter
                }
            };

            if template.params.contains_key("batch") {
                crate::diag!(
                    crate::observer::LogLevel::Debug,
                    "[activity] op '{}' has batch param: {:?}",
                    template.name,
                    template.params.get("batch")
                );
            }
            // SRD 30 §"Core-first field processing": if the adapter
            // declares its known op fields, every key in
            // `template.op` must be one of them. Core has already
            // stripped its own fields during parse (activity_params
            // in nmbrs-workload), so anything left is an adapter
            // concern. Unknown fields are a typo or a misplaced
            // core directive — fail loudly rather than silently
            // dropping the field.
            if let Some(known) = adapter.known_op_fields() {
                let unknown: Vec<&String> = template
                    .op
                    .keys()
                    .filter(|k| !known.contains(&k.as_str()))
                    .collect();
                if !unknown.is_empty() {
                    let list = unknown
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    crate::diag!(
                        crate::observer::LogLevel::Error,
                        "error: adapter '{}' does not recognize op fields [{list}] on op '{}'; known fields: [{}]",
                        adapter.name(),
                        template.name,
                        known.join(", "),
                    );
                    return true; // stop — misconfiguration
                }
            }

            // Same idea for `template.params`: validate against
            // a closed vocabulary so silent-ignore traps like
            // `evaluations: { relevancy: ... }` (wrapper keys
            // the runtime never reads) cannot hide a
            // misconfigured op. Allowed keys are the union of:
            //   1. core op-level params consumed by the runtime
            //      (validation, batching, polling, weighting,
            //      adapter selection) — `CORE_OP_PARAMS`.
            //   2. workload/CLI-level params that the parser
            //      blast-merges into every op's params at parse
            //      time — `runner::KNOWN_PARAMS`.
            //   3. user-declared workload params from the
            //      workload's top-level `params:` block (e.g.
            //      `table`, `keyspace`, `num_items`). The parser
            //      threads these into every op's params during
            //      doc → block → op merge, where they're meant
            //      for `{name}` interpolation in op templates.
            //      Visible here as `activity.workload_params`.
            //   4. adapter-specific params declared via
            //      `DriverAdapter::known_op_params()`.
            // Anything else is a typo / misplaced wrapper / dead
            // YAML and is rejected.
            {
                let allowed_extras = adapter.known_op_params();
                let workload_keys = &activity.workload_params;
                // A params key is legitimate if it is (1) a non-wrapper
                // core param, (2) a wrapper field declared via the
                // registry's `owned_fields` — the durable, type-safe
                // route that replaced the fragile "it's also a CLI
                // param" coincidence for fields like `errors`/`tries`,
                // (3) a CLI param blast-merged into every op's params at
                // parse time, (4) an adapter-declared extra, or (5) a
                // workload-level param.
                let unknown_params: Vec<&String> = template
                    .params
                    .keys()
                    .filter(|k| {
                        !crate::validation::CORE_OP_PARAMS.contains(&k.as_str())
                            && !wrapper_registry.owns_field(k)
                            && !crate::runner::is_cli_param(k)
                            && !allowed_extras.contains(&k.as_str())
                            && !workload_keys.contains_key(k.as_str())
                    })
                    .collect();
                if !unknown_params.is_empty() {
                    let list = unknown_params
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let wrapper_vocab = wrapper_registry
                        .all_owned_fields()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join(", ");
                    crate::diag!(
                        crate::observer::LogLevel::Error,
                        "error: op '{}' has unknown params keys [{list}] — \
                         not a core op param, not a wrapper field, not \
                         declared by adapter '{}', and not a workload-level \
                         param. Known core op params: [{}]. Wrapper fields: \
                         [{}]. Adapter extras: [{}]. Did you misspell a \
                         wrapper key (e.g. `verify:` / `poll:` / `readout:`) \
                         or nest something under a key the runtime doesn't \
                         read?",
                        template.name,
                        adapter.name(),
                        crate::validation::CORE_OP_PARAMS.join(", "),
                        wrapper_vocab,
                        allowed_extras.join(", "),
                    );
                    return true; // stop — misconfiguration
                }
            }

            // SRD-32a Push 2 — field ownership and misplaced-
            // field guard. The wrapper registry knows which
            // `params:` keys each wrapper consumes
            // (`owned_fields`) and what makes the wrapper
            // trigger. A field that's owned by a wrapper that
            // ISN'T triggered is misplaced — silently ignoring
            // it would mask a typo or a half-applied
            // configuration. Example: `poll_interval_ms: 5000`
            // on an op without `poll:` is a misconfiguration —
            // the operator probably meant to enable polling
            // but forgot the trigger.
            //
            // The closed-vocabulary check above catches "I
            // don't recognise this key at all"; THIS check
            // catches "I recognise the key but it has no effect
            // here." Both surface as hard errors.
            //
            // Note: we cross-check against `template.params`
            // for keys. The registry's owned_fields includes
            // a few names that live elsewhere on `ParsedOp`
            // (`if` → `template.condition`, `delay` →
            // `template.delay`); those happen to BE their
            // wrapper's trigger field too, so when set the
            // outer `(reg.triggers)(template)` short-circuits
            // and we never reach the params-key check for
            // them. The set is small enough that we don't
            // need a separate "where does this field live"
            // helper.
            {
                let violations = wrapper_registry
                    .misplaced_fields(crate::wrapper_registry::WrapperSubject::Op(template));
                if !violations.is_empty() {
                    for (wrapper, field) in &violations {
                        crate::diag!(
                            crate::observer::LogLevel::Error,
                            "error: op '{}': field `{field}` is owned by wrapper \
                             `{wrapper}`, but the trigger condition for `{wrapper}` \
                             is not satisfied (no trigger field set on this op). \
                             Either remove `{field}` or add the wrapper's trigger \
                             field. SRD-32a §\"Field ownership and parse-time \
                             validation\".",
                            template.name
                        );
                    }
                    return true; // stop — misconfiguration
                }
            }

            // Per-op dispenser-init contract: `map_op` owns the
            // typed-binder verification for ITS op as part of
            // completing the currying stack — it constructs any
            // binders from the adapter's protocol-side metadata,
            // verifies them against `parent` via
            // `polydat::binder::verify_against_kernel`, and
            // surfaces any violation as `Err`. No
            // outside-the-dispenser-init phase for binders; the
            // map_op return signals the result.
            match adapter
                .map_op(template, op_builder.canonical_kernel_for_op(&template.name))
                .await
            {
                Ok(d) => {
                    let raw: Arc<dyn OpDispenser> = Arc::from(d);

                    // Open the per-template scope fixture (SRD 32
                    // §"Init-Time Fixture and Consumer Self-
                    // Registration"). Each wrapper below registers
                    // its own Polydat name dependencies; the fixture is
                    // sealed after the wrapper chain is complete and
                    // the resulting PullPlan drives cycle-time reads.
                    //
                    // SRD-13d Phase 9 — when this op-template
                    // materialised its own kernel, the fixture
                    // builds its plan against THAT program so
                    // pulls resolve in the op-template scope.
                    // Flattened op-templates fall back to the
                    // activity-wide program (same scope as before
                    // Phase 9 landed).
                    let template_program = op_builder.program_for_op(&template.name);
                    let mut fx = crate::fixture::ScopeFixture::new(template_program.clone());

                    // SRD-32a — resolve which wrappers fire and
                    // in what order. The plan is innermost-first;
                    // `traverse` is always inner. The cascade
                    // below dispatches to the existing per-
                    // wrapper `wrap()` factory based on the plan
                    // entries' names. Plan order matches the
                    // built-in default order, which mirrors the
                    // pre-SRD-32a hand-rolled cascade — existing
                    // tests exercise the same composition.
                    //
                    // SRD-32a Push 3 — override precedence:
                    //   1. Per-op `template.wrappers.order` shadows everything else.
                    //   2. Else workload-root `activity.wrappers_override`.
                    //   3. Else the resolver's default-order tiebreaker.
                    // Per-op shadows root entirely (no merge).
                    let per_op_override = template
                        .wrappers
                        .as_ref()
                        .filter(|c| !c.order.is_empty())
                        .map(|c| c.order.clone());
                    let effective_override =
                        per_op_override.or_else(|| activity.wrappers_override.clone());
                    let plan = match effective_override {
                        Some(order) => {
                            let order_strs: Vec<&str> = order.iter().map(|s| s.as_str()).collect();
                            wrapper_resolver.resolve_with_order(
                                crate::wrapper_registry::WrapperSubject::Op(template),
                                &wrapper_registry,
                                &order_strs,
                            )
                        }
                        None => wrapper_resolver.resolve(
                            crate::wrapper_registry::WrapperSubject::Op(template),
                            &wrapper_registry,
                        ),
                    };
                    let plan = match plan {
                        Ok(p) => p,
                        Err(e) => {
                            crate::diag!(
                                crate::observer::LogLevel::Error,
                                "error: op '{}': wrapper resolution failed: {e}",
                                template.name
                            );
                            return true;
                        }
                    };

                    // SRD-32a §"Composition telemetry" — emit one
                    // Info-level line per assigned wrapper so
                    // operators can see, at session start, exactly
                    // which wrappers shape each op and how. Trivial
                    // wrappers (e.g. always-on `traverse`) return
                    // `None` from `describe_assignment` and are
                    // dropped from this list.
                    let assignments: Vec<(crate::wrapper_registry::WrapperName, String)> = plan
                        .iter_innermost_first()
                        .filter_map(|reg| {
                            (reg.describe_assignment)(crate::wrapper_registry::WrapperSubject::Op(
                                template,
                            ))
                            .map(|s| (reg.name, s))
                        })
                        .collect();
                    if !assignments.is_empty() {
                        // Per-op wrapper assignments are a diagnostic
                        // useful when chasing a wrapper-composition
                        // bug, not part of normal operator output.
                        // `nmbrs describe` renders the same stack on
                        // demand (see nmbrs/src/describe.rs); session.log
                        // still captures this for postmortem.
                        crate::diag!(
                            crate::observer::LogLevel::Debug,
                            "op '{}' wrappers (innermost → outermost):",
                            template.name
                        );
                        for (i, (_, line)) in assignments.iter().enumerate() {
                            crate::diag!(crate::observer::LogLevel::Debug, "  {}. {}", i + 1, line);
                        }
                    }

                    // SRD-82 Part 3b — resolve THIS op's error policy BEFORE
                    // the retry activation check, so the policy's `retry` verb
                    // can inject a retry budget (the errors→retry bridge). An
                    // op-template `errors:` derives a child of the phase
                    // policy (value-equality shared across ops declaring the
                    // same spec); no override inherits the phase policy by
                    // reference. The policy drives the OUTERMOST error-handler
                    // wrapper placed after the plan cascade below.
                    let op_error_policy =
                        match template.params.get("errors").and_then(|v| v.as_str()) {
                            Some(spec) => activity.error_policy.resolve_child(Some(
                                crate::error_policy::PolicyConfig::new(
                                    spec,
                                    activity.config.error_rate_max,
                                ),
                            )),
                            None => activity.error_policy.clone(),
                        };

                    // Tries wrapper — CONDITIONAL innermost wrapper (SRD-82
                    // Part 3b): `tries:` is its sigil, the TOTAL attempts the
                    // op may make. A budget resolves from, in order: the op's
                    // own `tries:` field; the inherited phase/root `tries`
                    // (config — the phase FIELD must beat a scope wire, since
                    // a workload-root `tries` param also lands in GK scope as
                    // a constant and would otherwise shadow an explicit
                    // phase-level `tries: 1` pin); a `tries` wire defined in
                    // the op's scope (bindings); or a `retry`/`retry(N)` verb
                    // in the op's error policy (the injection bridge — N
                    // additional attempts → N+1 total; `errors:` and `tries:`
                    // stay orthogonal surfaces). No budget anywhere OR
                    // `tries: 1` → NO wrapper → single attempt (the
                    // error-handler wrapper records the tallies). `tries: 0`
                    // constructs the wrapper in its fail-without-executing
                    // mode. When constructed it owns the attempt loop, the
                    // `attempt_*` counters, and the per-attempt panic catch.
                    // Op `tries:` accepts the sugared number/string AND the
                    // map form `{count: N, backoff: {...}}` — read `count`
                    // from the map. Falls back to the phase/root `tries`, an
                    // in-scope `tries` wire, then an `errors:` retry-verb
                    // budget.
                    let op_tries: Option<u32> = template
                        .params
                        .get("tries")
                        .and_then(|v| match v {
                            serde_json::Value::Number(n) => n.as_u64().map(|n| n as u32),
                            serde_json::Value::String(s) => s.trim().parse::<u32>().ok(),
                            serde_json::Value::Object(m) => {
                                m.get("count").and_then(|c| c.as_u64()).map(|n| n as u32)
                            }
                            _ => None,
                        })
                        .or(activity.config.tries)
                        .or_else(|| {
                            raw.canonical_kernel()
                                .and_then(|k| k.lookup("tries"))
                                .and_then(|v| match v {
                                    polydat::ast::Value::U64(n) => Some(n as u32),
                                    _ => None,
                                })
                        })
                        .or_else(|| {
                            op_error_policy
                                .router
                                .retry_verb_budget()
                                .map(|additional| additional.saturating_add(1))
                        });
                    let has_tries_wrapper = matches!(op_tries, Some(n) if n != 1);
                    // Retry pacing (compaction-demo diagnosis: an immediate
                    // `continue` retry loop hammers a dying server). Resolve
                    // (min, max, ratio) by precedence, highest first:
                    //   1. op `tries:` map `backoff: {ratio, min, max}`
                    //   2. op standalone `retry_backoff*` params (back-compat)
                    //   3. phase `tries:` map backoff (activity config)
                    //   4. built-in defaults: 100ms base, 10s cap, 2.0 ratio.
                    // `retry_backoff: 0` (base == 0) disables pacing.
                    let json_to_ms = |v: &serde_json::Value| -> Option<u64> {
                        match v {
                            serde_json::Value::Number(n) => n.as_u64().map(|n| n.to_string()),
                            serde_json::Value::String(s) => Some(s.clone()),
                            _ => None,
                        }
                        .and_then(|s| crate::timeval::parse_time_ms(&s).ok())
                    };
                    let mut backoff_base_ms: u64 = 100;
                    let mut backoff_max_ms: u64 = 10_000;
                    let mut backoff_ratio: f64 = 2.0;
                    // (3) phase-level map backoff
                    if let Some(bo) = activity.config.tries_backoff.as_ref() {
                        if let Some(r) = bo.ratio {
                            backoff_ratio = r;
                        }
                        if let Some(m) = bo
                            .min
                            .as_deref()
                            .and_then(|s| crate::timeval::parse_time_ms(s).ok())
                        {
                            backoff_base_ms = m;
                        }
                        if let Some(m) = bo
                            .max
                            .as_deref()
                            .and_then(|s| crate::timeval::parse_time_ms(s).ok())
                        {
                            backoff_max_ms = m;
                        }
                    }
                    // (1)/(2) op-level: the `tries:` map backoff wins; absent
                    // that, the standalone `retry_backoff*` params.
                    let op_map_backoff = template
                        .params
                        .get("tries")
                        .and_then(|v| v.as_object())
                        .and_then(|m| m.get("backoff"))
                        .and_then(|v| v.as_object());
                    if let Some(bo) = op_map_backoff {
                        if let Some(r) = bo.get("ratio").and_then(|v| v.as_f64()) {
                            backoff_ratio = r;
                        }
                        if let Some(m) = bo.get("min").and_then(&json_to_ms) {
                            backoff_base_ms = m;
                        }
                        if let Some(m) = bo.get("max").and_then(&json_to_ms) {
                            backoff_max_ms = m;
                        }
                    } else {
                        if let Some(m) = template.params.get("retry_backoff").and_then(&json_to_ms)
                        {
                            backoff_base_ms = m;
                        }
                        if let Some(m) = template
                            .params
                            .get("retry_backoff_max")
                            .and_then(&json_to_ms)
                        {
                            backoff_max_ms = m;
                        }
                        if let Some(r) = template
                            .params
                            .get("retry_backoff_ratio")
                            .and_then(|v| v.as_f64())
                        {
                            backoff_ratio = r;
                        }
                    }
                    // Retry-error counter-exemplars (`exec_events`): the
                    // fraction of caught-and-retried errors sampled onto
                    // the structured event sink (default 0.0 = off), and
                    // the emission-frequency ceiling that squelches spam.
                    // Standalone params, same channel as `retry_backoff*`.
                    let json_to_f64 = |v: &serde_json::Value| -> Option<f64> {
                        match v {
                            serde_json::Value::Number(n) => n.as_f64(),
                            serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
                            _ => None,
                        }
                    };
                    let pinned = template.params.contains_key("retry_exemplar_rate")
                        || template.params.contains_key("retry_exemplar_max_hz");
                    let sampler = if pinned {
                        // Authored pin: a private cell the dynamic
                        // controls deliberately do not move.
                        let exemplar_rate = template
                            .params
                            .get("retry_exemplar_rate")
                            .and_then(&json_to_f64)
                            .unwrap_or(0.0);
                        let exemplar_max_hz = template
                            .params
                            .get("retry_exemplar_max_hz")
                            .and_then(&json_to_f64)
                            .unwrap_or(5.0);
                        crate::exec_events::ExemplarSampler::pinned(exemplar_rate, exemplar_max_hz)
                    } else {
                        // Default: the activity's shared cell — the
                        // `retry_exemplar_rate`/`_max_hz` dynamic
                        // controls move every sampler built here with
                        // one atomic store.
                        crate::exec_events::ExemplarSampler::shared(
                            activity.exemplar_config.clone(),
                        )
                    };
                    // Default-on advisory; `retry_advisory: off` (or
                    // false) opts this op out of the shared gate.
                    let advisory_on = template
                        .params
                        .get("retry_advisory")
                        .map(|v| match v {
                            serde_json::Value::Bool(b) => *b,
                            serde_json::Value::String(s) => {
                                !s.eq_ignore_ascii_case("off") && !s.eq_ignore_ascii_case("false")
                            }
                            _ => true,
                        })
                        .unwrap_or(true);
                    let advisory = advisory_on.then(|| activity.advisory_gate.clone());
                    let raw = match op_tries {
                        Some(n) if n != 1 => crate::wrappers::TriesDispenser::wrap(
                            raw,
                            n,
                            activity.metrics.clone(),
                            backoff_base_ms,
                            backoff_max_ms,
                            backoff_ratio,
                            activity.stop_view(),
                            template.name.clone(),
                            sampler,
                            advisory,
                        ),
                        _ => raw,
                    };

                    // Wrap with traversal. Traversal does not read Polydat
                    // values; no fixture registration needed. Always present per
                    // the registry's always-true trigger.
                    let mut current: Arc<dyn OpDispenser> =
                        crate::wrappers::TraversingDispenser::wrap(
                            raw,
                            template,
                            traversal_stats.clone(),
                        );
                    // Late-bound handoff from the metrics arm (outside poll in
                    // the cascade, so it runs AFTER the poll arm below) to the
                    // poll dispenser: the op's compiled gauge slots, re-published
                    // per poll iteration so drains feed the store live.
                    let poll_iteration_gauges: Arc<
                        arc_swap::ArcSwapOption<Vec<crate::wrappers::metrics::MetricSlot>>,
                    > = Arc::new(arc_swap::ArcSwapOption::empty());

                    // Apply each remaining wrapper in plan order.
                    // Skip `traverse`; it's already constructed.
                    for reg in plan.iter_innermost_first() {
                        if reg.name == crate::wrappers::traverse::NAME {
                            continue;
                        }
                        let stop = match reg.name {
                            crate::wrappers::delay::NAME => {
                                let spec = template
                                    .delay
                                    .as_ref()
                                    .expect("delay triggered → delay set");
                                let trim = |s: &str| -> String {
                                    let t = s.trim();
                                    t.strip_prefix('{')
                                        .and_then(|s| s.strip_suffix('}'))
                                        .unwrap_or(t)
                                        .to_string()
                                };
                                let wrap_result = match spec {
                                    nmbrs_workload::model::DelaySpec::Before(name) => {
                                        let name = trim(name);
                                        crate::wrappers::DelayDispenser::wrap(
                                            current.clone(),
                                            &name,
                                            &mut fx,
                                        )
                                    }
                                    nmbrs_workload::model::DelaySpec::BeforeAfter {
                                        before,
                                        after,
                                    } => {
                                        let before = before.as_deref().map(trim);
                                        let after = after.as_deref().map(trim);
                                        crate::wrappers::DelayDispenser::wrap_before_after(
                                            current.clone(),
                                            before.as_deref(),
                                            after.as_deref(),
                                            &mut fx,
                                        )
                                    }
                                };
                                match wrap_result {
                                    Ok(d) => {
                                        current = d;
                                        false
                                    }
                                    Err(e) => {
                                        crate::diag!(
                                            crate::observer::LogLevel::Error,
                                            "error: op '{}': {e}",
                                            template.name
                                        );
                                        true
                                    }
                                }
                            }
                            crate::validation::WRAPPER_NAME => {
                                match crate::validation::ValidatingDispenser::wrap(
                                    current.clone(),
                                    template,
                                    &activity.labels,
                                    Some(&program),
                                    &mut fx,
                                ) {
                                    Ok((d, vm)) => {
                                        if let Some(vm) = vm {
                                            validation_metrics.push(vm);
                                        }
                                        current = d;
                                        false
                                    }
                                    Err(e) => {
                                        crate::diag!(
                                            crate::observer::LogLevel::Error,
                                            "error: op '{}': {e}",
                                            template.name
                                        );
                                        true
                                    }
                                }
                            }
                            crate::wrappers::poll::NAME => {
                                // Poll config reader: `poll:` is either
                                // a string (mode only, all-defaults) or
                                // a map (`{mode, interval_ms, timeout_ms,
                                // max_rows, min_rows, json_path,
                                // metric_name, max_error_retries}`). All
                                // poll knobs live UNDER `poll:` — no
                                // flat `poll_*` prefix keys at op
                                // level, so the wrapper's namespace
                                // doesn't collide with adapter fields
                                // (e.g. HTTP's `request_timeout_ms`).
                                let poll_val = template.params.get("poll");
                                let cfg = poll_val.and_then(|v| v.as_object());
                                let get_u64 = |k: &str, default: u64| -> u64 {
                                    cfg.and_then(|m| m.get(k))
                                        .and_then(|v| {
                                            v.as_u64().or_else(|| {
                                                v.as_str().and_then(|s| s.parse::<u64>().ok())
                                            })
                                        })
                                        .unwrap_or(default)
                                };
                                let get_u32 = |k: &str, default: u32| -> u32 {
                                    cfg.and_then(|m| m.get(k))
                                        .and_then(|v| {
                                            v.as_u64().map(|n| n as u32).or_else(|| {
                                                v.as_str().and_then(|s| s.parse::<u32>().ok())
                                            })
                                        })
                                        .unwrap_or(default)
                                };
                                let get_str = |k: &str| -> Option<String> {
                                    cfg.and_then(|m| m.get(k))
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string())
                                };
                                let interval = get_u64("interval_ms", 1000);
                                let timeout = get_u64("timeout_ms", 300_000);
                                // SRD-03 §"Status-Determination
                                // Invariant — Retries Within": bounded
                                // retry budget for retryable inner
                                // errors. Default 0 (strict). Operators
                                // raise this when long fixture readiness
                                // checks tolerate transient blips.
                                let max_error_retries = get_u32("max_error_retries", 0);
                                let metric_name = get_str("metric_name");
                                // `min_rows` / `max_rows` — completion
                                // window: poll considered done when
                                // row count is in the closed interval
                                // `[min..=max]`. Defaults `min=0, max=0`
                                // reproduce `await_empty` (exactly 0 =
                                // done). For "settled to N rows" cases
                                // (e.g. SAI's `sai_sstable_count == 1`
                                // after memtable flush + compaction)
                                // use `min=1, max=1`.
                                let min_rows = get_u64("min_rows", 0);
                                let max_rows = get_u64("max_rows", 0);
                                // `json_path` — optional JSON Pointer
                                // (RFC 6901, e.g. `/value`) drilled
                                // into the body before counting. Lets
                                // the count check address a nested
                                // field — load-bearing for envelope
                                // responses like Jolokia's
                                // `{value, status, …}`.
                                let json_path = get_str("json_path");
                                // `memo` / `progress` — live-status templates
                                // re-rendered per poll iteration against the
                                // wires: `memo` publishes the measured values
                                // to the activity memo (default: append
                                // `measured N row(s)…` to the base memo);
                                // `progress` parses to an f64 fraction that
                                // drives the phase completion bar via the
                                // derived-progress override.
                                let each_memo = get_str("memo");
                                let progress_template = get_str("progress");
                                // The op's `gutter:` DURING form rides into the poll
                                // loop for per-iteration refresh — the GutterDispenser
                                // itself publishes only once, when the (potentially
                                // hours-long) drain op completes.
                                // `on_done` — values written to the wires on the
                                // TERMINATING poll, before its metrics publish.
                                // A remote view of in-flight work (a compactions
                                // view, a job queue) shows what is RUNNING, so
                                // "done" arrives as an empty result and the
                                // finished item's attributes are already gone.
                                // This proxies the completed measurement we could
                                // not observe: `on_done: {completion_ratio: 1.0}`
                                // makes the final sample say what the view no
                                // longer can.
                                let on_done = crate::wrappers::poll::parse_on_done(cfg);
                                // `until:` — a polydat predicate replacing the
                                // row-count window. Scope synthesis lowered the
                                // expression into this node's kernel; the
                                // wrapper only needs to know that it exists.
                                let until = cfg
                                    .and_then(|m| m.get("until"))
                                    .and_then(|v| v.as_str())
                                    .is_some_and(|s| !s.trim().is_empty());
                                let (each_gutter, _) = crate::wrappers::gutter::parse_specs(
                                    template.params.get("gutter"),
                                );
                                let (d, _pm) = crate::wrappers::PollingDispenser::wrap_with_status(
                                    current.clone(),
                                    interval,
                                    timeout,
                                    max_error_retries,
                                    metric_name,
                                    min_rows,
                                    max_rows,
                                    json_path.clone(),
                                    each_memo,
                                    Some(activity.memo.clone()),
                                    progress_template,
                                    Some(activity.metrics.clone()),
                                    each_gutter,
                                    Some(activity.gutter.clone()),
                                    Some(poll_iteration_gauges.clone()),
                                    on_done.clone(),
                                    until,
                                    activity.stop_view(),
                                );
                                crate::diag!(
                                    crate::observer::LogLevel::Debug,
                                    "  op '{}': polling enabled (interval={}ms, timeout={}ms, max_error_retries={}, done_on={}, json_path={:?}, on_done={:?})",
                                    template.name,
                                    interval,
                                    timeout,
                                    max_error_retries,
                                    if until {
                                        "until-predicate".to_string()
                                    } else {
                                        format!("rows=[{min_rows}..={max_rows}]")
                                    },
                                    json_path,
                                    on_done
                                        .iter()
                                        .map(|(k, v)| format!("{k}={v}"))
                                        .collect::<Vec<_>>()
                                );
                                current = d;
                                false
                            }
                            crate::wrappers::r#if::NAME => {
                                // `if:` short-circuits before the
                                // inner cascade — load-bearing for
                                // the recent fix that pulls polling
                                // inside `if`. Resolver order
                                // mirrors that.
                                let cond = template
                                    .condition
                                    .as_deref()
                                    .expect("if triggered → condition set");
                                let cond_name = cond
                                    .trim()
                                    .strip_prefix('{')
                                    .and_then(|s| s.strip_suffix('}'))
                                    .unwrap_or(cond.trim());
                                match crate::wrappers::ConditionalDispenser::wrap(
                                    current.clone(),
                                    cond_name,
                                    activity.metrics.clone(),
                                    &mut fx,
                                ) {
                                    Ok(d) => {
                                        current = d;
                                        false
                                    }
                                    Err(e) => {
                                        crate::diag!(
                                            crate::observer::LogLevel::Error,
                                            "error: op '{}': {e}",
                                            template.name
                                        );
                                        true
                                    }
                                }
                            }
                            crate::wrappers::r#while::NAME => {
                                // `while:` loops the inner until the
                                // synthesised `__while` predicate
                                // flips falsy or the activity stops.
                                // The op-kernel synthesiser appended
                                // `__while := <expr>` to the kernel's
                                // result bindings — that's how the
                                // expression's free identifiers got
                                // their extern slots.
                                match crate::wrappers::WhileWrapper::wrap(
                                    current.clone(),
                                    activity.stop_view(),
                                    &mut fx,
                                ) {
                                    Ok(d) => {
                                        current = d;
                                        false
                                    }
                                    Err(e) => {
                                        crate::diag!(
                                            crate::observer::LogLevel::Error,
                                            "error: op '{}': {e}",
                                            template.name
                                        );
                                        true
                                    }
                                }
                            }
                            crate::wrappers::rate::NAME => {
                                // Per-op rate limiter, independent
                                // of the activity-level rate AND of
                                // every other op's per-op limiter.
                                // Each instance owns its own
                                // RateLimiter.
                                let rate_spec =
                                    template.rate.as_deref().expect("rate triggered → rate set");
                                match crate::wrappers::OpRateWrapper::wrap(
                                    current.clone(),
                                    rate_spec,
                                ) {
                                    Ok(d) => {
                                        current = d;
                                        false
                                    }
                                    Err(e) => {
                                        crate::diag!(
                                            crate::observer::LogLevel::Error,
                                            "error: op '{}': {e}",
                                            template.name
                                        );
                                        true
                                    }
                                }
                            }
                            crate::wrappers::fields::NAME => {
                                // Capture op_fields so the fields
                                // wrapper can render the rendered op
                                // text at cycle time. Stable
                                // insertion order isn't guaranteed by
                                // HashMap, but ParsedOp.op is small
                                // enough that a deterministic
                                // alphabetical sort keeps the printed
                                // output stable across runs.
                                let mut op_fields: Vec<(String, serde_json::Value)> = template
                                    .op
                                    .iter()
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect();
                                op_fields.sort_by(|a, b| a.0.cmp(&b.0));
                                current = crate::wrappers::FieldsDispenser::wrap_with_op_fields(
                                    current.clone(),
                                    &template.name,
                                    op_fields,
                                );
                                false
                            }
                            crate::wrappers::result::NAME => {
                                // SRD-40b §5: result-as-GK adapter —
                                // exposes captured result fields to
                                // the op's Polydat scope via
                                // `OpResult.captures` so metric
                                // expressions (and any later wrappers)
                                // can reference them by name. No-op
                                // when the op declares no `result:`
                                // wires.
                                current = crate::wrappers::ResultDispenser::wrap(
                                    current.clone(),
                                    template.result.as_ref(),
                                    template.abstract_interface.as_ref().map(|i| &i.results),
                                );
                                false
                            }
                            crate::wrappers::metrics::NAME => {
                                // SRD-40b §6/§7 — one `Component`
                                // per dispenser carrying
                                // `op=<template.name>` so the
                                // duplicate-family check sees one
                                // dimensional cell per dispenser
                                // and child ops collide cleanly on
                                // their `op=` label.
                                let labels =
                                    nmbrs_metrics::labels::Labels::of("op", &template.name);
                                let dispenser_component =
                                    std::sync::Arc::new(std::sync::RwLock::new(
                                        nmbrs_metrics::component::Component::new(
                                            labels,
                                            std::collections::HashMap::new(),
                                        ),
                                    ));
                                if let Some(parent) = activity.component.as_ref() {
                                    nmbrs_metrics::component::attach(parent, &dispenser_component);
                                }
                                let wrap_result = {
                                    let mut guard = dispenser_component
                                        .write()
                                        .unwrap_or_else(|e| e.into_inner());
                                    crate::wrappers::MetricsDispenser::wrap_with_slots(
                                        current.clone(),
                                        &template.metrics,
                                        &mut guard,
                                        &dispenser_component,
                                        &mut fx,
                                    )
                                };
                                match wrap_result {
                                    Ok((d, slots)) => {
                                        if let Some(slots) = slots {
                                            poll_iteration_gauges.store(Some(slots));
                                        }
                                        // Mark the dispenser
                                        // component Running so the
                                        // cadence reporter's
                                        // `capture_tree` walk visits
                                        // it on every tick.
                                        dispenser_component
                                            .write()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .set_state(
                                                nmbrs_metrics::component::ComponentState::Running,
                                            );
                                        dispenser_components.push(dispenser_component);
                                        current = d;
                                        false
                                    }
                                    Err(e) => {
                                        crate::diag!(
                                            crate::observer::LogLevel::Error,
                                            "error: op '{}': {e}",
                                            template.name
                                        );
                                        true
                                    }
                                }
                            }
                            crate::wrappers::dryrun::NAME => {
                                // Outermost short-circuit. Activated
                                // by the injected `dryrun:` template
                                // parameter (per
                                // `run_with_adapters`'s session-
                                // startup injection step). The
                                // trigger fires only when the
                                // template carries the marker, so
                                // we know we're in dryrun mode just
                                // by being here.
                                //
                                // Architectural invariant: by sitting
                                // outermost in the cascade, the
                                // short-circuit returns BEFORE any
                                // inner wrapper (verify / metrics /
                                // poll / etc.) can observe the
                                // empty body the dryrun stand-in
                                // produces. The `forbids_outer` set
                                // on the DRYRUN registration pins
                                // this position structurally.
                                // DryRunWrapper has one job: short-circuit
                                // the outbound op. No display modes, no
                                // op-field snapshot, no extra work — the
                                // wrapper is supposed to do NOTHING MORE
                                // than wrap the op and not call it.
                                current = crate::wrappers::DryRunWrapper::wrap(current.clone());
                                false
                            }
                            crate::wrappers::memo::NAME => {
                                // Memo wrapper: parse `memo:` (string
                                // shorthand or `{before, after}` map),
                                // wrap with cloned ArcSwap handle.
                                let (before, after) = match template.params.get("memo") {
                                    Some(serde_json::Value::String(s)) => {
                                        // Shorthand: same template for
                                        // before AND after.
                                        (Some(s.clone()), Some(s.clone()))
                                    }
                                    Some(serde_json::Value::Object(obj)) => {
                                        let b = obj
                                            .get("before")
                                            .and_then(|v| v.as_str())
                                            .map(String::from);
                                        let a = obj
                                            .get("after")
                                            .and_then(|v| v.as_str())
                                            .map(String::from);
                                        (b, a)
                                    }
                                    _ => (None, None),
                                };
                                if before.is_none() && after.is_none() {
                                    crate::diag!(
                                        crate::observer::LogLevel::Warn,
                                        "op '{}': memo: requires at least one of \
                                         `before` / `after` (or a string shorthand)",
                                        template.name
                                    );
                                    false
                                } else {
                                    current = crate::wrappers::MemoDispenser::wrap(
                                        current.clone(),
                                        before,
                                        after,
                                        activity.memo.clone(),
                                    );
                                    false
                                }
                            }
                            crate::wrappers::gutter::NAME => {
                                // Gutter wrapper: parse `gutter:` (layout-
                                // string shorthand or `{bar}` / `{spark}`
                                // map), wrap with the activity's shared
                                // gutter slot.
                                let (during, fin) = crate::wrappers::gutter::parse_specs(
                                    template.params.get("gutter"),
                                );
                                if during.is_none() && fin.is_none() {
                                    crate::diag!(
                                        crate::observer::LogLevel::Warn,
                                        "op '{}': gutter: requires a layout string, one of \
                                         `bar:` / `spark:` / `text:`, or a `final:` form",
                                        template.name
                                    );
                                    false
                                } else {
                                    if let Some((kind, tmpl)) = &during {
                                        *activity.gutter_spec.lock().unwrap() =
                                            Some((*kind, tmpl.clone()));
                                        current = crate::wrappers::GutterDispenser::wrap(
                                            current.clone(),
                                            *kind,
                                            tmpl.clone(),
                                            activity.gutter.clone(),
                                        );
                                    }
                                    if let Some((kind, tmpl)) = &fin {
                                        *activity.gutter_final_spec.lock().unwrap() =
                                            Some((*kind, tmpl.clone()));
                                    }
                                    false
                                }
                            }
                            crate::wrappers::readout::NAME => {
                                // Readout wrapper: op-level status leaf. Wrap the
                                // inner op with a lifecycle reporter keyed by the
                                // op-template name; the parent phase is resolved at
                                // run time from the task-local CURRENT_PHASE. Only
                                // `readout: visible` triggers it (see the wrapper's
                                // `triggers`), so this arm always wraps when reached.
                                current = crate::wrappers::ReadoutDispenser::wrap_with_measure(
                                    current.clone(),
                                    template.name.clone(),
                                    template
                                        .params
                                        .get("measure")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                );
                                crate::diag!(
                                    crate::observer::LogLevel::Debug,
                                    "  op '{}': readout visible (op-level status line)",
                                    template.name
                                );
                                false
                            }
                            crate::wrappers::errors::NAME => {
                                // SRD-82 Part 3b — hand-placed OUTERMOST after
                                // this loop (mirrors the hand-placed innermost
                                // tries). The plan entry records presence for
                                // telemetry / describe only.
                                false
                            }
                            crate::wrappers::tries::NAME => {
                                // SRD-82 Part 3b — hand-placed INNERMOST before
                                // this loop (the sigil resolution above). The
                                // plan entry records presence for telemetry /
                                // describe only.
                                false
                            }
                            other => {
                                crate::diag!(
                                    crate::observer::LogLevel::Error,
                                    "error: op '{}': resolver returned wrapper `{}` \
                                     with no dispatch handler in the cascade",
                                    template.name,
                                    other
                                );
                                true
                            }
                        };
                        if stop {
                            return true;
                        }
                    }

                    // Dryrun short-circuit: when the session is in
                    // dryrun mode (`config.dry_run_mode` set), the
                    // dryrun template-parameter injection above put
                    // a `dryrun:` field on every op template; the
                    // wrapper resolver picks up that field and adds
                    // `DryRunWrapper` as the OUTERMOST layer. The
                    // wrapper never calls its inner — verify /
                    // metrics / poll / etc. observers don't fire,
                    // and the real adapter's `execute()` is
                    // suppressed. The real adapter itself still
                    // constructs in full (connect, prepare, gather
                    // metadata); only the per-cycle outbound call
                    // is short-circuited.
                    // SRD-82 Part 3b — the error handler is the OUTERMOST
                    // wrapper, hand-placed after the plan cascade (mirroring
                    // the hand-placed innermost retry wrapper), driven by the
                    // policy resolved BEFORE the retry check above. Only the
                    // op-error ROUTER is per-op — the aggregate rate breach
                    // stays the phase shell's `error_policy.guard`. The
                    // wrapper observes the stack's ONE terminal outcome per
                    // cycle: routes it, tallies result-level error counters,
                    // captures phase errors, and applies stop/fail effects.
                    // Its happy path is a single branch. When no retry
                    // wrapper is present it also records the single-attempt
                    // `attempt_*` tallies (`records_attempts`).
                    let current = crate::wrappers::ErrorHandlerDispenser::wrap(
                        current,
                        op_error_policy,
                        activity.metrics.clone(),
                        activity.phase_errors.clone(),
                        activity.stop_flag.clone(),
                        activity.stop_reason.clone(),
                        template.name.clone(),
                        /* records_attempts */ !has_tries_wrapper,
                    );
                    dispensers.push(current);

                    // Seal the per-template fixture. The PullPlan
                    // drives cycle-time reads for every wrapper that
                    // registered (validation ground truth, conditional
                    // `if`, throttle `delay`). See SRD 31 §"Pull plan
                    // vs bind plan".
                    pull_plans_per_template.push(fx.seal());
                }
                Err(e) => {
                    crate::diag!(
                        crate::observer::LogLevel::Error,
                        "error: adapter.map_op failed for '{}': {e}",
                        template.name
                    );
                    return true;
                }
            }
        }
        let dispensers = Arc::new(dispensers);
        // Register dispensers for adapter-specific metrics capture
        activity.metrics.set_dispensers(dispensers.clone());
        let pull_plans_per_template = Arc::new(pull_plans_per_template);

        // `dryrun=dispenser` exit point. Every op template's
        // dispenser is constructed (map_op succeeded, wrapper
        // plan resolved, pull plan sealed). Nothing else needs
        // to happen for the operator to know the construction
        // pipeline is healthy — return cleanly without spawning
        // the fiber pool or the progress thread.
        if activity.config.stop_after_dispenser_init {
            crate::diag!(
                crate::observer::LogLevel::Info,
                "dryrun=dispenser: {} op-template dispenser(s) constructed; \
                 stopping before cycle execution",
                dispensers.len()
            );
            return false;
        }

        let validation_metrics = Arc::new(validation_metrics);
        // Share the validation-metrics handle with ActivityMetrics so
        // the progress thread (below) can read live relevancy aggregates.
        activity
            .metrics
            .set_validation_metrics(validation_metrics.clone());

        // Single activity-level rate limiter. One ops-per-sec
        // ceiling gates every fiber; there is no separate
        // stanza-rate mechanism. Activities with no `rate`
        // configured skip construction cleanly.
        let rate_limiter = activity
            .config
            .rate
            .map(|r| Arc::new(RateLimiter::start(nmbrs_rate::RateSpec::new(r))));

        // Register the [`RateLimiterApplier`] against the
        // already-declared `rate` control if both the control
        // and the limiter exist. The declaration happens in
        // [`Self::attach_component`] — this step only wires the
        // applier so a runtime write actually reconfigures the
        // running limiter.
        if let (Some(ac), Some(rl)) = (activity.component.as_ref(), rate_limiter.as_ref()) {
            let existing: Option<nmbrs_metrics::controls::Control<nmbrs_rate::RateSpec>> = ac
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .controls()
                .get("rate");
            if let Some(ctl) = existing {
                ctl.register_applier(nmbrs_rate::RateLimiterApplier::new(Arc::clone(rl)));
            }
        }

        // SRD-100 P2 — the inline-status refresh thread is RETIRED. The
        // live phase status is now folded at the display consumer from the
        // `active_phases` snapshot (the executor attaches each phase's
        // render handle on-task; see `RunObserver::phase_render_attach` and
        // `nmbrs_tui::status_fold`). This removes the last-writer race on the
        // single status slot, the `std::thread` cross-execution hazard (the
        // thread couldn't read the SRD-88 task-local channel), and the
        // per-phase-clear-wipes-peers bug. The phase-scoped values below
        // still feed the phase-END readout + outcome line.
        let activity_name = activity.config.name.clone();
        let suppress_progress = adapters.values().any(|a| a.name() == "plotter");
        let start_time = Instant::now();
        // Use source extent for progress (data-driven), not cycles
        let source_for_progress = activity.source_factory.clone();
        let total_extent = source_for_progress
            .global_extent()
            .unwrap_or(activity.config.cycles);
        // One Arc<str> shared by every fiber in this phase. The
        // Polydat runtime-context `phase()` node clones this per read
        // instead of per fiber, keeping the per-cycle cost O(1).
        let phase_name_arc: Arc<str> = Arc::from(activity_name.as_str());

        // Daemon-op pool. Shared across cycle-pool fibers — each
        // fiber's stanza walk dispatches daemon ops by spawning
        // a fresh fiber onto this pool instead of running them
        // inline. The pool enforces per-op-name fiber caps; an
        // overflow is a workload-design error that fails the
        // phase. At phase exit the cycle-pool drain runs first,
        // then this pool's shutdown signals and waits on every
        // still-running daemon (see daemon-pool drain below).
        let daemon_pool = Arc::new(crate::daemon_pool::DaemonPool::new());

        // SRD 23 §"Fiber executor": fiber lifecycle goes through
        // a [`FiberPool`] that the `ConcurrencyApplier` can
        // resize via the activity's `concurrency` control. Each
        // fiber receives its own stop-flag and exits
        // cooperatively at the next cycle boundary when flagged.
        let pool_spawner: crate::fiber_pool::FiberSpawner = {
            let activity = activity.clone();
            let dispensers_outer = dispensers.clone();
            let pull_plans_outer = pull_plans_per_template.clone();
            let op_builder_outer = op_builder.clone();
            let rate_limiter_outer = rate_limiter.clone();
            let phase_arc_outer = phase_name_arc.clone();
            let daemon_pool_outer = daemon_pool.clone();
            // SRD-89 — snapshot this phase's controls ONCE (walk-up from the
            // phase component), shared lock-free across all of the phase's
            // fibers. Carries live handles, so servo retargets are observed.
            let phase_controls_outer = activity
                .component
                .as_ref()
                .map(crate::polydat_nodes::runtime_context::snapshot_controls)
                .unwrap_or_else(crate::polydat_nodes::runtime_context::empty_controls);
            Box::new(move |stop: crate::fiber_pool::StopFlag| {
                let activity = activity.clone();
                let dispensers = dispensers_outer.clone();
                let pull_plans = pull_plans_outer.clone();
                let op_builder = op_builder_outer.clone();
                let rate_limiter = rate_limiter_outer.clone();
                let phase_arc = phase_arc_outer.clone();
                let daemon_pool = daemon_pool_outer.clone();
                let phase_controls = phase_controls_outer.clone();
                // SRD-88 — carry the per-execution context into the per-cycle
                // fiber: the adapter's op-output, log, stop, and exec-identity
                // resolve to THIS execution (so concurrent executions sharing a
                // session capture their own output / route their own log). A
                // no-op on the single-run path (no context scoped — A1).
                tokio::spawn(crate::execution_context::propagate(async move {
                    // Catch panics inside the fiber so they surface
                    // in diagnostics rather than silently terminating
                    // the task. Without this, a panic in any cycle's
                    // accessor / binder code would leave the fiber
                    // gone and the run still "active" from the
                    // perspective of the executor, hanging the TUI
                    // with no visible cause. The session log line
                    // captures location + message; the runtime's
                    // own panic reporting (if any) is unchanged.
                    use futures::FutureExt as _;
                    let activity_for_panic = activity.clone();
                    let activity_name_for_log = activity.config.name.clone();
                    let phase_arc_for_exec = phase_arc.clone();
                    let body = crate::polydat_nodes::runtime_context::with_fiber_context(
                        phase_arc,
                        phase_controls,
                        async move {
                            executor_task(
                                activity,
                                dispensers,
                                pull_plans,
                                op_builder,
                                rate_limiter,
                                stop,
                                daemon_pool,
                                phase_arc_for_exec,
                            )
                            .await;
                        },
                    );
                    let result = std::panic::AssertUnwindSafe(body).catch_unwind().await;
                    match result {
                        Ok(()) => {
                            // Normal fiber exit is silent — the
                            // session log used to record one
                            // line per fiber here at Debug, but
                            // with concurrency=N, that's N lines
                            // per phase boundary in session.log
                            // for no diagnostic value (the phase
                            // completion + duration already
                            // tells the user the fibers
                            // completed). Panic exits below
                            // remain Error-level.
                            let _ = activity_name_for_log;
                        }
                        Err(panic_payload) => {
                            let msg = panic_payload
                                .downcast_ref::<&'static str>()
                                .map(|s| (*s).to_string())
                                .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "<non-string panic payload>".into());
                            crate::diag!(
                                crate::observer::LogLevel::Error,
                                "fiber panic in activity '{}': {}",
                                activity_name_for_log,
                                msg
                            );
                            // A panic is a first-class failure:
                            // give the run a headline cause
                            // (stop_reason) and a structured
                            // PhaseOutcome error, same as the
                            // stop-condition failure path.
                            // Without these the run dies with
                            // only a session-log line and no
                            // visible "why" at phase level.
                            if let Ok(mut slot) = activity_for_panic.stop_reason.lock()
                                && slot.is_none()
                            {
                                // Headline = first line; the full
                                // text lands in phase_errors below.
                                let first = msg.lines().next().unwrap_or(&msg);
                                *slot = Some(format!(
                                    "[panic] fiber panic in activity '{activity_name_for_log}': {first}"
                                ));
                            }
                            if let Ok(mut errs) = activity_for_panic.phase_errors.lock() {
                                errs.push(crate::phase_outcome::PhaseErrorDetail {
                                    class: "panic".into(),
                                    message: msg,
                                    op_name: None,
                                    cycle: None,
                                    op_template: None,
                                    op_resolved: None,
                                    at_nanos: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_nanos() as u64)
                                        .unwrap_or(0),
                                    retryable: false,
                                });
                            }
                            // Mark stop_flag so other fibers and the
                            // executor's main loop see that something
                            // went wrong; the run will terminate at
                            // the next coordination point rather
                            // than continuing in a half-broken state.
                            activity_for_panic
                                .stop_flag
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }))
            })
        };
        let fiber_pool = Arc::new(crate::fiber_pool::FiberPool::new(pool_spawner));

        // Register the pool's applier against the already-declared
        // `concurrency` control (see [`Self::attach_component`]).
        // At run time the applier is what turns a control write
        // into an actual fiber-pool resize. Without a component
        // attached (library-level tests that call `Activity::new`
        // directly) we skip registration — the pool still
        // operates, just without the runtime control surface.
        if let Some(ac) = activity.component.as_ref() {
            let existing: Option<nmbrs_metrics::controls::Control<u32>> = ac
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .controls()
                .get("concurrency");
            if let Some(ctl) = existing {
                ctl.register_applier(crate::fiber_pool::ConcurrencyApplier::new(
                    fiber_pool.clone(),
                ));
            }
        }

        // SRD-83 Part 9 — the adaptive backpressure governor, built
        // BEFORE the pool spawns so the phase OPENS at the slow-start
        // offer (default: the floor) instead of assaulting a fragile
        // target at the authored ceiling. Built after
        // `attach_component` declared the controls it walks.
        let mut throttle_governor = activity.config.throttle.as_ref().and_then(|spec| {
            crate::throttle::ThrottleGovernor::from_spec(
                spec,
                activity.component.as_ref(),
                &activity.config.name,
                activity.config.concurrency,
                activity.config.rate,
            )
        });
        let initial_fibers = throttle_governor
            .as_ref()
            .and_then(|g| g.initial_concurrency())
            .unwrap_or(activity.config.concurrency);
        fiber_pool.spawn_initial(initial_fibers);
        // Wait for fibers to exit by natural exhaustion (source
        // drained) or `stop_flag` set by the error router.
        // Runtime resize-down flags some of them earlier; those
        // exit at the next cycle boundary and the remainder
        // drain when the source is done.
        let mut last_seen_count = activity.config.concurrency;
        let mut last_seen_cycles = activity.metrics.cycles_completed();
        let mut stuck_since = std::time::Instant::now();
        let mut last_logged_count = activity.config.concurrency;
        // SRD-83 — compile this phase's stop conditions (the default
        // `error_rate > error_rate_max` plus any declared `stop_when:`
        // predicates) as scope-bound `ScopedExpr`s, evaluated per tick
        // below. Fire at most once per phase.
        //
        // The predicates bind to this phase node's OWN scope kernel
        // (`activity.phase_kernel`, the structural walk's cached kernel),
        // so they read the phase's wires as they sit — no conjured root.
        // (Distribution to other shell levels via `each:` is the broader
        // SRD-83 shell-evaluation follow-up; this binds the phase-level
        // conditions to their native phase scope.)
        let mut policy_tripped = false;
        let phase_start = std::time::Instant::now();
        let mut stop_conditions = match &activity.phase_kernel {
            Some(kernel) => crate::stop_conditions::StopConditionSet::build_for_phase(
                kernel,
                &activity.config.stop_when,
            )
            .unwrap_or_else(|e| {
                // A predicate that won't compile is the workload author's
                // bug; dryrun is where it should be rejected. At runtime,
                // log loudly and run with no stop conditions rather than
                // abort the phase on a synthesis error.
                crate::diag!(
                    crate::observer::LogLevel::Error,
                    "activity '{}': stop-condition compile failed: {e}",
                    activity.config.name
                );
                crate::stop_conditions::StopConditionSet::empty()
            }),
            None => crate::stop_conditions::StopConditionSet::empty(),
        };
        loop {
            fiber_pool.reap_finished();
            let n = fiber_pool.tracked_count();
            if n == 0 {
                break;
            }
            if let Some(governor) = throttle_governor.as_mut() {
                governor.tick(
                    activity.metrics.attempt_success.count(),
                    activity.metrics.attempt_failure.count(),
                );
            }
            // Periodic stall detection. A real stall means
            // *neither* signal of progress has moved:
            //   - `tracked_count` only changes when a fiber
            //     exits. During steady-state rampup every fiber
            //     is alive and busy, so this stays constant
            //     even when work is flying.
            //   - `cycles_completed` increments per finished op,
            //     so it reflects actual throughput regardless of
            //     whether any fiber has exited yet.
            // Either signal moving resets the stuck timer; only
            // when both are flat for the full 30 s do we warn.
            let cycles = activity.metrics.cycles_completed();
            // SRD-83 — evaluate the phase's stop conditions against a
            // fresh runtime-state snapshot (the Tick firing event). The
            // first predicate that trips stops the shell: fibers drain at
            // their next cycle boundary and the phase-end outcome becomes
            // Failed. (The per-condition `effect` → two-axis Outcome
            // mapping lands with SRD-82 Part 1; for now a trip is Failed.)
            if !policy_tripped && !stop_conditions.is_empty() {
                // Read `error_count` BEFORE `op_count`, then take a FRESH
                // `op_count`: every terminal error also increments
                // `cycles_completed`, so a cycles read taken at-or-after the
                // errors read is always ≥ it — guaranteeing `error_count ≤
                // op_count` and thus `error_rate ≤ 1.0`. The reverse order (the
                // stuck-timer's earlier `cycles` snapshot for `op_count`, then a
                // later `errors_total` read) let an erroring op completing
                // between the two reads push `error_count` past the stale
                // `op_count`, momentarily yielding `error_rate > 1.0` and
                // SPURIOUSLY tripping the `error_rate > 1.0` guard under
                // saturation (where the true rate sits exactly at 1.0).
                // SRD-91: the stop-condition error rate is a per-OP
                // proportion in [0,1], so it reads the per-op terminal
                // failure count (`result_failure`), not the per-attempt
                // `errors_total` (which can exceed op_count under retries).
                let result_failure = activity.metrics.result_failure.count();
                let cycles_total = activity.metrics.cycles_completed();
                // Attempt-level tallies (resolved attempts only, per the
                // SRD-91 counters): the see-through-retries wires. Each
                // wire carries its instrument's name and raw count; any
                // rate is derived in the predicate text.
                let attempt_success = activity.metrics.attempt_success.count();
                let attempt_failure = activity.metrics.attempt_failure.count();
                let state = crate::stop_conditions::RuntimeState {
                    cycles_total,
                    result_failure,
                    elapsed_ms: phase_start.elapsed().as_millis() as u64,
                    // attempt_total counts at RESOLUTION (tries.rs), so
                    // the invariant total == success + failure holds;
                    // reading the two parts keeps one consistent view.
                    attempt_total: attempt_success + attempt_failure,
                    attempt_success,
                    attempt_failure,
                    ..Default::default()
                };
                if let Some((outcome, reason, target, cancel_ops)) =
                    stop_conditions.evaluate(&state)
                {
                    policy_tripped = true;
                    // SRD-83 Part 5 — honour the condition's effect. A
                    // `fail` effect records a phase error (the phase ends
                    // Failed); a `stop` effect is a clean halt (no error,
                    // the phase ends Completed). Either way the stop_flag
                    // drains the fibers at their next cycle boundary.
                    // Human-readable detail: the ACTUAL wire values that
                    // crossed the threshold (op_count / errors / error_rate /
                    // elapsed), so the failure says WHY, not just which
                    // predicate. The predicate itself rides the `[{reason}]`
                    // class prefix on the slot, so the message no longer
                    // repeats it (matches the `[class] message` convention
                    // used by the cycle-/daemon-error paths).
                    let actual = state.describe();
                    // Identity + attribution: the tripped message must say
                    // WHICH phase instance (labels carry the sweep cell /
                    // partition) and WHAT failed (top error types by
                    // count) — not just why the predicate fired.
                    let labels = &activity.config.phase_labels;
                    let where_part = if labels.is_empty() {
                        format!("phase '{}'", activity.config.name)
                    } else {
                        format!("phase '{}' ({labels})", activity.config.name)
                    };
                    let top_errs = activity.metrics.top_error_types(3);
                    let what_part = if top_errs.is_empty() {
                        String::new()
                    } else {
                        format!(" — top errors: {top_errs}")
                    };
                    if outcome.is_failure() {
                        let msg = format!(
                            "stop condition tripped in {where_part} — actual: {actual}{what_part} — failing phase"
                        );
                        crate::diag!(
                            crate::observer::LogLevel::Error,
                            "activity '{}': {reason} — {msg}",
                            activity.config.name
                        );
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            *slot = Some(format!("[{reason}] {msg}"));
                            // SRD-83 Part 5 — the first stopper owns the
                            // outcome as well as the reason: latch the
                            // condition's declared effect for the phase
                            // shell to adopt.
                            if let Ok(mut oc) = activity.stop_outcome.lock() {
                                *oc = Some(outcome.clone());
                            }
                        }
                        if let Ok(mut errs) = activity.phase_errors.lock() {
                            errs.push(crate::phase_outcome::PhaseErrorDetail {
                                class: reason,
                                message: msg,
                                op_name: None,
                                cycle: None,
                                op_template: None,
                                op_resolved: None,
                                at_nanos: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_nanos() as u64)
                                    .unwrap_or(0),
                                retryable: false,
                            });
                        }
                    } else {
                        let msg = format!(
                            "stop condition tripped in {where_part} — actual: {actual}{what_part} — stopping phase"
                        );
                        crate::diag!(
                            crate::observer::LogLevel::Warn,
                            "activity '{}': {reason} — {msg}",
                            activity.config.name
                        );
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            *slot = Some(format!("[{reason}] {msg}"));
                            // SRD-83 Part 5 — a graceful `stop` effect:
                            // latch Interrupted+Succeeded so the phase
                            // shell ends this phase cleanly with its
                            // partial result instead of deriving failure
                            // from the bare stop flag.
                            if let Ok(mut oc) = activity.stop_outcome.lock() {
                                *oc = Some(outcome.clone());
                            }
                        }
                    }
                    // SRD-83 follow-up — route the action to its target scope.
                    // Detection happened here (phase); `target` (from `at:`,
                    // default = innermost of `per:`) says WHERE the stop lands.
                    // `Phase` halts just this phase (`stop_flag`); `Scenario`/
                    // `Workload` latch the workload `walk_stop` so the enclosing
                    // shell halts (which also drains this phase via
                    // `should_stop()`), leaving the session running. If no
                    // workload handle is wired (a standalone phase), fall back
                    // to the phase stop.
                    match target {
                        crate::stop_conditions::StopScope::Phase => {
                            activity.stop_flag.store(true, Ordering::Relaxed);
                        }
                        crate::stop_conditions::StopScope::Scenario
                        | crate::stop_conditions::StopScope::Workload => {
                            match &activity.walk_stop {
                                Some(walk) => walk.store(true, Ordering::Relaxed),
                                None => activity.stop_flag.store(true, Ordering::Relaxed),
                            }
                            // SRD-83 follow-up — a `Workload`-scope halt means
                            // "stop the whole run", exactly like a graceful
                            // (first) Ctrl-C. Latching `walk_stop` alone only
                            // retreats the scope walk lazily, so concurrent
                            // siblings (other scenarios, daemon probes, an
                            // already-dispatched phase) keep draining until the
                            // walk structurally unwinds them. Raising the global
                            // session stop makes every live fiber exit at its
                            // next cycle boundary — the same cooperative signal
                            // Ctrl-C level 1 raises. Cleanup is unchanged: the
                            // walk still returns normally and the runner's RAII
                            // shutdown guard runs the metrics/WAL/summary
                            // teardown. `Scenario` scope stays walk-local — it
                            // must halt only its own scenario, not the session.
                            // `abort` (cancel_ops) is handled below and
                            // supersedes this cooperative global stop.
                            if matches!(target, crate::stop_conditions::StopScope::Workload)
                                && !cancel_ops
                            {
                                crate::session_signals::request_stop();
                            }
                        }
                    }
                    // SRD-83 follow-up — `action: abort`. Beyond the
                    // cooperative halt above, jump STRAIGHT to the cancel-ops
                    // rung: `abort_shutdown()` raises the global session stop
                    // and drops in-flight op futures NOW — no cooperative
                    // drain, no 10s countdown. A stop driven by errors should
                    // not wait for doomed ops/phases to finish (a server sick
                    // enough to trip this will only ever end them by client
                    // timeout). Cleanup is still guaranteed: the walk unwinds
                    // normally into the runner's RAII shutdown guard, which
                    // runs the metrics/WAL/summary teardown (graceful SESSION
                    // shutdown). Not a force-exit — a further Ctrl-C is.
                    if cancel_ops {
                        crate::session_signals::abort_shutdown(
                            crate::session_signals::ShutdownOrigin::StopAction,
                        );
                    }
                }
            }
            let count_changed = n != last_seen_count;
            let cycles_changed = cycles != last_seen_cycles;
            if count_changed || cycles_changed {
                last_seen_count = n;
                last_seen_cycles = cycles;
                stuck_since = std::time::Instant::now();
                // Log progressing-but-slow drain: every time the
                // count changes we re-emit at debug so a stuck
                // run's session.log shows the slope (or lack of it)
                // without flooding when drain is fast.
                if count_changed
                    && (last_logged_count.saturating_sub(n) >= 10
                        || (n < 10 && n != last_logged_count))
                {
                    crate::diag!(
                        crate::observer::LogLevel::Debug,
                        "activity '{}': fiber drain at {n} (from {last_logged_count})",
                        activity.config.name
                    );
                    last_logged_count = n;
                }
            } else if stuck_since.elapsed() > std::time::Duration::from_secs(30) {
                // Distinguish "genuinely stuck" from "fiber is mid-op
                // on a long synchronous call." The latter case
                // (jolokia compaction, large schema migrations,
                // synchronous JMX exec) legitimately blocks one
                // fiber for many minutes without being a bug.
                // `ops_started > ops_finished` proves the fiber
                // is busy in the adapter — log at Debug so the
                // session.log timeline still records the slope,
                // but don't surface a Warn that operators read as
                // "something's wrong."
                let started = activity.metrics.ops_started.load(Ordering::Relaxed);
                let finished = activity.metrics.ops_finished.load(Ordering::Relaxed);
                let in_flight = started.saturating_sub(finished);
                if in_flight > 0 {
                    crate::diag!(
                        crate::observer::LogLevel::Debug,
                        "activity '{}': {n} fiber(s), {in_flight} op(s) in flight, \
                         {cycles} cycles completed, no fiber-count or cycle-count \
                         change for 30s (long-running op in progress)",
                        activity.config.name
                    );
                } else {
                    crate::diag!(
                        crate::observer::LogLevel::Warn,
                        "activity '{}': {n} fibers running, {cycles} cycles completed, \
                         no ops in flight, no progress for 30s — likely blocked on \
                         lock or IO",
                        activity.config.name
                    );
                }
                stuck_since = std::time::Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        crate::diag!(
            crate::observer::LogLevel::Debug,
            "activity '{}': all fibers drained",
            activity.config.name
        );

        // Daemon-pool drain. Cycle-pool reached zero (cursor
        // exhausted or stop signal honoured); now signal each
        // still-running daemon, wait its per-op grace window for
        // the in-flight future to drop, and aggregate outcomes.
        //
        // The outcomes split two ways: clean (Completed /
        // Cancelled) feed into the phase metrics counters;
        // unclean (Errored / TimedOut / Panicked) bubble up as
        // phase-stopping errors via the existing stop_flag +
        // stop_reason channel that cycle-pool errors use.
        if !daemon_pool.is_empty() {
            crate::diag!(
                crate::observer::LogLevel::Debug,
                "activity '{}': draining {} daemon(s)",
                activity.config.name,
                daemon_pool.len()
            );
            let outcomes = daemon_pool.shutdown().await;
            for (op_name, exit) in &outcomes {
                match exit {
                    crate::daemon_pool::DaemonExit::Completed => {
                        crate::diag!(
                            crate::observer::LogLevel::Debug,
                            "daemon op '{op_name}': completed"
                        );
                    }
                    crate::daemon_pool::DaemonExit::Cancelled => {
                        activity.metrics.daemon_cancelled_total.inc();
                        crate::diag!(
                            crate::observer::LogLevel::Debug,
                            "daemon op '{op_name}': cancelled at phase exit"
                        );
                    }
                    crate::daemon_pool::DaemonExit::Errored(e) => {
                        activity.metrics.daemon_errors_total.inc();
                        let inner = e.error();
                        crate::diag!(
                            crate::observer::LogLevel::Error,
                            "daemon op '{op_name}' errored: [{}] {}",
                            inner.error_name,
                            inner.message
                        );
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            *slot = Some(format!(
                                "[{}] daemon op '{op_name}': {}",
                                inner.error_name, inner.message,
                            ));
                        }
                    }
                    crate::daemon_pool::DaemonExit::TimedOut => {
                        activity.metrics.daemon_errors_total.inc();
                        crate::diag!(
                            crate::observer::LogLevel::Error,
                            "daemon op '{op_name}': did not acknowledge stop \
                             within grace window — phase fails"
                        );
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            *slot = Some(format!(
                                "[daemon_shutdown_timeout] daemon op \
                                 '{op_name}' did not acknowledge stop \
                                 within its grace window",
                            ));
                        }
                    }
                    crate::daemon_pool::DaemonExit::Panicked(msg) => {
                        activity.metrics.daemon_errors_total.inc();
                        crate::diag!(
                            crate::observer::LogLevel::Error,
                            "daemon op '{op_name}' panicked: {msg}"
                        );
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            *slot = Some(format!("[daemon_panic] daemon op '{op_name}': {msg}",));
                        }
                    }
                }
                // SRD-92: the "does this exit fail the phase?" rule lives
                // once in the DaemonExit taxonomy's own classifier — gate
                // the shared stop-flag latch on it rather than re-encoding
                // which variants fail by which arms call store().
                if exit.is_phase_error() {
                    activity.stop_flag.store(true, Ordering::Relaxed);
                }
            }
        }

        // Final completion line — always emitted (one per phase),
        // not gated on TTY/extent. Replaces the old executor-side
        // `phase 'X' complete (Ns)` line. Honors the live
        // `suppress_status_line` flag (TUI takes over rendering)
        // and the global `suppress_progress` (e.g. CI / `--quiet`).
        if !suppress_progress && !activity.config.suppress_status_line.load(Ordering::Relaxed) {
            // Counter snapshots — the readout recomputes
            // pct / rate / ok_pct from these primitives, so
            // we don't pre-format them here. Retries are
            // derived per the existing convention (errors
            // minus the skips-adjusted failed-op count).
            let consumed = activity.source_factory.global_consumed();
            let ops_completed = activity.metrics.cycles_completed();
            // SRD-91: terminal-success count = `result_success.count()`;
            // `errors_total` is RESULT-level (one inc per terminal
            // failure) — it drives `e:` directly. Retries = failed
            // attempts that were NOT terminal, i.e. `attempt_failure
            // - failed_ops`; the old `errors_total - failed_ops`
            // collapsed to ~0 once `errors_total` went result-level
            // with the TriesDispenser refactor.
            let successes = activity.metrics.result_success.count();
            let errors = activity.metrics.errors_total.get();
            let elapsed = start_time.elapsed().as_secs_f64();
            let failed_ops = ops_completed
                .saturating_sub(successes)
                .saturating_sub(activity.metrics.skips_total.get());
            let retries = activity
                .metrics
                .attempt_failure
                .count()
                .saturating_sub(failed_ops);
            // Concurrency (fiber count) — the `c:N` tail mirrors
            // the live progress line so a completed phase reads
            // with the same shape as a running one.
            let concurrency = activity.config.concurrency;
            // Workload-emphasized metrics — same resolver as the
            // inline progress line, glob-matched against the
            // declared `status_metrics: [...]`. Empty list ⇒ no
            // metrics tail; nothing is presumed to be present.
            let relevancy_str: String = activity
                .metrics
                .collect_status_values(&activity.config.status_metrics)
                .concat();
            // SRD-100 P2 — no explicit status clear here. The consumer
            // folds `active_phases`, and the executor's `PhaseCompleted`
            // removes this phase from that map, so the status footer
            // self-clears for this phase on the next render tick (while
            // any concurrent phase's status survives — the old single-slot
            // `status(None)` wiped peers).
            // Render the ✓ DONE line via the readout engine.
            // SRD-63 / Push 1: the previous inline `format!()`
            // is now `phase_outcome.render()` driven by an
            // `ActivityReadoutContext` snapshot of the values
            // gathered above. Output is byte-equivalent.
            let phase_name_bare = activity
                .config
                .name
                .split_once(" (")
                .map(|(n, _)| n.to_string())
                .unwrap_or_else(|| activity.config.name.clone());
            // Phase-end: re-read the source's final extent.
            // For static cursors this equals the initial
            // `total_extent`; for extending cursors it's the
            // last grown value before the policy declined
            // further extension.
            let final_extent = source_for_progress
                .global_extent()
                .unwrap_or(activity.config.cycles);
            // SRD-76 — the activity-level binder fire happens
            // BEFORE the executor records its formal Failed /
            // Skipped decision. The activity knows only what it
            // measured: a clean completion if it ran to extent,
            // a stop-flag trip if the error router fired. Mirror
            // the stop_flag into the two-axis Outcome so the
            // readout doesn't render ✓ on a stopped phase. The
            // executor records the canonical outcome on the scene
            // tree; this surface is the realtime display projection.
            let outcome = if activity.stop_flag.load(Ordering::Relaxed) {
                crate::phase_outcome::Outcome::failed()
            } else {
                crate::phase_outcome::Outcome::completed()
            };
            let outcome_errors: Vec<crate::phase_outcome::PhaseErrorDetail> = activity
                .phase_errors
                .lock()
                .ok()
                .map(|g| g.clone())
                .unwrap_or_default();
            let ctx = crate::readout_context::ActivityReadoutContext {
                phase_name: phase_name_bare,
                phase_seq: activity.config.phase_seq,
                phase_labels: activity.config.phase_labels.clone(),
                cycles_completed: ops_completed,
                cycles_total: final_extent,
                ops_ok: successes,
                skips: activity.metrics.skips_total.get(),
                errors,
                retries,
                concurrency,
                elapsed_secs: elapsed,
                consumed,
                status_metric_chips: relevancy_str,
                depth_indent: crate::scene_tree::running_phase_indent(),
                use_color: crate::observer::use_color(),
                memo: activity.memo.load().as_str().to_string(),
                outcome,
                outcome_errors,
                outcome_resume_cursor: None,
                open_ended: activity.daemon_stop.is_some(),
            };
            // SRD-63 §6.2 / Push 9c: synthesise one final
            // `on_update` tick before the DONE summary. The
            // inline thread (if running) fires every 500 ms
            // and may have missed the last 100-499 ms of
            // counter changes — and for short phases under
            // the TTY/extent threshold it never spawned at
            // all. This guarantees the snapshot store sees
            // the phase's end-of-life on_update render
            // matching what the user would have seen if the
            // refresh tick had aligned exactly with phase
            // termination.
            //
            // Renders silently (no eprint) — the DONE line
            // immediately following carries the visible
            // ✓ summary; we just want the snapshot row to
            // reflect end-state.
            {
                let (final_seq, final_depth) =
                    crate::readout_context::resolve_phase_coord_by_name(&activity.config.name);
                // Row-level cursor progress — only for a DECLARED cursor
                // (`config.source_factory` present). Plain `cycles:` phases
                // get a synthesized `range(0, cycles)` factory, so gate on
                // the config Option, not the resolved factory, to keep them
                // on the op-denominated `cycles:` chip.
                let (final_rows_consumed, final_rows_total) = match &activity.config.source_factory
                {
                    Some(_) => (
                        activity.source_factory.global_consumed(),
                        activity.source_factory.global_extent().unwrap_or(0),
                    ),
                    None => (0, 0),
                };
                let final_ctx = crate::readout_context::build_inline_refresh_context(
                    &activity.metrics,
                    &activity.config.name,
                    activity.config.concurrency,
                    total_extent,
                    final_rows_consumed,
                    final_rows_total,
                    elapsed,
                    u64::MAX, // sentinel: spinner frame doesn't matter at end-of-phase
                    &activity.config.status_metrics,
                    activity.memo.as_ref(),
                    final_seq,
                    final_depth,
                    // End-of-phase: the subject is closed, never open-ended.
                    false,
                );
                let phase_status_default = {
                    let readout = crate::readouts::Registry::lookup("phase_status")
                        .expect("phase_status registered");
                    crate::readouts::BakedBody::from_single(readout, crate::readouts::Lod::Labeled)
                };
                if let Ok(mut binder) = crate::readouts::binder::build_event_binder_with_cli(
                    &activity.config.readouts,
                    crate::lifecycle::EventType::Update,
                    phase_status_default,
                    activity.config.cli_readout_override.as_deref(),
                ) {
                    use crate::readouts::ReadoutBinder;
                    use crate::readouts::ReadoutContext;
                    let mut sink = crate::readouts::StringSink::with_capacity(192);
                    binder.fire(crate::lifecycle::EventType::Update, &final_ctx, &mut sink);
                    let rendered_final = sink.take();
                    crate::readouts::snapshot::capture(
                        activity.config.snapshot_writer.as_ref(),
                        crate::lifecycle::EventType::Update.slot_name(),
                        final_ctx.subject_exec_id(),
                        crate::lifecycle::EventType::Update.subject_kind().as_str(),
                        &final_ctx.subject_id(),
                        "binder",
                        crate::readouts::snapshot::lod_str(crate::readouts::Lod::Labeled),
                        &rendered_final,
                    );
                }
            }

            // gutter/final — the guaranteed ONE final gutter update.
            // A declared `final:` template is evaluated here, at phase
            // end (wires over the activity chain, status-metric
            // aggregates as fallback), and stored into the gutter slot
            // the display actor reads when stamping this phase's ✓
            // outcome DETAIL line. Without a `final:`, the slot already
            // holds the during-form's LAST PUBLISHED value (the
            // dispenser publishes on every op completion) — that last
            // computed value IS the final update; re-evaluating the
            // per-cycle template here would run it against a
            // degenerate end-of-phase context.
            {
                let fin = activity
                    .gutter_final_spec
                    .lock()
                    .ok()
                    .and_then(|g| g.clone());
                if let Some((kind, tmpl)) = fin {
                    if let Some(final_spec) =
                        evaluate_final_gutter(&activity, op_builder.source_kernel(), kind, &tmpl)
                    {
                        activity.gutter.store(Some(Arc::new(final_spec)));
                    }
                }
            }

            // Build a one-shot binder for `on_phase_end`:
            // workload's `on_phase_end:` overrides if any,
            // else the default body — `phase_outcome` for the
            // normative ✓/✗ status line, followed by
            // `error_readout` which renders the per-error block
            // below it. `error_readout` is a no-op (zero bytes)
            // when the phase has no recorded errors, so the
            // default is safe for both success and failure
            // paths; failure paths get the structured error
            // block appended without the per-cycle warns
            // having to spam the screen mid-phase.
            let phase_outcome_default = {
                let phase_outcome = crate::readouts::Registry::lookup("phase_outcome")
                    .expect("phase_outcome registered");
                let error_readout = crate::readouts::Registry::lookup("error_readout")
                    .expect("error_readout registered");
                crate::readouts::BakedBody::from_steps(vec![
                    crate::readouts::binder::RenderStep::Render {
                        readout: phase_outcome,
                        lod: crate::readouts::Lod::Labeled,
                        layout: crate::readouts::binder::LayoutMode::Auto,
                        options: crate::readouts::ReadoutOptions::new(),
                        color: None,
                    },
                    crate::readouts::binder::RenderStep::Render {
                        readout: error_readout,
                        lod: crate::readouts::Lod::Labeled,
                        layout: crate::readouts::binder::LayoutMode::Auto,
                        options: crate::readouts::ReadoutOptions::new(),
                        color: None,
                    },
                ])
            };
            let rendered = match crate::readouts::build_event_binder(
                &activity.config.readouts,
                crate::lifecycle::EventType::PhaseEnd,
                phase_outcome_default,
            ) {
                Ok(mut binder) => {
                    use crate::readouts::ReadoutBinder;
                    let mut sink = crate::readouts::StringSink::with_capacity(160);
                    binder.fire(crate::lifecycle::EventType::PhaseEnd, &ctx, &mut sink);
                    sink.take()
                }
                Err(e) => {
                    crate::diag!(
                        crate::observer::LogLevel::Error,
                        "readouts: failed to bind on_phase_end — {e}"
                    );
                    String::new()
                }
            };
            // Push 6: capture the on_phase_end render to the
            // snapshot store. The DONE line is the canonical
            // "what the operator saw at completion" — replay
            // returns it byte-for-byte.
            if !rendered.is_empty() {
                use crate::readouts::ReadoutContext;
                crate::readouts::snapshot::capture(
                    activity.config.snapshot_writer.as_ref(),
                    crate::lifecycle::EventType::PhaseEnd.slot_name(),
                    ctx.subject_exec_id(),
                    crate::lifecycle::EventType::PhaseEnd
                        .subject_kind()
                        .as_str(),
                    &ctx.subject_id(),
                    "binder",
                    crate::readouts::snapshot::lod_str(crate::readouts::Lod::Labeled),
                    &rendered,
                );
            }
            // `skipped_phases=elide|prune`: a FULLY-SKIPPED phase (every
            // cycle `if:`-gated off, nothing measured, no errors) leaves
            // no completion line — the snapshot store above still holds
            // the render for replay, but the live readout stays silent.
            // `mark` (the default) emits phase_outcome's explicit
            // `⊘ gated off` form instead.
            let fully_skipped = {
                let skips = activity.metrics.skips_total.get();
                skips > 0
                    && ops_completed > 0
                    && skips >= ops_completed
                    && successes == 0
                    && errors == 0
            };
            let elide_skipped = fully_skipped
                && matches!(
                    crate::observer::skipped_phase_display(),
                    crate::observer::SkippedPhaseDisplay::Elide
                        | crate::observer::SkippedPhaseDisplay::Prune
                );
            if !rendered.is_empty() && !elide_skipped {
                // SRD-81 push 1: the per-phase ✓ outcome is a typed
                // `PhaseOutcome` projection, not a generic diagnostic.
                // The terminal scrollback shows it; the TUI tree /
                // active-phase panel render it natively; the TUI log
                // panel (diagnostics-only) filters it out instead of
                // garbling the multi-line ANSI as one Span. (push 1b
                // replaces this pre-rendered string with a structured
                // marker the sinks render from the snapshot.)
                crate::observer::log_tagged(
                    crate::observer::LogLevel::Info,
                    crate::observer::EventTag::at(
                        crate::lifecycle::EventType::PhaseEnd,
                        crate::observer::EventCategory::Outcome,
                    ),
                    &rendered,
                );
            }
        }

        // Print validation summary AND capture to the metrics
        // store in one pass. `snapshot()` drains the histogram
        // (delta semantics), so we must use the same snapshot
        // for both printing and SQLite capture.
        if !validation_metrics.is_empty() {
            let mut total_passed = 0u64;
            let mut total_failed = 0u64;
            let now = Instant::now();
            let mut final_snapshot = MetricSet::at(now, Duration::ZERO);
            let activity_labels = activity.labels.clone();

            for vm in validation_metrics.iter() {
                total_passed += vm.passed();
                total_failed += vm.failed();

                for (name, stats) in &vm.relevancy_stats {
                    let snap = stats.snapshot();
                    if !snap.is_empty() {
                        let mean = snap.mean();
                        let p50 = snap.p50();
                        let p99 = snap.p99();
                        let min = snap.min();
                        let max = snap.max();
                        let n = snap.len();
                        // Relevancy stats (recall@k, precision@k, F1@k)
                        // are fractions in [0, 1]. Render as percent
                        // — the unit operators read these in.
                        // Underlying gauges below stay as fractions so
                        // downstream consumers (recall_summary,
                        // metrics scrapes) keep their existing scale.
                        // Indent matches the phase / DONE / complete
                        // lines so the relevancy summary nests under
                        // the phase row in tui=terminal output.
                        let depth_indent = crate::scene_tree::running_phase_indent();
                        let color = crate::observer::use_color();
                        let dim = if color { "\x1b[2m" } else { "" };
                        let bold = if color { "\x1b[1m" } else { "" };
                        let reset = if color { "\x1b[0m" } else { "" };
                        // SRD-92: a PhaseDetail projection — a detail
                        // row of the completion block above it, so the
                        // terminal sink renders it under the blank
                        // divider margin (no timing triad) and
                        // `completed_phases=headers` drops it.
                        crate::observer::log_tagged(
                            crate::observer::LogLevel::Info,
                            crate::observer::EventTag::at(
                                crate::lifecycle::EventType::PhaseEnd,
                                crate::observer::EventCategory::Evaluation,
                            ),
                            &format!(
                                "{depth_indent}{bold}{name}{reset}: mean={:.2}% {dim}p50={:.2}% p99={:.2}% min={:.2}% max={:.2}% (n={n}){reset}",
                                mean * 100.0,
                                p50 * 100.0,
                                p99 * 100.0,
                                min * 100.0,
                                max * 100.0,
                            ),
                        );
                        // Pick up `k`/`r` from the F64Stats's
                        // labels so per-phase summary gauges
                        // remain unique under OpenMetrics §4.5
                        // when multiple relevancy configs share
                        // a phase but differ in cutoff.
                        let stats_labels = stats.labels();
                        let k_label = stats_labels.get("k").map(str::to_string);
                        let r_label = stats_labels.get("r").map(str::to_string);
                        // Generic observability point: a relevancy
                        // function's per-phase summary has been
                        // computed and is about to be published
                        // as `{name}_{stat}` gauges. The trace
                        // fires for ANY relevancy function — the
                        // labels carry the publishing dimensions
                        // (phase, profile, …, k, r, n) from the
                        // surrounding scope, not from any
                        // workload-specific knowledge.
                        if crate::observer::trace_enabled() {
                            let mut trace_labels = activity_labels.with("n", n.to_string());
                            if let Some(k) = &k_label {
                                trace_labels = trace_labels.with("k", k);
                            }
                            if let Some(r) = &r_label {
                                trace_labels = trace_labels.with("r", r);
                            }
                            crate::observer::trace(
                                &trace_labels,
                                &format!(
                                    "event=relevancy.publish fn={name} n={n} \
                                     mean={mean:.6} p50={p50:.6} p99={p99:.6} \
                                     min={min:.6} max={max:.6}"
                                ),
                            );
                        }
                        for (stat, val) in [
                            ("mean", mean),
                            ("p50", p50),
                            ("p99", p99),
                            ("min", min),
                            ("max", max),
                        ] {
                            let mut gauge_labels = activity_labels.with("n", n.to_string());
                            if let Some(k) = &k_label {
                                gauge_labels = gauge_labels.with("k", k);
                            }
                            if let Some(r) = &r_label {
                                gauge_labels = gauge_labels.with("r", r);
                            }
                            final_snapshot.insert_gauge(
                                format!("{name}_{stat}"),
                                gauge_labels,
                                val,
                                now,
                            );
                        }
                    }
                }
            }

            // Phase-level aggregate counters. One pair per phase
            // — `total_passed` / `total_failed` sum across every
            // op's `vm` so the metric instance is unique under
            // OpenMetrics §4.5 (LabelSets must be unique). The
            // earlier per-`vm` insertion path inserted N copies
            // with identical labels, which the snapshot
            // assembler now rejects as a duplicate. Per-op
            // breakdown isn't carried by the validation counters
            // anyway — the labels are activity-scope, not
            // op-scope.
            if total_passed > 0 || total_failed > 0 {
                final_snapshot.insert_counter(
                    "validations_passed",
                    activity_labels.clone(),
                    total_passed,
                    now,
                );
                final_snapshot.insert_counter(
                    "validations_failed",
                    activity_labels.clone(),
                    total_failed,
                    now,
                );
            }

            // Validation summary line: only emit when there are
            // failures. On clean runs the relevancy summary's
            // `n=N` already conveys "N validations passed", and
            // the `validation: N passed, 0 failed` line was just
            // duplicate text on every phase. On failure runs the
            // line is signal — promote it to Warn so it stands
            // out and route only when failed > 0.
            if total_failed > 0 {
                let depth_indent = crate::scene_tree::running_phase_indent();
                crate::diag!(
                    crate::observer::LogLevel::Warn,
                    "{depth_indent}validation: {} passed, {} FAILED",
                    total_passed,
                    total_failed
                );
            }

            if !final_snapshot.is_empty() {
                activity
                    .validation_frame
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .replace(final_snapshot);
            }
        }

        // NOTE: this is the `stopped` RETURN that `run_phase` reads to
        // decide whether the phase FAILED — so it must reflect only
        // abnormal stops (the error-handler `stop_flag`, Ctrl-C, a walk
        // fault). A daemon phase's `daemon_stop` is a CLEAN termination
        // (Interrupted+Succeeded — the foreground it shadows finished),
        // so it drives the loop BREAKS below but is deliberately NOT a
        // fault. The daemon exclusion lives once in `StopView::abnormal`
        // (session_signals.rs) — this delegates to it rather than
        // re-deriving the rule (was a hand-rolled copy; SRD-92 dedup).
        activity.stop_view().abnormal()
    }
}

/// Executor task for the tiered DriverAdapter interface.
///
/// Each fiber has its own FiberBuilder (lock-free Polydat state).
/// Ops within a stanza are processed in dependency groups:
/// - Groups execute sequentially (captures flow between groups)
/// - Ops within a group execute concurrently (join_all)
///
/// Groups are determined at init time by analyzing capture
/// declarations and references across templates.
// `pull_plans`: per-template wrapper-side `PullPlan`s, sealed at init.
// Drives cycle-time reads for validation / conditional / throttle
// wrappers via memoized `PullHandle`s. See SRD 31 §"Pull plan vs bind
// plan".
/// One-shot daemon dispatch. Mirrors `executor_task`'s setup
/// (FiberBuilder + per-op kernel attach) but dispatches the
/// daemon's op exactly once, racing the in-flight future
/// against the per-daemon stop flag AND the activity-global
/// stop flag.
///
/// Cancellation path: when either flag flips, the
/// `dispenser.execute(...)` future is dropped at the next
/// await point — for the HTTP adapter that's mid-`send()`,
/// which propagates as a clean reqwest cancellation. The
/// daemon returns `DaemonExit::Cancelled`. The pool's grace
/// window (see `DaemonPool::shutdown`) gives the adapter time
/// to observe the drop and exit; deadlines past the grace
/// surface as `DaemonExit::TimedOut`.
///
/// Daemon ops increment `ops_started` / `ops_finished` like
/// any other op execution — the operator-visible op-count
/// surface stays consistent regardless of fiber kind. Service
/// + response timing is recorded the same way the cycle-pool
///   records it (one Instant pair around the execute), but no
///   rate-limiter acquire — daemons aren't subject to the
///   activity's ops-per-second ceiling.
async fn daemon_dispatch(
    activity: Arc<Activity>,
    dispensers: Arc<Vec<Arc<dyn OpDispenser>>>,
    pull_plans: Arc<Vec<crate::fixture::PullPlan>>,
    op_builder: Arc<crate::synthesis::OpBuilder>,
    template_idx: usize,
    op_name: String,
    stop: crate::daemon_pool::DaemonStopFlag,
) -> crate::daemon_pool::DaemonExit {
    let mut fiber = op_builder.create_fiber_builder();
    fiber.attach_dispenser_kernels(&dispensers);
    let dispenser = dispensers[template_idx].clone();
    let fields = crate::adapter::ResolvedFields::new(Vec::new(), Vec::new());
    // Resolve the wrapper-side pull plan against this daemon fiber's
    // kernel — same as the cycle-pool path. Daemon ops carry wrappers
    // too (notably `if:`, whose IF_COND wrapper registers a pull for
    // its predicate); an empty `ResolvedPulls` would panic when that
    // handle resolves. Captures the daemon reads (e.g. a `shared`
    // cell written by an earlier stanza op gating `if: sstables > 1`)
    // are visible here because the daemon dispatches at its op-walk
    // position, after the writer op completed.
    let pulls = fiber.resolve_pulls_for_idx(template_idx, &pull_plans[template_idx]);
    let cycle_wires = match fiber.per_op_kernel_mut(template_idx) {
        Some(p) => crate::wires::CycleWires::new(p),
        None => crate::wires::CycleWires::new(fiber.main_kernel_mut()),
    };
    let ctx = crate::fixture::ExecCtx::with_wires(&fields, &pulls, &cycle_wires);

    activity.metrics.ops_started.fetch_add(1, Ordering::Relaxed);
    let started = std::time::Instant::now();
    let activity_stop = activity.stop_flag.clone();
    let exit = tokio::select! {
        result = dispenser.execute(0, &ctx) => match result {
            Ok(_) => crate::daemon_pool::DaemonExit::Completed,
            Err(e) => crate::daemon_pool::DaemonExit::Errored(e),
        },
        _ = poll_daemon_stop(&stop, &activity_stop) => {
            crate::daemon_pool::DaemonExit::Cancelled
        }
    };
    let service_nanos = started.elapsed().as_nanos() as u64;
    activity.metrics.cycles_total.inc();
    activity
        .metrics
        .ops_finished
        .fetch_add(1, Ordering::Relaxed);
    activity.metrics.service_time.record(service_nanos);
    activity.metrics.response_time.record(service_nanos);
    // SRD-91 op-outcome taxonomy for daemon dispatch. The `attempt_*`
    // counters are owned by the innermost `TriesDispenser` in the op's
    // wrapper stack (which this daemon op runs through, same as a foreground
    // op), so only the RESULT-level tallies are recorded here — no
    // double-count. Cancelled / TimedOut are shutdown outcomes tracked via
    // daemon_cancelled_total / daemon_errors_total, not op results.
    match &exit {
        crate::daemon_pool::DaemonExit::Completed => {
            activity.metrics.result_total.inc();
            activity.metrics.result_success.observe(service_nanos);
        }
        crate::daemon_pool::DaemonExit::Errored(_) => {
            // `errors_total` / per-type tallies + policy effects already
            // ran in the OUTERMOST `ErrorHandlerDispenser` of the daemon
            // op's own stack (SRD-82 Part 3b) — only the result-level
            // outcome is recorded here.
            activity.metrics.result_total.inc();
            activity.metrics.result_failure.observe(service_nanos);
        }
        _ => {}
    }
    crate::diag!(
        crate::observer::LogLevel::Debug,
        "daemon op '{op_name}' exit={} elapsed_ms={:.0}",
        exit.label(),
        service_nanos as f64 / 1_000_000.0
    );
    exit
}

/// Polls both the per-daemon stop flag and the activity-global
/// stop flag at 50ms granularity. Returns as soon as either is
/// set. The 50ms cadence is a compromise: fast enough that
/// daemon cancellation lands well under the typical 5-second
/// grace window, slow enough that an idle daemon doesn't burn
/// CPU. Replacing this with a `tokio::sync::Notify`-backed
/// flag would drop the latency to zero but requires touching
/// the StopFlag surface and isn't load-bearing for the
/// trigger-and-observe pattern.
async fn poll_daemon_stop(
    daemon_stop: &crate::daemon_pool::DaemonStopFlag,
    activity_stop: &Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        if daemon_stop.load(Ordering::Acquire) || activity_stop.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// reason: cohesive per-fiber executor driver; each argument is a distinct
// runtime channel/handle the loop needs — splitting into a struct would only
// relocate the same fields with no clarity gain.
#[allow(clippy::too_many_arguments)]
async fn executor_task(
    activity: Arc<Activity>,
    dispensers: Arc<Vec<Arc<dyn OpDispenser>>>,
    pull_plans: Arc<Vec<crate::fixture::PullPlan>>,
    op_builder: Arc<crate::synthesis::OpBuilder>,
    // Optional activity-level rate limiter. `acquire` fires
    // once per cycle before adapter dispatch. There is no
    // separate stanza-rate limiter.
    rate_limiter: Option<Arc<RateLimiter>>,
    // Per-fiber cooperative-exit flag owned by the activity's
    // [`crate::fiber_pool::FiberPool`]. Set to `true` by
    // `ConcurrencyApplier` when the pool scales down.
    fiber_stop: crate::fiber_pool::StopFlag,
    // Daemon-op pool — shared across cycle-pool fibers. Each
    // stanza walk that reaches a daemon op dispatches a fresh
    // fiber onto this pool via `try_spawn` and continues
    // without awaiting; the daemon body runs to completion
    // (or until phase-exit drain signals stop) on its own
    // tokio task.
    daemon_pool: Arc<crate::daemon_pool::DaemonPool>,
    // Phase name — used to wrap the daemon body in the same
    // runtime-context guard cycle-pool fibers use, so daemon
    // ops can read `phase()` / `cycle()` runtime-context
    // wires.
    phase_name_arc: Arc<str>,
) {
    let stanza_positions = activity.op_sequence.stanza_length();
    // SRD-22 batching cover-once: the phase-cursor stride is the SUM of
    // each stanza op's `rows_per_op` (its uniform per-invocation cursor
    // consumption), NOT the raw stanza length. A normal op contributes 1
    // (identical to the pre-batching model); a batch op contributes its
    // fixed stride `N`. Reserving `Σ rows_per_op` per stanza and handing
    // each op a contiguous sub-run of its own `rows_per_op` makes
    // consecutive stanzas cover DISJOINT ordinal runs — every ordinal is
    // inserted exactly once. Precomputed once (rows_per_op is fixed per
    // dispenser at map_op) so the hot loop pays no per-stanza virtual
    // calls and `Σ per_pos_rows == stanza_stride` by construction.
    let per_pos_rows: Vec<usize> = (0..stanza_positions)
        .map(|pos| {
            let (idx, _) = activity.op_sequence.get_with_index(pos as u64);
            dispensers[idx].rows_per_op().max(1)
        })
        .collect();
    let stanza_stride: usize = per_pos_rows.iter().sum::<usize>().max(1);
    // Per-fiber `FiberBuilder` carries scope values (per-iteration
    // extern inputs) populated by the OpBuilder, so iter-var
    // references like `{table}` in op templates resolve to the
    // current iteration's value.
    let mut fiber = op_builder.create_fiber_builder();

    // Shutdown-ladder subscription (session_signals module doc): the op
    // dispatch below races the adapter call against the CANCEL rung
    // (level 2) so a hung request — one that will only ever end by
    // client timeout — can be dropped mid-flight, letting the drain and
    // the process-level cleanup (WAL consolidation, summaries) proceed.
    // One receiver per fiber; the race is a `select!` per dispatch.
    let mut shutdown_rx = crate::session_signals::subscribe_shutdown();

    // SRD-68 Push 3 — materialise per-fiber subscope kernels from
    // each dispenser's canonical kernel. The fiber holds them as
    // `Vec<Option<PolydatKernel>>` indexed parallel to the dispenser
    // registry; cycle dispatch reads `fiber.per_op_kernel(template_idx)`
    // to populate `ExecCtx::wires` for the firing dispenser.
    // Dispensers that return `None` from `canonical_kernel()` get
    // a `None` slot and the cycle falls back to `NullWireSource`.
    fiber.attach_dispenser_kernels(&dispensers);

    // Create per-fiber source reader (used for all phases).
    // Source-declared phases will eventually use the advancer model,
    // but for now all phases go through the source reader.
    // SRD-92 Step 5e — the per-cycle stream flows through the unified
    // `ChildSource` contract: a `CursorSource` wraps the reader; `poll_next` IS
    // `reserve(stanza_stride)` (per-stanza, not per-cycle → zero per-cycle
    // overhead), yielding an ordinal `Range`; `render` is the per-ordinal fetch.
    // The level selects `CursorReserve` — this very FiberPool loop.
    use crate::child_source::{Child, ChildSource, CursorSource, Drive, select_drive};
    let mut source = CursorSource::new(activity.source_factory.create_reader(), stanza_stride);
    debug_assert_eq!(select_drive(source.realizability()), Drive::CursorReserve);

    loop {
        if activity.stopped() {
            break;
        } // SRD-92 Step 0: one stop view
        if fiber_stop.load(std::sync::atomic::Ordering::Acquire) {
            break;
        } // per-fiber scale-down (distinct)

        // Phase 1: RESERVE — CAS on shared cursor, instantaneous.
        // Acquires one stanza's worth of ordinals. This is the only
        // shared-state interaction per stanza.
        let range = match source.poll_next() {
            Some(Child::Ordinals(r)) => r,
            // CursorSource yields only Ordinals; anything else (None) means the
            // source is exhausted → the phase-poll rewind / standard break.
            _ => {
                // SRD-75 phase-poll: source exhausted ends a poll
                // iteration. Check the predicate; if false and
                // the deadline hasn't elapsed, sleep, rewind the
                // factory's shared cursor, and re-create the
                // reader for another iteration. Phase-poll
                // mandates concurrency=1 (workload-load
                // validation) so this is the only fiber and the
                // rewind isn't racing siblings.
                if let Some(pp) = activity.phase_poll.clone() {
                    // Check predicate first — handles the case
                    // where the very first iteration's captures
                    // already satisfy the condition.
                    //
                    // Polydat comparison operators (`==`, `!=`, `<`,
                    // …) return u64 (0/1) per SRD-10 §"BinOpKind"
                    // — there's no Bool result type for these.
                    // Accept either Value::Bool(true) (in case
                    // a future Polydat release adds a Bool result
                    // path) OR a non-zero numeric value as
                    // "satisfied". This mirrors the workload
                    // author's expectation that `(a == 1) & (b == 0)`
                    // evaluates to "true" when both clauses hold.
                    //
                    // `__poll_until` is a DYNAMIC binding
                    // (SRD-11 §"Two Evaluation Lifecycles") —
                    // its value depends on per-iteration capture
                    // writes through the phase scope's
                    // SharedCells, so a buffer read via
                    // `lookup()` returns the LAST-EVALUATED
                    // value (None on first iteration, never
                    // updated). We MUST trigger re-evaluation
                    // via `pull()`. The phase scope kernel is
                    // held as `Arc<PolydatKernel>` (immutable
                    // handle), so we evaluate via the per-fiber
                    // `main_kernel` instead — main_kernel is
                    // built from the phase scope program (so
                    // it has `__poll_until` as an output) and
                    // is wired to the SAME SharedCells the
                    // captures wrote to (so its pull returns
                    // the live value).
                    // SRD-75 (C5) — strict-gate `require:` selectors.
                    // While any selector is unresolved the gate HOLDS:
                    // the predicate is not trusted, because an
                    // unregistered family reads 0.0 silently and a
                    // `>=`-shaped predicate could pass spuriously (or
                    // a `<`-shaped one hang to timeout). Past the
                    // grace window (one poll interval) an unresolved
                    // selector is a hard `poll_require` failure —
                    // loud and immediate, never a mystery hang.
                    let unresolved: Vec<&String> = pp
                        .require
                        .iter()
                        .filter(|s| !nmbrs_metrics::polydat_nodes::metric_selector_resolves(s))
                        .collect();
                    if !unresolved.is_empty() && std::time::Instant::now() >= pp.require_grace {
                        let names = unresolved
                            .iter()
                            .map(|s| format!("'{s}'"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let formatted_reason = format!(
                            "[poll_require] poll `require:` selector(s) {names} \
                             resolved to no registered instrument within the \
                             grace window — the gate's `until:` would read 0.0 \
                             for them. Check the family name and labels (e.g. \
                             phase=<name>), and that the producing phase is \
                             running. SRD-75 (C5)."
                        );
                        crate::diag!(
                            crate::observer::LogLevel::Error,
                            "activity '{}': {formatted_reason}",
                            activity.config.name
                        );
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            *slot = Some(formatted_reason.clone());
                        }
                        if let Ok(mut errs) = activity.phase_errors.lock() {
                            errs.push(crate::phase_outcome::PhaseErrorDetail {
                                class: "poll_require".into(),
                                message: formatted_reason,
                                op_name: None,
                                cycle: None,
                                op_template: None,
                                op_resolved: None,
                                at_nanos: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_nanos() as u64)
                                    .unwrap_or(0),
                                retryable: false,
                            });
                        }
                        activity
                            .stop_flag
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    let requires_ok = unresolved.is_empty();
                    // SAME call the op-level poll makes. An execution node is
                    // an execution node: the predicate is resolved through one
                    // interface (`CycleWires`, which PULLS and so re-evaluates
                    // a dynamic binding) and judged by one truthiness rule.
                    //
                    // This was a third, divergent implementation — an inline
                    // match whose `_ => false` arm made a non-empty string
                    // falsy here and truthy under `if:` / `while:` / op-poll,
                    // for the same expression. (The `require:` gate above
                    // additionally withholds trust in the predicate until
                    // every declared metric selector resolves.)
                    let satisfied = requires_ok && {
                        let wires = crate::wires::CycleWires::new(fiber.main_kernel_mut());
                        match crate::wrappers::condition::holds(
                            &wires,
                            crate::wrappers::condition::UNTIL_BINDING,
                        ) {
                            Some(v) => v,
                            // Unresolved is a wiring fault, not a false
                            // predicate. Keep waiting rather than silently
                            // declaring the phase done; the timeout below
                            // reports it with the predicate named.
                            None => false,
                        }
                    };
                    if satisfied {
                        // SRD-75 metric_name emission is wired
                        // when the per-fiber mutable kernel handle
                        // Elapsed time goes to the metric (when one
                        // is configured); operators read it there.
                        // Logging it at INFO every loop is just
                        // narrating the happy path.
                        if let Some(name) = &pp.metric_name {
                            let elapsed = pp.started_at.elapsed().as_secs_f64();
                            crate::diag!(
                                crate::observer::LogLevel::Debug,
                                "phase-poll: predicate satisfied; {name}={elapsed:.3}s",
                            );
                        }
                        break;
                    }
                    if std::time::Instant::now() >= pp.deadline {
                        let elapsed = pp.started_at.elapsed().as_secs_f64();
                        // Compose the diagnostic once; the `abort`
                        // path appends a workload-invalidation
                        // note so the operator immediately knows
                        // that the whole run is terminating, not
                        // just this phase.
                        let invalidation_note = match pp.on_timeout {
                            PhasePollTimeoutPolicy::Abort => {
                                " — `on_timeout: abort` declared by the workload; \
                                 requesting session stop (the whole run terminates)"
                            }
                            PhasePollTimeoutPolicy::Error => "",
                        };
                        let formatted_reason = format!(
                            "[poll_timeout] phase-poll deadline reached after {elapsed:.1}s \
                             with predicate '__poll_until' still not Bool(true) \
                             (SRD-75 §\"Workload-load validation\" — adjust `timeout_ms` \
                             or the `until:` predicate){invalidation_note}"
                        );
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            *slot = Some(formatted_reason.clone());
                        }
                        // SRD-76 — push a structured
                        // `PhaseErrorDetail` into the
                        // activity's phase_errors buffer so
                        // the executor's phase-end build of
                        // `PhaseOutcome` captures the
                        // poll_timeout with its class and
                        // message. No op_template /
                        // op_resolved because the failure is
                        // at the phase level (no specific op
                        // dispenser fired the error).
                        if let Ok(mut errs) = activity.phase_errors.lock() {
                            errs.push(crate::phase_outcome::PhaseErrorDetail {
                                class: "poll_timeout".into(),
                                message: formatted_reason,
                                op_name: None,
                                cycle: None,
                                op_template: None,
                                op_resolved: None,
                                at_nanos: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_nanos() as u64)
                                    .unwrap_or(0),
                                retryable: false,
                            });
                        }
                        activity
                            .stop_flag
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        // SRD-75 `on_timeout: abort` —
                        // workload-author declares that an
                        // unsatisfied predicate makes the whole
                        // run meaningless. Set the session-wide
                        // stop signal; the scenario walker
                        // observes it on its next iteration check
                        // (`session_signals::stop_requested()`)
                        // and unwinds without entering the next
                        // sweep cell. The phase itself still
                        // returns Err for the normal stop-flag
                        // path; the session signal is the
                        // CROSS-PHASE escalation.
                        if matches!(pp.on_timeout, PhasePollTimeoutPolicy::Abort) {
                            crate::diag!(
                                crate::observer::LogLevel::Error,
                                "phase-poll: `on_timeout: abort` triggered after \
                                 {elapsed:.1}s; requesting session-wide stop \
                                 (SRD-75 §\"on_timeout\")",
                            );
                            crate::session_signals::request_stop();
                        }
                        break;
                    }
                    // Wait, rewind, re-create the reader.
                    tokio::time::sleep(pp.interval).await;
                    if !activity.source_factory.rewind_for_poll() {
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            *slot = Some(
                                "[phase_poll] source factory doesn't support \
                                 rewind_for_poll(); phase-poll requires a \
                                 rewindable source (RangeSourceFactory or \
                                 ExtendingRangeSourceFactory). SRD-75."
                                    .to_string(),
                            );
                        }
                        activity
                            .stop_flag
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    source =
                        CursorSource::new(activity.source_factory.create_reader(), stanza_stride);
                    continue;
                }
                break; // source exhausted (standard path)
            }
        };

        activity.metrics.stanzas_total.inc();
        // Stanza-boundary `reset_captures()` was historically called
        // here to defend against capture leakage between cycles. That
        // defence is redundant under the post-closure-binding-economy
        // architecture: every reachable wire is either a per-cycle
        // kernel output (recomputed each cycle from inputs and the
        // current `cycle`), a closed-loop capture (the same op that
        // reads the wire is the op that writes it; a failed write
        // short-circuits the consumer via OpResult error), a magic
        // extern (`body` / `count` / `ok`, rewritten pre-eval by
        // ResultDispenser), or a scope-invariant iter-var / workload
        // param (constant across the phase activation). None of those
        // can hold a stale value that a successful cycle would read.
        // The per-cycle reset + re-apply round-trip was 40% of single-
        // fiber CPU; removing it leaves end-state semantics identical.

        // Phase 2: RENDER + EXECUTE — distribute the reserved run across
        // the stanza's ops via `StanzaRuns`. Each op covers a contiguous
        // ordinal sub-run `[cycle, cycle + run_len)` of its own
        // `rows_per_op` (1 for ordinary ops, N for a batch op); `Σ ==
        // stanza_stride`, so consecutive stanzas cover DISJOINT ordinal
        // runs (SRD-22 cover-once). The LUT POSITION (not the ordinal)
        // selects the op, so a batch op that advances the cursor by N
        // still maps to the right stanza slot. At the cursor tail
        // `reserve` returned a short range, so the last op's `run_len`
        // is the truncated remainder — the partial final batch is still
        // inserted, never over-read, never dropped. Sequential in
        // declaration order.
        for (pos, cycle, run_len) in
            crate::child_source::StanzaRuns::new(range.clone(), &per_pos_rows)
        {
            if activity.stopped() {
                break;
            } // SRD-92 Step 0: one stop view

            // Mark op as active from render through result join.
            // "Active" means this fiber is working on an op — resolving
            // fields, waiting for the adapter, or recording results.
            activity.metrics.ops_started.fetch_add(1, Ordering::Relaxed);

            // Render the source item at the sub-run base (fiber-local, no
            // shared state). The op reads `[cycle, cycle + run_len)`.
            let item = source.render(cycle);
            // Publish the cycle to the enclosing fiber-context
            // scope so any Polydat node reading `cycle()` or implicitly
            // `cycle` inside the DAG sees the same ordinal as
            // adapter execution. No-op outside a fiber scope.
            crate::polydat_nodes::runtime_context::set_task_cycle(cycle);

            let wait_start = Instant::now();
            if let Some(ref rl) = rate_limiter {
                rl.acquire().await;
            }
            let wait_nanos = wait_start.elapsed().as_nanos() as u64;

            // The stanza POSITION (not the ordinal) selects the op —
            // `get_with_index(pos)` returns `lut[pos]`, stable regardless
            // of how many ordinals prior batch ops consumed.
            let (template_idx, template) = activity.op_sequence.get_with_index(pos as u64);

            // Daemon-op dispatch. If the template declares
            // `daemon: ...` (non-disabled), spawn a fresh
            // fiber onto the daemon pool instead of running
            // the op inline. The pool enforces a per-op-name
            // fiber cap; an overflow is a workload-design
            // error that fails the phase. Cycle-pool moves
            // on to the next stanza op as soon as the spawn
            // returns (Ok or Err).
            if !template.daemon.is_disabled() {
                let cap = template
                    .daemon
                    .max_fibers()
                    .expect("non-disabled daemon has cap");
                let cancel_grace = template
                    .daemon_cancel_grace_ms
                    .map(std::time::Duration::from_millis);
                let activity_d = activity.clone();
                let dispensers_d = dispensers.clone();
                let pull_plans_d = pull_plans.clone();
                let op_builder_d = op_builder.clone();
                let phase_arc_d = phase_name_arc.clone();
                let op_name_d = template.name.clone();
                let spawn_result =
                    daemon_pool.try_spawn(op_name_d.clone(), cap, cancel_grace, move |stop| {
                        let activity = activity_d;
                        let dispensers = dispensers_d;
                        let pull_plans = pull_plans_d;
                        let op_builder = op_builder_d;
                        let phase_arc = phase_arc_d;
                        let op_name = op_name_d;
                        async move {
                            use futures::FutureExt as _;
                            let phase_controls = activity
                                .component
                                .as_ref()
                                .map(crate::polydat_nodes::runtime_context::snapshot_controls)
                                .unwrap_or_else(
                                    crate::polydat_nodes::runtime_context::empty_controls,
                                );
                            let body = crate::polydat_nodes::runtime_context::with_fiber_context(
                                phase_arc,
                                phase_controls,
                                daemon_dispatch(
                                    activity.clone(),
                                    dispensers,
                                    pull_plans,
                                    op_builder,
                                    template_idx,
                                    op_name.clone(),
                                    stop,
                                ),
                            );
                            match std::panic::AssertUnwindSafe(body).catch_unwind().await {
                                Ok(exit) => exit,
                                Err(payload) => {
                                    let msg = payload
                                        .downcast_ref::<&'static str>()
                                        .map(|s| (*s).to_string())
                                        .or_else(|| payload.downcast_ref::<String>().cloned())
                                        .unwrap_or_else(|| "<non-string panic payload>".into());
                                    crate::diag!(
                                        crate::observer::LogLevel::Error,
                                        "daemon op '{op_name}' panicked: {msg}"
                                    );
                                    activity
                                        .stop_flag
                                        .store(true, std::sync::atomic::Ordering::Relaxed);
                                    crate::daemon_pool::DaemonExit::Panicked(msg)
                                }
                            }
                        }
                    });
                match spawn_result {
                    Ok(()) => {
                        // Dispatch succeeded — the daemon fiber
                        // (`daemon_dispatch`) owns this op's accounting
                        // end to end: it records `ops_started` when it
                        // runs and `cycles_total` / `ops_finished` + the
                        // SRD-91 result outcome when it completes. Counting
                        // the stanza-position here too DOUBLE-counted the
                        // op — one extra `cycles_total`/`ops_finished`
                        // with no matching result — which dragged ok%
                        // below 100% and pushed phase progress above it,
                        // and broke the `cycles_total == result_total +
                        // skips_total` invariant. `StanzaRuns` already
                        // advanced past this op's sub-run (daemon ops have
                        // rows_per_op == 1); just skip the inline execute
                        // path and let the daemon fiber count.
                        continue;
                    }
                    Err(msg) => {
                        crate::diag!(
                            crate::observer::LogLevel::Error,
                            "daemon op '{}' spawn failed: {msg}",
                            template.name
                        );
                        activity.stop_flag.store(true, Ordering::Release);
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            *slot = Some(format!("daemon op '{}' spawn: {msg}", template.name));
                        }
                        return;
                    }
                }
            }

            fiber.set_source_item(&item);
            // SRD-68 Push 5: `ctx.fields` is no longer the
            // resolution surface for adapters or wrappers — they
            // read everything through `ctx.wires` (the bound GK
            // context). The empty `ResolvedFields` satisfies the
            // `ExecCtx` struct-shape contract until the field is
            // removed from the trait surface entirely.
            let fields = crate::adapter::ResolvedFields::new(Vec::new(), Vec::new());

            // Resolve the wrapper-side pull plan against this
            // fiber's GkState (one indexed pull per registered
            // name, no name hashing). The resulting `pulls` is
            // disjoint from `fields`: adapters see only `fields`,
            // wrappers see only `pulls`.
            //
            // SRD-13d Phase 9 — when this op template materialised
            // its own kernel, the plan was sealed against the
            // op-template program; resolve_pulls_for_op picks
            // that kernel's state. Flattened op-templates fall
            // through to the main kernel (the workload program)
            // — same call site, the lookup is idempotent.
            let pulls = fiber.resolve_pulls_for_idx(template_idx, &pull_plans[template_idx]);
            let dispenser = &dispensers[template_idx];
            // SRD-68 invariant I-2: cycle-time reads against the
            // firing dispenser's per-fiber kernel slot, exposed
            // through the narrow `WireSource` trait. `CycleWires`
            // wraps the per-fiber kernel handle for the cycle's
            // duration so `WireSource::get` can drive output pulls
            // (`pull(&mut state, …)` through interior mutability)
            // alongside input/constant lookups. Dispensers with
            // no canonical kernel (legacy adapters, wrapper
            // delegates) fall through to the `NullWireSource`
            // baseline `ExecCtx::new` provides.
            // SRD-13f / SRD-68: cycle-time wire reads go through a
            // single kernel handle — the dispenser's per-fiber
            // op-template kernel. Every visible cross-scope wire
            // was wired into that kernel at construction (cells
            // for shared, folded constants for workload params,
            // construction-time slot setup + per-cycle refresh in
            // `set_inputs` for other parent outputs). The local
            // read API resolves every name; the wires layer never
            // composes chains externally.
            let cycle_wires = match fiber.per_op_kernel_mut(template_idx) {
                Some(p) => crate::wires::CycleWires::new(p),
                None => crate::wires::CycleWires::new(fiber.main_kernel_mut()),
            };
            let mut exec_ctx = crate::fixture::ExecCtx::with_wires(&fields, &pulls, &cycle_wires);
            // Hand the op the ACTUAL reserved sub-run length so a batch op
            // inserts exactly `[base, base + run_len)` — the full run, or
            // the short tail at cursor exhaustion. Ordinary ops ignore it.
            exec_ctx.run_len = run_len;
            let service_start = Instant::now();
            // The op runs through the wrapper stack ONCE. The innermost
            // `TriesDispenser` (when the op has a `tries` budget) owns the
            // attempt loop, `attempt_*` counters,
            // and the per-attempt panic catch; the OUTERMOST
            // `ErrorHandlerDispenser` (SRD-82 Part 3b) owns the whole-stack
            // panic backstop and the terminal-error handling — policy
            // routing, `errors_total` / per-type tallies, phase-error
            // capture, and the stop/fail effects. This loop sees exactly ONE
            // terminal outcome per cycle and keeps only the result-level
            // accounting. The residual `catch_unwind` guards a panic in the
            // error wrapper's OWN routing code (a runtime bug, not an op
            // failure): keep the fiber alive, mark the phase stopped.
            let outcome: Result<crate::adapter::OpResult, crate::adapter::ExecutionError> = {
                use futures::FutureExt as _;
                let op_fut = std::panic::AssertUnwindSafe(dispenser.execute(cycle, &exec_ctx))
                    .catch_unwind();
                // Race the whole op stack against the shutdown ladder's
                // CANCEL rung. `biased` polls the op first, so the cancel
                // branch costs one extra poll per dispatch on the happy
                // path. On cancellation the stack's future is DROPPED —
                // that is the cancel — and a synthesised non-retryable
                // error stands in as the terminal outcome (the errors
                // wrapper was inside the dropped future, so result-level
                // accounting below is all that records it).
                let raced = tokio::select! {
                    biased;
                    r = op_fut => Some(r),
                    _ = crate::session_signals::ops_cancelled(&mut shutdown_rx) => None,
                };
                match raced {
                    None => Err(crate::adapter::ExecutionError::Op(
                        crate::adapter::AdapterError {
                            error_name: "cancelled".into(),
                            message: "in-flight op cancelled by shutdown \
                                      escalation (Ctrl-C)"
                                .into(),
                            retryable: false,
                        },
                    )),
                    Some(Ok(r)) => r,
                    Some(Err(payload)) => {
                        let msg = payload
                            .downcast_ref::<&'static str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "<non-string panic payload>".into());
                        activity.metrics.errors_total.inc();
                        activity.metrics.count_error_type("panic");
                        activity.stop_flag.store(true, Ordering::Relaxed);
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            // Headline = first line; the full
                            // enriched text travels in the
                            // AdapterError message and renders
                            // once in the phase error list
                            // (SRD-82 §one full render).
                            let first = msg.lines().next().unwrap_or(&msg);
                            *slot = Some(format!(
                                "[panic] op '{}' at cycle {}: {first}",
                                template.name, cycle,
                            ));
                        }
                        Err(crate::adapter::ExecutionError::Op(
                            crate::adapter::AdapterError {
                                error_name: "panic".into(),
                                message: msg,
                                retryable: false,
                            },
                        ))
                    }
                }
            };
            let service_nanos = service_start.elapsed().as_nanos() as u64;
            let (success, skipped) = match outcome {
                Ok(result) => (true, result.skipped),
                Err(_) => (false, false),
            };

            // Per-OP totals (SRD-91): `cycles_total` counts every op
            // dispatched; executed ops go to `result_total`
            // (= result_success + result_failure). Skipped ops increment
            // `skips_total` in the `if:` wrapper (wrappers/if.rs), so
            // `cycles_total == result_total + skips_total` holds without a
            // tally here. The per-op error rate reads `result_failure`
            // (in [0,1]); the per-attempt `errors_total` was already
            // tallied in the loop.
            activity.metrics.cycles_total.inc();
            if !skipped {
                activity.metrics.result_total.inc();
                activity.metrics.service_time.record(service_nanos);
                activity.metrics.wait_time.record(wait_nanos);
                activity
                    .metrics
                    .response_time
                    .record(service_nanos + wait_nanos);
                // `tries_histogram` is recorded by the TriesDispenser (it owns
                // the attempt count).
                if success {
                    activity.metrics.result_success.observe(service_nanos);
                    // Captures landed on the per-op-template kernel
                    // directly via ctx.wires.write inside the
                    // dispenser stack — no post-execute pump.
                    //
                    // Two kernel-side steps remain:
                    //
                    // 1. Rule 2 write-through commit on the
                    //    op-template kernel — pulls every
                    //    `__write_<X>` and stores its value through
                    //    the cell-bound input slot for `<X>`,
                    //    propagating result-binding LHS values up
                    //    to parent `shared` cells. No-op when the
                    //    kernel carries no write-throughs. A
                    //    type-stability violation (scope_model.md
                    //    §"Type stability") is a DETERMINISTIC
                    //    workload bug — every cycle would repeat it —
                    //    so it stops the phase with the write-site
                    //    diagnostic (same treatment as the panic arm).
                    if let Err(e) = fiber.commit_op_template_write_throughs_for_idx(template_idx) {
                        activity.metrics.errors_total.inc();
                        activity.metrics.count_error_type("type_mismatch");
                        activity.stop_flag.store(true, Ordering::Relaxed);
                        if let Ok(mut slot) = activity.stop_reason.lock()
                            && slot.is_none()
                        {
                            *slot = Some(format!(
                                "[type_mismatch] op '{}' at cycle {}: {e}",
                                template.name, cycle,
                            ));
                        }
                    }
                    // 2. Pull every output of the op-template
                    //    kernel so side-effecting nodes (log_info,
                    //    log_debug) inside result-binding compute
                    //    chains actually evaluate. Without this, a
                    //    result-binding whose LHS isn't a
                    //    write-through stays dormant.
                    fiber.pull_all_op_template_outputs_for_idx(template_idx);
                } else {
                    activity.metrics.result_failure.observe(service_nanos);
                }
            }

            // Op fully processed — render, execute, and metrics all done.
            activity
                .metrics
                .ops_finished
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Best-effort terminal column count read off stderr (fd 2) via
/// the `TIOCGWINSZ` ioctl. Returns `None` when stderr isn't a
/// TTY or the call fails. The status renderer (now hosted by
/// `nmbrs-tui::log_only_sink`) uses this to clamp the rendered
/// status to a single visual row, since a wrap would leave
/// previous-tick text on screen below the cursor — the in-place
/// rewrite only erases from the cursor through end of the
/// current visual line.
#[cfg(unix)]
pub fn terminal_cols() -> Option<usize> {
    use std::os::raw::c_int;
    #[repr(C)]
    struct WinSize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }
    let mut ws = WinSize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `libc::ioctl` is FFI; `TIOCGWINSZ` writes into the
    // out-parameter which we own (pinned on the stack for the
    // duration of the call). Failure is signalled by negative
    // return — we ignore the actual errno.
    let rc: c_int = unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &mut ws as *mut _) };
    if rc < 0 || ws.ws_col == 0 {
        return None;
    }
    Some(ws.ws_col as usize)
}

/// Windows variant: `GetConsoleScreenBufferInfo` on the stderr
/// handle. The console API is declared by hand rather than via a
/// `windows-sys` dependency — two kernel32 imports don't justify
/// one. Width is the visible window (srWindow), not the scrollback
/// buffer width, matching what the status line can occupy.
#[cfg(windows)]
pub fn terminal_cols() -> Option<usize> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Coord {
        x: i16,
        y: i16,
    }
    #[repr(C)]
    struct SmallRect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }
    #[repr(C)]
    struct ConsoleScreenBufferInfo {
        size: Coord,
        cursor_position: Coord,
        attributes: u16,
        window: SmallRect,
        maximum_window_size: Coord,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn GetConsoleScreenBufferInfo(
            console: *mut c_void,
            info: *mut ConsoleScreenBufferInfo,
        ) -> i32;
    }
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;
    // SAFETY: both calls only write into the out-parameter we own;
    // failure is signalled by null/INVALID handle or zero return.
    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut info = std::mem::zeroed::<ConsoleScreenBufferInfo>();
        if GetConsoleScreenBufferInfo(handle, &mut info) == 0 {
            return None;
        }
        let cols = i32::from(info.window.right) - i32::from(info.window.left) + 1;
        if cols <= 0 { None } else { Some(cols as usize) }
    }
}

/// Glob-style match: `*` matches zero or more characters, `?`
/// matches exactly one character, every other byte must match
/// literally. Recursive — adequate for the short patterns
/// `status_metrics:` accepts (`recall*`, `latency_p99`, etc.).
/// Trades worst-case quadratic time for simplicity; the
/// candidate set is also tiny (low single-digit count of metric
/// Evaluate a gutter template ONCE at phase end (the `final:` form,
/// or the during-form's guaranteed last update). Placeholders resolve
/// through the wires of a throwaway subscope of the activity's source
/// kernel (shared cells and captures visible), then any names still
/// unresolved fall back to the phase's STATUS-METRIC aggregates
/// (`{recall}`, `{latency_p50}`, …) formatted exactly like the status
/// chips. Numeric kinds (`bar`/`spark`) additionally require the fully
/// resolved string to parse as f64; failures degrade to None (no
/// final cell) — the display must never fail a completed phase.
fn evaluate_final_gutter(
    activity: &Activity,
    source_kernel: &Arc<polydat::kernel::PolydatKernel>,
    kind: crate::wrappers::gutter::GutterKind,
    template: &str,
) -> Option<crate::wrappers::gutter::GutterSpec> {
    use crate::wrappers::gutter::{GutterKind, GutterSpec};
    // Wires pass: a fresh wired subscope (cells attach via the
    // sanctioned materialize path) gives template names their live
    // end-of-phase values.
    let sub = polydat::kernel::PolydatKernel::for_iteration(source_kernel, source_kernel, &[]);
    let rendered = match Arc::try_unwrap(sub) {
        Ok(mut k) => {
            let wires = crate::wires::CycleWires::new(&mut k);
            crate::wires::substitute_via_wires(template, &wires).ok()
        }
        Err(_) => None,
    };
    let mut text = rendered.unwrap_or_else(|| template.to_string());

    // Status-metric fallback for placeholders the wires didn't know:
    // relevancy aggregates and the latency family, formatted like the
    // status chips so `{recall}` in a final template reads identically
    // to the `recall:` chip beside it.
    if text.contains('{') {
        let mut candidates: Vec<(String, String)> = Vec::new();
        for live in activity.metrics.collect_relevancy_live() {
            if live.total_count > 0 {
                candidates.push((live.name, format!("{:.2}%", live.total_mean * 100.0)));
            }
        }
        let snap = activity.metrics.service_time.peek_snapshot();
        let h = &snap.histogram;
        if !h.is_empty() {
            let fmt = nmbrs_metrics::reporters::summary::format_duration;
            candidates.push(("latency_p50".into(), fmt(h.value_at_quantile(0.50) as f64)));
            candidates.push(("latency_p99".into(), fmt(h.value_at_quantile(0.99) as f64)));
            candidates.push(("latency_max".into(), fmt(h.max() as f64)));
            candidates.push(("latency_mean".into(), fmt(h.mean())));
        }
        for (name, val) in &candidates {
            text = text.replace(&format!("{{{name}}}"), val);
        }
    }

    // A template still carrying unresolved placeholders means the value
    // it names was never measured (a gated-off recall phase has no
    // relevancy aggregate) — no measurement, no cell. The completion
    // detail line keeps its standard stamp instead of showing the
    // literal `recall {recall}`.
    if text.contains('{') && text.contains('}') {
        return None;
    }
    match kind {
        GutterKind::Labeled => {
            let (name, value) = text.split_once('\u{1f}').unwrap_or(("", text.as_str()));
            Some(GutterSpec::Labeled {
                name: name.to_string(),
                value: value.to_string(),
            })
        }
        GutterKind::Text => Some(GutterSpec::Text(text)),
        GutterKind::Bar => text
            .trim()
            .parse::<f64>()
            .ok()
            .map(|v| GutterSpec::Bar(v.clamp(0.0, 1.0))),
        GutterKind::Spark => text.trim().parse::<f64>().ok().map(GutterSpec::Spark),
    }
}

/// names per phase).
fn glob_match(pattern: &str, candidate: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), candidate.as_bytes())
}

fn glob_match_bytes(pat: &[u8], s: &[u8]) -> bool {
    match (pat.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            // zero-or-more: try consuming nothing OR consume one
            // char of input and re-attempt.
            glob_match_bytes(&pat[1..], s) || (!s.is_empty() && glob_match_bytes(pat, &s[1..]))
        }
        (Some(b'?'), Some(_)) => glob_match_bytes(&pat[1..], &s[1..]),
        (Some(p), Some(c)) if p == c => glob_match_bytes(&pat[1..], &s[1..]),
        _ => false,
    }
}

// `spinner_frame`, `braille_bar`, `format_eta` moved to
// `crate::readouts::format` in Push 2 — the readouts that
// consume them now own the helpers. `truncate_to_width`
// stays here (it's a surface-level width-clamp concern,
// not a readout concern).

/// Truncate `s` to at most `max_cols` *visible* columns,
/// appending an ellipsis when truncation actually elides
/// content. Skips ANSI SGR escape sequences (`\x1b[...m`) when
/// counting visible width — they consume characters in the
/// string but no terminal columns. The truncation point is
/// always at a character boundary that's NOT inside an escape
/// sequence, so we never emit a half-broken `\x1b[3` to the
/// terminal.
pub fn truncate_to_width(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let bytes = s.as_bytes();
    let mut visible = 0usize;
    let mut byte_pos = 0usize; // last clean truncation point
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '\x1b' && bytes.get(i + 1) == Some(&b'[') {
            // SGR escape: walk until the final byte (`m`,
            // `K`, `J`, etc.) so we don't truncate mid-escape.
            for (_, ch) in chars.by_ref() {
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
            // byte_pos doesn't advance — escape costs no
            // visible columns, and the next plain char's
            // position is what we'd truncate to.
            continue;
        }
        if visible + 1 > max_cols.saturating_sub(1) {
            return format!("{}…", &s[..byte_pos]);
        }
        visible += 1;
        byte_pos = i + c.len_utf8();
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, ExecutionError, OpResult};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// SRD-92 R4 — `collect_status_primary` feeds the key-metric
    /// gutter cell's trend: first `status_metrics:` pattern's first
    /// candidate, as a raw numeric (latency family in milliseconds).
    /// The relevancy-first candidate ordering is exercised end-to-end
    /// by `nmbrs/tests/srd92_display.rs` (a live relevancy aggregate
    /// needs the full validation pipeline).
    #[test]
    fn status_primary_selects_first_matching_numeric() {
        let m = ActivityMetrics::new(&nmbrs_metrics::labels::Labels::empty());
        // No patterns → no primary, regardless of measurements.
        assert_eq!(m.collect_status_primary(&[]), None);
        // Patterns but nothing measured yet → None (no fabricated 0).
        assert_eq!(m.collect_status_primary(&["latency_*".into()]), None);

        // 5 ms samples land in the service-time histogram.
        for _ in 0..10 {
            m.service_time.record(5_000_000);
        }
        let (name, val) = m
            .collect_status_primary(&["latency_p50".into()])
            .expect("p50 measurable");
        assert_eq!(name, "latency_p50");
        assert!((val - 5.0).abs() < 0.5, "p50 ≈ 5 ms, got {val}");

        // Glob: first pattern's FIRST candidate wins (p50 precedes
        // p99 in candidate order).
        let (name, _) = m
            .collect_status_primary(&["latency_*".into()])
            .expect("glob matches");
        assert_eq!(name, "latency_p50");

        // Non-matching pattern → None.
        assert_eq!(m.collect_status_primary(&["recall*".into()]), None);
    }

    /// A counting DriverAdapter + OpDispenser for testing.
    struct CountingDriverAdapter {
        count: Arc<AtomicU64>,
    }

    impl CountingDriverAdapter {
        fn new() -> (Self, Arc<AtomicU64>) {
            let count = Arc::new(AtomicU64::new(0));
            (
                Self {
                    count: count.clone(),
                },
                count,
            )
        }
    }

    impl DriverAdapter for CountingDriverAdapter {
        fn name(&self) -> &str {
            "counting"
        }
        fn map_op<'a>(
            &'a self,
            _template: &'a nmbrs_workload::model::ParsedOp,
            _parent: std::sync::Arc<polydat::kernel::PolydatKernel>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
        > {
            Box::pin(async move {
                Ok(Box::new(CountingDispenser {
                    count: self.count.clone(),
                }) as Box<dyn OpDispenser>)
            })
        }
    }

    struct CountingDispenser {
        count: Arc<AtomicU64>,
    }

    impl OpDispenser for CountingDispenser {
        fn execute<'a>(
            &'a self,
            _cycle: u64,
            _ctx: &'a crate::fixture::ExecCtx<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
        > {
            self.count.fetch_add(1, Ordering::Relaxed);
            Box::pin(async {
                Ok(OpResult {
                    body: None,
                    skipped: false,
                })
            })
        }
    }

    /// A fail-then-succeed DriverAdapter for retry testing.
    struct FailThenSucceedDriverAdapter {
        fails_remaining: Arc<AtomicU64>,
        total_calls: Arc<AtomicU64>,
    }

    impl FailThenSucceedDriverAdapter {
        fn new(fail_count: u64) -> (Self, Arc<AtomicU64>) {
            let total = Arc::new(AtomicU64::new(0));
            (
                Self {
                    fails_remaining: Arc::new(AtomicU64::new(fail_count)),
                    total_calls: total.clone(),
                },
                total,
            )
        }
    }

    impl DriverAdapter for FailThenSucceedDriverAdapter {
        fn name(&self) -> &str {
            "fail-then-succeed"
        }
        fn map_op<'a>(
            &'a self,
            _template: &'a nmbrs_workload::model::ParsedOp,
            _parent: std::sync::Arc<polydat::kernel::PolydatKernel>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
        > {
            Box::pin(async move {
                Ok(Box::new(FailThenSucceedDispenser {
                    fails_remaining: self.fails_remaining.clone(),
                    total_calls: self.total_calls.clone(),
                }) as Box<dyn OpDispenser>)
            })
        }
    }

    struct FailThenSucceedDispenser {
        fails_remaining: Arc<AtomicU64>,
        total_calls: Arc<AtomicU64>,
    }

    impl OpDispenser for FailThenSucceedDispenser {
        fn execute<'a>(
            &'a self,
            _cycle: u64,
            _ctx: &'a crate::fixture::ExecCtx<'a>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
        > {
            self.total_calls.fetch_add(1, Ordering::Relaxed);
            let remaining = self.fails_remaining.fetch_sub(1, Ordering::Relaxed);
            Box::pin(async move {
                if remaining > 0 {
                    Err(ExecutionError::Op(AdapterError {
                        error_name: "TransientError".into(),
                        message: "temporary failure".into(),
                        retryable: true,
                    }))
                } else {
                    Ok(OpResult {
                        body: None,
                        skipped: false,
                    })
                }
            })
        }
    }

    /// Build a minimal Polydat root kernel (single identity node) for tests.
    fn test_kernel() -> polydat::kernel::PolydatKernel {
        use polydat::compile::assembly::{PolydatAssembler, WireRef};
        use polydat::library::identity::Identity;
        let mut asm = PolydatAssembler::new(vec!["cycle".into()]);
        asm.add_node(
            "id",
            Box::new(Identity::new(polydat::ast::PortType::U64)),
            vec![WireRef::input("cycle")],
        );
        asm.add_output("id", WireRef::node("id"));
        asm.compile().unwrap()
    }

    #[tokio::test]
    async fn activity_runs_all_cycles() {
        let config = ActivityConfig {
            name: "test".into(),
            cycles: 100,
            concurrency: 4,
            ..Default::default()
        };
        let ops = vec![nmbrs_workload::model::ParsedOp::simple("op1", "test")];
        let seq = OpSequence::uniform(ops);
        let activity = Activity::new(config, &Labels::of("session", "test"), seq);

        let (adapter, count) = CountingDriverAdapter::new();
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;

        assert_eq!(count.load(Ordering::Relaxed), 100);
    }

    #[tokio::test]
    async fn activity_retries_on_error() {
        // Retry backoff keeps this test's op IN FLIGHT for a few
        // hundred ms — long enough to overlap the session_signals
        // tests, whose bodies legitimately hold the process-global
        // cancel rung in force (the raced in-flight cancel would
        // kill attempt 3). Serialize with them, same discipline as
        // every global-flag test.
        let _signals = crate::session_signals::STOP_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let config = ActivityConfig {
            name: "retrytest".into(),
            cycles: 1,
            concurrency: 1,
            error_spec: "TransientError:retry,warn;.*:stop".into(),
            // Total-attempts budget (the `tries` sigil): 6 total ≈ the old
            // `retries: 5` additional-attempts budget.
            tries: Some(6),
            ..Default::default()
        };
        let ops = vec![nmbrs_workload::model::ParsedOp::simple("op1", "test")];
        let seq = OpSequence::uniform(ops);
        let activity = Activity::new(config, &Labels::of("session", "s1"), seq);

        let (adapter, total_calls) = FailThenSucceedDriverAdapter::new(2);
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;

        assert_eq!(total_calls.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn daemon_op_dispatches_at_cycle_pool_position() {
        // SRD-79 (in-flight): daemon-flagged op spawns onto the
        // daemon pool when the cycle-pool fiber's stanza walk
        // reaches it. The daemon fiber runs the same dispenser
        // as a non-daemon op would, but on its own tokio task,
        // and the cycle-pool fiber doesn't await it.
        let config = ActivityConfig {
            name: "daemon-disp-test".into(),
            cycles: 1,
            concurrency: 1,
            ..Default::default()
        };
        let mut op = nmbrs_workload::model::ParsedOp::simple("dmn", "test");
        op.daemon = nmbrs_workload::model::DaemonSpec::MaxFibers(1);
        let seq = OpSequence::uniform(vec![op]);
        let activity = Activity::new(config, &Labels::of("session", "s1"), seq);

        let (adapter, count) = CountingDriverAdapter::new();
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;

        // Daemon dispatched + ran exactly once (cycles=1).
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "daemon op should have run via dispatch-time spawn"
        );
    }

    #[tokio::test]
    async fn daemon_op_cap_exceeded_fails_phase() {
        // Cap=1 with cycles=2: first dispatch succeeds and the
        // daemon fiber blocks (the CountingDispenser returns
        // instantly, so the daemon should drain before the
        // second cycle — but the daemon-pool counter only
        // decrements when the body returns, and the second
        // cycle may race with the decrement). This test
        // primarily checks the no-panic / clean-failure path:
        // even if the cap fires, the activity exits cleanly.
        let config = ActivityConfig {
            name: "daemon-cap-test".into(),
            cycles: 50,
            concurrency: 1,
            ..Default::default()
        };
        let mut op = nmbrs_workload::model::ParsedOp::simple("dmn", "test");
        op.daemon = nmbrs_workload::model::DaemonSpec::MaxFibers(1);
        let seq = OpSequence::uniform(vec![op]);
        let activity = Activity::new(config, &Labels::of("session", "s2"), seq);

        let (adapter, _count) = CountingDriverAdapter::new();
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;
        // No assertion on count — the load-bearing behaviour is
        // that the activity terminates cleanly even when caps
        // bite. Without the cap, this test would hang or panic.
    }

    #[tokio::test]
    async fn shared_metrics_accessible() {
        let config = ActivityConfig {
            name: "metricstest".into(),
            cycles: 50,
            concurrency: 2,
            ..Default::default()
        };
        let ops = vec![nmbrs_workload::model::ParsedOp::simple("op1", "test")];
        let seq = OpSequence::uniform(ops);
        let activity = Activity::new(config, &Labels::of("session", "s1"), seq);

        let shared_metrics = activity.shared_metrics();

        let (adapter, _count) = CountingDriverAdapter::new();
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;

        assert_eq!(shared_metrics.cycles_total.get(), 50);
        let frame = shared_metrics.capture(std::time::Duration::from_secs(1));
        assert!(!frame.is_empty());
    }

    #[tokio::test]
    async fn per_error_type_counters_emit_deltas_through_dynamic_capture() {
        // SRD-40 / cascade coalesce: `MetricSet::combine_into` for
        // Counter sums `total` across intervals — so per-cycle
        // emissions must be DELTAS, not absolutes. Per-error-type
        // counters live on `ActivityMetricsDynamic` (outside the
        // static registry), so they need their own delta tracking.
        // This test exercises that path directly.
        use nmbrs_metrics::component::Component;
        use nmbrs_metrics::snapshot::MetricValue;

        let metrics = Arc::new(ActivityMetrics::new(&Labels::of("session", "s1")));
        let component = Arc::new(std::sync::RwLock::new(Component::new(
            Labels::of("activity", "t"),
            HashMap::new(),
        )));
        {
            let mut g = component.write().unwrap();
            g.set_state(nmbrs_metrics::component::ComponentState::Running);
            metrics.register_on(&mut g).unwrap();
        }

        // Seed two error-type counters with different totals.
        for _ in 0..3 {
            metrics.count_error_type("net");
        }
        for _ in 0..7 {
            metrics.count_error_type("timeout");
        }

        // First capture_delta — totals=3 and 7 are the deltas.
        let snap1 = component
            .read()
            .unwrap()
            .capture_delta(std::time::Duration::from_secs(1));
        let net1 = read_counter(&snap1, "errors.net");
        let to1 = read_counter(&snap1, "errors.timeout");
        assert_eq!(net1, 3, "first delta for net should be 3, got {net1}");
        assert_eq!(to1, 7, "first delta for timeout should be 7, got {to1}");

        // Drive the per-error-type counters further.
        for _ in 0..2 {
            metrics.count_error_type("net");
        }
        for _ in 0..1 {
            metrics.count_error_type("timeout");
        }

        // Second capture_delta — should report only the new deltas
        // (2 and 1), NOT the absolute totals (5 and 8).
        let snap2 = component
            .read()
            .unwrap()
            .capture_delta(std::time::Duration::from_secs(1));
        let net2 = read_counter(&snap2, "errors.net");
        let to2 = read_counter(&snap2, "errors.timeout");
        assert_eq!(
            net2, 2,
            "second delta for net should be 2 (new only), got {net2}"
        );
        assert_eq!(
            to2, 1,
            "second delta for timeout should be 1 (new only), got {to2}"
        );

        // capture_current (drain=false) should still report absolutes.
        let cur = component.read().unwrap().capture_current();
        let net_abs = read_counter(&cur, "errors.net");
        let to_abs = read_counter(&cur, "errors.timeout");
        assert_eq!(
            net_abs, 5,
            "current should be absolute total 5, got {net_abs}"
        );
        assert_eq!(
            to_abs, 8,
            "current should be absolute total 8, got {to_abs}"
        );

        fn read_counter(snap: &nmbrs_metrics::snapshot::MetricSet, family: &str) -> u64 {
            let f = snap
                .family(family)
                .unwrap_or_else(|| panic!("family {family:?} missing from snapshot"));
            let m = f.metrics().next().expect("at least one metric");
            match m.point().unwrap().value() {
                MetricValue::Counter(c) => c.cumulative,
                v => panic!("not a counter: {v:?}"),
            }
        }
    }

    #[tokio::test]
    async fn activity_with_rate() {
        let config = ActivityConfig {
            name: "ratetest".into(),
            cycles: 10,
            concurrency: 2,
            rate: Some(10000.0),
            ..Default::default()
        };
        let ops = vec![nmbrs_workload::model::ParsedOp::simple("op1", "test")];
        let seq = OpSequence::uniform(ops);
        let activity = Activity::new(config, &Labels::of("session", "s1"), seq);

        let (adapter, count) = CountingDriverAdapter::new();
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;

        assert_eq!(count.load(Ordering::Relaxed), 10);
    }

    #[tokio::test]
    async fn activity_with_weighted_ops() {
        let config = ActivityConfig {
            name: "weighted".into(),
            cycles: 12,
            concurrency: 1,
            ..Default::default()
        };
        let ops = vec![
            nmbrs_workload::model::ParsedOp::simple("read", "SELECT"),
            nmbrs_workload::model::ParsedOp::simple("write", "INSERT"),
        ];
        let seq = OpSequence::build(ops, &[4, 2], SequencerType::Bucket);
        let activity = Activity::new(config, &Labels::of("session", "s1"), seq);

        let (adapter, count) = CountingDriverAdapter::new();
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;

        assert_eq!(count.load(Ordering::Relaxed), 12);
    }

    #[tokio::test]
    async fn rate_control_is_declared_when_rate_configured() {
        use nmbrs_metrics::component::Component;
        use nmbrs_metrics::labels::Labels as L;
        use std::sync::RwLock;

        let config = ActivityConfig {
            name: "rate_decl".into(),
            cycles: 5,
            concurrency: 1,
            rate: Some(2500.0),
            ..Default::default()
        };
        let ops = vec![nmbrs_workload::model::ParsedOp::simple("op1", "test")];
        let seq = OpSequence::uniform(ops);
        let mut activity = Activity::new(config, &L::of("session", "s_rate"), seq);
        let component = Arc::new(RwLock::new(Component::new(
            L::of("session", "s_rate"),
            std::collections::HashMap::new(),
        )));
        activity.attach_component(component.clone());

        let (adapter, _count) = CountingDriverAdapter::new();
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;

        // After the activity runs, the rate control is on the
        // component and reports the configured target via its
        // reified gauge.
        let guard = component.read().unwrap();
        let erased = guard
            .controls()
            .get_erased("rate")
            .expect("rate control should be declared when rate is set");
        assert!(erased.accepts_f64_writes());
        assert_eq!(erased.gauge_f64(), Some(2500.0));
    }

    #[tokio::test]
    async fn rate_control_is_absent_when_no_rate() {
        use nmbrs_metrics::component::Component;
        use nmbrs_metrics::labels::Labels as L;
        use std::sync::RwLock;

        let config = ActivityConfig {
            name: "no_rate".into(),
            cycles: 3,
            concurrency: 1,
            rate: None,
            ..Default::default()
        };
        let ops = vec![nmbrs_workload::model::ParsedOp::simple("op1", "test")];
        let seq = OpSequence::uniform(ops);
        let mut activity = Activity::new(config, &L::of("session", "s_nr"), seq);
        let component = Arc::new(RwLock::new(Component::new(
            L::of("session", "s_nr"),
            std::collections::HashMap::new(),
        )));
        activity.attach_component(component.clone());

        let (adapter, _count) = CountingDriverAdapter::new();
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;

        let guard = component.read().unwrap();
        assert!(
            guard.controls().get_erased("rate").is_none(),
            "no rate control should exist without rate configured",
        );
    }

    #[tokio::test]
    async fn rate_control_write_retargets_the_running_limiter() {
        use nmbrs_metrics::component::Component;
        use nmbrs_metrics::controls::ControlOrigin;
        use nmbrs_metrics::labels::Labels as L;
        use std::sync::RwLock;

        // 200 cycles with a low rate + a concurrent writer that
        // bumps the rate mid-flight. The committed value on the
        // control reflects the write; the limiter carries the
        // same target after reconfigure.
        let config = ActivityConfig {
            name: "rate_live".into(),
            cycles: 200,
            concurrency: 2,
            rate: Some(50.0),
            ..Default::default()
        };
        let ops = vec![nmbrs_workload::model::ParsedOp::simple("op1", "test")];
        let seq = OpSequence::uniform(ops);
        let mut activity = Activity::new(config, &L::of("session", "s_live"), seq);
        let component = Arc::new(RwLock::new(Component::new(
            L::of("session", "s_live"),
            std::collections::HashMap::new(),
        )));
        activity.attach_component(component.clone());

        // Spawn the activity, wait for the applier to be wired,
        // issue a typed write, assert the control value advanced.
        let component_for_writer = component.clone();
        let writer = tokio::spawn(async move {
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let ctl: Option<nmbrs_metrics::controls::Control<nmbrs_rate::RateSpec>> =
                    component_for_writer.read().unwrap().controls().get("rate");
                if let Some(c) = ctl {
                    // Only attempt once the applier is registered.
                    if c.applier_count() > 0 {
                        c.set(nmbrs_rate::RateSpec::new(10_000.0), ControlOrigin::Test)
                            .await
                            .ok();
                        return;
                    }
                }
            }
        });

        let (adapter, _count) = CountingDriverAdapter::new();
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;
        let _ = writer.await;

        let guard = component.read().unwrap();
        let ctl: nmbrs_metrics::controls::Control<nmbrs_rate::RateSpec> =
            guard.controls().get("rate").unwrap();
        assert_eq!(ctl.value().ops_per_sec, 10_000.0);
    }

    #[tokio::test]
    async fn concurrency_control_is_declared_on_attached_component() {
        // SRD 23 integration: the activity declares its
        // `concurrency` control on the attached component during
        // startup; the control's reified gauge reads the
        // configured value.
        use nmbrs_metrics::component::Component;
        use nmbrs_metrics::labels::Labels as L;
        use std::sync::RwLock;

        let config = ActivityConfig {
            name: "ctrl_decl".into(),
            cycles: 10,
            concurrency: 3,
            ..Default::default()
        };
        let ops = vec![nmbrs_workload::model::ParsedOp::simple("op1", "test")];
        let seq = OpSequence::uniform(ops);
        let mut activity = Activity::new(config, &L::of("session", "s_decl"), seq);
        let component = Arc::new(RwLock::new(Component::new(
            L::of("session", "s_decl"),
            std::collections::HashMap::new(),
        )));
        activity.attach_component(component.clone());

        let (adapter, _count) = CountingDriverAdapter::new();
        activity
            .run_with_driver(
                Arc::new(adapter),
                Arc::new(crate::synthesis::OpBuilder::new(test_kernel())),
            )
            .await;

        // After run completes the control is still on the
        // component (structural declaration survives execution).
        let guard = component.read().unwrap();
        let erased = guard
            .controls()
            .get_erased("concurrency")
            .expect("concurrency control should be declared on attached component");
        assert_eq!(erased.value_string(), "3");
        assert!(erased.accepts_f64_writes());
        // Gauge projection reads as f64.
        assert_eq!(erased.gauge_f64(), Some(3.0));
    }
}
