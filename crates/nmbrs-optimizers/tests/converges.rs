// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-86 — per-optimizer convergence against synthetic multivariate
//! manifold models. Each optimizer is exercised on the test functions it
//! is suited to, asserting it locates the optimum within a tolerance and
//! budget. Manifolds are *shifted* off the box centre so a method cannot
//! "win" by starting at the optimum.

use nmbrs_optimizers::testmodels::{
    BRANIN_MIN, Minimize, NoisyFidelity, branin, rastrigin, rosenbrock, sphere,
};
use nmbrs_optimizers::{
    Axis, Budget, Objective, OptimizerParams, Report, SearchSpace, StopReason, by_name,
};

/// A `dims`-D continuous box `[lo, hi]^dims` with axes `x0..`.
fn cont(dims: usize, lo: f64, hi: f64) -> SearchSpace {
    SearchSpace::new(
        (0..dims)
            .map(|i| Axis::continuous(format!("x{i}"), lo, hi))
            .collect(),
    )
}

fn solve(name: &str, space: &SearchSpace, obj: &mut dyn Objective, evals: usize) -> Report {
    let mut opt = by_name(name, &OptimizerParams::new()).expect("optimizer is registered");
    let r = opt.optimize(space, obj, &Budget::seeded(evals, 0xC0FFEE));
    assert_eq!(opt.name(), name);
    assert!(
        r.evals <= evals,
        "{name} overran budget: {} > {evals}",
        r.evals
    );
    r
}

/// Shift a minimization function so its optimum is at `target` (interior,
/// not the box centre).
fn shifted(base: fn(&[f64]) -> f64, target: Vec<f64>) -> impl FnMut(&[f64]) -> f64 {
    move |x: &[f64]| {
        let shifted: Vec<f64> = x.iter().zip(&target).map(|(v, t)| v - t).collect();
        base(&shifted)
    }
}

// ── Registry ───────────────────────────────────────────────────────────

#[test]
fn registry_resolves_all_nine_and_rejects_unknown() {
    let names = nmbrs_optimizers::registered_names();
    assert_eq!(names.len(), 9);
    for n in &names {
        assert!(by_name(n, &OptimizerParams::new()).is_some(), "missing {n}");
    }
    assert!(by_name("does_not_exist", &OptimizerParams::new()).is_none());
}

// ── Continuous local / global methods on the (shifted) sphere ──────────

#[test]
fn continuous_methods_solve_shifted_sphere() {
    // Box centre (0,0); optimum shifted to (1.5, -2.0). Value max is 0.
    // The local/ES methods drive to high precision; `bayes_opt` is
    // sample-efficient rather than a precision local optimizer (in
    // practice you polish a BO optimum with a local method).
    for (name, bar) in [
        ("nelder_mead", -2.0e-2),
        ("hooke_jeeves", -2.0e-2),
        ("bobyqa", -2.0e-2),
        ("cmaes", -2.0e-2),
        ("bayes_opt", -0.15),
    ] {
        let space = cont(2, -5.0, 5.0);
        let mut obj = Minimize::new(shifted(sphere, vec![1.5, -2.0]));
        let r = solve(name, &space, &mut obj, 400);
        assert!(
            r.best_value > bar,
            "{name}: sphere best_value {} (< {bar}, best {:?})",
            r.best_value,
            r.best
        );
    }
}

// ── Curved valley (Rosenbrock): the harder local test ─────────────────

#[test]
fn methods_make_progress_on_rosenbrock() {
    // Rosenbrock min 0 at (1,1) — already interior to [-2, 2]^2.
    // Tolerances reflect each method's strength on a curved ridge.
    // Separable CMA-ES cannot rotate to Rosenbrock's correlated valley,
    // so it converges along it more slowly than the simplex — a looser
    // (still meaningful) bar reflects that.
    let cases = [("nelder_mead", -1.0e-2), ("bobyqa", -1.0), ("cmaes", -0.15)];
    for (name, bar) in cases {
        let space = cont(2, -2.0, 2.0);
        let mut obj = Minimize::new(rosenbrock);
        let r = solve(name, &space, &mut obj, 800);
        assert!(
            r.best_value > bar,
            "{name}: rosenbrock best_value {} (< {bar})",
            r.best_value
        );
    }
}

// ── Multimodal (Rastrigin): CMA-ES should escape local minima ─────────

#[test]
fn cmaes_handles_multimodal_rastrigin() {
    let space = cont(2, -5.12, 5.12);
    let mut obj = Minimize::new(rastrigin);
    let r = solve("cmaes", &space, &mut obj, 1500);
    // Rastrigin is a field of local minima; reaching within ~3 of the
    // global bowl is a real result for a 2-D budgeted search.
    assert!(
        r.best_value > -3.0,
        "cmaes: rastrigin best_value {}",
        r.best_value
    );
}

// ── Branin: the canonical Bayesian-optimization benchmark ─────────────

#[test]
fn bayes_opt_and_cmaes_find_branin_minimum() {
    for name in ["bayes_opt", "cmaes"] {
        let space = SearchSpace::new(vec![
            Axis::continuous("x1", -5.0, 10.0),
            Axis::continuous("x2", 0.0, 15.0),
        ]);
        let mut obj = Minimize::new(branin);
        let r = solve(name, &space, &mut obj, 400);
        // value = -branin; optimum value is -BRANIN_MIN ≈ -0.398.
        assert!(
            r.best_value > -(BRANIN_MIN + 0.3),
            "{name}: branin best_value {} (target ≈ {})",
            r.best_value,
            -BRANIN_MIN
        );
    }
}

// ── Discrete sweep optimizers: sweep + cost-greedy traversal ───────────

#[test]
fn sweep_and_traversal_enumerate_and_find_grid_optimum() {
    // Optimum (0,0) is a grid point; both must evaluate every cell and
    // return it exactly, in the same number of evals.
    for name in ["sweep", "cost_greedy_traversal"] {
        let space = SearchSpace::new(vec![
            Axis::discrete("x", vec![-2.0, -1.0, 0.0, 1.0, 2.0]),
            Axis::discrete("y", vec![-2.0, -1.0, 0.0, 1.0, 2.0]),
        ]);
        let mut obj = Minimize::new(sphere);
        let r = solve(name, &space, &mut obj, 100);
        assert_eq!(
            r.stop,
            StopReason::Converged,
            "{name} should exhaust the grid"
        );
        assert_eq!(r.evals, 25, "{name} should evaluate the full 5x5 grid");
        assert!(
            r.best_value > -1e-9,
            "{name}: grid optimum value {}",
            r.best_value
        );
        assert_eq!(r.best, vec![0.0, 0.0], "{name}: best grid cell");
    }
}

#[test]
fn traversal_varies_expensive_axis_least_often() {
    use nmbrs_optimizers::Changeover;
    // x is a Fixture axis (expensive), y a Control axis (cheap). The
    // cost-greedy order must loop x outermost, so x changes only when y
    // has cycled through all its detents.
    let space = SearchSpace::new(vec![
        Axis::discrete("x", vec![0.0, 1.0, 2.0]).with_changeover(Changeover::Fixture),
        Axis::discrete("y", vec![0.0, 1.0]).with_changeover(Changeover::Control),
    ]);
    // Record the visitation order via a probe that logs the point.
    struct Recorder {
        seen: std::rc::Rc<std::cell::RefCell<Vec<(f64, f64)>>>,
    }
    impl Objective for Recorder {
        fn query(&mut self, x: &[f64]) -> nmbrs_optimizers::Observation {
            self.seen.borrow_mut().push((x[0], x[1]));
            nmbrs_optimizers::Observation::value(0.0)
        }
    }
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut obj = Recorder { seen: seen.clone() };
    solve("cost_greedy_traversal", &space, &mut obj, 100);
    let order = seen.borrow().clone();
    assert_eq!(order.len(), 6);
    // x (expensive) should change 2 times across 6 visits; y every step.
    let x_changes = order.windows(2).filter(|w| w[0].0 != w[1].0).count();
    let y_changes = order.windows(2).filter(|w| w[0].1 != w[1].1).count();
    assert_eq!(
        x_changes, 2,
        "expensive axis x should change least: {order:?}"
    );
    assert!(
        y_changes >= x_changes,
        "cheap axis y should change at least as often"
    );
}

// ── Centroid screening: rank the high-impact axis first ───────────────

#[test]
fn centroid_variant_ranks_high_impact_axis_first() {
    // Symmetric quadratic at the centre: x0 has 20× the curvature of x1,
    // so the combined-impact ranking must put x0 first (the gradient is
    // ~0 at the centroid, so this exercises the curvature term).
    let space = cont(2, -3.0, 3.0);
    let mut obj = Minimize::new(|x: &[f64]| 10.0 * x[0] * x[0] + 0.5 * x[1] * x[1]);
    let r = solve("centroid_variant", &space, &mut obj, 50);
    assert_eq!(r.ranked_axes.len(), 2);
    assert_eq!(r.ranked_axes[0].name, "x0", "x0 is far more impactful");
    assert!(r.ranked_axes[0].curvature.abs() > r.ranked_axes[1].curvature.abs());
    // 1 + 2*2 probes.
    assert_eq!(r.evals, 5);
}

// ── Multi-fidelity: Hyperband on a noisy-at-low-fidelity sphere ───────

#[test]
fn hyperband_finds_good_region_under_noise() {
    let space = cont(2, -5.0, 5.0);
    // Optimum at the origin; low-fidelity evaluations are noisy, full
    // fidelity is exact. The reported best is full-fidelity, so it must
    // be trustworthy.
    let mut obj = NoisyFidelity::new(sphere, 5.0, 0xBEEF);
    let r = solve("hyperband", &space, &mut obj, 600);
    // Random-search-quality on a 2-D box: getting within ~distance 1.6
    // of the origin (value > -2.5) is a fair bar.
    assert!(
        r.best_value > -2.5,
        "hyperband: best_value {}",
        r.best_value
    );
    assert!(r.best_value.is_finite());
}

// ── Infeasibility: an all-infeasible space reports NoFeasiblePoint ────

#[test]
fn all_infeasible_reports_no_feasible_point() {
    struct Infeasible;
    impl Objective for Infeasible {
        fn query(&mut self, _x: &[f64]) -> nmbrs_optimizers::Observation {
            nmbrs_optimizers::Observation::infeasible()
        }
    }
    let space = cont(2, -1.0, 1.0);
    let mut obj = Infeasible;
    let r = solve("nelder_mead", &space, &mut obj, 50);
    assert_eq!(r.stop, StopReason::NoFeasiblePoint);
}
