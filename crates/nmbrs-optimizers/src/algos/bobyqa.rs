// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `bobyqa` — a Bound-constrained derivative-free quadratic trust-region
//! method (SRD-86 §9). This is a *separable* (diagonal-Hessian) BOBYQA-
//! style implementation: at each iteration it samples `±radius` along
//! each axis, fits a one-dimensional quadratic per axis through the
//! 3 points, and steps to the (trust-region-clamped, bound-clamped) model
//! minimizer; the radius shrinks when a step fails. It solves quadratic
//! bowls (sphere) essentially exactly and makes steady progress on curved
//! valleys. (Full cross-term interpolation à la Powell's BOBYQA is a
//! future refinement; the separable model is robust and dependency-free.)
//! Minimizes `g = -value`.

use crate::optimizer::{Budget, Eval, Objective, Optimizer, Report, StopReason};
use crate::registry::OptimizerParams;
use crate::space::SearchSpace;

#[derive(Clone)]
pub struct Bobyqa {
    tol: f64,
}

impl Bobyqa {
    pub fn from_params(p: &OptimizerParams) -> Self {
        Self {
            tol: p.get("tol", 1e-8),
        }
    }
}

impl Optimizer for Bobyqa {
    fn name(&self) -> &str {
        "bobyqa"
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
        let lo = space.lower();
        let hi = space.upper();
        let mut radius = space.steps();

        let mut x = space.center();
        let mut gx = -ev.at(&x); // minimize g = -value

        let mut stop = StopReason::BudgetExhausted;
        while ev.budget_left() {
            let mut next = x.clone();
            for i in 0..d {
                if !ev.budget_left() {
                    break;
                }
                let h = radius[i].max(f64::EPSILON);
                let mut xp = x.clone();
                xp[i] = (x[i] + h).min(hi[i]);
                let mut xm = x.clone();
                xm[i] = (x[i] - h).max(lo[i]);
                let gp = -ev.at(&xp);
                let gm = if ev.budget_left() { -ev.at(&xm) } else { gx };

                // Quadratic g(t) ≈ gx + b·t + a·t² through (−h, gm), (0, gx), (+h, gp).
                let a = (gp - 2.0 * gx + gm) / (2.0 * h * h);
                let b = (gp - gm) / (2.0 * h);
                if a > 1e-12 {
                    // Convex along this axis: step to the model minimizer,
                    // clamped to the trust region and bounds.
                    let s = (-b / (2.0 * a)).clamp(-h, h);
                    next[i] = (x[i] + s).clamp(lo[i], hi[i]);
                } else {
                    // Non-convex: descend toward the lower-valued probe.
                    next[i] = if gp < gm { xp[i] } else { xm[i] };
                }
            }
            if !ev.budget_left() {
                break;
            }
            let gnext = -ev.at(&next);
            if gnext < gx - 1e-12 {
                x = next;
                gx = gnext;
                // Modest trust-region growth on a successful step.
                for r in radius.iter_mut() {
                    *r *= 1.25;
                }
            } else {
                // Failed step: shrink the trust region.
                let max_r = radius.iter().cloned().fold(0.0_f64, f64::max);
                if max_r <= self.tol {
                    stop = StopReason::Converged;
                    break;
                }
                for r in radius.iter_mut() {
                    *r *= 0.5;
                }
            }
        }
        ev.into_report(stop)
    }
}
