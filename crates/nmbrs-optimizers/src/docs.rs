// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Embedded user-level documentation for each optimizer, in markdown
//! (SRD-86 §5). Surfaced through the `Optimizer::doc_md()` trait method and
//! the inventory registration, and rendered by `nmbrs describe optimizers`.

pub const COST_GREEDY_TRAVERSAL: &str = "\
# cost_greedy_traversal

A discrete **traversal-order** optimizer. It changes no points — only the
order they are visited — enumerating the full grid but looping the most
*expensive*-to-change axis outermost (so it changes least often) and the
cheapest innermost, minimizing cumulative changeover cost. Reports the best
point found.

**Use when** the parameter set is discrete and you want a full sweep but with
minimal fixture churn (e.g. an expensive index rebuild varied least often).

**Axes:** discrete. **Cost-aware:** yes (uses each axis's changeover class).
";

pub const CENTROID_VARIANT: &str = "\
# centroid_variant

A sensitivity **screening** optimizer (not a minimizer). With `1 + 2k` probes
it evaluates the centroid baseline, then steps each axis `±Δ`
one-factor-at-a-time (a 3-point curve per axis) and ranks the axes by a
combined main-effect + curvature impact.

**Use when** you want to discover *which* factors matter before spending a
real optimizer's budget on them.

**Params:** `delta_scale` (probe step multiplier, default 1.0).
**Output:** `ranked_axes` (most impactful first).
";

pub const NELDER_MEAD: &str = "\
# nelder_mead

The downhill-**simplex** method: a robust derivative-free local optimizer that
reflects / expands / contracts / shrinks a `d+1`-vertex simplex downhill.

**Use when** the objective is reasonably smooth and you want a dependable
local search with no tuning.

**Params:** `tol` (convergence tolerance, default 1e-8). **Axes:** continuous.
";

pub const HOOKE_JEEVES: &str = "\
# hooke_jeeves

**Pattern search**: an exploratory phase probes `±step` along each axis from a
base point, and a pattern phase extrapolates the successful direction; the step
shrinks when no improvement is found. Varying one axis at a time maps naturally
onto the changeover economy (cheap axes inner, expensive outer).

**Use when** you want a simple, cost-structured local search.

**Params:** `tol` (default 1e-8), `shrink` (step shrink factor, default 0.5).
";

pub const BOBYQA: &str = "\
# bobyqa

A bound-constrained derivative-free **quadratic trust-region** method
(separable / diagonal-Hessian variant). At each step it fits a 1-D quadratic
per axis through `±radius` probes and steps to the model minimizer, clamped to
the trust region and bounds; the radius shrinks on a failed step.

**Use when** the objective is smooth and you want fast convergence on a bowl
(solves a quadratic essentially exactly).

**Params:** `tol` (default 1e-8). **Axes:** continuous.
";

pub const CMAES: &str = "\
# cmaes

**Separable CMA-ES** — a robust population-based evolution strategy for noisy,
multimodal, or ill-conditioned landscapes where the simplex / trust-region
methods stall. It adapts a per-axis scale (the anisotropy a screen reveals) and
evaluates a batch (generation) of candidates each step. Deterministic from the
budget seed.

**Use when** the landscape is rugged or noisy, or the axes have very different
sensitivities.

**Params:** `lambda` (population size; default `4 + 3·ln d`), `sigma0_scale`
(initial step as a fraction of the box, default 0.3), `tol`.
";

pub const BAYES_OPT: &str = "\
# bayes_opt

**Bayesian optimization** with a Gaussian-process surrogate and
Expected-Improvement acquisition. The premier method for *expensive*
evaluations: it fits a GP to the observations and proposes the point of maximum
expected improvement, so it spends few costly evaluations.

**Use when** each evaluation is expensive (a slow phase / fixture rebuild) and
sample-efficiency matters more than fine local precision.

**Params:** `xi` (exploration, default 0.01), `init` (initial design size),
`lengthscale`, `candidates`. **Axes:** continuous (mixed via realization).
";

pub const HYPERBAND: &str = "\
# hyperband

**Multi-fidelity bandit** optimization (successive halving). It evaluates many
random configurations cheaply (a fraction of full resource), then allocates more
resource only to survivors — turning *fidelity* (e.g. phase cycles) into a
first-class lever. Each bracket winner is re-evaluated at full fidelity so the
reported best is trustworthy. Deterministic from the budget seed.

**Use when** a cheap low-fidelity estimate of the objective is available (e.g.
recall measured on fewer cycles) before committing full budget.

**Params:** `eta` (halving rate, default 3), `max_resource` (default 81).
";
