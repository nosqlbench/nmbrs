# nmbrs-optimizers

A black-box (derivative-free) optimization library. Each optimizer
maximizes an objective you supply over a search space of continuous and
discrete axes, within an evaluation budget. nmbrs uses it to search
workload parameters, where every evaluation is a benchmark run. The default
build has no dependencies, so it can be used on its own outside nmbrs.

## Where it sits in nmbrs

The algorithms in this crate do not depend on any other nmbrs crate. The
optional `runtime` feature adds a bridge that registers these optimizers
with [`nmbrs-runtime`](https://crates.io/crates/nmbrs-runtime), where a
phase run is used as the objective. The
[`nmbrs`](https://crates.io/crates/nmbrs) CLI enables that feature.

End users normally install the [`nmbrs`](https://crates.io/crates/nmbrs)
CLI rather than depending on this crate directly.

## What it provides

- **`Objective`**: the function being optimized. Implement
  `query(&mut self, x: &[f64]) -> Observation`; optionally override
  `query_fidelity` for multi-fidelity methods. An `Observation` carries the
  value (maximized), a feasibility flag, the evaluation cost in seconds, and
  optional named metrics. Infeasible points are penalized and the search
  continues. To minimize `f`, return `-f(x)`.
- **`SearchSpace`** of named **`Axis`** values: `Axis::continuous(name, lo, hi)`
  or `Axis::discrete(name, detents)`, optionally tagged with a
  `Changeover` cost class (`Control`, `Coordinate`, `Fixture`) that
  cost-aware methods use as a prior. `SearchSpace::realize` clamps and snaps
  raw points before each query.
- **`Budget`**: maximum evaluations and a seed (`Budget::evals`,
  `Budget::seeded`). The stochastic optimizers use a built-in deterministic
  PRNG, so runs are reproducible from the seed.
- **`Optimizer`** and **`Report`**: `optimize(&space, &mut objective, &budget)`
  returns the best point and value, the evaluation count, a `StopReason`
  (`Converged`, `BudgetExhausted`, `NoFeasiblePoint`, `Aborted`), per-axis
  impact rankings from screening optimizers, and the full evaluation
  history.
- **Registry**: `by_name(name, &OptimizerParams)` returns a boxed optimizer;
  `registered_names()` lists them. `OptimizerParams` passes per-optimizer
  numeric settings by key.
- **Test models** (`testmodels`): `sphere`, `rosenbrock`, `rastrigin` and
  `branin`, with `Minimize` (wraps a minimization function as an
  `Objective`) and `NoisyFidelity` (adds noise at reduced fidelity).
- **Docs** (`docs`): a markdown description for each optimizer, as string
  constants.

### Optimizers

| Name | Method |
|---|---|
| `sweep` | Evaluates the Cartesian product of discrete axes and the `{lo, hi}` corners of continuous axes. The default. |
| `cost_greedy_traversal` | Same points as a sweep, ordered so the most expensive-to-change axis changes least often. |
| `centroid_variant` | Sensitivity screening: ranks axes by impact rather than minimizing. |
| `nelder_mead` | Downhill simplex. |
| `hooke_jeeves` | Pattern search. |
| `bobyqa` | Bound-constrained quadratic trust region (separable, diagonal-Hessian variant). |
| `cmaes` | Separable CMA-ES. |
| `bayes_opt` | Bayesian optimization with a Gaussian-process surrogate and Expected Improvement. |
| `hyperband` | Multi-fidelity successive halving. |

### Example

```rust
use nmbrs_optimizers::testmodels::{sphere, Minimize};
use nmbrs_optimizers::{by_name, Axis, Budget, OptimizerParams, SearchSpace};

let space = SearchSpace::new(vec![
    Axis::continuous("x0", -5.0, 5.0),
    Axis::continuous("x1", -5.0, 5.0),
]);

// Shift the sphere so its minimum is at (1.0, -2.0).
let mut objective = Minimize::new(|x: &[f64]| sphere(&[x[0] - 1.0, x[1] + 2.0]));

let mut optimizer = by_name("nelder_mead", &OptimizerParams::new()).unwrap();
let report = optimizer.optimize(&space, &mut objective, &Budget::seeded(400, 42));

println!(
    "best {:?} value {} after {} evals ({:?})",
    report.best, report.best_value, report.evals, report.stop
);
```

## Cargo features

| Feature | Enables |
|---|---|
| `runtime` (off by default) | The `bridge` module, which adapts each optimizer to the `nmbrs-runtime` optimizer contract and registers it through `inventory`, so the runtime finds it at link time. Adds `nmbrs-runtime` and `inventory` as dependencies. The algorithms themselves do not use it. |

With default features the crate has no dependencies.

## Links

- Repository: <https://github.com/nosqlbench/nmbrs>
- Crate source: <https://github.com/nosqlbench/nmbrs/tree/main/nmbrs-optimizers>
- API docs: <https://docs.rs/nmbrs-optimizers>
- Design notes (SRD 86): <https://github.com/nosqlbench/nmbrs/blob/main/docs/SRD/86_optimization.md>

## License

Apache-2.0
