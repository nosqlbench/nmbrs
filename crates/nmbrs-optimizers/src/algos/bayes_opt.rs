// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `bayes_opt` — Bayesian optimization with a Gaussian-process surrogate
//! and Expected-Improvement acquisition (SRD-86 §9). The premier method
//! for *expensive* black-box evaluation: it fits a GP (RBF kernel, fixed
//! hyperparameters) to the observations, then proposes the point of
//! maximum Expected Improvement — sample-efficient, so it spends few
//! costly evaluations. After a small random initial design it iterates
//! fit → maximize-EI → evaluate. Deterministic from `budget.seed`.

use crate::algos::linalg::{chol_solve, cholesky, forward_solve, norm_cdf, norm_pdf};
use crate::optimizer::{Budget, Eval, Objective, Optimizer, Report, StopReason};
use crate::registry::OptimizerParams;
use crate::rng::Rng;
use crate::space::SearchSpace;

#[derive(Clone)]
pub struct BayesOpt {
    xi: f64,
    init: usize,
    candidates: usize,
    lengthscale: f64,
}

impl BayesOpt {
    pub fn from_params(p: &OptimizerParams) -> Self {
        Self {
            xi: p.get("xi", 0.01),
            init: p.get("init", 0.0) as usize,
            candidates: p.get("candidates", 0.0) as usize,
            lengthscale: p.get("lengthscale", 0.0),
        }
    }
}

const SF2: f64 = 1.0; // signal variance
const NOISE: f64 = 1e-6; // jitter / observation noise

impl Optimizer for BayesOpt {
    fn name(&self) -> &str {
        "bayes_opt"
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

        // RBF lengthscale: a quarter of the box diagonal unless overridden.
        let diag = (0..d).map(|i| (hi[i] - lo[i]).powi(2)).sum::<f64>().sqrt();
        let ell = if self.lengthscale > 0.0 {
            self.lengthscale
        } else {
            (0.25 * diag).max(1e-6)
        };
        let n_cand = if self.candidates >= 1 {
            self.candidates
        } else {
            (200 * d).max(256)
        };

        // Initial random design.
        let n_init = if self.init >= 1 { self.init } else { 2 * d + 2 };
        let mut xs: Vec<Vec<f64>> = Vec::new();
        let mut ys: Vec<f64> = Vec::new();
        for _ in 0..n_init {
            if !ev.budget_left() {
                break;
            }
            let raw: Vec<f64> = (0..d).map(|i| rng.range(lo[i], hi[i])).collect();
            let xr = space.realize(&raw);
            let y = ev.at(&raw);
            xs.push(xr);
            ys.push(y);
        }

        let kernel = |a: &[f64], b: &[f64]| -> f64 {
            let d2: f64 = a.iter().zip(b).map(|(p, q)| (p - q) * (p - q)).sum();
            SF2 * (-0.5 * d2 / (ell * ell)).exp()
        };

        let mut stop = StopReason::BudgetExhausted;
        while ev.budget_left() {
            let n = xs.len();
            if n == 0 {
                break;
            }
            // Standardize observations.
            let ymean = ys.iter().sum::<f64>() / n as f64;
            let yvar = (ys.iter().map(|v| (v - ymean).powi(2)).sum::<f64>() / n as f64).max(1e-12);
            let ystd = yvar.sqrt();
            let yz: Vec<f64> = ys.iter().map(|v| (v - ymean) / ystd).collect();

            // Build and factor the kernel matrix.
            let mut kmat = vec![vec![0.0; n]; n];
            for i in 0..n {
                for j in 0..n {
                    kmat[i][j] = kernel(&xs[i], &xs[j]) + if i == j { NOISE } else { 0.0 };
                }
            }
            if !cholesky(&mut kmat) {
                // Ill-conditioned: take a random exploration point and retry.
                let raw: Vec<f64> = (0..d).map(|i| rng.range(lo[i], hi[i])).collect();
                let xr = space.realize(&raw);
                let y = ev.at(&raw);
                xs.push(xr);
                ys.push(y);
                continue;
            }
            let alpha = chol_solve(&kmat, &yz); // K^{-1} yz  (kmat is now L)
            let fstar = yz.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

            // Best observed point — perturb around it for a quarter of the candidates.
            let best_idx = ys
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i)
                .unwrap_or(0);

            let mut best_ei = -1.0;
            let mut best_cand: Option<Vec<f64>> = None;
            for c in 0..n_cand {
                let cand: Vec<f64> = if c < n_cand / 2 {
                    // Exploitation: perturb the current best at several
                    // scales so the search can refine to fine precision
                    // (a fixed scale floors the achievable accuracy).
                    let scale = [0.2_f64, 0.05, 0.01][c % 3];
                    (0..d)
                        .map(|i| {
                            (xs[best_idx][i] + scale * (hi[i] - lo[i]) * rng.normal())
                                .clamp(lo[i], hi[i])
                        })
                        .collect()
                } else {
                    (0..d).map(|i| rng.range(lo[i], hi[i])).collect()
                };
                let kstar: Vec<f64> = (0..n).map(|i| kernel(&xs[i], &cand)).collect();
                let mu = (0..n).map(|i| kstar[i] * alpha[i]).sum::<f64>();
                let v = forward_solve(&kmat, &kstar);
                let var = (SF2 - v.iter().map(|x| x * x).sum::<f64>()).max(0.0);
                let sd = var.sqrt();
                let ei = if sd < 1e-12 {
                    0.0
                } else {
                    let z = (mu - fstar - self.xi) / sd;
                    (mu - fstar - self.xi) * norm_cdf(z) + sd * norm_pdf(z)
                };
                if ei > best_ei {
                    best_ei = ei;
                    best_cand = Some(cand);
                }
            }

            match best_cand {
                Some(raw) if best_ei > 1e-12 => {
                    let xr = space.realize(&raw);
                    let y = ev.at(&raw);
                    xs.push(xr);
                    ys.push(y);
                }
                _ => {
                    stop = StopReason::Converged;
                    break;
                }
            }
        }
        ev.into_report(stop)
    }
}
