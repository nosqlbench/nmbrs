// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Synthetic multivariate "manifold" models for exercising optimizers
//! (SRD-86 §Cross-reference). Each is a standard derivative-free
//! optimization test function with a known minimum; wrap one in
//! [`Minimize`] to turn it into a **maximizing** [`Objective`]
//! (`value = -f(x)`), so the optimum value is `-f_min`.

use crate::optimizer::{Objective, Observation};
use crate::rng::Rng;

/// Sphere: `sum x_i^2`. Min 0 at the origin. Convex, separable — the
/// sanity-check baseline every optimizer must solve.
pub fn sphere(x: &[f64]) -> f64 {
    x.iter().map(|v| v * v).sum()
}

/// Rosenbrock ("banana"): `sum 100*(x_{i+1}-x_i^2)^2 + (1-x_i)^2`. Min 0
/// at `(1, …, 1)`. Non-convex with a curved valley — the classic test of
/// a method's ability to follow a narrow ridge.
pub fn rosenbrock(x: &[f64]) -> f64 {
    x.windows(2)
        .map(|w| 100.0 * (w[1] - w[0] * w[0]).powi(2) + (1.0 - w[0]).powi(2))
        .sum()
}

/// Rastrigin: `10n + sum (x_i^2 - 10 cos(2π x_i))`. Min 0 at the origin.
/// Highly multimodal — a field of local minima around a global bowl;
/// separates global (CMA-ES, restarts) from purely local methods.
pub fn rastrigin(x: &[f64]) -> f64 {
    let n = x.len() as f64;
    10.0 * n
        + x.iter()
            .map(|v| v * v - 10.0 * (std::f64::consts::TAU * v).cos())
            .sum::<f64>()
}

/// Branin (2-D): three equal global minima ≈ 0.397887. Domain
/// `x1 ∈ [-5, 10]`, `x2 ∈ [0, 15]`. A staple Bayesian-optimization
/// benchmark.
pub fn branin(x: &[f64]) -> f64 {
    let (x1, x2) = (x[0], x[1]);
    let a = 1.0;
    let b = 5.1 / (4.0 * std::f64::consts::PI.powi(2));
    let c = 5.0 / std::f64::consts::PI;
    let r = 6.0;
    let s = 10.0;
    let t = 1.0 / (8.0 * std::f64::consts::PI);
    a * (x2 - b * x1 * x1 + c * x1 - r).powi(2) + s * (1.0 - t) * x1.cos() + s
}

/// The Branin global minimum value (for tolerance assertions).
pub const BRANIN_MIN: f64 = 0.397887;

/// Wraps a minimization function `f` as a **maximizing** [`Objective`]
/// (`value = -f(x)`), counting calls. The optimum value is `-f_min`.
pub struct Minimize<F: FnMut(&[f64]) -> f64> {
    pub f: F,
    pub calls: usize,
}

impl<F: FnMut(&[f64]) -> f64> Minimize<F> {
    pub fn new(f: F) -> Self {
        Self { f, calls: 0 }
    }
}

impl<F: FnMut(&[f64]) -> f64> Objective for Minimize<F> {
    fn query(&mut self, x: &[f64]) -> Observation {
        self.calls += 1;
        Observation::value(-(self.f)(x))
    }
}

/// A maximizing objective for the minimization function `f` whose
/// low-*fidelity* evaluations are noisy: `value = -f(x) + noise*(1-fid)*N(0,1)`.
/// Full fidelity (1.0) is exact. Models the multi-fidelity setting
/// Hyperband exploits (a cheap, noisy estimate at low resource).
pub struct NoisyFidelity<F: FnMut(&[f64]) -> f64> {
    pub f: F,
    pub noise: f64,
    rng: Rng,
    pub calls: usize,
}

impl<F: FnMut(&[f64]) -> f64> NoisyFidelity<F> {
    pub fn new(f: F, noise: f64, seed: u64) -> Self {
        Self {
            f,
            noise,
            rng: Rng::new(seed),
            calls: 0,
        }
    }
}

impl<F: FnMut(&[f64]) -> f64> Objective for NoisyFidelity<F> {
    fn query(&mut self, x: &[f64]) -> Observation {
        self.query_fidelity(x, 1.0)
    }

    fn query_fidelity(&mut self, x: &[f64], fidelity: f64) -> Observation {
        self.calls += 1;
        let base = -(self.f)(x);
        let n = self.noise * (1.0 - fidelity.clamp(0.0, 1.0)) * self.rng.normal();
        Observation::value(base + n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minima_are_where_expected() {
        assert!(sphere(&[0.0, 0.0, 0.0]).abs() < 1e-12);
        assert!(rosenbrock(&[1.0, 1.0, 1.0]).abs() < 1e-9);
        assert!(rastrigin(&[0.0, 0.0]).abs() < 1e-9);
        // Branin's three global minimizers.
        for p in [
            [-std::f64::consts::PI, 12.275],
            [std::f64::consts::PI, 2.275],
            [9.42478, 2.475],
        ] {
            assert!((branin(&p) - BRANIN_MIN).abs() < 1e-3, "branin at {p:?}");
        }
    }

    #[test]
    fn minimize_negates_for_maximization() {
        let mut o = Minimize::new(sphere);
        let obs = o.query(&[1.0, 2.0]);
        assert_eq!(obs.value, -5.0);
        assert_eq!(o.calls, 1);
    }
}
