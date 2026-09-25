// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-86 §4–§6 — the **Control-class actuation daemon**.
//!
//! Where a `Coordinate`-class axis is actuated by *re-running* the phase once
//! per coordinate (`executor::dispatch_optimization`'s default loop), a
//! `Control`-class axis is actuated by **live-retargeting** the phase's dynamic
//! control (SRD-23 `concurrency` / `rate`) on **one continuous phase** — no
//! restart. The optimizer becomes a servoing daemon: pull the next setting →
//! retarget the live control (confirmed-apply, `await`ed) → wait for the
//! windowed objective to *settle* at that setting → read it → `step`. Repeat
//! until the budget is spent, then stop the phase.
//!
//! ## Why an async task, not a cadence callback
//!
//! The settle detector ([`super::settle`]) rides the **sync** metrics-cadence
//! callback, but [`Control::set`](nmbrs_metrics::controls::Control::set) is
//! **async** (confirmed-apply over async appliers). So the servo is a
//! concurrent **async future**, [`tokio::join!`]'d with the activity loop inside
//! `run_phase`. It reuses [`start_settle`] (which legitimately rides the sync
//! callback) only to *read* the settled objective; the async retarget lives
//! here, `await`ed directly so setting *N* is confirmed in effect before it is
//! settled and read.
//!
//! The live control is resolved off the **phase component** — where the fiber
//! pool / rate limiter declared it in `Activity::attach_component`, which runs
//! *before* the activity loop, so the handle always exists by the time the
//! servo retargets.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use nmbrs_metrics::cadence_reporter::CadenceReporter;
use nmbrs_metrics::component::Component;
use nmbrs_metrics::controls::ControlOrigin;
use polydat::kernel::PolydatKernel;

use super::settle::start_settle;
use super::{Budget, Coord, LexSource, OptimizerParams, PullSource, SearchSpace};

/// One `Control`-class axis: its index in a coordinate tuple + the name of the
/// live control (`"concurrency"` / `"rate"`) it servos.
#[derive(Debug, Clone)]
pub struct ControlAxis {
    pub axis_idx: usize,
    pub control: String,
}

/// The best coordinate + objective value a servoing run found.
#[derive(Clone)]
pub struct ServoBest {
    pub coord: Coord,
    pub value: f64,
}

/// What a servoing run produced — read by the dispatch after the continuous
/// phase returns. `best` is `None` if no setting ever yielded a value (e.g. the
/// phase ended before the first settle).
#[derive(Clone, Default)]
pub struct ServoOutcome {
    pub best: Option<ServoBest>,
    pub evals: usize,
}

/// The servoing job, handed from `dispatch_optimization` to `run_phase` through
/// `ctx.optimize_servo`. The `result` cell is written by [`servo`] and read by
/// the dispatch after the continuous phase returns. (`Clone` only so the
/// enclosing `ExecCtx` stays `Clone`; the spec is moved, not cloned, in use.)
#[derive(Clone)]
pub struct ServoSpec {
    pub method: String,
    pub params: Vec<(String, f64)>,
    pub objective: String,
    pub max_evals: usize,
    pub seed: u64,
    pub space: SearchSpace,
    pub controls: Vec<ControlAxis>,
    pub result: Arc<ArcSwap<ServoOutcome>>,
}

/// Poll interval for a per-setting settle verdict (the settle detector runs on
/// the cadence worker; the servo awaits its `outcome` cell).
const SETTLE_POLL: Duration = Duration::from_millis(50);

/// Retarget one live control to `value` (SRD-23 confirmed-apply). Resolves the
/// erased handle off the phase component, where the applier declared it.
async fn retarget(
    phase_component: &Arc<RwLock<Component>>,
    control: &str,
    value: f64,
) -> Result<(), String> {
    let erased = {
        let guard = phase_component.read().unwrap_or_else(|e| e.into_inner());
        guard.find_control_erased_up(control)
    };
    let Some(erased) = erased else {
        return Err(format!(
            "optimizer Control-class axis targets control '{control}', but the phase \
             declares no such control"
        ));
    };
    erased
        .set_f64(
            value,
            ControlOrigin::Api {
                source: "optimizer".into(),
            },
        )
        .await
        .map(|_rev| ())
        .map_err(|e| format!("optimizer retarget '{control}' = {value}: {e}"))
}

/// Drive the optimizer over **one continuous phase** by live-retargeting its
/// controls (SRD-86 Control-class actuation). Runs concurrent with the activity
/// loop (`tokio::join!` in `run_phase`):
///
/// 1. pull the next coordinate, **retarget** each control axis to it (`await`ed,
///    confirmed-apply);
/// 2. **settle** the windowed objective at that setting via [`start_settle`]
///    (its verdict raises a *throwaway* per-setting flag, never the phase stop
///    flag), reading the stabilized value from the settle register;
/// 3. `step` the optimizer and track the best.
///
/// On budget exhaustion (or when the phase ends first — `phase_done`), it
/// publishes the best into `spec.best` and raises `stop_flag` to end the phase.
pub async fn servo(
    spec: ServoSpec,
    stop_flag: Arc<AtomicBool>,
    reporter: Arc<CadenceReporter>,
    parent: Arc<PolydatKernel>,
    phase_kernel: Arc<PolydatKernel>,
    phase_component: Arc<RwLock<Component>>,
    phase_done: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut params = OptimizerParams::new();
    for (k, v) in &spec.params {
        params = params.with(k.clone(), *v);
    }
    let optimizer = super::by_name(&spec.method, &params)
        .ok_or_else(|| format!("unknown optimizer method '{}'", spec.method))?;
    let budget = Budget::seeded(spec.max_evals, spec.seed);
    let lex: Box<dyn PullSource> = Box::new(LexSource::new(&spec.space));
    let mut src = optimizer.coordinate_source(&spec.space, &budget, lex);

    let mut best_value = f64::NEG_INFINITY;
    let mut best_coord: Option<Coord> = None;
    let mut evals = 0usize;

    // Accumulate any fatal servoing error here; ALL exit paths still publish the
    // best-so-far and stop the phase, so the join never hangs and the dispatch
    // always reads a result.
    let mut err: Option<String> = None;
    let mut batch = crate::executor::source_next(&mut src, &[]);
    'outer: while let Some(coords) = batch.take() {
        let mut evaluated: Vec<(Coord, f64)> = Vec::new();
        for coord in coords {
            if evals >= spec.max_evals || phase_done.load(Ordering::Relaxed) {
                break 'outer;
            }

            // (1) Retarget every control axis to this coordinate.
            for ca in &spec.controls {
                let value = coord[ca.axis_idx].as_num();
                if let Err(e) = retarget(&phase_component, &ca.control, value).await {
                    err = Some(e);
                    break 'outer;
                }
            }

            // (2) Settle the windowed objective at this setting. A throwaway
            // flag absorbs the settle's own stop verdict so it does NOT end the
            // phase (the servo owns the phase stop, on budget exhaustion).
            let settle_done = Arc::new(AtomicBool::new(false));
            let handle = match start_settle(
                &parent,
                &phase_kernel,
                &spec.objective,
                &reporter,
                settle_done,
            ) {
                Ok(handle) => handle,
                Err(super::settle::SettleSkip::NotWindowed) => {
                    err = Some(format!(
                        "optimizer objective '{}' is not a windowed metric — a Control-class \
                         sweep settles the live windowed objective per setting; use \
                         `metric_window(...)` or `metricsql_scalar(rate(...[W]))`",
                        spec.objective
                    ));
                    break 'outer;
                }
                Err(e) => {
                    err = Some(format!(
                        "optimizer objective '{}' cannot be settled: {e}",
                        spec.objective
                    ));
                    break 'outer;
                }
            };
            let value = loop {
                if phase_done.load(Ordering::Relaxed) {
                    reporter.unsubscribe(handle.subscriber);
                    break 'outer;
                }
                if handle.outcome.load().is_some() {
                    let v = handle.register.load().value;
                    reporter.unsubscribe(handle.subscriber);
                    break v;
                }
                tokio::time::sleep(SETTLE_POLL).await;
            };

            // (3) Step the optimizer and track the best.
            evals += 1;
            if value > best_value {
                best_value = value;
                best_coord = Some(coord.clone());
            }
            evaluated.push((coord, value));
        }
        batch = crate::executor::source_next(&mut src, &evaluated);
    }

    spec.result.store(Arc::new(ServoOutcome {
        best: best_coord.map(|coord| ServoBest {
            coord,
            value: best_value,
        }),
        evals,
    }));
    // End the continuous phase (read at its next cycle boundary).
    stop_flag.store(true, Ordering::Relaxed);
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
