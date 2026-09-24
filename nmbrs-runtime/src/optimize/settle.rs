// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-86 §"Settling via the cadence pulse" — the per-pulse settle
//! interpreter and its [`PulseEvaluator`] adapter.
//!
//! For a *volatile* optimizer objective (one defined over a windowed
//! run-produced metric such as `metric_window("...","errors")` or a
//! `metricsql_*` reader), the objective value chases the live cadence
//! window and at phase completion the trailing window is empty. The
//! objective therefore cannot be read by a one-shot post-execution
//! pull; it must be *settled* across the run and held in a register the
//! executor reads at completion.
//!
//! [`SettleInterpreter`] is the settle-signal engine. It owns one
//! persistent settle-kernel — typically the phase's objective bindings
//! plus `(stable_value, stable) := is_stable(<objective>, …)` — and is
//! driven once per cadence pulse:
//!
//! 1. `set_input` on the kernel's poke input advances the generation,
//!    which dirties **every** non-deterministic node (engine rule, see
//!    `kernel::engines::set_input`): the embedded volatile objective
//!    reader re-reads the latest published window and
//!    [`is_stable`](polydat::library::stability) re-evaluates — exactly
//!    one new sample on its ring.
//! 2. Pull `stable` then `stable_value`; both land on that single
//!    evaluation (generation cache), so the multi-output node
//!    contributes one sample per pulse, not two.
//! 3. Publish `stable_value` into the shared register (an [`ArcSwap`],
//!    lock-free for the executor's completion read).
//!
//! The interpreter does **not** decide phase stop — that is the
//! [`SettleEvaluator`]'s job (settled ⇒ `interrupted`; timed out ⇒
//! `failed`), driven through the general
//! [`super::phase_pulse::PhaseStopEvaluator`] callback registered on the
//! metrics cadence feed.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use nmbrs_metrics::cadence_reporter::{CadenceReporter, SubscriberId};
use nmbrs_metrics::snapshot::MetricSet;
use polydat::Kernel;
use polydat::ast::Value;
use polydat::kernel::{PolydatKernel, PolydatProgram};

use super::phase_pulse::{PhaseStopEvaluator, PulseEvaluator, StopOutcomeCell};
use crate::phase_outcome::Outcome;

/// Convert a pulled objective wire to f64 with the same numeric
/// coercion as `read_objective_at_completion` (F64 as-is, U64 widened,
/// Bool 0/1); a non-numeric objective reads as 0.0 (the
/// volatile-objective gate upstream only admits numeric readers).
fn objective_to_f64(v: &Value) -> f64 {
    match v {
        Value::F64(f) => *f,
        Value::U64(u) => *u as f64,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

/// The latest reading a [`SettleInterpreter`] publishes.
#[derive(Clone, Copy, Debug)]
pub struct SettleReading {
    /// The stabilized objective value — `is_stable`'s `stable_value`
    /// output (the median of the recent window). This is what the
    /// phase executor reads as the objective at completion.
    pub value: f64,
    /// Whether the signal reached steady state on the latest pulse.
    pub stable: bool,
    /// Count of pulses delivered so far (viability / diagnostics).
    pub pulses: u64,
}

impl Default for SettleReading {
    fn default() -> Self {
        Self {
            value: 0.0,
            stable: false,
            pulses: 0,
        }
    }
}

/// Per-pulse interpreter of objective settling. See the module docs.
pub struct SettleInterpreter {
    /// A standalone kernel on whatever engine the compile chose — it
    /// takes no part in the scope tree, so it is driven through the
    /// engine-neutral [`Kernel`] trait.
    kernel: Box<dyn Kernel>,
    /// Index of the poke input on the settle kernel, if present. Absent
    /// only for a kernel with no inputs — pulses then evaluate the
    /// kernel as-is (no generation advance, so a volatile reader would
    /// not re-read; such a kernel is only meaningful in tests).
    source_input: Option<usize>,
    value_wire: String,
    stable_wire: String,
    register: Arc<ArcSwap<SettleReading>>,
    pulses: u64,
}

impl SettleInterpreter {
    /// Build an interpreter over a compiled settle kernel. `source` is
    /// the input poked to advance the generation each pulse;
    /// `value_wire` / `stable_wire` are the `is_stable` multi-output
    /// wire names.
    pub fn new(kernel: Box<dyn Kernel>, source: &str, value_wire: &str, stable_wire: &str) -> Self {
        let source_input = kernel.input_index(source);
        Self {
            kernel,
            source_input,
            value_wire: value_wire.to_string(),
            stable_wire: stable_wire.to_string(),
            register: Arc::new(ArcSwap::from_pointee(SettleReading::default())),
            pulses: 0,
        }
    }

    /// The shared register handle. The executor reads the settled
    /// objective from this at phase completion; the interpreter
    /// publishes into it on every pulse.
    pub fn register(&self) -> Arc<ArcSwap<SettleReading>> {
        self.register.clone()
    }

    /// Deliver one cadence pulse. Advances the kernel generation (so the
    /// embedded volatile objective re-reads), evaluates `is_stable`
    /// exactly once, publishes the stabilized value, and returns the
    /// reading. `poke` is written to the source input only to force the
    /// generation advance — its value is not part of the computation
    /// when the objective is embedded in the kernel.
    pub fn pulse(&mut self, poke: f64) -> SettleReading {
        self.pulses += 1;
        if let Some(idx) = self.source_input {
            // `source` is an `f64` extern, so an `F64` write cannot be
            // refused; a refusal is a malformed settle kernel.
            self.kernel
                .set_input_at(idx, Value::F64(poke))
                .expect("settle kernel refused its f64 `source` extern");
        }
        // Every pulse is one `is_stable` sample, a poke equal to the
        // last one included — which a compiled engine would otherwise
        // treat as a cycle whose inputs did not move.
        self.kernel.invalidate_all();
        let stable = self.kernel.pull(&self.stable_wire).as_u64() != 0;
        let value = self.kernel.pull(&self.value_wire).as_f64();

        let reading = SettleReading {
            value,
            stable,
            pulses: self.pulses,
        };
        self.register.store(Arc::new(reading));
        reading
    }
}

/// The settle detector as a [`PulseEvaluator`]. It holds the phase's
/// **objective kernel** (a clone of node X's kernel — already bound to
/// this evaluation's coordinate, the same kernel
/// `read_objective_at_completion` pulls) and a fixed `is_stable`
/// engine. Each cadence pulse:
///
/// 1. pokes the objective kernel's generation-advance input (typically
///    `cycle`), which dirties every non-deterministic node so the
///    volatile objective reader re-reads the latest published window;
/// 2. pulls the objective wire — the fresh windowed objective value;
/// 3. feeds it to [`SettleInterpreter`] (`is_stable`).
///
/// It yields a terminal [`Outcome`] when the objective settles
/// (`interrupted` — stopped early, register trustworthy) or when the
/// settle `timeout` elapses without settling (`failed` — SRD-86 §6
/// step 5). `None` while the loop should hold.
pub struct SettleEvaluator {
    objective: PolydatKernel,
    objective_wire: String,
    poke: Option<usize>,
    interp: SettleInterpreter,
    timeout: Duration,
    /// SRD-86 viability gate — minimum WALL-CLOCK a coordinate must run before a
    /// stable verdict is trusted, so the windowed objective's rollup has cleared
    /// the prior coordinate (and the leading transient). Wall-clock, not pulse
    /// count: under concurrent scheduling cadence pulses are delivered in
    /// bursts (many per cadence interval), so a pulse gate collapses to far less
    /// than the window — the gate must measure real time.
    min_viable: Duration,
    started: Option<Instant>,
    pulses: u64,
}

impl SettleEvaluator {
    /// `objective` is the phase's objective kernel (node X clone);
    /// `objective_wire` the objective output; `poke_input` the input
    /// poked each pulse to force the volatile reader to re-read
    /// (typically `cycle`); `interp` the `is_stable` engine fed the
    /// objective value.
    pub fn new(
        objective: PolydatKernel,
        objective_wire: &str,
        poke_input: &str,
        interp: SettleInterpreter,
        timeout: Duration,
        min_viable: Duration,
    ) -> Self {
        let poke = objective.program().find_input(poke_input);
        Self {
            objective,
            objective_wire: objective_wire.to_string(),
            poke,
            interp,
            timeout,
            min_viable,
            started: None,
            pulses: 0,
        }
    }

    /// The settled-value register (grab it before boxing the evaluator).
    pub fn register(&self) -> Arc<ArcSwap<SettleReading>> {
        self.interp.register()
    }
}

impl PulseEvaluator for SettleEvaluator {
    fn evaluate(&mut self, _window: &MetricSet) -> Option<Outcome> {
        let start = *self.started.get_or_insert_with(Instant::now);
        self.pulses += 1;
        // Advance the objective kernel's generation so its volatile
        // reader re-reads the latest published window, then read it.
        if let Some(idx) = self.poke {
            self.objective
                .state()
                .set_input(idx, Value::U64(self.pulses));
        }
        let obj = objective_to_f64(self.objective.pull_ref(&self.objective_wire));
        // SRD-89 — a NaN objective is a windowed metric reading **no data** (an
        // empty `rate(...[W])` lookback — see `nodes::no_data_value`), distinct
        // from a real 0. HOLD on it: do not feed the stability detector (a
        // fabricated 0 would let `is_stable` settle on the empty leading reads,
        // mis-converging the optimizer under concurrency where early windows are
        // routinely empty) and do not advance the register. The timeout still
        // bounds the wait, so a window that never produces data fails rather
        // than hanging.
        if obj.is_nan() {
            if start.elapsed() >= self.timeout {
                return Some(Outcome::failed());
            }
            return None;
        }
        let reading = self.interp.pulse(obj);
        // SRD-86 viability gate — do NOT trust a stable verdict until the
        // coordinate has run for `min_viable` of WALL-CLOCK, so the windowed
        // objective's rollup has cleared the prior coordinate (and its own
        // leading transient). At a coordinate's START the windowed objective is
        // a stable run of stale data — `rate(errors_total[W])` reads ~0 before
        // the first error registers at warmup, and at a transition it reads the
        // PRIOR coordinate's value drifting out of the window. `is_stable`
        // (which fires on `SETTLE_MIN_SAMPLES`, and whose relative margin admits
        // a slow drift as "stable") would latch that stale value, mis-converging
        // the optimizer (it keeps a saturating `concurrency` because it "saw no
        // errors", or accepts a half-cleared transition value). The gate is
        // wall-clock, not pulse count: under concurrent scheduling cadence
        // pulses arrive in bursts, so a pulse gate collapses to far less than
        // the window — only real elapsed time guarantees the window has cleared.
        if reading.stable && start.elapsed() >= self.min_viable {
            return Some(Outcome::interrupted());
        }
        if start.elapsed() >= self.timeout {
            return Some(Outcome::failed());
        }
        None
    }
}

/// Metrics-reader node names whose presence makes a phase's objective
/// potentially volatile (read from the live cadence feed). The
/// `metric` / `metric_window` stat-readers and the four `metricsql_*`
/// readers. (First-push heuristic: program-wide presence, not
/// objective-cone-precise — a non-reader objective in a phase that also
/// reads metrics elsewhere would settle trivially on its constant.)
const READER_NODES: &[&str] = &[
    "metric",
    "metric_window",
    "metricsql",
    "metricsql_scalar",
    "metricsql_vector",
    "metricsql_window",
];

/// True if `program` contains a metrics-reader node — the signal that
/// the phase's objective may be a volatile windowed metric that the
/// one-shot post-completion read cannot capture.
pub fn program_reads_live_metrics(program: &PolydatProgram) -> bool {
    (0..program.node_count()).any(|i| READER_NODES.contains(&program.node_meta(i).name.as_str()))
}

/// Node name of the session-cumulative reader (`metric(...)`), which
/// reads `MetricsQuery::session_lifetime` — a running total over ALL
/// coordinates. Unlike the windowed readers it has no bounded lookback,
/// so the viability gate cannot scope it to one coordinate.
const SESSION_CUMULATIVE_READER: &str = "metric";

/// True if `program` reads the session-cumulative `metric(...)` reader —
/// an objective aggregated across every coordinate, which the per-eval
/// warmup gate cannot isolate (no window to clear). The settle warns
/// rather than silently treating it as per-coordinate.
fn program_reads_session_cumulative_metrics(program: &PolydatProgram) -> bool {
    (0..program.node_count()).any(|i| program.node_meta(i).name == SESSION_CUMULATIVE_READER)
}

/// Warn at most once per distinct objective string that it reads a
/// session-cumulative metric. Returns `true` the first time a given
/// objective is seen (so the caller emits the diagnostic once, not once
/// per coordinate across a search).
fn warn_once_session_cumulative(objective: &str) -> bool {
    static WARNED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(Default::default);
    WARNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(objective.to_string())
}

// First-push settle parameters (the SRD-86 §6 `settle:` YAML surface +
// finer-cadence reconfiguration are deferred). Sized for the default
// 1 s cadence and short phases: `is_stable`'s windowed median is
// published every pulse regardless of the `stable` flag, so the
// register holds a smoothed objective even before a full settle; the
// 8-deep horizon / 4-sample warmup let a steady objective settle (and
// stop the phase early) within a handful of pulses. The generous
// timeout means a phase that completes first simply uses its smoothed
// value — only a genuinely non-settling long phase trips `failed`.
//
// The settle is gated by a viability horizon (see `SettleEvaluator::evaluate`):
// a stable verdict is only honored once `SETTLE_HORIZON` pulses have been
// delivered, so the verdict is always taken over a full horizon of
// in-coordinate samples (≈ `SETTLE_HORIZON × cadence` of wall-clock, the rollup
// window by the usual `window = horizon × cadence` sizing). Without it, a
// windowed objective's leading transient — `rate(...[W])` reading ~0 before the
// coordinate's first data lands — is itself momentarily "stable", and
// `is_stable` (firing on `SETTLE_MIN_SAMPLES`) latches that phantom value; under
// concurrent scheduling a sub-window eval did exactly that.
const SETTLE_MARGIN: f64 = 0.05;
const SETTLE_MIN_SAMPLES: u64 = 4;
const SETTLE_HORIZON: u64 = 8;
const SETTLE_TIMEOUT: Duration = Duration::from_secs(60);

/// What the executor holds while a settle detector runs: the cadence
/// subscription to tear down, the settled-value register, and the
/// terminal-disposition cell.
pub struct SettleHandle {
    pub subscriber: SubscriberId,
    pub register: Arc<ArcSwap<SettleReading>>,
    pub outcome: StopOutcomeCell,
}

/// Start a cadence-fed settle detector for `objective` on the running
/// phase **iff** the objective reads live metrics. Builds the objective
/// kernel as node X's program rebound to `parent` (carrying the
/// coordinate — mirrors `read_objective_at_completion`), wraps it in a
/// [`PhaseStopEvaluator`], and subscribes it to the smallest cadence.
/// Returns `None` for a non-volatile objective (the one-shot read path
/// is correct there) or if the metrics cadence is disabled.
pub fn start_settle(
    parent: &Arc<PolydatKernel>,
    phase_kernel: &Arc<PolydatKernel>,
    objective: &str,
    reporter: &Arc<CadenceReporter>,
    stop_flag: Arc<AtomicBool>,
) -> Option<SettleHandle> {
    let program = phase_kernel.program();
    if !program_reads_live_metrics(program) {
        return None;
    }
    // A session-cumulative `metric(...)` objective has no bounded window
    // for the gate to scope, so it cannot isolate per-coordinate — warn
    // once and servo the author to a windowed reader.
    if program_reads_session_cumulative_metrics(program) && warn_once_session_cumulative(objective)
    {
        crate::diag!(
            crate::observer::LogLevel::Warn,
            "optimizer objective '{objective}' reads a session-cumulative metric \
             (`metric(...)` → session_lifetime): it aggregates across coordinates and \
             will not isolate per-coordinate. Use `metric_window(...)` or \
             `metricsql_scalar(rate(...[W]))` for a per-coordinate objective."
        );
    }
    let cadence = reporter.declared_cadences().smallest();
    if cadence.is_zero() {
        return None;
    }

    let matter = polydat::kernel::subcontext::PolydatMatter::builder()
        .program(program.clone())
        .build()
        .ok()?;
    let obj_kernel = parent.build_subscope(matter).ok()?;

    let is_stable_kernel = polydat::dsl::compile::compile_polydat(&format!(
        "extern source: f64\n(stable_value, stable) := is_stable(source, {SETTLE_MARGIN}, \
         {SETTLE_MIN_SAMPLES}, {SETTLE_HORIZON})"
    ))
    .ok()?;
    let interp = SettleInterpreter::new(is_stable_kernel, "source", "stable_value", "stable");
    // Viability gate = the stability horizon's worth of cadence intervals, in
    // WALL-CLOCK. With the usual `window = SETTLE_HORIZON × cadence` sizing this
    // is the rollup window — long enough for the objective's window to clear the
    // prior coordinate before a stable verdict is honored.
    let min_viable = cadence.saturating_mul(SETTLE_HORIZON as u32);
    let eval = SettleEvaluator::new(
        obj_kernel,
        objective,
        "cycle",
        interp,
        SETTLE_TIMEOUT,
        min_viable,
    );
    let register = eval.register();

    let pse = PhaseStopEvaluator::new(Box::new(eval), stop_flag);
    let outcome = pse.outcome_cell();
    // SRD-88 — bind this subscription to THIS execution's context so its
    // delivery fiber pulls the objective as the owning execution (the
    // metric read scopes to its own `exec_id`). Captured in the
    // execution's scope here; `None` in single-run (A1).
    let mut opts = nmbrs_metrics::cadence_reporter::SubscriptionOpts::default();
    if let Some(ctx) = crate::execution_context::try_current() {
        opts.context_wrap = Some(std::sync::Arc::new(move |fut| {
            Box::pin(crate::execution_context::scope(ctx.clone(), fut))
                as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));
    }
    let subscriber = reporter.subscribe(cadence, Box::new(pse), opts).ok()?;
    Some(SettleHandle {
        subscriber,
        register,
        outcome,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase_outcome::{Disposition, Validity};
    use polydat::dsl::compile::{compile_polydat, compile_polydat_interpreter};

    /// The fixed `is_stable` engine fed the per-pulse objective value.
    fn settle_interp() -> SettleInterpreter {
        let kernel = compile_polydat(
            "extern source: f64\n(stable_value, stable) := is_stable(source, 0.05, 4, 8)",
        )
        .expect("is_stable kernel compiles");
        SettleInterpreter::new(kernel, "source", "stable_value", "stable")
    }

    fn obj_kernel(src: &str) -> PolydatKernel {
        compile_polydat_interpreter(src).expect("objective kernel compiles")
    }

    // A constant objective: settles regardless of the poke.
    const STEADY_OBJ: &str = "input cycle: u64\nobj := 5.0";
    // A ramping objective = the poke value (cycle): never settles.
    const RAMP_OBJ: &str = "input cycle: u64\nobj := cycle";

    fn window() -> MetricSet {
        MetricSet::new(Duration::from_secs(1))
    }

    #[test]
    fn interpreter_publishes_settled_value_into_the_register() {
        let mut i = settle_interp();
        let mut last = SettleReading::default();
        for _ in 0..8 {
            last = i.pulse(5.0);
        }
        assert!(last.stable, "a steady value settles");
        assert!(
            (i.register().load().value - 5.0).abs() < 1e-9,
            "register holds the steady level"
        );
    }

    #[test]
    fn evaluator_yields_interrupted_when_settled() {
        let mut ev = SettleEvaluator::new(
            obj_kernel(STEADY_OBJ),
            "obj",
            "cycle",
            settle_interp(),
            Duration::from_secs(60),
            Duration::ZERO,
        );
        let reg = ev.register();
        let mut verdict = None;
        for _ in 0..16 {
            if let Some(o) = ev.evaluate(&window()) {
                verdict = Some(o);
                break;
            }
        }
        let o = verdict.expect("a steady objective settles within the budget");
        assert_eq!(o.disposition, Disposition::Interrupted);
        assert_eq!(o.validity, Validity::Succeeded);
        assert!(
            (reg.load().value - 5.0).abs() < 1e-9,
            "settled register reads 5.0"
        );
    }

    #[test]
    fn evaluator_yields_failed_on_settle_timeout() {
        let mut ev = SettleEvaluator::new(
            obj_kernel(RAMP_OBJ),
            "obj",
            "cycle",
            settle_interp(),
            Duration::from_millis(40),
            Duration::ZERO,
        );
        // First pulse starts the clock; the ramp never settles.
        assert!(
            ev.evaluate(&window()).is_none(),
            "no verdict before timeout"
        );
        std::thread::sleep(Duration::from_millis(55));
        let o = ev.evaluate(&window()).expect("timeout fires a verdict");
        assert_eq!(o.disposition, Disposition::Interrupted);
        assert_eq!(
            o.validity,
            Validity::Failed,
            "a settle timeout is the untrustworthy quadrant"
        );
    }

    #[test]
    fn viability_gate_withholds_settle_until_min_viable_elapses() {
        // A steady objective is "stable" almost immediately, but the gate
        // withholds the settle until `min_viable` of WALL-CLOCK has elapsed —
        // so a windowed objective's rollup has cleared the prior coordinate /
        // warmup transient before its value is trusted (the bug that let a
        // sub-window concurrent eval latch a phantom score).
        let mut ev = SettleEvaluator::new(
            obj_kernel(STEADY_OBJ),
            "obj",
            "cycle",
            settle_interp(),
            Duration::from_secs(60),
            Duration::from_millis(60),
        );
        // Many pulses arrive in a burst (as under concurrent scheduling): the
        // objective is stable, but the gate holds because no real time passed.
        for _ in 0..32 {
            assert!(
                ev.evaluate(&window()).is_none(),
                "stable-but-gated: a burst of pulses must not settle before min_viable wall-clock"
            );
        }
        std::thread::sleep(Duration::from_millis(70));
        let o = ev
            .evaluate(&window())
            .expect("settles once min_viable has elapsed");
        assert_eq!(o.disposition, Disposition::Interrupted);
        assert_eq!(o.validity, Validity::Succeeded);
    }

    #[test]
    fn detects_session_cumulative_reader_only() {
        // `metric(...)` reads session_lifetime (cumulative across all
        // coordinates) → flagged as un-gateable. `metric_window(...)` is
        // a bounded windowed reader, and a plain objective reads nothing
        // → neither is flagged.
        let cum = compile_polydat_interpreter(r#"obj := metric("cycles_total, phase=p", "rate")"#)
            .expect("metric node compiles");
        assert!(program_reads_session_cumulative_metrics(cum.program()));

        let win =
            compile_polydat_interpreter(r#"obj := metric_window("cycles_total, phase=p", "rate")"#)
                .expect("metric_window node compiles");
        assert!(
            !program_reads_session_cumulative_metrics(win.program()),
            "metric_window is windowed, not session-cumulative"
        );

        let plain = compile_polydat_interpreter("obj := 5.0").expect("plain objective compiles");
        assert!(!program_reads_session_cumulative_metrics(plain.program()));
    }
}
