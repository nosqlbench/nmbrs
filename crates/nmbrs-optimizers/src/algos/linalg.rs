// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Tiny linear-algebra + statistics helpers for the model-based
//! optimizers: Cholesky factorization + solves for the GP surrogate
//! (`bayes_opt`), and the standard normal CDF/PDF for Expected
//! Improvement. Dependency-free and sized for the small matrices an
//! evaluation-budgeted optimizer produces.

// Triangular factorizations and solves are inherently index-based — each
// entry depends on earlier ones in the same row/column — so the
// range-indexed form is the clearest, standard expression here.
#![allow(clippy::needless_range_loop)]

/// In-place Cholesky factorization. On success `a` becomes the lower-
/// triangular `L` with `A = L Lᵀ` (the strict upper triangle is zeroed).
/// Returns `false` if `A` is not positive-definite.
pub fn cholesky(a: &mut [Vec<f64>]) -> bool {
    let n = a.len();
    for i in 0..n {
        for j in 0..=i {
            let mut sum = a[i][j];
            for k in 0..j {
                sum -= a[i][k] * a[j][k];
            }
            if i == j {
                if sum <= 0.0 {
                    return false;
                }
                a[i][j] = sum.sqrt();
            } else {
                a[i][j] = sum / a[j][j];
            }
        }
        for j in (i + 1)..n {
            a[i][j] = 0.0;
        }
    }
    true
}

/// Forward substitution: solve `L y = b` for the lower-triangular `L`.
pub fn forward_solve(l: &[Vec<f64>], b: &[f64]) -> Vec<f64> {
    let n = l.len();
    let mut y = vec![0.0; n];
    for i in 0..n {
        let mut s = b[i];
        for k in 0..i {
            s -= l[i][k] * y[k];
        }
        y[i] = s / l[i][i];
    }
    y
}

/// Solve `A x = b` from the Cholesky factor `L` (`A = L Lᵀ`): forward
/// solve `L y = b`, then back solve `Lᵀ x = y`.
pub fn chol_solve(l: &[Vec<f64>], b: &[f64]) -> Vec<f64> {
    let n = l.len();
    let y = forward_solve(l, b);
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut s = y[i];
        for k in (i + 1)..n {
            s -= l[k][i] * x[k];
        }
        x[i] = s / l[i][i];
    }
    x
}

/// Abramowitz–Stegun erf approximation (max abs error ~1.5e-7).
pub fn erf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let y = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t
            * (-x * x).exp();
    if x >= 0.0 { y } else { -y }
}

/// Standard normal CDF Φ(x).
pub fn norm_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

/// Standard normal PDF φ(x).
pub fn norm_pdf(x: f64) -> f64 {
    (-(0.5 * x * x)).exp() / std::f64::consts::TAU.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cholesky_solves_a_spd_system() {
        // A = [[4,2],[2,3]], b = [1,1]; x = A^{-1} b.
        let mut a = vec![vec![4.0, 2.0], vec![2.0, 3.0]];
        assert!(cholesky(&mut a));
        let x = chol_solve(&a, &[1.0, 1.0]);
        // Verify A x ≈ b with the original A.
        assert!((4.0 * x[0] + 2.0 * x[1] - 1.0).abs() < 1e-9);
        assert!((2.0 * x[0] + 3.0 * x[1] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn normal_cdf_pdf_anchor_points() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-6);
        assert!((norm_cdf(1.96) - 0.975).abs() < 1e-3);
        assert!((norm_pdf(0.0) - 0.398_942).abs() < 1e-5);
    }
}
