// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The optimizer contract (SRD-86 §Contract surface).
//!
//! An [`Optimizer`] maximizes an [`Objective`] over a
//! [`SearchSpace`](crate::space::SearchSpace) within a [`Budget`]. The
//! objective is queried via [`Objective::query`], which returns an
//! [`Observation`] (value to maximize + feasibility + cost). The result
//! is a [`Report`] (best point, stop reason, optional axis rankings, full
//! history).
//!
//! [`Eval`] is the shared evaluation harness optimizers use internally:
//! it realizes a raw point into the space, queries the objective, folds
//! the best-so-far, counts evaluations against the budget, and turns an
//! infeasible point into a large penalty so the search avoids it.

use crate::space::SearchSpace;

/// The thing an optimizer queries. The runtime's `PhaseFeed` implements
/// this by running a probe phase at the coordinate and reading the
/// objective metric; tests implement it with a synthetic manifold.
pub trait Objective {
    /// Evaluate the objective at `x` (a realized coordinate). The
    /// optimizer **maximizes** [`Observation::value`].
    fn query(&mut self, x: &[f64]) -> Observation;

    /// Evaluate at a reduced *fidelity* in `(0, 1]` (1.0 = full). The
    /// default ignores fidelity and calls [`query`](Self::query); the
    /// runtime seam maps fidelity to e.g. a fraction of phase cycles, and
    /// fidelity-aware test models add fidelity-decaying noise. Used by
    /// multi-fidelity methods (Hyperband).
    fn query_fidelity(&mut self, x: &[f64], _fidelity: f64) -> Observation {
        self.query(x)
    }
}

/// One objective evaluation result.
#[derive(Debug, Clone)]
pub struct Observation {
    /// The value function at `x` (maximized).
    pub value: f64,
    /// `false` ⇒ the point ran but its result is untrustworthy
    /// (infeasible). The optimizer treats it as a heavy penalty and
    /// keeps searching (SRD-86 A7).
    pub feasible: bool,
    /// Observed evaluation cost in seconds (for cost-aware methods).
    pub cost: f64,
    /// Raw metrics at this point, for named-target ranking / rescoring.
    pub metrics: Vec<(String, f64)>,
}

impl Observation {
    /// A feasible observation with the given value and zero recorded cost.
    pub fn value(v: f64) -> Self {
        Self {
            value: v,
            feasible: true,
            cost: 0.0,
            metrics: Vec::new(),
        }
    }

    /// A feasible observation carrying its evaluation cost.
    pub fn valued(v: f64, cost: f64) -> Self {
        Self {
            value: v,
            feasible: true,
            cost,
            metrics: Vec::new(),
        }
    }

    /// An infeasible observation (the point is a feasibility penalty).
    pub fn infeasible() -> Self {
        Self {
            value: f64::NEG_INFINITY,
            feasible: false,
            cost: 0.0,
            metrics: Vec::new(),
        }
    }
}

/// The evaluation budget and reproducibility seed.
#[derive(Debug, Clone)]
pub struct Budget {
    /// Hard ceiling on objective evaluations.
    pub max_evals: usize,
    /// Optional wall-clock ceiling (the harness here only bounds evals;
    /// the runtime seam enforces wall time).
    pub max_seconds: Option<f64>,
    /// Deterministic seed for stochastic optimizers.
    pub seed: u64,
}

impl Budget {
    pub fn evals(max_evals: usize) -> Self {
        Self {
            max_evals,
            max_seconds: None,
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }

    pub fn seeded(max_evals: usize, seed: u64) -> Self {
        Self {
            max_evals,
            max_seconds: None,
            seed,
        }
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::evals(200)
    }
}

/// Why a search ended (SRD-86 A8; maps to a two-axis Outcome at the seam).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The search converged (a method-specific settle test).
    Converged,
    /// The evaluation budget was exhausted before convergence.
    BudgetExhausted,
    /// No feasible point was ever found.
    NoFeasiblePoint,
    /// Externally aborted (kept the best-so-far).
    Aborted,
}

/// Per-axis sensitivity, populated by screening optimizers
/// (`centroid_variant`). `main_effect` is the central-difference slope;
/// `curvature` the second difference of the 3-point OFAT curve.
#[derive(Debug, Clone)]
pub struct AxisImpact {
    pub name: String,
    pub main_effect: f64,
    pub curvature: f64,
}

/// The result of a search.
#[derive(Debug, Clone)]
pub struct Report {
    pub best: Vec<f64>,
    pub best_value: f64,
    pub evals: usize,
    pub stop: StopReason,
    /// Axes ranked by impact (descending), when the optimizer screens.
    pub ranked_axes: Vec<AxisImpact>,
    /// Every (realized point, value) evaluated, in order.
    pub history: Vec<(Vec<f64>, f64)>,
}

/// A non-derivative, multi-factor optimizer over a search space.
pub trait Optimizer {
    /// The registered name (matches the registry key).
    fn name(&self) -> &str;

    /// Run the search. Implementations should respect `budget.max_evals`
    /// by checking [`Eval::budget_left`] in their loops.
    fn optimize(&mut self, space: &SearchSpace, obj: &mut dyn Objective, budget: &Budget)
    -> Report;
}

/// Shared evaluation harness for optimizer implementations. Realizes raw
/// points, tracks the best-so-far and full history, counts evaluations,
/// and converts infeasible points to a heavy penalty so the maximizing
/// search steers away from them.
pub(crate) struct Eval<'a> {
    space: &'a SearchSpace,
    obj: &'a mut dyn Objective,
    pub best: Vec<f64>,
    pub best_value: f64,
    pub evals: usize,
    pub max_evals: usize,
    pub history: Vec<(Vec<f64>, f64)>,
    pub any_feasible: bool,
    penalty: f64,
}

impl<'a> Eval<'a> {
    pub fn new(space: &'a SearchSpace, obj: &'a mut dyn Objective, budget: &Budget) -> Self {
        Self {
            space,
            obj,
            best: space.center(),
            best_value: f64::NEG_INFINITY,
            evals: 0,
            max_evals: budget.max_evals,
            history: Vec::new(),
            any_feasible: false,
            penalty: -1.0e18,
        }
    }

    /// Whether the evaluation budget still allows another query.
    pub fn budget_left(&self) -> bool {
        self.evals < self.max_evals
    }

    /// Evaluate at a raw point (realized into the space) at full fidelity.
    pub fn at(&mut self, raw: &[f64]) -> f64 {
        self.at_fidelity(raw, 1.0)
    }

    /// Evaluate at a raw point and a fidelity in `(0, 1]`. Returns the
    /// scalar to **maximize** — the value, or a heavy penalty if the
    /// point is infeasible / non-finite. Counts toward the budget and
    /// records history at every fidelity, but only updates the
    /// best-so-far for *full*-fidelity (`>= 1.0`) evaluations, so a
    /// trustworthy best is never displaced by a noisy low-fidelity probe.
    pub fn at_fidelity(&mut self, raw: &[f64], fidelity: f64) -> f64 {
        let x = self.space.realize(raw);
        let obs = self.obj.query_fidelity(&x, fidelity);
        self.evals += 1;
        let v = if obs.feasible && obs.value.is_finite() {
            self.any_feasible = true;
            obs.value
        } else {
            self.penalty
        };
        if fidelity >= 1.0 && v > self.best_value {
            self.best_value = v;
            self.best = x.clone();
        }
        self.history.push((x, v));
        v
    }

    /// Seal the harness into a [`Report`]. If no feasible point was ever
    /// found, the stop reason is overridden to [`StopReason::NoFeasiblePoint`].
    pub fn into_report(self, stop: StopReason) -> Report {
        self.into_report_with_axes(stop, Vec::new())
    }

    /// Seal with explicit axis rankings (screening optimizers).
    pub fn into_report_with_axes(self, stop: StopReason, ranked_axes: Vec<AxisImpact>) -> Report {
        let stop = if self.any_feasible {
            stop
        } else {
            StopReason::NoFeasiblePoint
        };
        Report {
            best: self.best,
            best_value: self.best_value,
            evals: self.evals,
            stop,
            ranked_axes,
            history: self.history,
        }
    }
}
