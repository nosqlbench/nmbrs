// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `cmaes` — separable CMA-ES (SRD-86 §9). A robust population-based
//! evolution strategy for noisy / multimodal / ill-conditioned
//! landscapes where the trust-region (`bobyqa`) and simplex
//! (`nelder_mead`) methods stall. This is the **separable** variant
//! (diagonal covariance, Ros & Hansen 2008): it adapts a per-axis scale
//! — exactly the anisotropy the `centroid_variant` screen exposes —
//! without the cost of a full covariance eigendecomposition. A
//! generation is a batch of λ candidates (the natural parallel unit at
//! the runtime seam). Deterministic from `budget.seed`. Minimizes
//! `g = -value`.

use crate::optimizer::{Budget, Eval, Objective, Optimizer, Report, StopReason};
use crate::registry::OptimizerParams;
use crate::rng::Rng;
use crate::space::SearchSpace;

#[derive(Clone)]
pub struct Cmaes {
    lambda_override: Option<usize>,
    sigma0_scale: f64,
    tol: f64,
}

impl Cmaes {
    pub fn from_params(p: &OptimizerParams) -> Self {
        let l = p.get("lambda", 0.0);
        Self {
            lambda_override: if l >= 1.0 { Some(l as usize) } else { None },
            sigma0_scale: p.get("sigma0_scale", 0.3),
            tol: p.get("tol", 1e-11),
        }
    }
}

impl Optimizer for Cmaes {
    fn name(&self) -> &str {
        "cmaes"
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
        let dn = d as f64;
        let mut rng = Rng::new(budget.seed);
        let lo = space.lower();
        let hi = space.upper();
        let range: f64 = (0..d).map(|i| hi[i] - lo[i]).fold(0.0, f64::max);

        // Population and recombination weights.
        let lambda = self
            .lambda_override
            .unwrap_or_else(|| 4 + (3.0 * dn.ln()).floor() as usize)
            .max(4);
        let mu = lambda / 2;
        let mut weights: Vec<f64> = (0..mu)
            .map(|i| (mu as f64 + 0.5).ln() - ((i + 1) as f64).ln())
            .collect();
        let wsum: f64 = weights.iter().sum();
        for w in weights.iter_mut() {
            *w /= wsum;
        }
        let mueff = 1.0 / weights.iter().map(|w| w * w).sum::<f64>();

        // Adaptation constants (with the separable-CMA rank-one/rank-mu boost).
        let cs = (mueff + 2.0) / (dn + mueff + 5.0);
        let damps = 1.0 + 2.0 * (((mueff - 1.0) / (dn + 1.0)).sqrt() - 1.0).max(0.0) + cs;
        let cc = (4.0 + mueff / dn) / (dn + 4.0 + 2.0 * mueff / dn);
        let mut c1 = 2.0 / ((dn + 1.3).powi(2) + mueff);
        let mut cmu =
            (2.0 * (mueff - 2.0 + 1.0 / mueff) / ((dn + 2.0).powi(2) + mueff)).min(1.0 - c1);
        let boost = (dn + 2.0) / 3.0;
        c1 = (c1 * boost).min(1.0);
        cmu = (cmu * boost).min(1.0 - c1);
        let chi_n = dn.sqrt() * (1.0 - 1.0 / (4.0 * dn) + 1.0 / (21.0 * dn * dn));

        // Distribution state.
        let mut mean = space.center();
        let mut sigma = (self.sigma0_scale * range).max(1e-6);
        let mut diag_c: Vec<f64> = vec![1.0; d];
        let mut diag_d: Vec<f64> = diag_c.iter().map(|v| v.sqrt()).collect();
        let mut ps = vec![0.0; d];
        let mut pc = vec![0.0; d];
        let mut generation: i32 = 0;

        let mut stop = StopReason::BudgetExhausted;
        while ev.budget_left() {
            // Sample + evaluate λ offspring.
            let mut pop: Vec<(Vec<f64>, f64)> = Vec::with_capacity(lambda);
            for _ in 0..lambda {
                if !ev.budget_left() {
                    break;
                }
                let z = rng.normal_vec(d);
                let x: Vec<f64> = (0..d).map(|i| mean[i] + sigma * diag_d[i] * z[i]).collect();
                let g = -ev.at(&x);
                pop.push((x, g));
            }
            if pop.len() < mu {
                break;
            }
            pop.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            // Recombination (mean of the μ best sampled points).
            let old_mean = mean.clone();
            for (i, m) in mean.iter_mut().enumerate() {
                *m = (0..mu).map(|k| weights[k] * pop[k].0[i]).sum();
            }
            let y: Vec<f64> = (0..d).map(|i| (mean[i] - old_mean[i]) / sigma).collect();

            // Step-size evolution path (diagonal C^{-1/2} = 1/diag_d).
            let cs_fac = (cs * (2.0 - cs) * mueff).sqrt();
            for i in 0..d {
                ps[i] = (1.0 - cs) * ps[i] + cs_fac * (y[i] / diag_d[i]);
            }
            let ps_norm = ps.iter().map(|v| v * v).sum::<f64>().sqrt();
            let hsig = (ps_norm / (1.0 - (1.0 - cs).powi(2 * (generation + 1))).sqrt() / chi_n)
                < (1.4 + 2.0 / (dn + 1.0));
            let hsig_f = if hsig { 1.0 } else { 0.0 };

            // Covariance evolution path + diagonal C update.
            let cc_fac = (cc * (2.0 - cc) * mueff).sqrt();
            for i in 0..d {
                pc[i] = (1.0 - cc) * pc[i] + hsig_f * cc_fac * y[i];
            }
            let delta_hsig = (1.0 - hsig_f) * cc * (2.0 - cc);
            for i in 0..d {
                let rank_mu: f64 = (0..mu)
                    .map(|k| {
                        let yi = (pop[k].0[i] - old_mean[i]) / sigma;
                        weights[k] * yi * yi
                    })
                    .sum();
                diag_c[i] = (1.0 - c1 - cmu) * diag_c[i]
                    + c1 * (pc[i] * pc[i] + delta_hsig * diag_c[i])
                    + cmu * rank_mu;
                if diag_c[i] < 1e-20 {
                    diag_c[i] = 1e-20;
                }
                diag_d[i] = diag_c[i].sqrt();
            }

            // Step-size update.
            sigma *= ((cs / damps) * (ps_norm / chi_n - 1.0)).exp();
            if !sigma.is_finite() || sigma <= 0.0 {
                break;
            }
            generation += 1;

            // Convergence: the sampling spread is negligible.
            let spread = sigma * diag_d.iter().cloned().fold(0.0, f64::max);
            if spread < self.tol * range.max(1.0) {
                stop = StopReason::Converged;
                break;
            }
        }
        ev.into_report(stop)
    }
}
