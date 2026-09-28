// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `hyperband` — multi-fidelity bandit optimization (SRD-86 §9; Li et al.
//! 2017). Cost-aware in the *fidelity* axis: evaluate many random
//! configurations cheaply (a fraction of full resource), then allocate
//! more resource only to survivors via successive halving. It turns
//! "fidelity = phase cycles" into a first-class lever — exactly the
//! economy of running short query phases before long ones. Each bracket
//! winner is re-evaluated at full fidelity so the reported best is
//! trustworthy. Deterministic from `budget.seed`.

use crate::optimizer::{Budget, Eval, Objective, Optimizer, Report, StopReason};
use crate::registry::OptimizerParams;
use crate::rng::Rng;
use crate::space::SearchSpace;

#[derive(Clone)]
pub struct Hyperband {
    eta: f64,
    max_resource: f64,
}

impl Hyperband {
    pub fn from_params(p: &OptimizerParams) -> Self {
        Self {
            eta: p.get("eta", 3.0),
            max_resource: p.get("max_resource", 81.0),
        }
    }
}

impl Optimizer for Hyperband {
    fn name(&self) -> &str {
        "hyperband"
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
        let mut rng = Rng::new(budget.seed);

        let eta = self.eta.max(2.0);
        let r_max = self.max_resource.max(eta);
        let s_max = (r_max.ln() / eta.ln()).floor() as i32;
        let b_total = (s_max as f64 + 1.0) * r_max;

        let mut stop = StopReason::Converged;
        'brackets: for s in (0..=s_max).rev() {
            if !ev.budget_left() {
                stop = StopReason::BudgetExhausted;
                break;
            }
            // Initial config count and resource for this bracket.
            let n = ((b_total / r_max) * (eta.powi(s) / (s as f64 + 1.0))).ceil() as usize;
            let r = r_max * eta.powi(-s);
            let mut configs: Vec<Vec<f64>> = (0..n.max(1))
                .map(|_| (0..d).map(|i| rng.range(lo[i], hi[i])).collect())
                .collect();

            // Successive halving.
            for i in 0..=s {
                let n_i = (n as f64 * eta.powi(-i)).floor() as usize;
                let r_i = r * eta.powi(i);
                let fidelity = (r_i / r_max).clamp(1e-3, 1.0);

                let mut scored: Vec<(Vec<f64>, f64)> = Vec::with_capacity(configs.len());
                for c in &configs {
                    if !ev.budget_left() {
                        stop = StopReason::BudgetExhausted;
                        break 'brackets;
                    }
                    let v = ev.at_fidelity(c, fidelity);
                    scored.push((c.clone(), v));
                }
                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let keep = (((n_i as f64) / eta).floor() as usize).clamp(1, scored.len());
                configs = scored.into_iter().take(keep).map(|(c, _)| c).collect();
                if configs.len() <= 1 {
                    break;
                }
            }

            // Register the bracket winner at full fidelity (updates best).
            if ev.budget_left()
                && let Some(winner) = configs.first()
            {
                ev.at(&winner.clone());
            }
        }
        ev.into_report(stop)
    }
}
