// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! A tiny deterministic PRNG (SplitMix64) + a normal sampler, so the
//! stochastic optimizers (CMA-ES, Bayesian optimization seeding,
//! Hyperband sampling) are reproducible from a [`Budget`](crate::Budget)
//! seed with no external dependency.

/// SplitMix64 generator. Cheap, deterministic, good enough for
/// optimizer sampling (not cryptographic).
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
    /// Cached second value from the Box–Muller pair.
    spare: Option<f64>,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_add(0x9E37_79B9_7F4A_7C15),
            spare: None,
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        // Top 53 bits → a double in [0,1).
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }

    /// Uniform in `[lo, hi)`.
    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + self.unit() * (hi - lo)
    }

    /// Standard normal `N(0, 1)` via Box–Muller (with a cached spare).
    pub fn normal(&mut self) -> f64 {
        if let Some(s) = self.spare.take() {
            return s;
        }
        // Avoid u = 0 (log(0)); unit() is in [0,1).
        let u1 = (self.unit()).max(1e-12);
        let u2 = self.unit();
        let mag = (-2.0 * u1.ln()).sqrt();
        let z0 = mag * (std::f64::consts::TAU * u2).cos();
        let z1 = mag * (std::f64::consts::TAU * u2).sin();
        self.spare = Some(z1);
        z0
    }

    /// A `dims`-length standard-normal vector.
    pub fn normal_vec(&mut self, dims: usize) -> Vec<f64> {
        (0..dims).map(|_| self.normal()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_from_seed() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn unit_in_range() {
        let mut r = Rng::new(7);
        for _ in 0..10_000 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u));
        }
    }

    #[test]
    fn normal_is_roughly_standard() {
        let mut r = Rng::new(123);
        let n = 50_000;
        let mut sum = 0.0;
        let mut sumsq = 0.0;
        for _ in 0..n {
            let z = r.normal();
            sum += z;
            sumsq += z * z;
        }
        let mean = sum / n as f64;
        let var = sumsq / n as f64 - mean * mean;
        assert!(mean.abs() < 0.05, "mean {mean} not ~0");
        assert!((var - 1.0).abs() < 0.1, "var {var} not ~1");
    }
}
