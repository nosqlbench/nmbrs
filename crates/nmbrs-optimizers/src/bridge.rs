// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-86 — the **inventory bridge** to the core runtime (feature `runtime`).
//!
//! This crate's optimizers target a *local* trait so the algorithms are
//! fully testable with no runtime dependency (SRD-86 §2). This module — the
//! only part that depends on `nmbrs-runtime` — adapts each local optimizer to
//! the core [`Optimizer`](nmbrs_runtime::optimize::Optimizer) contract and
//! `inventory::submit!`s an
//! [`OptimizerRegistration`](nmbrs_runtime::optimize::OptimizerRegistration),
//! so the core discovers them at link time without ever naming this crate.

use nmbrs_runtime::optimize as core;
use std::sync::mpsc::{Receiver, Sender, channel};

// ── local ↔ core type conversions (the contracts are structurally
//    identical; the duplication is the price of full decoupling) ──────────

fn to_local_params(p: &core::OptimizerParams) -> crate::OptimizerParams {
    crate::OptimizerParams {
        overrides: p.overrides.clone(),
    }
}

fn to_local_budget(b: &core::Budget) -> crate::Budget {
    crate::Budget {
        max_evals: b.max_evals,
        max_seconds: b.max_seconds,
        seed: b.seed,
    }
}

fn to_local_changeover(c: core::Changeover) -> crate::Changeover {
    match c {
        core::Changeover::Control => crate::Changeover::Control,
        core::Changeover::Coordinate => crate::Changeover::Coordinate,
        core::Changeover::Fixture => crate::Changeover::Fixture,
    }
}

fn to_local_kind(k: &core::AxisKind) -> crate::AxisKind {
    match k {
        core::AxisKind::Continuous { lo, hi, min_step } => crate::AxisKind::Continuous {
            lo: *lo,
            hi: *hi,
            min_step: *min_step,
        },
        // Ordinal detents project to their numbers; the local algorithms are f64.
        core::AxisKind::Discrete { detents } => crate::AxisKind::Discrete {
            detents: detents.iter().map(core::AxisValue::as_num).collect(),
        },
        // Nominal options project to their STABLE positional indices `0..N`; the
        // numeric stub maps back to the same position (order preserved, not sorted).
        core::AxisKind::Categorical { options } => crate::AxisKind::Discrete {
            detents: (0..options.len()).map(|i| i as f64).collect(),
        },
    }
}

fn to_local_space(s: &core::SearchSpace) -> crate::SearchSpace {
    crate::SearchSpace::new(
        s.axes
            .iter()
            .map(|a| crate::Axis {
                name: a.name.clone(),
                kind: to_local_kind(&a.kind),
                changeover: to_local_changeover(a.changeover),
            })
            .collect(),
    )
}

/// Project a local f64 coordinate (what a numeric algorithm proposes) back onto
/// the core [`AxisValue`] coordinate, snapping per axis kind: continuous →
/// clamped/quantized `Num`; ordinal → nearest detent; categorical → the option
/// at the rounded index. This is the numeric stub the metric-space solvers ride
/// on; labels never reach the algorithm and are restored here.
fn project_to_coord(space: &core::SearchSpace, xf: &[f64]) -> Vec<core::AxisValue> {
    space
        .axes
        .iter()
        .zip(xf)
        .map(|(axis, &v)| match &axis.kind {
            core::AxisKind::Continuous { lo, hi, min_step } => {
                let c = v.clamp(*lo, *hi);
                let c = if *min_step > 0.0 {
                    (lo + ((c - lo) / min_step).round() * min_step).clamp(*lo, *hi)
                } else {
                    c
                };
                core::AxisValue::Num(c)
            }
            core::AxisKind::Discrete { detents } => detents
                .iter()
                .min_by(|a, b| {
                    (a.as_num() - v)
                        .abs()
                        .partial_cmp(&(b.as_num() - v).abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .cloned()
                .unwrap_or(core::AxisValue::Num(v)),
            core::AxisKind::Categorical { options } => {
                let n = options.len().max(1) as i64;
                let i = (v.round() as i64).clamp(0, n - 1) as usize;
                options.get(i).cloned().unwrap_or(core::AxisValue::Num(v))
            }
        })
        .collect()
}

// ── loop → pull-through source bridge (SRD-86) ───────────────────────────
//
// Each local optimizer is loop-form: it drives an `Objective` to completion.
// The core contract is a pull-through `CoordinateSource`. We adapt one to the
// other by running the search loop on a worker thread: every `query` ferries
// its coordinate out through `step` and blocks for the value. The executor
// only ever sees a clean `FeedbackSource`, so the loop stays the algorithm's
// natural form — the loop-vs-state-machine choice never leaks past this seam.

/// The local `Objective` the worker thread queries. Each call hands the
/// coordinate to the consumer (via `step`) and blocks for the returned value.
struct ChannelObjective {
    coord_tx: Sender<Option<Vec<f64>>>,
    value_rx: Receiver<f64>,
}

impl crate::Objective for ChannelObjective {
    fn query(&mut self, x: &[f64]) -> crate::Observation {
        // Hand the coordinate out; a closed channel means the consumer is
        // gone, so wind the loop down by reporting infeasible.
        if self.coord_tx.send(Some(x.to_vec())).is_err() {
            return crate::Observation::infeasible();
        }
        match self.value_rx.recv() {
            Ok(v) => crate::Observation::value(v),
            Err(_) => crate::Observation::infeasible(),
        }
    }
}

/// A pull-through [`core::CoordinateSource`] backed by a loop-form optimizer
/// running on a worker thread. `step` feeds back the value(s) for the
/// coordinate(s) it last yielded and returns the next coordinate the loop
/// wants evaluated, or `None` once the loop finishes.
struct ThreadBridge {
    coord_rx: Receiver<Option<Vec<f64>>>,
    value_tx: Sender<f64>,
    /// The core space — used to project the algorithm's f64 coordinates back
    /// onto `AxisValue` (restoring detents / labels).
    core_space: core::SearchSpace,
    done: bool,
}

impl ThreadBridge {
    fn spawn<L: crate::Optimizer + Send + 'static>(
        mut inner: L,
        local_space: crate::SearchSpace,
        core_space: core::SearchSpace,
        budget: crate::Budget,
    ) -> Self {
        let (coord_tx, coord_rx) = channel::<Option<Vec<f64>>>();
        let (value_tx, value_rx) = channel::<f64>();
        std::thread::spawn(move || {
            let mut obj = ChannelObjective {
                coord_tx: coord_tx.clone(),
                value_rx,
            };
            let _ = inner.optimize(&local_space, &mut obj, &budget);
            let _ = coord_tx.send(None); // loop finished — no more coordinates
        });
        Self {
            coord_rx,
            value_tx,
            core_space,
            done: false,
        }
    }
}

impl core::FeedbackSource for ThreadBridge {
    fn step(&mut self, evaluated: &[(core::Coord, f64)]) -> Option<Vec<core::Coord>> {
        if self.done {
            return None;
        }
        // Hand back the value(s) for the coordinate(s) previously yielded,
        // unblocking the worker's `query`.
        for (_coord, value) in evaluated {
            let _ = self.value_tx.send(*value);
        }
        // Receive the next coordinate the loop wants evaluated.
        match self.coord_rx.recv() {
            Ok(Some(xf)) => Some(vec![project_to_coord(&self.core_space, &xf)]),
            Ok(None) | Err(_) => {
                self.done = true;
                None
            }
        }
    }
}

impl core::CoordinateSource for ThreadBridge {
    fn as_feedback(&mut self) -> Option<&mut dyn core::FeedbackSource> {
        Some(self)
    }
}

/// Adapts a local optimizer to the core [`Optimizer`](core::Optimizer)
/// contract — a stateless functor producing a pull-through coordinate source
/// — carrying its registry name + markdown doc.
pub struct CoreBridge<L: crate::Optimizer> {
    inner: L,
    name: &'static str,
    doc: &'static str,
}

impl<L: crate::Optimizer + Clone + Send + 'static> core::Optimizer for CoreBridge<L> {
    fn name(&self) -> &str {
        self.name
    }
    fn doc_md(&self) -> &str {
        self.doc
    }
    fn coordinate_source(
        &self,
        space: &core::SearchSpace,
        budget: &core::Budget,
        _lex: Box<dyn core::PullSource>,
    ) -> Box<dyn core::CoordinateSource> {
        Box::new(ThreadBridge::spawn(
            self.inner.clone(),
            to_local_space(space),
            space.clone(),
            to_local_budget(budget),
        ))
    }
}

/// Submit one optimizer to the core's link-time registry. `$build` receives
/// the (local) params and returns the local optimizer.
macro_rules! register_optimizer {
    ($name:literal, $doc:path, |$p:ident| $build:expr) => {
        inventory::submit! {
            core::OptimizerRegistration {
                name: || $name,
                doc_md: || $doc,
                make: |params| {
                    let $p = to_local_params(params);
                    Box::new(CoreBridge { inner: $build, name: $name, doc: $doc })
                },
            }
        }
    };
}

register_optimizer!(
    "cost_greedy_traversal",
    crate::docs::COST_GREEDY_TRAVERSAL,
    |_p| { crate::algos::traversal::CostGreedyTraversal }
);
register_optimizer!("centroid_variant", crate::docs::CENTROID_VARIANT, |p| {
    crate::algos::centroid::CentroidVariant::from_params(&p)
});
register_optimizer!("nelder_mead", crate::docs::NELDER_MEAD, |p| {
    crate::algos::nelder_mead::NelderMead::from_params(&p)
});
register_optimizer!("hooke_jeeves", crate::docs::HOOKE_JEEVES, |p| {
    crate::algos::hooke_jeeves::HookeJeeves::from_params(&p)
});
register_optimizer!("bobyqa", crate::docs::BOBYQA, |p| {
    crate::algos::bobyqa::Bobyqa::from_params(&p)
});
register_optimizer!("cmaes", crate::docs::CMAES, |p| {
    crate::algos::cmaes::Cmaes::from_params(&p)
});
register_optimizer!("bayes_opt", crate::docs::BAYES_OPT, |p| {
    crate::algos::bayes_opt::BayesOpt::from_params(&p)
});
register_optimizer!("hyperband", crate::docs::HYPERBAND, |p| {
    crate::algos::hyperband::Hyperband::from_params(&p)
});

#[cfg(test)]
mod tests {
    use super::*;

    /// Maximize `-||x - target||²` (a single bowl, max 0 at `target`).
    struct ShiftedSphere {
        target: Vec<f64>,
    }
    impl core::Objective for ShiftedSphere {
        fn query(&mut self, x: &[core::AxisValue]) -> core::Observation {
            let s: f64 = x
                .iter()
                .map(core::AxisValue::as_num)
                .zip(&self.target)
                .map(|(v, t)| (v - t) * (v - t))
                .sum();
            core::Observation::value(-s)
        }
    }

    fn cont_space(dims: usize, lo: f64, hi: f64) -> core::SearchSpace {
        core::SearchSpace::new(
            (0..dims)
                .map(|i| core::Axis {
                    name: format!("x{i}"),
                    kind: core::AxisKind::Continuous {
                        lo,
                        hi,
                        min_step: 0.0,
                    },
                    changeover: core::Changeover::Coordinate,
                })
                .collect(),
        )
    }

    /// Drives a loop-form local optimizer through the core driver: it goes
    /// `coordinate_source` → `ThreadBridge` (a worker thread runs the search
    /// loop) → `step` (objective values fed back per pulled coordinate). This
    /// is registry-free — it constructs the `CoreBridge` directly, so it
    /// exercises the bridge without depending on the inventory link path.
    #[test]
    fn thread_bridge_drives_a_loop_optimizer_through_the_core_driver() {
        let bridge = CoreBridge {
            inner: crate::algos::cmaes::Cmaes::from_params(&crate::OptimizerParams::new()),
            name: "cmaes",
            doc: crate::docs::CMAES,
        };
        let space = cont_space(2, -5.0, 5.0);
        let mut obj = ShiftedSphere {
            target: vec![1.5, -2.0],
        };
        let report = core::Optimizer::optimize(
            &bridge,
            &space,
            &mut obj,
            &core::Budget::seeded(400, 0xC0FFEE),
        );
        assert!(report.evals <= 400, "overran budget: {}", report.evals);
        assert!(
            report.best_value > -2.0e-2,
            "core-bridged cmaes failed to converge: best_value {} at {:?}",
            report.best_value,
            report.best
        );
    }

    /// The numeric stub: a categorical axis projects down to ordinal indices
    /// for the f64 algorithm and back up to its labels (SRD-86 §2). Labels
    /// never reach the algorithm.
    #[test]
    fn categorical_axis_round_trips_labels_through_the_numeric_stub() {
        let space = core::SearchSpace::new(vec![core::Axis {
            name: "metric".into(),
            kind: core::AxisKind::Categorical {
                options: vec![
                    core::AxisValue::Label("cosine".into()),
                    core::AxisValue::Label("dot".into()),
                    core::AxisValue::Label("euclidean".into()),
                ],
            },
            changeover: core::Changeover::Coordinate,
        }]);
        // Down: categorical → ordinal indices {0, 1, 2}.
        let local = to_local_space(&space);
        assert!(
            matches!(&local.axes[0].kind, crate::AxisKind::Discrete { detents } if *detents == vec![0.0, 1.0, 2.0])
        );
        // Up: index → label, with rounding and clamping.
        assert_eq!(
            project_to_coord(&space, &[1.0]),
            vec![core::AxisValue::Label("dot".into())]
        );
        assert_eq!(
            project_to_coord(&space, &[2.4]),
            vec![core::AxisValue::Label("euclidean".into())]
        );
        assert_eq!(
            project_to_coord(&space, &[-3.0]),
            vec![core::AxisValue::Label("cosine".into())]
        );
    }

    /// Value-native path: the built-in `sweep` enumerates the categorical
    /// options as labels directly (no numeric cast); the objective reads the
    /// label, and the reported best carries it (SRD-86 A4).
    #[test]
    fn null_selects_the_best_categorical_label() {
        struct PrefersDot;
        impl core::Objective for PrefersDot {
            fn query(&mut self, x: &[core::AxisValue]) -> core::Observation {
                let v = match &x[0] {
                    core::AxisValue::Label(s) if s == "dot" => 1.0,
                    _ => 0.0,
                };
                core::Observation::value(v)
            }
        }
        let space = core::SearchSpace::new(vec![core::Axis {
            name: "metric".into(),
            kind: core::AxisKind::Categorical {
                options: vec![
                    core::AxisValue::Label("cosine".into()),
                    core::AxisValue::Label("dot".into()),
                    core::AxisValue::Label("euclidean".into()),
                ],
            },
            changeover: core::Changeover::Coordinate,
        }]);
        let report = core::Optimizer::optimize(
            &core::SweepOptimizer,
            &space,
            &mut PrefersDot,
            &core::Budget::seeded(10, 0),
        );
        assert_eq!(report.best, vec![core::AxisValue::Label("dot".into())]);
        assert_eq!(report.best_value, 1.0);
    }
}
