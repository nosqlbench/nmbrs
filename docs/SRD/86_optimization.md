# SRD-86 — Optimization (the Optimizer as a pull-through coordinate-source functor)

**Status:** DRAFT — the optimizer subsystem.

- **LANDED + verified (2026-06-17):** the **pull-through functor contract**
  (in `nmbrs-runtime`) — `Optimizer::coordinate_source`, the
  `CoordinateSource` capability decorators (`as_feedback`/`as_pull`),
  `FeedbackSource::step` (the optimizer primitive), `PullSource`/`LexSource`,
  the capability-favoring default driver, and `NullOptimizer` as the literal
  identity. The **`nmbrs-optimizers` plugin** keeps its 9 loop-form algorithms
  unchanged and adapts each to the source contract through one generic
  **`ThreadBridge`** in the `runtime` bridge; `inventory` registers them and
  `nmbrs describe optimizers` discovers them. The phase-level `optimize:` block
  parses in `nmbrs-workload`.
- **LANDED + verified (2026-06-18):** the **value-model `Coord`**
  (`Vec<AxisValue>`); the phase-level **`optimize:` block** wired end-to-end
  through `executor::dispatch_optimization` (axes auto-gathered from the phase's
  multi-clause `for_each`); the **objective wire** (a named phase-kernel wire,
  read post-execution or *settled* across the run); the **two actuation
  strategies** selected by changeover class — `Coordinate`/`Fixture` →
  phase-rerun, `Control` → in-phase servoing daemon (`optimize::servo`) — plus
  the **hybrid** (mixed coordinate + control in one node,
  `executor::run_hybrid_search`); the **settle-detector pipeline** (cadence-pulse
  `is_stable` verdict + a time-based viability gate); and all three **axis
  kinds** — discrete, **continuous** (sampled by metric-space solvers like
  `nelder_mead`/`cmaes`), and **categorical** — at single OR multiple axes.
  Examples: `nmbrs/examples/workloads/optimizer_*.yaml`; user guide:
  `docs/guide/optimizer.md`.
- **DESIGNED, not yet shipped:** the **node-level `optimizer:` property** over an
  arbitrary subtree (the phase-level `optimize:` block shipped instead); the §6
  settle refinements (viability ladder-walking, EWMA/slope nodes, the `settle:`
  YAML trigger, finer-cadence auto-reconfiguration); the `Control` daemon's
  **Pulse-event** trigger; `Fixture` re-install/re-stack as a path distinct from
  the Coordinate rerun; and a **continuous coordinate** axis alongside a control
  axis in one node (`IndexFn::Hybrid`). (`rate` as a servoed control **landed** —
  via the direct `servo: rate` form, since the fixed `f64` field can't carry a
  `{var}`.)

**Owner:**
- `nmbrs-runtime` owns the **contract** (`nmbrs_runtime::optimize`): the
  `Optimizer` functor, `CoordinateSource`/`PullSource`/`FeedbackSource`,
  `Coord`/`AxisValue`, `SearchSpace`/`Axis`/`AxisKind`/`Changeover`,
  `LexSource`/`PullOnly`, `Budget`/`Report`/`Observation`/`StopReason`,
  `NullOptimizer`, the `OptimizerRegistration` inventory type +
  `by_name`/`describe` discovery. **No dependency on any algorithm crate.**
- `nmbrs-optimizers` owns the **algorithms** (a local loop-form mirror trait +
  the 9 optimizers + manifold tests, fully testable standalone) and the
  `runtime`-feature **bridge** (`bridge.rs`): the `ThreadBridge` loop→source
  adapter, the `AxisValue ↔ f64` numeric projector, and one
  `inventory::submit!` per optimizer.
- `nmbrs` (binary) force-links the plugin and runs `nmbrs describe optimizers`.
- `nmbrs-workload` owns the `optimizer:` configuration surface (a node-level
  property).

**Cross-refs:**
- [SRD-83](83_stop_conditions.md) — stop conditions + trigger algebra. The
  optimizer assigns a **disposition** (`effect: Outcome`) per stop *(step 4
  landed)*; the **settle trigger** and the **settle-timeout error** are
  SRD-83 `(when: polydat-predicate, trigger, effect)` conditions.
- [SRD-82](82_uniform_execution_shells.md) — two-axis `Outcome`; the
  `ErrorPolicy` that routes a non-settling timeout (default `stop`).
- [SRD-42](42_windowed_metrics.md) / [SRD-47](47_metricsql_streaming.md) /
  [SRD-48](48_metricsql_continuous_query.md) — windowed metrics + the **cadence
  window ladder**. The control feedback signal **subscribes to a metrics cadence
  pulse** (the smallest by default); the smallest window is the settle sample
  interval (the pipeline is built efficient at small windows), and subscribing
  **up** the ladder is the sparsity auto-widening — coarser pre-computed
  windows, no new synthesis. Optional finer-interval **auto-reconfiguration**
  and a per-cadence **coalesce-time self-metric** (nanos) are SRD-42 capabilities
  the settle path relies on.
- [SRD-23](23_dynamic_controls.md) — `Control<T>` retarget is the realization
  of a `Control`-class axis and the actuator of the **in-phase daemon**.
- [SRD-40b](40b_synthetic_metrics_from_polydat.md) — the objective is a
  synthetic-metric polydat `value:` expression; **read as a wire** off the
  phase kernel.
- [SRD-77](77_working_sessions_and_refine.md) / [SRD-44](44_workload_checkpointing.md)
  — `refine` skip-set + `instance_hash` = the Fixture re-stack mechanism.
- [SRD-18b](18b_scenario_tree_and_scheduler.md) — one walker; one kernel per
  scope; iteration vars as scope outputs. The optimizer adds *selection*, not
  a new walk.
- [SRD-71](71_cursor_partitions.md) — interior comprehension nesting; the
  template for control-axis interior time-frames within one phase.
- [SRD-78](78_polystreamer.md) — `CoordinateStream` is the lazy pull surface;
  the default `LexSource` is its optimizer-facing form.
- [SRD-35](35_driver_resources.md) — fixture resources outlive a per-point
  re-stack.
- [SRD-05](05_dependency_rules.md) — layer placement + Contract Registry.

---

## Why this exists

We want non-derivative (gradient-free, black-box) multi-factor optimization
of a workload: choose parameters that maximize a value function over produced
metrics — `recall − λ·latency_p99`, a constrained throughput-under-SLA, a
named metric. The factors are **comprehension axes**; a value along an axis is
a **detent**; a testing point is a coordinate.

The planning pass already produces a coordinate stream over the axes (the full
Cartesian product, in lex order). An optimizer is **a stateless functor that
transforms that default stream** — given the param space + the default lex
stream, it produces the (possibly adaptive) stream of coordinates the executor
will actually run, and is told each coordinate's objective so it can choose
the next. Its **default is `sweep`: the identity functor** — it returns the lex
stream unchanged, reproducing today's Cartesian sweep. Every real optimizer is
a drop-in at this one functor.

The full rationale and the changeover "visions" live in
`local/opt_vision_{1,2,3,4}_*.md` (exploration). This SRD is the canonical,
normative form.

---

## Contract surface

### The functor + the pull-through source (core, `nmbrs_runtime::optimize`)

An optimizer is a **stateless functor**; the search state lives in the
**source** it produces, which the executor pulls through.

```rust
/// One coordinate — one realized value per axis. The model carries the
/// COMPREHENSION'S ACTUAL VALUES (labels/detents), never pre-cast numbers.
pub type Coord = Vec<AxisValue>;

/// The value an axis takes at a coordinate — representative of the source
/// comprehension, presented to solvers as-is. Numeric casting is a solver-
/// internal stub (see "Tiered consumption"), never in the model.
pub enum AxisValue { Num(f64), Label(String), Bool(bool) }

/// A coordinate source the executor pulls through. It advertises capability
/// decorators; the executor probes them, FAVORING the most capable
/// (`FeedbackSource`) and requiring at least one.
pub trait CoordinateSource {
    fn as_feedback(&mut self) -> Option<&mut dyn FeedbackSource> { None }
    fn as_pull(&mut self) -> Option<&mut dyn PullSource> { None }
}

/// Least-capable: yield the next batch, no feedback (null / lex / discrete).
pub trait PullSource { fn pull(&mut self) -> Option<Vec<Coord>>; }

/// Most-capable — THE optimizer primitive. Given the just-evaluated
/// (coord, objective) pairs, yield the next batch (None = done). First call
/// gets `&[]`. Batch methods (CMA-ES generation, Hyperband bracket) return
/// several coords and buffer the matching values across calls.
pub trait FeedbackSource {
    fn step(&mut self, evaluated: &[(Coord, f64)]) -> Option<Vec<Coord>>;
}

/// The optimizer = a stateless functor producing a source. `sweep` returns
/// `lex` unchanged (identity).
pub trait Optimizer {
    fn name(&self) -> &str;
    fn doc_md(&self) -> &str;                       // markdown, surfaced by `describe`
    fn coordinate_source(&self, space: &SearchSpace, budget: &Budget,
                         lex: Box<dyn PullSource>) -> Box<dyn CoordinateSource>;
    /// Default standalone driver (pull → eval vs `Objective` → feed back →
    /// track best). The executor drives the source itself (pull-through).
    fn optimize(&self, space: &SearchSpace, obj: &mut dyn Objective,
                budget: &Budget) -> Report { /* drive_source(...) */ }
}

/// The default lexicographic stream over the space (mixed-radix Cartesian of
/// the detents / continuous corners). `sweep` is `PullOnly(LexSource)`.
pub struct LexSource { /* … */ }
pub struct PullOnly(pub Box<dyn PullSource>);   // wrap a PullSource as a CoordinateSource
```

The `SearchSpace` carries the **actual detents and the axis kind** so a solver
can tell ordered-metric (ordinal) axes from nominal ones — a stable,
positionally-ordered list of choices with no distance metric:

```rust
pub struct SearchSpace { pub axes: Vec<Axis> }
pub struct Axis { pub name: String, pub kind: AxisKind, pub changeover: Changeover }
pub enum AxisKind {
    Continuous  { lo: f64, hi: f64, min_step: f64 },   // a real interval
    Discrete    { detents: Vec<AxisValue> },           // ordered detents (ordinal)
    Categorical { options: Vec<AxisValue> },           // Nominal: STABLE ordered list (positions meaningful), no metric
}
/// Cost prior AND actuation selector (see "Two actuation strategies").
pub enum Changeover { Control, Coordinate, Fixture }

pub struct Budget { pub max_evals: usize, pub max_seconds: Option<f64>, pub seed: u64 }
pub struct Observation { pub value: f64, pub feasible: bool, pub cost: f64,
                         pub metrics: Vec<(String, f64)> }
pub struct Report { pub best: Coord, pub best_value: f64, pub evals: usize,
                    pub stop: StopReason, pub ranked_axes: Vec<AxisImpact>,
                    pub history: Vec<(Coord, f64)> }

pub fn by_name(name: &str, params: &OptimizerParams) -> Option<Box<dyn Optimizer>>;
pub fn registered_names() -> Vec<&'static str>;
```

### Tiered consumption — values first, numeric as a solver stub

The model presents **actual values**; only the metric-space solvers cast, and
they cast **internally**, snapping back to a real detent/label before they
yield a coordinate — so the user-visible loop reads `metric=cosine`, never
`metric=0`:

| Tier | Methods | Sees | Cast |
|---|---|---|---|
| **Value-native** | `sweep`, `cost_greedy_traversal`, screening, bandits | the `AxisValue`s / labels directly (reorders, samples) | none |
| **Numeric** | `nelder_mead`, `hooke_jeeves`, `bobyqa`, `cmaes`, `bayes_opt` | f64 via a stub `AxisValue ↔ f64` projector (ordinal→number, categorical→index/one-hot, continuous→itself), keyed by `AxisKind` | solver-internal |

A numeric solver **rejects categorical axes in its own purview** (a nominal
label has a stable position but no metric/distance — a numeric solver assumes a
metric space); validation routes a categorical axis to an **outer node**
([§3 The node-level `optimizer:` property](#3-the-node-level-optimizer-property-a9),
enumerate/bandit). The projector lives in the runtime
[`nmbrs-optimizers/src/bridge.rs`](../../nmbrs-optimizers/src/bridge.rs) next to the
[`ThreadBridge`](../../nmbrs-optimizers/src/bridge.rs) (the loop→pull-through
adapter — see [Contract surface §"The loop→source bridge"](#the-loopsource-bridge-runtime-nmbrs-optimizerssrcbridgers)); the local f64 algorithms are unchanged.

### The loop→source bridge (runtime, `nmbrs-optimizers/src/bridge.rs`)

The 9 algorithms stay **loop-form** (`optimize(&mut self, space, obj, budget)
-> Report` — their natural shape). One generic `ThreadBridge` adapts a loop to
the pull-through source: the search loop runs on a worker thread; each
`Objective::query(coord)` ferries the coord out through `step` and blocks for
the value (`ChannelObjective` over two mpsc channels). `CoreBridge::
coordinate_source` spawns it. The loop-vs-state-machine choice never leaks past
this seam; the executor only ever sees a clean `FeedbackSource`.

### Required inbound contract (what the runtime supplies)

- A `SearchSpace` **auto-gathered from the subtree's comprehensions** (names,
  detents, kinds, changeover classes) — not relisted in config.
- A `Budget` from the node's `optimizer:` block.
- The **objective wire name**; the executor reads it off the live phase kernel
  after each iteration and `tell`s the scalar via `step`. The optimizer is
  pure — it never sees wires, kernels, or polydat.

---

## Axioms (load-bearing — a proposal may not contradict these)

- **A1 — One functor seam, `sweep` = identity.** There is exactly one optimizer
  in the phase-execution driver, expressed as a functor over the coordinate
  stream. Its default is the identity `sweep` (returns the lex stream
  unchanged). Installing it is a behavioural no-op until a non-`sweep` method is
  named. No optimizer adds a second walk modality (SRD-18b One Walker).
- **A2 — The algorithm library is runtime-free + loop-form.** `nmbrs-optimizers`
  depends on no nmbrs runtime crate, no polydat, no metrics; its algorithms keep
  their natural loops and are unit-tested against synthetic manifolds with zero
  runtime. The loop→pull-through adaptation is confined to the runtime bridge.
  D5-exempt, extractable (like polydat / nmbrs-metricsql).
- **A3 — Algorithmic state is Rust; declarative surfaces are polydat.**
  Cross-point state (simplex, surrogate, trust region, best-so-far) lives in
  the source. Polydat owns only the objective expression, the search-domain
  comprehension, the convergence predicate, and the settle statistics. The
  optimizer never smuggles mutable cross-point state into a kernel.
- **A4 — The model is the comprehension's values, not numbers.** A coordinate
  is `Vec<AxisValue>` carrying the actual detents/labels. The schema is
  canonical; the lex stream is derived from it. Numeric casting is a
  solver-internal stub, never in the model — labels survive end-to-end so the
  user-visible feedback stays semantic.
- **A5 — Changeover class selects the *actuation*, not the search.** Each axis
  declares `Control | Coordinate | Fixture` — *how a coordinate change is
  physically enacted*. The `FeedbackSource` is identical across classes; the
  **executor picks the actuation strategy**: `Coordinate`/`Fixture` re-run the
  phase per coordinate; `Control` retargets a live control via an **in-phase
  daemon** on one continuous phase. Mixed classes compose by node nesting.
- **A6 — Nesting defines prerequisite order; refine computes the re-stack.** A
  coordinate change re-runs exactly the phases whose `instance_hash` changed —
  the install phase plus everything interior. This is the existing `RefinePlan`
  skip-set; the optimizer adds no re-stack machinery. Fixtures outlive the
  per-point re-stack (SRD-35).
- **A7 — A point failure is a datum, not a search abort.** Within an
  optimization scope, a probe whose `Outcome.validity == Failed` is an
  infeasible observation (objective = penalty); the search continues. The
  default SRD-83 `children_failed > 0 → fail` is reconfigured to
  penalize-and-continue inside the scope.
- **A8 — Every stop carries a disposition.** A stop's `effect` is a two-axis
  `Outcome` (SRD-83): `converged`/`budget` → `Interrupted+Succeeded` (keep
  best); `no-feasible-point` and a non-settling timeout → `Interrupted+Failed`.
- **A9 — The optimizer is a node property; purview is the subtree.** An
  `optimizer:` declared at any scope-tree node governs the axes at that node
  and its interior **only**; everything outside sequences as plain lex (before
  and after). Nested nodes carry independent functors that **compose** (the
  outer sees the inner's transformed stream). Default everywhere is `sweep`.
- **A10 — The objective is a wire on the phase scope's single kernel.** Kernels
  are **one (max) per scope-tree node**, aligned to the graph — a phase scope has
  one phase kernel; op templates are proper subscopes of it (occasionally shared/
  re-used). The objective is an ordinary phase wire — pure, stateful (registers,
  accumulators, PID loops), or volatile, with statefulness carried by upstream
  provenance taint — so it can read the run's metrics, not only the coordinate.
  The executor reads it off **that one kernel** at completion
  (post-state-population), the canonical completion-pull; **never** a
  reconstructed second instance. Coordinates are set as a matter of phase setup;
  the optimizer never writes coordinates to read the objective.
- **A12 — An `optimize` node is a search specification, not a traversal
  pre-plan.** A `for_each` node's meaning is the enumerated coordinate set the
  One Walker (SRD-18b) pre-plans; an `optimize` node's coordinates are
  feedback-driven and cannot be pre-enumerated, so its meaning is a *bounded
  search* `(method, space, objective, budget)`. The walker reconciles by depth:
  **pre-map** represents the node as "search over S, ≤N evals" (never an
  enumeration) and pre-maps the coordinate-invariant subtree **once** (it runs
  ≤N times — resource pre-map, SRD-35, stays correct); **execution** runs the
  adaptive loop, visited coordinates emerging at runtime. The node carries a
  distinct **kind** (Search, not ForEach) and **every user view renders it as
  such** — the scene tree shows a search header with its spec and *evaluations*
  as children (not a pre-listed coordinate set), the TUI shows progress against
  budget + current best (not a fixed list), and dryrun/describe report the spec
  and budget bound, never a fabricated enumeration. A discrete candidate grid
  may inform the *space*, but it is never the node's *representation*.
- **A11 — A settle verdict is only valid when the signal is viable.** A
  steady-state reading requires viability (warmup elapsed + enough distinct,
  fresh samples). The detector emits **three** states — *indeterminate* (hold),
  *settled* (step), *timed-out* (error). A sparse or cold signal never
  masquerades as converged; a never-settling signal fails with a disposition.
  The detector **subscribes to a metrics cadence pulse** (the smallest by
  default) rather than a private sampler, and widens by **walking up the cadence
  window ladder** — the metrics pipeline is built efficient at small windows
  precisely to be the settle clock.

---

## Mechanism tier

### 1. The functor + capability dispatch

The executor builds the lex source for a node, applies the optimizer functor,
and drives the result, **favoring the feedback decorator**:

```
let lex = LexSource::new(space);
let mut src = optimizer.coordinate_source(space, budget, Box::new(lex));
// per node iteration: probe the most-capable decorator, require one
while let Some(coords) = next(&mut src, &evaluated) {   // step(...) or pull()
    for c in coords { run_iteration_at(c); evaluated.push((c, read_objective_wire())); }
}
```

`sweep` → `PullOnly(lex)` (identity). An adaptive method → a `FeedbackSource`
whose `step` consumes the just-evaluated values. Batch methods buffer their
batch inside the source and advance on `step`.

### 2. The value model + numeric stub (A4)

`AxisValue` carries the actual comprehension value. Value-native methods use it
directly. Numeric methods wrap their f64 loop with the `bridge.rs` projector:
`AxisValue → f64` on the way in (ordinal detent → its number; categorical →
index; continuous → itself), `f64 → AxisValue` on the way out (snap to the
nearest detent / clamp the interval / round to a label index). The executor
maps `AxisValue ↔ polydat::Value` at scope materialization — the boundary it
already owns.

### 3. The node-level `optimizer:` property (A9)

`optimizer:` is a property on any scope-tree node (scenario / phase / `for_each`
scope), default `sweep`. The executor:

1. **Auto-gathers** the `SearchSpace` from the node's subtree comprehensions
   (each comprehension axis contributes its name, detents/range, kind, and
   changeover class). Axes are not relisted in config.
2. Wraps the subtree's coordinate stream with the functor and drives it through
   the **existing `for_each` iteration machinery** (the subtree runs once per
   pulled coordinate).
3. Reads the **objective wire** off the phase kernel after each iteration and
   feeds the scalar back via `step` (A10).

Outside the node, the walk is plain lex. Nested `optimizer:` nodes compose:
the inner functor transforms its sub-stream; the outer sees that transformed
stream as part of its purview. This is also how **mixed numeric/categorical**
and **mixed changeover** resolve — categorical / fixture on an outer node,
numeric / control on an inner node.

```yaml
sweep:
  optimizer: { method: cmaes, objective: recall_score, budget: 300 }
  for_each: "ef in 64.0 .. 512.0"        # ← axis, auto-subsumed
  phases:
    probe:
      for_each: "rerank in 10 .. 100"     # ← interior axis, also subsumed
      ...                                  # recall_score read per coordinate
```

**Search-space structure — holes and nesting must be preserved.** The space is
derived from the *layered* comprehensions, and that layering carries structure a
flat per-axis Cartesian product would discard:

- **Holes** — when a subtree's comprehensions do not enumerate a full Cartesian
  product (a filtered/conditional comprehension), the feasible set has holes.
  The optimizer must propose only feasible coordinates; the enumerated step set
  *is* the feasible set, holes included — never its Cartesian hull.
- **Hierarchic subspaces** — when an inner axis's domain depends on an outer
  parameter's value (`y in 1 .. x`), the inner space is a **function of the
  outer coordinate**, not a fixed range. The space is a *layering of subspaces*
  (outer value → inner subspace), preserving proper subspace structure with
  respect to the outer (non-normalized) parameters — exactly the node nesting
  (A9), at the search-space level.

**Two coordinate-realization paths (`CoordEval`).** How a proposed coordinate
becomes the `IterationStep` to run depends on whether the axis is enumerable:

- **Discrete / categorical — `Enumerated`.** The feasible set *is* the
  pre-enumerated grid (it carries holes), so a proposed coordinate is matched to
  its step by coordinate key and an off-grid proposal is skipped as infeasible.
- **Pure-continuous — `Synthesized`.** A float-range source (`x in lo .. hi` →
  `Source::ContinuousInterval`) enumerates **zero** tuples; the optimizer *is*
  the sampling strategy. The space is read straight from the comprehension's
  static `IndexFn::Continuous { intervals }` metadata (no enumeration), and each
  proposed real coordinate is bound into a fresh iteration kernel via the **same
  `PolydatKernel::for_iteration` path** a discrete tuple uses — `AxisValue →
  polydat::Value` at the scope-materialization boundary the executor already
  owns. No hull, no holes, no skipping: every proposal is feasible by
  construction. (V8's continuous-requires-order check is a *validation* concern;
  the executor reads `metadata()` directly and never enumerates, so it never
  applies here.) Reference: `nmbrs/examples/workloads/optimizer/continuous.yaml`.

*Status:* The **continuous** path is fully operational at **single OR multiple**
axes — a multi-clause `for_each` of float ranges (`x in 0.0 .. 6.0, y in 0.0 ..
8.0`) is sampled jointly by a metric-space solver (see
`nmbrs/examples/workloads/optimizer/multiaxis.yaml`). **Refinement still open** on the
**discrete** path: `search_space_from_steps` derives a flat per-axis hull from
the enumerated grid — it visits the feasible set *correctly* (off-grid proposals
are skipped) but presents the **hull** to the optimizer, so an adaptive method
wastes proposals on holes and mismodels dependent axes. Preserving the holey /
hierarchic structure (so the optimizer proposes only feasible coordinates and
models the per-outer subspace) is the next refinement. A **continuous coordinate**
axis *combined with a control axis* in one node (`IndexFn::Hybrid`) is the
remaining hybrid follow-up (today the hybrid path requires enumerated coordinate
axes).

The `objective:` is either a **bare wire reference** — a single identifier
naming any binding / metric / capture in the subtree, read directly off the
phase kernel — or an **inline polydat expression** (anything with operators or
calls, e.g. `objective: "0 - metricsql_scalar(\"sum(rate(errors_total[3s]))\")"`).
An inline expression is lowered to a synthesized `volatile __objective := <expr>`
binding on the phase kernel by `synthesize_phase_scope_bindings`, and the
optimizer reads the resolved wire from `scope::objective_wire` (the bare name, or
`__objective`) — so an author can express the objective inline without
pre-declaring a `bindings:` entry. Both forms behave identically downstream:
volatility (settle-vs-one-shot) keys off program-wide reader-node presence, so a
metricsql inline objective settles per setting and a deterministic one takes the
one-shot read. As a YAML surface convenience, a bare **string** `optimize:` value
is sugar for `{ objective: <string> }` with every other field defaulted
(`OptimizeBlock::from_yaml_value` / `de_optimize`), so the common "just maximize
this expression" case drops the map entirely. The requirement is
**capability-conditional**: a `FeedbackSource`
optimizer cannot run without it (validation errors if absent); a pull-only
optimizer (`sweep`) treats it as optional (used only to report the best). Dryrun
also checks the resolved wire is a pullable phase-kernel output.

### 4. Two actuation strategies (A5) — selected by changeover class

The `FeedbackSource` is the same; the changeover class picks how the executor
*enacts* each coordinate and feeds the value back:

| Class | Actuation strategy | Mechanism |
|---|---|---|
| `Coordinate` / `Fixture` | **phase-rerun** | pull coord → re-run the target phase (Fixture: re-install + re-stack via refine, A6) → read objective → `step`. One phase per coordinate. |
| `Control` | **in-phase daemon** | ONE continuous phase + a concurrent daemon: pull the next control setting → apply it to the live `Control<T>` (SRD-23 retarget, no restart) → wait the feedback cadence → read the windowed objective wire → `step`. |

Mixed = **partition within one node** (or node nesting): coordinate/fixture axes
iterate at their boundary (rerun), control axes servo interior — see the hybrid
note below.

**Status — `Coordinate`, `Control`, and mixed (`hybrid`) are wired; `Fixture`
shares the Coordinate path.** Every axis is a `Coordinate` (stepped through by
re-running the phase) by **default**; servoing is an **explicit, validated
opt-in** via `optimize.servo` (`servo: concurrency` or `servo: [concurrency,
rate]`). The classifier (`executor::classify_control_axes`) resolves each *named*
var to a live control either **directly** (the var's name IS a control —
`servo: concurrency`, `servo: rate`) or **indirectly** (it sinks into a control
field, `concurrency: "{conc}"` ⇒ the `concurrency` control, then `servo: conc`).
Downstream (`require_windowed_objective`) the objective must be a windowed metric
the servo can settle — any miss is a **clear error**, never a silent downgrade
(so a half-specified servo surfaces, rather than masking the author's mistake).
There is no inference and no separate override: presence/absence of `servo:` *is*
the choice. (The **direct** form is the only way to servo `rate`, whose fixed
`f64` field can't carry a `{var}`; servoing `rate` therefore requires the phase to
set `rate:` — its value is the warmup the daemon retargets from — checked at
validation time, not at runtime, since the `rate` control is only declared when the
field is set, whereas `concurrency` is always declared. The indirect "sinks into a
control" check is a textual `{var}` match against `concurrency:`; the principled
form traces the var's l-value flow to a control sink.)
The daemon (`optimize::servo`) is a **concurrent async task `tokio::join!`'d with
one continuous phase** inside `run_phase` — *not* a cadence callback, because
`Control::set` is async (confirmed-apply) while the cadence callback is sync. It
reuses `start_settle` **per setting** (a throwaway per-setting stop flag absorbs
the settle verdict so it doesn't end the phase), `await`s the retarget
(`ErasedControl::set_f64` resolved off the phase component), and ends the phase
itself on budget exhaustion — a **clean** stop (`servo_completed`), distinct from
an error-handler stop, mirroring `settle_succeeded`. Caveat: a continuous control
phase that dwells at a saturating setting sustains a high error rate, so a
`Control` sweep must not carry a *tripping* error guard (`error_rate_max: 1.0`
compiles to `error_rate > 1.0` — never trips — i.e. "allow 100%").

**Hybrid (mixed coordinate + control in ONE node) — `executor::run_hybrid_search`.**
A single optimize node may carry both: a multi-clause phase `for_each: "batch in
[1,2], conc in [8,16]"` with `servo: conc` — `conc` is servoed, `batch` (not in
`servo:`) steps through. The dispatch
**partitions** the search space — control axes are the inner servoed subspace K,
the rest are the coordinate subspace C. The coordinate axes form the OUTER rerun
grid (the distinct cells are enumerated and de-duplicated from the steps); for
each cell the phase is re-run bound at that cell and `run_servo_cell` runs the
Control daemon over K interior to it (`method`/`budget` per cell). A coordinate
axis is therefore realized **only** by iterating its scope (never set
ineffectually); the control varies live within each fixed-coordinate phase. The
best is reported as `(coordinate-cell ; control-setting)`. `conc` written last is
innermost by lex order, but the partition is order-independent within a node.
References: `nmbrs/examples/workloads/optimizer/control.yaml` (all-control),
`nmbrs/examples/workloads/optimizer/hybrid.yaml` (mixed).

**Deferred:** the `Control`-daemon's `Pulse-event` trigger and configurable
feedback cadence (§5) beyond the reused settle; `Fixture` re-install/re-stack
(A6) as a distinct path; a **continuous** coordinate axis alongside a control
axis in one node (`IndexFn::Hybrid`); a joint optimizer over coordinate×control
with adaptive coordinate search + axis reordering to push controls inner from an
intermediate level (honoring infra-stacking rules + stack-churn cost).

### 5. The in-phase daemon + configurable feedback cadence

A `Control`-class node runs its phase continuously; the daemon servos it. The
feedback signal is a **subscription to a metrics cadence pulse** — by default
the **smallest configured cadence** (no separate sampler; a specific rate, e.g.
10 Hz, means configuring a metrics cadence at that rate). At each pulse the
daemon evaluates a configurable SRD-83 trigger:

- **Pulse-event:** step the optimizer on every pulse. Aligned to reporting.
- **Steady-state settle:** step only once the objective has *settled* — the
  settle predicate evaluated at each pulse (next).

### 6. The settle-detector pipeline (A11)

A small pipeline on existing rails — the metrics cadence window ladder
(SRD-42/47/48), SRD-83 triggers/stop, the SRD-82 ErrorPolicy, polydat
predicates — plus a few statistic nodes.

1. **Sample cadence — subscribe to a metrics cadence pulse.** The detector
   evaluates at each pulse of a metrics cadence — **the smallest configured
   cadence by default**, never the activity loop and never per-op. There is no
   private sampler: a specific rate (e.g. 10 Hz) means *configuring a metrics
   cadence* at that rate. Optionally a settle may **name a target cadence**, and
   the metrics system **auto-reconfigures a cadence to a finer interval** when
   the loop needs faster feedback than any existing layer provides (the dual of
   walking *up* the ladder for sparsity). The pipeline (SRD-42/47/48) is built
   efficient at small windows precisely so this base sampling is cheap.
2. **Viability gate — before any verdict.** A polydat predicate over the
   windowed wires: **warmup** (`elapsed ≥ min_elapsed` AND `≥ min_samples`) and
   **density/freshness** (enough *distinct, non-stale* samples — new data
   actually arrived). Until viable → **indeterminate**, the loop holds.
   `min_elapsed` is **auto-sized to the objective's widest rollup window** —
   `PolydatProgram::max_temporal_window_ms`, fed by each metricsql reader node's
   `temporal_window_ms` (the max `[W]` in `rate(m[W])` / `*_over_time(m[W])`).
   This is what makes a per-coordinate `rate()` correct over a *session-cumulative*
   counter (SRD-42 cumulative-counter model): a coordinate cannot settle until its
   rate window has cleared the prior coordinate's data, so the window is scoped to
   the coordinate's time range with no engine-level range clamp ("no storage trick").
   Only **bounded** readers are gate-scopeable (`metricsql_*` rollups, `metric_window`).
   The session-cumulative `metric(...)` reader (`session_lifetime`) has no bounded
   window — it aggregates across every coordinate — so no warmup can isolate it; the
   settle **warns** that such an objective won't isolate per-coordinate and servos the
   author to a windowed reader instead.
3. **Sparsity handling — walk *up* the cadence ladder.** When the current
   cadence window is too sparse for viability, **subscribe to the next-coarser
   cadence's pulse** — a larger, pre-computed window the pipeline already serves
   (SRD-47/48). The ladder of cadence window sizes **is** the auto-widening:
   nothing is synthesized and no window grows ad hoc; the detector re-reads at
   the coarser cadence until viable.
4. **Settle verdict.** Over the subscribed cadence window, `measure ∈ {cv,
   stddev, slope}` within `margin`. The windowed mean and standard deviation are
   the pipeline's windowed aggregations; the detector adds an EWMA-smoothed
   central estimate (the "leavened integral"), `cv = σ/μ` (or absolute σ) below
   the margin, and optionally a zero-trend slope so a slow drift doesn't read as
   settled.
5. **Timeout → error → handler (default `stop`).** If no viable-and-settled
   verdict within `timeout`, emit a `settle_timeout` error routed through its
   own SRD-82 `ErrorPolicy`, defaulting to `stop` (configurable). Structurally
   an SRD-83 stop-condition `(when: timeout-elapsed AND not settled, effect:
   failed-Outcome)`.

```yaml
trigger:
  settle:
    cadence: <name>                        # optional; default = the smallest configured metrics cadence
    measure: cv                            # cv | stddev | slope
    margin: 0.02
    warmup: { min_samples: 30, min_elapsed: 500ms }
    # sparsity → subscribe up the cadence ladder automatically (coarser windows)
    timeout: 30s
    on_timeout: { error: settle_timeout, handler: stop }   # SRD-82 policy
# To settle at e.g. 10 Hz, configure a 10 Hz metrics cadence; by default the
# detector subscribes to the smallest cadence's pulse.
```

**New pieces are minimal:** windowed mean/variance come from the pipeline's
existing windowed aggregations (SRD-42/47/48); the detector adds an EWMA
accumulator, a distinct-sample-count + freshness viability node, and a
zero-trend slope, then composes the viability + settle predicates over them. The
cadence ladder, pulse subscription, triggers, and error policy are all existing
machinery.

**Coalescing hot-spot guard.** Auto-reconfiguring a finer cadence makes the
cadence coalescing logic do more work per unit time, so the coalescing path
**self-instruments**: each cadence folds in a metric of its own coalescing time
over time (nanoseconds — the Nanos Standard), with the **metric name aligned to
the cadence** (there are many cadences, so the timing is per-cadence). This
makes a settle-driven finer cadence's cost visible — so auto-reconfiguration can
back off (or refuse to go finer) rather than silently becoming a hot spot. This
is an **SRD-42 capability** the settle path depends on, not optimizer-local.

### 7. Axis changeover classes (A5) — the V1/V2/V3 unification

| Class | Realization | Cost | Example |
|---|---|---|---|
| `Control` | retarget a live `Control<T>`; in-phase daemon (SRD-23) | ~0 | rate, concurrency, wire behaviours |
| `Coordinate` | re-bind as an iteration extern; re-run the target phase | medium | a query-shaping param read per cycle |
| `Fixture` | install via setup lifecycle; re-install + re-stack interior (refine) | high | index `m`, dataset load, DDL |

### 8. Nesting → minimal re-stack (A6)

The objective phase's ancestor chain is its prerequisite set. Changing an outer
fixture coordinate re-fingerprints the install phase and everything interior;
`RefinePlan` re-runs exactly that sub-tree and skips the rest. Fixtures outlive
the re-stack (SRD-35).

### 9. Dispositions (A7/A8) — SRD-83 step 4 *(landed)*

`StopCondition` carries `effect: Outcome`; `StopConditionSet::evaluate ->
Option<(Outcome, String)>`. Per-point: `Validity::Failed` → objective penalty,
continue. Per-search: `Report.stop` → search `Outcome` (`Converged`/`Budget` →
`Interrupted+Succeeded`; `NoFeasiblePoint` / settle-timeout →
`Interrupted+Failed`).

### 10. The value function

The objective is an ordinary phase wire (A10) — it may read the run's synthetic
metrics, stateful registers, or volatile nodes, not only the coordinate.

**Where it is defined — fully qualified on the node (normative).** The
objective's wire references must be **fully qualified on the node where it is
defined** — it reads only wires already resolved on that node's matter. It is
then an ordinary compiled wire on the node's **single** kernel, and its validity
is left to the **existing, already-supported** kernel compilation: an objective
that references an unresolved wire simply fails to compile (no new validation
machinery). The executor reads it off that one kernel at completion — one kernel
per node, never a reconstructed instance: a clone lacks the node's per-eval state
(the coordinate, any stateful registers). (The `metric(...)`/`metric_window(...)`
readers resolve a *global* query handle — see the reader surface below — so they
read the same from any kernel; it is the coordinate and the registers, not the
metric readers, that a clone loses.)

**Causal ordering, the freshness register, and settling.** The objective
*measures the phase's net effect*, so it is read **after execution** — reading
before the ops run reads the wrong epoch. (Qualified ≠ pure ≠ stable: a pure
function of a volatile metric is still pure, but its input is not stable when you
look.) At completion a *windowed* read — `metric_window`, or a no-lookback instant
query — sees the *empty trailing window* (`metric(...)` itself is session-cumulative,
not windowed; a lookback `metricsql` query self-averages — see the reader surface
below), so a volatile windowed objective must be **captured into a register during
the run**:

- `objective_value := <expr, incl. metric(...)>` — the raw, possibly volatile
  objective.
- `stable_objective_value` — a **register wire**.
- `is_stable(objective_value, stable_objective_value, …)` — a side-effecting
  function evaluated each cycle: it stuffs the latest reading into the register
  and returns a **stable signal**.

The optimizer's `objective:` then names `stable_objective_value` — a plain wire
read off node X's kernel after completion (the register already holds the value;
no metric re-eval at read time). **Settling is wire conditioning**, not a
sampler bolted onto the executor: `is_stable` is the filter; the executor reads
its stable signal to decide when the phase **stops** and returns the conditioned
register up to the harness (graph-ordered or optimizer-driven). Because
`is_stable` takes its inputs **as wires** (not runtime globals like
`control_set`/`metric`), it is **fully verifiable in polydat function space** —
settling is proven by polydat function tests, and the thin executor seam (pull
per cycle, read the signal, read the register) by the causal integration test.

*Future possibility — deferred matter.* For an objective that must bind wires
resolved only later, the objective-function matter could be **appended** to the
phase kernel's AST in the post-resolve / pre-compile window (incrementally valid,
referencing only resolved wires) and then compiled. This is noted as a design
option, **not** required while objectives stay fully-qualified-on-node; it is also
the path that would let a phase combine `for_each` with phase-level `metrics:`
for a metric objective.

For the in-phase daemon the objective is a **windowed/settled** wire
(throughput/latency over the trailing window). Raw metrics persist per point
(SRD-44) for post-hoc rescoring (change λ without re-running) and multi-objective.

#### The metric-reader surface (how a wire reads metrics)

Two reader families coexist; an objective (or any wire) reads through whichever
fits. They are **not** unified — one is a cheap fixed-vocabulary stat reader, the
other a full query language — and neither is re-platformed onto the other.

**1. MetricSet stat-readers — `metric(...)` / `metric_window(...)` (kept).**
Family-named readers over the in-process `MetricSet`
(`nmbrs-metrics::polydat_nodes`). `metric(pattern, stat)` reads
`MetricsQuery::session_lifetime` (session-wide cumulative); `metric_window(pattern,
stat)` reads the smallest cadence's last closed window. The pattern names the
instrument FAMILY (a bare token; `key=value`/`key~sub` parts narrow the series —
2026-07-10, the canned `cycles`/`errors` stat vocabulary is retired), and both
return `f64` over the family's own point: `count`/`value`/`rate` (counters,
gauges), `mean`/`p50`/`p99` (histograms). They resolve a
**global** query handle set once by the runner — they are *not* component-bound,
so the kernel they are read from never changes their value. `metric` is monotonic
session-cumulative; `metric_window` is the per-eval-isolated but **volatile**
(empty-trailing-window) reader. Cheap, no query parse — the default for simple
objectives and status lines.

**2. MetricsQL query readers — the `metricsql_*` family (additive).** A full
MetricsQL expression carries **scope in its label matchers** (scope tags close
over the component dimensions) and **time-frame in its range/offset/`@`** — so the
where/when axes need no separate function-name taxonomy; the query states them.
Because polydat wires are typed and MetricsQL results take several shapes, the
family is keyed by **result-type affinity**, at **one precision throughout — f64**,
since the engine computes every value in f64 (`Sample.value: f64`); downcasting to
f32 on the way out would be lossy and split the family across precisions.

| accessor | asserts result shape | returns |
|---|---|---|
| `metricsql(q)` | any | `Value::Json` (full labeled series / matrix) |
| `metricsql_scalar(q)` | 1 label-less series × 1 sample | `f64` |
| `metricsql_vector(q)` | instant vector (N series × 1 sample) | `VecF64` (values; labels dropped) |
| `metricsql_window(q)` | range vector, **single** series × M samples | `VecF64` (the window's values) |

MetricsQL/PromQL has exactly four result types — **scalar, instant vector, range
vector, string**. String is producible only by a bare string literal (no selector,
aggregate, or rollup yields it; `label_join`/`label_replace` return vectors), so
there is **no** `metricsql_string`: the typed accessors **error** on a string — or
any shape that does not match — PromQL-type-checker-style. The label-bearing shapes
a flat `VecF64` cannot carry (an instant vector with N>1 series, or a range
*matrix* of N>1 series × M samples) are the job of the general `metricsql(...) →
Json`, which preserves labels and the 2-D shape; the typed accessors assert down to
the single-series / scalar cases.

**Dependency + coverage.** `nmbrs-metricsql` evaluates against the `MetricAccess`
data-access service ([SRD 40c](40c_metric_query_api.md)); the `metricsql_*` nodes reach
live data through its in-process backend (`MetricsQueryAccess`, a **cadence read** of the
metrics cadence-feed store), installed per session via `queryapi::install_live_access`. The
family's evaluable surface equals the engine's: selectors, `sum`-family aggregates,
`rate`/`*_over_time` rollups, binary ops — with `string` / `duration` / parens /
`WITH` / non-aggregate-non-rollup funcs surfacing `NotYetImplemented`.

**Relationship to settling.** A lookback query is self-averaging:
`metricsql_scalar("sum(rate(errors[1m]))")` evaluated *instant at completion* looks
back one minute, so it is **not** the empty-trailing-window case `metric_window`
hits. Where the objective is such a windowed query, the settle daemon's role
narrows to **steady-state detection + holding the last good value across the stop**
rather than *reconstructing* a value the read cannot otherwise see.

### 11. The optimizer registry

Inventory-discovered, selected by `optimizer: { method: <name> }`, default
`sweep`.

| name | space | tier | cost-aware | parallel | role |
|---|---|---|---|---|---|
| `sweep` *(default)* | any | value-native | — | yes | identity — current Cartesian sweep |
| `cost_greedy_traversal` | discrete | value-native | yes (learned) | yes | minimize changeover, fixed points |
| `centroid_variant` | mixed | value-native | partial | yes | screening — rank axis impact |
| `nelder_mead` | continuous | numeric | via economy | no | simplex, robust local |
| `hooke_jeeves` | mixed | numeric | yes | no | pattern search, cost-structured |
| `bobyqa` | continuous | numeric | via economy | no | trust-region quadratic model |
| `cmaes` | continuous | numeric | partial | yes (gen=batch) | noisy / multimodal / ill-conditioned |
| `bayes_opt` | mixed | numeric | yes (EI-per-cost) | batched | expensive eval, few re-stacks |
| `hyperband` | mixed | numeric | yes (fidelity) | yes | multi-fidelity: cheap probes → survivors |

`centroid_variant` (screening): `1 + 2k` probes — centroid baseline `f0`; per
axis `±Δ` OFAT → a 3-point curve; rank by main effect and curvature. Output
`ranked_axes` for a follow-on optimizer on the high-impact subset.

#### Planned — native-discrete optimizers

The numeric methods above indirect discrete/categorical axes through the f64
projector (detent → number / nominal → index). Some methods model discrete and
**conditional** spaces *natively* — no continuous relaxation, value-native
(§2) — which both avoids the projector for those axes and fits the holey /
hierarchic search space above. Three literature candidates, of which the best
two are adopted:

- **`tpe` — Tree-structured Parzen Estimator** *(adopt)* — Bergstra et al.,
  *Algorithms for Hyper-Parameter Optimization* (NeurIPS 2011); the engine
  behind Hyperopt / Optuna. Models `p(x | y)` with per-parameter densities
  (category frequencies for nominal axes, KDE for continuous) and proposes by the
  density ratio (an EI surrogate). **Natively handles categorical + integer +
  tree-structured / conditional spaces** — the natural fit for the
  holey/hierarchic search space. Value-native, dependency-free, deterministic
  from `budget.seed`.
- **`smac` — random-forest surrogate** *(adopt)* — Hutter, Hoos & Leyton-Brown,
  *Sequential Model-based Algorithm Configuration* (LION 2011); behind
  auto-sklearn. A random forest **splits on categorical and integer axes
  directly**, giving a mixed-type surrogate + uncertainty for the acquisition
  with no relaxation. Robust on mixed config spaces.
- **CoCaBO / Casmopolitan** *(not adopted)* — bandit × GP for categorical-
  continuous (Ru et al. 2020 / Wan et al. 2021). Effective but more specialized
  and heavier; TPE + SMAC dominate it for native-discrete *and* conditional
  coverage while staying dependency-free.

Both adopted methods consume `AxisKind::Discrete` / `Categorical` **value-native**
(no f64 projector for those axes); continuous axes still use the projector.

---

## Cross-reference

| Artifact | Location |
|---|---|
| Core contract (functor + sources + value model) | `nmbrs-runtime/src/optimize/contract.rs` |
| Runtime seam + node-level dispatch | `nmbrs-runtime/src/optimize/mod.rs`, `executor.rs` |
| Algorithm library (loop-form) | `nmbrs-optimizers/src/{lib,space,optimizer,registry}.rs` + `src/algos/*.rs` |
| Loop→source `ThreadBridge` + numeric projector + inventory | `nmbrs-optimizers/src/bridge.rs` |
| Embedded markdown docs | `nmbrs-optimizers/src/docs.rs` |
| Manifold test models | `nmbrs-optimizers/src/testmodels.rs` (sphere/rosenbrock/rastrigin/branin) |
| Convergence tests (local loops) + bridge e2e | `nmbrs-optimizers/tests/{converges,bridge_e2e}.rs` + `bridge.rs` unit test |
| `optimizer:` config | `nmbrs-workload/src/model.rs` (`OptimizeBlock`/`OptimizeAxis`), `parse.rs` |
| Disposition wiring | `nmbrs-runtime/src/stop_conditions.rs`, `workload_shell.rs` |
| Settle statistic nodes | `polydat/src/library/*` (EWMA / rolling-variance / freshness / slope) |
| `describe optimizers` | `nmbrs/src/describe.rs`; `nmbrs/tests/describe_optimizers.rs` |
| Example workloads (per feature) | `nmbrs/examples/workloads/optimizer_*.yaml` — testkit synthetic-manifold objectives, runnable standalone |
| Integration test | `nmbrs/tests/optimizer_manifold_e2e.rs` — testkit objective, drives the registry |
| Layer + Contract Registry | `docs/SRD/05_dependency_rules.md`, `nmbrs/tests/architecture_rules.rs` |

---

## Enforced edges

The optimizer follows the **adapter/plugin pattern** — inverted from a naive
"core depends on the algorithm library":

- **The contract lives in the core.** `nmbrs-runtime` (L4) defines the functor +
  source contract, the value model, the `OptimizerRegistration` inventory type,
  and discovery — with **no dependency on any algorithm crate**.
- **The algorithms are an inventory plugin.** `nmbrs-optimizers` (L5) depends on
  `nmbrs-runtime` *only* under its `runtime` feature, where `bridge.rs`
  `inventory::submit!`s one registration per optimizer; `nmbrs-optimizers →
  nmbrs-runtime` is a **downward** edge (D2). Verified by D0/D2 in
  `architecture_rules`.
- **The default build is standalone.** Without `runtime`, `nmbrs-optimizers`
  depends on nothing — fully-locally-testable, independently extractable;
  its algorithm API is its own contract → **D5-exempt**.
- **The binary force-links the plugin** (`extern crate nmbrs_optimizers;` in
  `nmbrs/src/run.rs`) so its registrations are discovered.
- The two-axis `Outcome` + the `ErrorPolicy` stay in `nmbrs-runtime`
  (SRD-82); the Report→Outcome mapping lives on the runtime side.

---

## Staging

**Landed + verified:**
1. SRD-83 step 4 (`effect: Outcome`) — A8.
2. The functor + source contract (`coordinate_source`,
   `CoordinateSource`/`PullSource`/`FeedbackSource`, `LexSource`/`PullOnly`,
   the capability-favoring driver, `sweep` = identity).
3. The 9 loop-form algorithms + the `ThreadBridge` loop→source adapter +
   inventory registration + `describe optimizers`; manifold + bridge tests.
4. The phase-level `optimize:` parse surface (`OptimizeBlock`).
5. **Value-model reshape** — `Coord = Vec<AxisValue>` (`Num`/`Label`/`Bool`);
   `AxisKind::Categorical`; the `AxisValue ↔ f64` numeric-stub projector in
   `bridge.rs`; `PolydatObjective` materializes each value to its real
   `polydat::Value`. Verified: numeric manifolds drive through the projector,
   the categorical index↔label round-trip, and value-native label enumeration
   (`sweep` visiting labels directly).

**Landed + verified (continued):**
6. **Node-level search wiring — ✓ LANDED (discrete / categorical / continuous,
   single + multi-axis).** `dispatch_optimization` (executor) drives an
   `optimize:` phase as a **search** (A12: a distinct Search scene node,
   depth-gated — pre-map represents + pre-maps the subtree once, execution runs
   the adaptive sequential loop). Axes auto-gathered from the phase's multi-clause
   `for_each` grid (`CoordEval::Enumerated`) or, for float ranges, sampled
   (`CoordEval::Synthesized`); the objective read off each iteration's kernel.
   Validated end-to-end — `sweep` (pull), `nelder_mead`/`cmaes` (feedback via
   `ThreadBridge`) converge in a real `nmbrs run` across discrete, continuous,
   multi-axis, and categorical axes (`nmbrs/examples/workloads/optimizer_*.yaml` +
   `workload_examples.rs`). *Follow-ups:* (a) a pre-existing synthesis gap means a
   phase cannot yet combine `for_each` with phase-level `metrics:`, so the
   objective is a `bindings:` wire (not a `metrics:` entry) for now; (b)
   nesting/composition of multiple optimize nodes (the node-level `optimizer:`
   property over a subtree).
7. **Two actuation strategies — ✓ LANDED.** The in-phase `Control` daemon
   (`optimize::servo`, `tokio::join!`'d with one continuous phase) over the SRD-23
   controls plane, plus changeover-class driver selection (`Coordinate`/`Fixture`
   → rerun, `Control` → daemon) and the **hybrid** (`run_hybrid_search`: mixed
   coordinate + control in one node — coordinate axes are the outer rerun grid,
   control axes servo interior per cell). Examples: `optimizer_control.yaml`,
   `optimizer_hybrid.yaml`.
8. **Settle-detector pipeline** — *first push SHIPPED 2026-06-18.* A
   `PhaseStopEvaluator` (general cadence-pulse phase evaluator,
   `nmbrs-runtime::optimize::phase_pulse`) subscribes to the smallest metrics
   cadence via the existing feed; each pulse it pokes the objective off node
   X's kernel (re-reading the latest window), feeds `is_stable`, publishes the
   windowed median to a register, and on a verdict sets a terminal phase
   disposition — **settled → Interrupted+Succeeded**, **timeout →
   Interrupted+Failed** — then **self-unregisters** (`Reporter::finished`, no
   self-join deadlock). The executor reads the register as the objective
   instead of the one-shot post-completion read, gated on a volatile objective
   (its program contains a metrics reader). Proven causal by
   `optimizer_saturation` (best = the concurrency that eliminates overloads).
   The **time-based viability gate** (`min_elapsed`, sized to the objective's
   widest rollup window) also landed, so a per-coordinate `rate(...[W])` cannot
   settle until its window has cleared the prior coordinate. The §5 in-phase
   `Control` daemon **reuses this settle per setting** (item 7). *Deferred:* the
   remaining SRD-86 §6 refinements — sparsity handling by walking up the cadence
   window ladder, the EWMA / freshness / slope statistic nodes (the first push
   uses `is_stable`'s median verdict), the settle-timeout `ErrorPolicy` surfacing,
   the `settle:` YAML trigger, and objective-cone-precise volatility detection.
   Short phases that span < a few cadence pulses still need item 9's finer-interval
   reconfiguration.
9. **(SRD-42) Cadence self-instrumentation + finer-interval reconfiguration** —
   a per-cadence coalesce-time metric (nanos, name aligned to the cadence) as
   the hot-spot guard, plus optional on-demand auto-reconfiguration of a cadence
   to a finer interval; the settle path consumes both.
10. **The MetricsQL reader family** *(SHIPPED 2026-06-18, except (e))* —
    `metricsql` / `metricsql_scalar` / `metricsql_vector` / `metricsql_window` polydat
    nodes **in `nmbrs-metricsql`** (`polydat_nodes`, feature `polydat-nodes`), evaluating
    over the `MetricAccess` data-access service in `nmbrs-metrics::queryapi`
    ([SRD 40c](40c_metric_query_api.md)). f64 throughout; result-shape assertions per
    §10's reader surface; string/non-matching → error; engine-gap exprs →
    `NotYetImplemented`. Shipped: (a) the `MetricAccess` service + the `MetricsQueryAccess`
    live backend (a **cadence read** of the cadence-feed store, `select_range` /
    `select_instant`) + the sqlite backend, with the `AccessProvider` inventory locator;
    (b) a `Vector` → `Value` projector with the four shape assertions; (c) the four nodes
    (parse-once at construction, evaluate per-eval; all `Purity::Nondeterministic` so they
    are never const-folded — [SRD 40c](40c_metric_query_api.md) MQ4); (d) coverage tests +
    `NotYetImplemented` surfacing. **(e) SHIPPED 2026-06-18** —
    `nmbrs/examples/workloads/optimizer/metricsql.yaml` + the
    `optimizer_metricsql_objective_settles` test: a `metricsql_scalar("sum(errors_total)")`
    objective, settled across the run by item 8's cadence-fed detector, picks the
    concurrency that eliminates overloads (best [2]) — the same causal oracle as
    `optimizer_saturation`, read through the metricsql reader node, now via
    `sum(rate(errors_total[3s]))`. *Range rollups — FIXED 2026-06-18* by the
    **cumulative-counter model** ([notes/cumulative_counter_model.md](notes/cumulative_counter_model.md)):
    counters are now stored cumulative (Prometheus/VM-schematic) so MetricsQL
    `rate()`/`increase`/`*_over_time` compute Δ/Δt correctly and **backend-independently** (live
    and sqlite both expose the cumulative through the one `MetricAccess` contract). The
    cross-coordinate concern dissolves: `rate()`'s `last − first` difference cancels the
    cumulative baseline carried over from the prior coordinate, so the value is per-coordinate
    without any storage trick — the example reads `best [2]`.
11. **Retire** the old `PolydatObjective`/`run_optimization` write-coords seam
    (superseded by the wire-read driver, A10).
12. **Staged further:** the cost-economy learner, multi-objective Pareto.

**Examples & acceptance.** Each feature ships with a paired **standalone**
example in `nmbrs/examples/workloads/optimizer_*.yaml` (the Examples-Run-Standalone
rule: defaults wired in, paced with `rate:`, testkit declared in-file, no
required flags). The objective is a synthetic-manifold polydat `value:`
expression over the coordinate axes — a known surface the optimizer climbs with
no backend — so each example is deterministic, observable, and the feature's
acceptance surface. They land *with* their feature (a non-running example would
violate the standalone rule). Set, one aspect each, all on testkit:
`optimizer_null_sweep` (identity baseline), `optimizer_numeric_manifold`
(cmaes/nelder_mead on a continuous paraboloid), `optimizer_categorical`
(`Label` axis, value-native method — feedback shows labels not indices),
`optimizer_nested` (outer categorical/fixture × inner numeric — node nesting +
mixed class), `optimizer_control_settle` (Control daemon + settling objective —
the settle detector), `optimizer_settle_timeout` (non-settling →
`settle_timeout` → ErrorPolicy), `optimizer_screening` (centroid),
`optimizer_traversal` (cost-greedy order), `optimizer_hyperband`
(multi-fidelity).
