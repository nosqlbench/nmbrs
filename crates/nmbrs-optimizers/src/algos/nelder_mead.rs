// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nelder_mead` — the downhill-simplex method (SRD-86 §9). A robust
//! derivative-free local optimizer: maintain a `d+1`-vertex simplex and
//! reflect / expand / contract / shrink it downhill. Operates on
//! `g(x) = -value` (it minimizes `g`, i.e. maximizes the objective).

use crate::optimizer::{Budget, Eval, Objective, Optimizer, Report, StopReason};
use crate::registry::OptimizerParams;
use crate::space::SearchSpace;

const ALPHA: f64 = 1.0; // reflection
const GAMMA: f64 = 2.0; // expansion
const RHO: f64 = 0.5; // contraction
const SIGMA: f64 = 0.5; // shrink

#[derive(Clone)]
pub struct NelderMead {
    tol: f64,
}

impl NelderMead {
    pub fn from_params(p: &OptimizerParams) -> Self {
        Self {
            tol: p.get("tol", 1e-8),
        }
    }
}

impl Optimizer for NelderMead {
    fn name(&self) -> &str {
        "nelder_mead"
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
        let steps = space.steps();
        let c = space.center();

        // Build the initial simplex: centre plus one offset vertex per axis.
        let mut verts: Vec<Vec<f64>> = Vec::with_capacity(d + 1);
        verts.push(c.clone());
        for i in 0..d {
            let mut v = c.clone();
            v[i] += steps[i];
            verts.push(v);
        }
        // g = -value (minimize g).
        let mut g: Vec<f64> = verts.iter().map(|v| -ev.at(v)).collect();

        let mut stop = StopReason::BudgetExhausted;
        while ev.budget_left() {
            // Order vertices by g ascending (best first).
            let mut order: Vec<usize> = (0..=d).collect();
            order.sort_by(|&a, &b| g[a].partial_cmp(&g[b]).unwrap_or(std::cmp::Ordering::Equal));
            let best = order[0];
            let worst = order[d];
            let second_worst = order[d - 1];

            // Convergence: the simplex value spread is tiny.
            if (g[worst] - g[best]).abs() <= self.tol * (1.0 + g[best].abs()) {
                stop = StopReason::Converged;
                break;
            }

            // Centroid of all vertices except the worst.
            let mut centroid = vec![0.0; d];
            for &oi in &order[..d] {
                for j in 0..d {
                    centroid[j] += verts[oi][j];
                }
            }
            for cj in centroid.iter_mut() {
                *cj /= d as f64;
            }

            // Reflection.
            let xr: Vec<f64> = (0..d)
                .map(|j| centroid[j] + ALPHA * (centroid[j] - verts[worst][j]))
                .collect();
            let gr = -ev.at(&xr);

            if gr < g[best] {
                // Expansion (reflection was the new best).
                let xe: Vec<f64> = (0..d)
                    .map(|j| centroid[j] + GAMMA * (xr[j] - centroid[j]))
                    .collect();
                let ge = if ev.budget_left() {
                    -ev.at(&xe)
                } else {
                    gr + 1.0
                };
                if ge < gr {
                    verts[worst] = xe;
                    g[worst] = ge;
                } else {
                    verts[worst] = xr;
                    g[worst] = gr;
                }
            } else if gr < g[second_worst] {
                // Accept the reflection.
                verts[worst] = xr;
                g[worst] = gr;
            } else {
                // Contraction.
                let (xc, outside) = if gr < g[worst] {
                    // Outside contraction.
                    (
                        (0..d)
                            .map(|j| centroid[j] + RHO * (xr[j] - centroid[j]))
                            .collect::<Vec<_>>(),
                        true,
                    )
                } else {
                    // Inside contraction.
                    (
                        (0..d)
                            .map(|j| centroid[j] - RHO * (centroid[j] - verts[worst][j]))
                            .collect::<Vec<_>>(),
                        false,
                    )
                };
                let gc = if ev.budget_left() {
                    -ev.at(&xc)
                } else {
                    f64::INFINITY
                };
                let accept = if outside { gc <= gr } else { gc < g[worst] };
                if accept {
                    verts[worst] = xc;
                    g[worst] = gc;
                } else {
                    // Shrink every vertex toward the best.
                    for k in 0..=d {
                        if k == best {
                            continue;
                        }
                        // verts[k] (written) and verts[best] (read) alias
                        // the same outer Vec, so an index loop is required.
                        #[allow(clippy::needless_range_loop)]
                        for j in 0..d {
                            verts[k][j] = verts[best][j] + SIGMA * (verts[k][j] - verts[best][j]);
                        }
                        if ev.budget_left() {
                            g[k] = -ev.at(&verts[k]);
                        }
                    }
                }
            }
        }
        ev.into_report(stop)
    }
}
