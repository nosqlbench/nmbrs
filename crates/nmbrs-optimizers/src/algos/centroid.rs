// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `centroid_variant` — the sensitivity-screening optimizer (SRD-86 §8a).
//!
//! Not a minimizer: it finds *which factors matter*, cheaply, so a real
//! optimizer can then run on the high-impact subset. With `1 + 2k`
//! probes it evaluates the centroid baseline `f0`, then steps each axis
//! `±Δ` one-factor-at-a-time (a 3-point curve `(f⁻, f0, f⁺)` per axis),
//! and ranks the axes by main effect (central-difference slope) and
//! curvature (second difference). The reported `best` is the best probe;
//! `ranked_axes` is the screen's product.

use crate::optimizer::{AxisImpact, Budget, Eval, Objective, Optimizer, Report, StopReason};
use crate::registry::OptimizerParams;
use crate::space::SearchSpace;

#[derive(Clone)]
pub struct CentroidVariant {
    /// Scales the per-axis probe step (`Δ = delta_scale * axis.step()`).
    delta_scale: f64,
}

impl CentroidVariant {
    pub fn from_params(p: &OptimizerParams) -> Self {
        Self {
            delta_scale: p.get("delta_scale", 1.0),
        }
    }
}

impl Optimizer for CentroidVariant {
    fn name(&self) -> &str {
        "centroid_variant"
    }

    fn optimize(
        &mut self,
        space: &SearchSpace,
        obj: &mut dyn Objective,
        budget: &Budget,
    ) -> Report {
        let mut ev = Eval::new(space, obj, budget);
        let d = space.dims();
        let center = space.center();
        let steps = space.steps();

        // Centre of gravity → baseline.
        let f0 = ev.at(&center);

        let mut scored: Vec<(AxisImpact, f64)> = Vec::with_capacity(d);
        for i in 0..d {
            let delta = (steps[i] * self.delta_scale).max(f64::EPSILON);
            let mut xp = center.clone();
            xp[i] += delta;
            let mut xm = center.clone();
            xm[i] -= delta;

            let fp = if ev.budget_left() { ev.at(&xp) } else { f0 };
            let fm = if ev.budget_left() { ev.at(&xm) } else { f0 };

            // Effects on the objective value (which we maximize).
            let main_effect = (fp - fm) / (2.0 * delta);
            let curvature = (fp - 2.0 * f0 + fm) / (delta * delta);
            // Combined, unit-consistent impact: the predicted change in
            // the objective over one step (a gradient term + a curvature
            // term). Robust whether the centroid sits on a slope
            // (gradient-dominated) or at a symmetric optimum where the
            // gradient is ~0 (curvature-dominated).
            let impact = main_effect.abs() * delta + 0.5 * curvature.abs() * delta * delta;
            scored.push((
                AxisImpact {
                    name: space.axes[i].name.clone(),
                    main_effect,
                    curvature,
                },
                impact,
            ));
        }

        // Rank by the combined impact (descending) — the axes whose
        // one-step variation moves the objective most.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let axes: Vec<AxisImpact> = scored.into_iter().map(|(a, _)| a).collect();

        ev.into_report_with_axes(StopReason::Converged, axes)
    }
}
