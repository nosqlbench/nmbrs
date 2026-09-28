// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-86 — the **core optimizer contract**.
//!
//! This is the service boundary the phase-execution driver uses to drive
//! an optimizer. It is defined *here, in the core runtime, with no
//! dependency on any optimizer-algorithm crate* (SRD-86 §Contract): the
//! engine owns the contract; algorithm crates (e.g. `nmbrs-optimizers`)
//! register implementations against it via `inventory` and are discovered
//! at link time — the core never names them.
//!
//! The contract has two halves:
//! - what the optimizer needs from the driver: an [`Objective`] it can
//!   query at a coordinate (the runtime's `PhaseFeed` implements it);
//! - what the driver needs from the optimizer: an [`Optimizer`] that, over
//!   a [`SearchSpace`] within a [`Budget`], maximizes the objective and
//!   returns a [`Report`].
//!
//! [`SweepOptimizer`] (`sweep`) is the built-in default (the full Cartesian
//! sweep + best-selection) — the identity until an adaptive method is named.
//! Algorithm crates submit an [`OptimizerRegistration`]; [`by_name`] and
//! [`describe`] discover them.

/// How an axis's value is changed at runtime (cost prior; the runtime
/// owns the actual realization — SRD-86 A5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changeover {
    Control,
    Coordinate,
    Fixture,
}

/// The realized value an axis takes at a coordinate — the comprehension's
/// actual value (SRD-86 A4). The model carries labels/detents as-is; numeric
/// casting for metric-space solvers is a solver-internal stub (the runtime
/// bridge), never in this model.
#[derive(Debug, Clone, PartialEq)]
pub enum AxisValue {
    Num(f64),
    Label(String),
    Bool(bool),
}

impl AxisValue {
    /// Project to the `f64` a numeric solver / manifold objective reads. A
    /// `Label` has no intrinsic number (categorical axes are handled by index,
    /// not this), so it yields `0.0` defensively.
    pub fn as_num(&self) -> f64 {
        match self {
            AxisValue::Num(f) => *f,
            AxisValue::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            AxisValue::Label(_) => 0.0,
        }
    }
}

impl std::fmt::Display for AxisValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AxisValue::Num(n) => write!(f, "{n}"),
            AxisValue::Label(s) => write!(f, "{s}"),
            AxisValue::Bool(b) => write!(f, "{b}"),
        }
    }
}

impl From<f64> for AxisValue {
    fn from(v: f64) -> Self {
        AxisValue::Num(v)
    }
}

/// A point in the search space — one [`AxisValue`] per axis. The model carries
/// the comprehension's actual values, so labels survive end-to-end.
pub type Coord = Vec<AxisValue>;

/// The domain of one axis.
#[derive(Debug, Clone)]
pub enum AxisKind {
    /// A real interval `[lo, hi]` (optionally quantized to `min_step`).
    Continuous { lo: f64, hi: f64, min_step: f64 },
    /// Ordered numeric detents — ordinal, so distance/order are meaningful.
    Discrete { detents: Vec<AxisValue> },
    /// Nominal choices (labels / bools) — a **stable, positionally-ordered**
    /// list (position is meaningful: "the first option" is well-defined and
    /// stable), but with **no metric/distance** between values. Handled by
    /// index, never sorted.
    Categorical { options: Vec<AxisValue> },
}

/// One factor of the search space.
#[derive(Debug, Clone)]
pub struct Axis {
    pub name: String,
    pub kind: AxisKind,
    pub changeover: Changeover,
}

impl Axis {
    /// The centre value of this axis — continuous midpoint (`Num`), the median
    /// detent, or the first categorical option. The default best.
    pub fn center(&self) -> AxisValue {
        match &self.kind {
            AxisKind::Continuous { lo, hi, .. } => AxisValue::Num(0.5 * (lo + hi)),
            AxisKind::Discrete { detents } => {
                let mut d = detents.clone();
                d.sort_by(|a, b| {
                    a.as_num()
                        .partial_cmp(&b.as_num())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                d.get(d.len() / 2).cloned().unwrap_or(AxisValue::Num(0.0))
            }
            AxisKind::Categorical { options } => {
                options.first().cloned().unwrap_or(AxisValue::Num(0.0))
            }
        }
    }
}

/// The search space the optimizer explores.
#[derive(Debug, Clone)]
pub struct SearchSpace {
    pub axes: Vec<Axis>,
}

impl SearchSpace {
    pub fn new(axes: Vec<Axis>) -> Self {
        Self { axes }
    }
    pub fn dims(&self) -> usize {
        self.axes.len()
    }
    pub fn center(&self) -> Coord {
        self.axes.iter().map(Axis::center).collect()
    }
}

/// One objective evaluation result.
#[derive(Debug, Clone)]
pub struct Observation {
    pub value: f64,
    pub feasible: bool,
    pub cost: f64,
    pub metrics: Vec<(String, f64)>,
}

impl Observation {
    pub fn value(v: f64) -> Self {
        Self {
            value: v,
            feasible: true,
            cost: 0.0,
            metrics: Vec::new(),
        }
    }
    pub fn infeasible() -> Self {
        Self {
            value: f64::NEG_INFINITY,
            feasible: false,
            cost: 0.0,
            metrics: Vec::new(),
        }
    }
}

/// The thing an optimizer queries. The runtime's `PhaseFeed` implements it
/// by running a probe phase at the coordinate and reading the objective
/// metric. The optimizer **maximizes** [`Observation::value`].
pub trait Objective {
    fn query(&mut self, x: &[AxisValue]) -> Observation;
    /// Evaluate at a reduced *fidelity* in `(0, 1]` (1.0 = full). Default
    /// ignores fidelity (used by multi-fidelity methods).
    fn query_fidelity(&mut self, x: &[AxisValue], _fidelity: f64) -> Observation {
        self.query(x)
    }
}

/// Evaluation budget + reproducibility seed.
#[derive(Debug, Clone)]
pub struct Budget {
    pub max_evals: usize,
    pub max_seconds: Option<f64>,
    pub seed: u64,
}

impl Budget {
    pub fn seeded(max_evals: usize, seed: u64) -> Self {
        Self {
            max_evals,
            max_seconds: None,
            seed,
        }
    }
}

/// Why a search ended (maps to a two-axis Outcome at the seam — SRD-86 A8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Converged,
    BudgetExhausted,
    NoFeasiblePoint,
    Aborted,
}

/// Per-axis sensitivity, populated by screening optimizers.
#[derive(Debug, Clone)]
pub struct AxisImpact {
    pub name: String,
    pub main_effect: f64,
    pub curvature: f64,
}

/// The result of a search.
#[derive(Debug, Clone)]
pub struct Report {
    pub best: Coord,
    pub best_value: f64,
    pub evals: usize,
    pub stop: StopReason,
    pub ranked_axes: Vec<AxisImpact>,
    pub history: Vec<(Coord, f64)>,
}

/// Optimizer-specific tuning knobs (resolved by name with a default).
#[derive(Debug, Clone, Default)]
pub struct OptimizerParams {
    pub overrides: Vec<(String, f64)>,
}

impl OptimizerParams {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with(mut self, key: impl Into<String>, value: f64) -> Self {
        self.overrides.push((key.into(), value));
        self
    }
    pub fn get(&self, key: &str, default: f64) -> f64 {
        self.overrides
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| *v)
            .unwrap_or(default)
    }
}

/// A coordinate source the graph executor pulls through (SRD-86). The source
/// advertises *capability decorators*; the executor probes them, **favoring
/// the most capable** ([`FeedbackSource`]) and requiring at least one.
pub trait CoordinateSource: Send {
    /// The feedback-driven decorator (adaptive optimizers), or `None` if this
    /// source ignores objective feedback.
    fn as_feedback(&mut self) -> Option<&mut dyn FeedbackSource> {
        None
    }
    /// The pull-only decorator (sweep / lex / discrete traversal), or `None` if
    /// this source is feedback-only.
    fn as_pull(&mut self) -> Option<&mut dyn PullSource> {
        None
    }
}

/// Least-capable decorator: yield the next batch of coordinates without
/// feedback. The default lexicographic stream and discrete reorderings are
/// pull sources.
pub trait PullSource: Send {
    fn pull(&mut self) -> Option<Vec<Coord>>;
}

/// Most-capable decorator — *the optimizer primitive*. Given the
/// just-evaluated `(coord, objective)` pairs, yield the next batch of
/// coordinates to evaluate, or `None` when the search is done. The first call
/// receives an empty slice (it produces the initial batch). Batch methods
/// (CMA-ES generation, Hyperband bracket) return several coordinates at once
/// and buffer the matching values across calls.
pub trait FeedbackSource {
    fn step(&mut self, evaluated: &[(Coord, f64)]) -> Option<Vec<Coord>>;
}

/// The default lexicographic coordinate stream over a search space: the full
/// Cartesian product of the discrete-axis detents and the `{lo, hi}` corners
/// of continuous axes, in lex order, one coordinate per `pull`. The `sweep`
/// functor returns it unchanged (identity).
pub struct LexSource {
    lists: Vec<Vec<AxisValue>>,
    idx: Vec<usize>,
    done: bool,
}

impl LexSource {
    pub fn new(space: &SearchSpace) -> Self {
        let lists: Vec<Vec<AxisValue>> = space
            .axes
            .iter()
            .map(|a| match &a.kind {
                AxisKind::Discrete { detents } => detents.clone(),
                AxisKind::Categorical { options } => options.clone(),
                AxisKind::Continuous { lo, hi, .. } => {
                    vec![AxisValue::Num(*lo), AxisValue::Num(*hi)]
                }
            })
            .collect();
        let n = lists.len();
        Self {
            lists,
            idx: vec![0; n],
            done: false,
        }
    }
}

impl PullSource for LexSource {
    fn pull(&mut self) -> Option<Vec<Coord>> {
        if self.done {
            return None;
        }
        let n = self.lists.len();
        let point: Vec<AxisValue> = (0..n).map(|d| self.lists[d][self.idx[d]].clone()).collect();
        // Advance the mixed-radix counter (last axis fastest).
        let mut d = n;
        loop {
            if d == 0 {
                self.done = true;
                break;
            }
            d -= 1;
            self.idx[d] += 1;
            if self.idx[d] < self.lists[d].len() {
                break;
            }
            self.idx[d] = 0;
        }
        Some(vec![point])
    }
}

/// Wraps a boxed [`PullSource`] as a pull-only [`CoordinateSource`].
pub struct PullOnly(pub Box<dyn PullSource>);

impl CoordinateSource for PullOnly {
    fn as_pull(&mut self) -> Option<&mut dyn PullSource> {
        Some(self.0.as_mut())
    }
}

/// A non-derivative, multi-factor optimizer — a **stateless functor** that
/// transforms the default lexicographic coordinate stream into a (possibly
/// adaptive) [`CoordinateSource`] (SRD-86). The search state lives in the
/// produced source, not in the optimizer.
pub trait Optimizer: Send {
    /// The registered name (matches the registry key).
    fn name(&self) -> &str;
    /// User-level documentation, in **markdown** (SRD-86 §5). Surfaced by
    /// `nmbrs describe optimizers`.
    fn doc_md(&self) -> &str;
    /// Build the coordinate source from the param space, the budget, and the
    /// default lex stream. `sweep` returns `lex` unchanged (identity). The
    /// source owns its own termination (budget / convergence); the driver
    /// pulls until it yields `None`.
    fn coordinate_source(
        &self,
        space: &SearchSpace,
        budget: &Budget,
        lex: Box<dyn PullSource>,
    ) -> Box<dyn CoordinateSource>;

    /// Standalone driver: run the search by pulling this optimizer's source
    /// and querying `obj`, returning a [`Report`]. Used for unit testing and
    /// the non-runtime path; the graph executor drives the source itself
    /// (pull-through, observing each phase's objective wire).
    fn optimize(&self, space: &SearchSpace, obj: &mut dyn Objective, budget: &Budget) -> Report {
        drive_source(self, space, obj, budget)
    }
}

/// The shared default-driver loop for [`Optimizer::optimize`]: pull a batch
/// from the source (via its most-capable decorator), evaluate each coordinate
/// against `obj`, feed the values back, and track the best.
fn drive_source(
    opt: &(impl Optimizer + ?Sized),
    space: &SearchSpace,
    obj: &mut dyn Objective,
    budget: &Budget,
) -> Report {
    let lex: Box<dyn PullSource> = Box::new(LexSource::new(space));
    let mut src = opt.coordinate_source(space, budget, lex);

    let mut best = space.center();
    let mut best_value = f64::NEG_INFINITY;
    let mut history: Vec<(Coord, f64)> = Vec::new();
    let mut any_feasible = false;
    let mut evals = 0usize;
    let mut budget_hit = false;

    fn next(src: &mut Box<dyn CoordinateSource>, evaluated: &[(Coord, f64)]) -> Option<Vec<Coord>> {
        if let Some(f) = src.as_feedback() {
            f.step(evaluated)
        } else if let Some(p) = src.as_pull() {
            p.pull()
        } else {
            None
        }
    }

    let mut batch = next(&mut src, &[]);
    while let Some(coords) = batch.take() {
        let mut evaluated: Vec<(Coord, f64)> = Vec::with_capacity(coords.len());
        for c in coords {
            if evals >= budget.max_evals {
                budget_hit = true;
                break;
            }
            let obs = obj.query(&c);
            evals += 1;
            let v = if obs.feasible && obs.value.is_finite() {
                any_feasible = true;
                obs.value
            } else {
                -1.0e18
            };
            if v > best_value {
                best_value = v;
                best = c.clone();
            }
            history.push((c.clone(), v));
            evaluated.push((c, v));
        }
        if budget_hit {
            break;
        }
        batch = next(&mut src, &evaluated);
    }

    let stop = if !any_feasible {
        StopReason::NoFeasiblePoint
    } else if budget_hit {
        StopReason::BudgetExhausted
    } else {
        StopReason::Converged
    };
    Report {
        best,
        best_value,
        evals,
        stop,
        ranked_axes: Vec::new(),
        history,
    }
}

/// The built-in default optimizer: the **identity functor**. It returns the
/// default lexicographic stream unchanged — a full Cartesian sweep, the
/// engine's ordinary parameter enumeration. Always available, no plugin
/// required (SRD-86 A1).
pub struct SweepOptimizer;

const SWEEP_DOC: &str = "# sweep\n\nThe identity optimizer — the **default** when no `method:` is \
set. Returns the default lexicographic coordinate stream unchanged — the full Cartesian product of \
the discrete-axis detents (and the `{lo, hi}` corners of continuous axes), in lex order. So `sweep` \
evaluates EVERY coordinate exhaustively and reports the best by the objective; it reproduces the \
engine's ordinary parameter sweep, now with best-selection. (Use an adaptive method — `nelder_mead`, \
`cmaes`, … — to search a continuous space without enumerating it.)\n";

impl Optimizer for SweepOptimizer {
    fn name(&self) -> &str {
        "sweep"
    }
    fn doc_md(&self) -> &str {
        SWEEP_DOC
    }
    fn coordinate_source(
        &self,
        _space: &SearchSpace,
        _budget: &Budget,
        lex: Box<dyn PullSource>,
    ) -> Box<dyn CoordinateSource> {
        Box::new(PullOnly(lex)) // identity
    }
}

// ===========================================================================
// Registry — inventory-based, link-time collection (mirrors the adapter
// registration pattern in `adapter.rs`). Algorithm crates submit one
// `OptimizerRegistration` per optimizer; the core discovers them with no
// dependency on the algorithm crate.
// ===========================================================================

/// An optimizer implementation's registration, submitted at link time via
/// `inventory` by an algorithm crate (e.g. `nmbrs-optimizers`).
pub struct OptimizerRegistration {
    /// The optimizer's registry name (`"nelder_mead"`, `"cmaes"`, …).
    pub name: fn() -> &'static str,
    /// User-level markdown documentation (SRD-86 §5).
    pub doc_md: fn() -> &'static str,
    /// Factory: build the optimizer from its params.
    pub make: fn(&OptimizerParams) -> Box<dyn Optimizer>,
}

inventory::collect!(OptimizerRegistration);

/// Name + markdown doc for one optimizer, for `nmbrs describe optimizers`.
#[derive(Debug, Clone)]
pub struct OptimizerInfo {
    pub name: &'static str,
    pub doc_md: &'static str,
}

/// Resolve an optimizer by name: the built-in `sweep`, or a link-time
/// registration. Returns `None` for an unknown name.
pub fn by_name(name: &str, params: &OptimizerParams) -> Option<Box<dyn Optimizer>> {
    if name == "sweep" {
        return Some(Box::new(SweepOptimizer));
    }
    inventory::iter::<OptimizerRegistration>
        .into_iter()
        .find(|r| (r.name)() == name)
        .map(|r| (r.make)(params))
}

/// Every available optimizer (the built-in `sweep` + all registrations),
/// sorted by name, for `nmbrs describe optimizers`.
pub fn describe() -> Vec<OptimizerInfo> {
    let mut out = vec![OptimizerInfo {
        name: "sweep",
        doc_md: SWEEP_DOC,
    }];
    for r in inventory::iter::<OptimizerRegistration> {
        out.push(OptimizerInfo {
            name: (r.name)(),
            doc_md: (r.doc_md)(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

/// Every registered optimizer name (built-in `sweep` + registrations).
pub fn registered_names() -> Vec<&'static str> {
    describe().into_iter().map(|i| i.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic objective: a negated paraboloid maximized at `target`.
    struct Paraboloid {
        target: Vec<f64>,
    }
    impl Objective for Paraboloid {
        fn query(&mut self, x: &[AxisValue]) -> Observation {
            let v: f64 = x
                .iter()
                .map(AxisValue::as_num)
                .zip(&self.target)
                .map(|(a, t)| -(a - t) * (a - t))
                .sum();
            Observation::value(v)
        }
    }

    #[test]
    fn sweep_is_always_available_and_sweeps_the_grid() {
        let space = SearchSpace::new(vec![
            Axis {
                name: "x".into(),
                kind: AxisKind::Discrete {
                    detents: vec![
                        AxisValue::Num(0.0),
                        AxisValue::Num(1.0),
                        AxisValue::Num(2.0),
                    ],
                },
                changeover: Changeover::Coordinate,
            },
            Axis {
                name: "y".into(),
                kind: AxisKind::Discrete {
                    detents: vec![
                        AxisValue::Num(0.0),
                        AxisValue::Num(1.0),
                        AxisValue::Num(2.0),
                    ],
                },
                changeover: Changeover::Coordinate,
            },
        ]);
        let opt = by_name("sweep", &OptimizerParams::new()).expect("sweep is built in");
        let mut obj = Paraboloid {
            target: vec![1.0, 2.0],
        };
        let r = opt.optimize(&space, &mut obj, &Budget::seeded(100, 0));
        assert_eq!(r.evals, 9);
        assert_eq!(r.best, vec![AxisValue::Num(1.0), AxisValue::Num(2.0)]);
        assert!(r.best_value.abs() < 1e-9);
        assert_eq!(r.stop, StopReason::Converged);
    }

    #[test]
    fn describe_includes_sweep_and_unknown_is_none() {
        let infos = describe();
        assert!(infos.iter().any(|i| i.name == "sweep"));
        assert!(infos.iter().any(|i| i.doc_md.contains("# sweep")));
        assert!(by_name("definitely_not_registered", &OptimizerParams::new()).is_none());
    }
}
