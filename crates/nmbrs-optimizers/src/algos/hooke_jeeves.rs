// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `hooke_jeeves` — pattern search (SRD-86 §9). A simple, robust
//! derivative-free method: an *exploratory* phase probes `±step` along
//! each axis from a base point, and a *pattern* phase extrapolates the
//! successful direction; the step shrinks when no improvement is found.
//! Because it varies one axis at a time it maps naturally onto the
//! changeover economy (cheap axes inner, expensive outer). Minimizes
//! `g = -value`.

use crate::optimizer::{Budget, Eval, Objective, Optimizer, Report, StopReason};
use crate::registry::OptimizerParams;
use crate::space::SearchSpace;

#[derive(Clone)]
pub struct HookeJeeves {
    tol: f64,
    shrink: f64,
}

impl HookeJeeves {
    pub fn from_params(p: &OptimizerParams) -> Self {
        Self {
            tol: p.get("tol", 1e-8),
            shrink: p.get("shrink", 0.5),
        }
    }
}

/// Exploratory move: from `base`, try `±step[i]` on each axis in turn,
/// keeping any improvement. Returns the (possibly improved) point and its
/// `g` value.
fn explore(ev: &mut Eval<'_>, base: &[f64], gbase: f64, step: &[f64]) -> (Vec<f64>, f64) {
    let d = base.len();
    let mut x = base.to_vec();
    let mut gx = gbase;
    for i in 0..d {
        for &dir in &[1.0_f64, -1.0] {
            if !ev.budget_left() {
                return (x, gx);
            }
            let mut trial = x.clone();
            trial[i] += dir * step[i];
            let gt = -ev.at(&trial);
            if gt < gx {
                x = trial;
                gx = gt;
                break; // accept the first improving direction on this axis
            }
        }
    }
    (x, gx)
}

impl Optimizer for HookeJeeves {
    fn name(&self) -> &str {
        "hooke_jeeves"
    }

    fn optimize(
        &mut self,
        space: &SearchSpace,
        obj: &mut dyn Objective,
        budget: &Budget,
    ) -> Report {
        let mut ev = Eval::new(space, obj, budget);
        let d = space.dims();
        if d == 0 {
            ev.at(&[]);
            return ev.into_report(StopReason::Converged);
        }
        let mut step = space.steps();
        let mut base = space.center();
        let mut gbase = -ev.at(&base);

        let mut stop = StopReason::BudgetExhausted;
        while ev.budget_left() {
            let (x, gx) = explore(&mut ev, &base, gbase, &step);
            if gx < gbase {
                // Improvement: pattern-move loop — repeatedly extrapolate
                // base → x and explore around the projected point.
                let mut b_old = base.clone();
                base = x;
                gbase = gx;
                while ev.budget_left() {
                    // Pattern point: reflect the old base through the new.
                    let pattern: Vec<f64> = (0..d).map(|j| 2.0 * base[j] - b_old[j]).collect();
                    let gpat = -ev.at(&pattern);
                    let (x2, g2) = explore(&mut ev, &pattern, gpat, &step);
                    if g2 < gbase {
                        b_old = base.clone();
                        base = x2;
                        gbase = g2;
                    } else {
                        break;
                    }
                }
            } else {
                // No improvement at this resolution: shrink the step.
                let max_step = step.iter().cloned().fold(0.0_f64, f64::max);
                if max_step <= self.tol {
                    stop = StopReason::Converged;
                    break;
                }
                for s in step.iter_mut() {
                    *s *= self.shrink;
                }
            }
        }
        ev.into_report(stop)
    }
}
