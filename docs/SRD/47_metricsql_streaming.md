# 47: MetricsQL Streaming Aggregation

`nmbrs-metricsql` provides a parser, prettifier, and batch evaluator
for the MetricsQL subset of PromQL (see the crate-level rustdoc and
the parity-test fixtures harvested from the upstream Go
`metricsql` library). This SRD specifies the **streaming /
incremental aggregation** layer that runs on top of that
batch evaluator.

The user-facing capability:

> A query can be specified once, fed samples progressively as they
> arrive, and produce a partial result at any time that is
> exact-as-of-now. After the last sample for a window arrives, the
> result is byte-identical to what a fully-batched evaluation
> would have produced over the same data.

This is the foundation for live TUI dashboards, web reporting,
and any continuous-query workload nmbrs adds. It composes with
the existing `DataSource` trait (SRD section §References) without
modifying its contract.

---

## Motivation

### Why this is hard, and why it's tractable

Naïve recomputation works but has two failure modes that get
worse as workload size grows:

1. **Repeated full-window scans.** A continuous query running every
   5 s over a 1 h window touches the same overlapping samples 720
   times. Storage I/O dominates wall-clock, and dashboards lag.
2. **Memory blowup at finalize time.** Holding a full window of
   samples per query just to compute one aggregate at the end
   wastes RAM proportional to (sample rate × cardinality × window).

The escape is **algebraic**: most aggregations admit a
fixed-size accumulator that absorbs samples one at a time and
can be merged with another accumulator. When that property
holds, streaming is free *and* the final answer matches batch
evaluation exactly. Where it doesn't, we make an explicit
choice (retain raw, sketch, or refuse).

### Three algebraic classes

| Class | Property | Examples in metricsql |
|-------|----------|----------------------|
| **Distributive** | `f(A ∪ B) = combine(f(A), f(B))`, no extra state. | `sum`, `count`, `min`, `max`, `group`, `*_over_time` (sum/count/min/max/first/last), set ops `and/or/unless`, filter-mode comparisons |
| **Algebraic** | Bounded-size accumulator richer than the result. | `avg` (sum, count), `stddev` (Welford), `rate`/`increase`/`delta` (first, last, ts), `avg_over_time`, `stddev_over_time` |
| **Holistic** | Bounded-size *exact* accumulator does not exist. | `quantile`, `histogram_quantile`, `mad`, `median`, `topk`, `bottomk`, `count_values` |

The streaming layer supports the **distributive** and
**algebraic** classes natively. Holistic functions need a
policy decision (see §"Holistic-function policy" below) which
is **out of scope for the first push** — the design admits
them, the first push doesn't ship them.

### Why the property guarantee matters

"Streaming evaluation produces the same result as batch
evaluation" is not a quality-of-life promise; it's a
**correctness invariant**. Live dashboards and final reports
read the same data path. If they diverge, either users get
mislead in real time or the final report contradicts what
operators saw during the run. Both are unacceptable.

The guarantee is operationalized as a property test (see §"The
equivalence test"). It runs every CI build and is the
load-bearing artifact: if it ever fails, the algebra is
broken; no other test substitutes for it.

---

## The `Reducer` algebra

### Trait contract

```rust
/// Commutative-monoid-with-finalize algebra for streaming
/// aggregation. Implementations MUST satisfy:
///
///   identity:    merge(empty, x)        ≡ x
///   commutative: merge(a, b)            ≡ merge(b, a)
///   associative: merge(merge(a,b), c)   ≡ merge(a, merge(b,c))
///
/// `Default::default()` must produce the identity element.
/// `ingest` is equivalent to `merge` with a single-sample
/// accumulator: `ingest(acc, s)` ≡ `merge(acc, single(s))`.
///
/// The streaming-equivalence property test verifies the
/// monoid laws for every implementor against random inputs
/// — every new reducer goes through that harness before
/// being considered done.
pub trait Reducer: Send + Sync {
    type Acc: Clone + Default + Send + Sync;
    fn ingest(&self, acc: &mut Self::Acc, sample: &Sample);
    fn merge(&self, into: &mut Self::Acc, other: Self::Acc);
    fn snapshot(&self, acc: &Self::Acc) -> f64;
}
```

### Why these four operations and not three

`ingest` could be defined in terms of `merge`, but exposing it
separately lets the implementation skip the construction of a
single-sample accumulator on the hot path. For `Sum` this is
the difference between `acc.total += sample.value` (single
add) and `acc.total += other.total` (read-then-add). The
single-sample fast path is what makes streaming free for the
distributive class.

### Identity element via `Default`

Requiring `Acc: Default` forces the identity to be syntactic,
not a magic value. `Sum` produces `0.0`, `Min` produces
`f64::INFINITY`, `Count` produces `0`, etc. `merge(empty, x)
≡ x` becomes a unit-test obligation, not a runtime dance.

### Numerical stability

For `Sum`/`Avg` over many samples, naïve summation accumulates
floating-point error proportional to N. The reducer impls use
**Kahan summation** in their accumulators (a `compensation`
field next to the running total). This makes the property
test pass at tight tolerance (`f64::EPSILON * len`) without
relying on summation order.

For `Stddev`, the algebraic accumulator is **Welford's
online algorithm** (mean + M2 + count). Numerically stable;
mergeable per Chan-Golub-LeVeque.

For `Min`/`Max` / `First`/`Last`: no floating-point
accumulation; merge is exact by construction.

### Time-aware merges

`First`, `Last`, and the rate family carry timestamp metadata
in their accumulators:

```rust
struct RateAcc {
    first_value: f64,
    first_ts: i64,
    last_value: f64,
    last_ts: i64,
    /// Empty until first ingest; signals "no samples yet"
    /// distinctly from `first_ts == 0`.
    has_data: bool,
}
```

`merge(a, b)` selects `first_*` from whichever has the smaller
`first_ts` (handling `has_data == false`), and `last_*` from
the larger `last_ts`. The merge is commutative — no
order-of-arrival assumption.

This makes `rate`'s natural-looking time-asymmetry compatible
with the commutative-monoid property without compromising
correctness: the *operation* is order-independent; what's
order-dependent is which sample wins for the "first" /
"last" slot, and that's resolved by timestamp comparison.

---

## Plan compilation

### `StreamingPlan` shape

```rust
pub struct StreamingPlan {
    root: PlanNode,
}

enum PlanNode {
    /// Leaf: a sample feed bound to a selector. Holds the
    /// matchers it accepts; the runtime filters incoming
    /// samples through them.
    Leaf {
        matchers: Vec<Matcher>,
    },
    /// Per-group aggregate. One accumulator per distinct
    /// group key; group key derived from input series labels
    /// via `match_key()` semantics from `eval.rs`.
    Group {
        reducer: Box<dyn Reducer<Acc = AccCell>>,
        grouping: GroupingMode,    // ByLabels(Vec<String>) | WithoutLabels(Vec<String>) | All
        groups: HashMap<Vec<(String,String)>, AccCell>,
        child: Box<PlanNode>,
    },
    /// Window-bucket reducer for `*_over_time`. One
    /// accumulator per (input series identity, current
    /// window). Evicted when the window closes.
    Window {
        reducer: Box<dyn Reducer<Acc = AccCell>>,
        window_ms: i64,
        per_series: HashMap<Vec<(String,String)>, (i64, AccCell)>,
        child: Box<PlanNode>,
    },
}
```

`AccCell` is a type-erased accumulator (a small enum or
`Box<dyn Any>`-like wrapper) — necessary because the plan
holds heterogeneous reducers. The trait surface stays generic;
the storage at the plan node is erased.

### `compile_streaming` entry point

```rust
pub fn compile_streaming(expr: &Expr) -> Result<StreamingPlan, CompileError>;

#[derive(Debug, Clone)]
pub enum CompileError {
    /// AST node shape isn't supported by the streaming
    /// compiler this push (binary ops, algebraic reducers,
    /// quantile, etc.). Reason text names the shape.
    Unsupported(String),
    /// AST is structurally invalid for a streaming plan
    /// (e.g. nested aggregates).
    InvalidShape(String),
}
```

The compiler is a pure function of the AST — runs once per
query, no data dependency. Failure is fast and explicit.

### Supported query shapes (this push)

```
<agg>(<selector>)
<agg>(<selector>) by (l1, l2, ...)
<agg>(<selector>) without (l1, l2, ...)
<rollup_fn>(<selector>[<window>])      // for *_over_time
<agg>(<rollup_fn>(<selector>[w])) by (l)   // composition
```

Where `<agg>` ∈ `{sum, count, min, max, group}` and
`<rollup_fn>` ∈ `{sum_over_time, count_over_time,
min_over_time, max_over_time, first_over_time,
last_over_time}`.

### Rejected shapes (with `CompileError::Unsupported`)

The following remain unsupported in `compile_streaming` as of
the current state:

| Shape | Reason for deferral |
|-------|--------------------|
| `topk` / `bottomk` / `count_values` (truly holistic) | No bounded mergeable sketch yet; would need Misra-Gries or similar streaming top-K. Batch supports them. |
| Range-query stepping inside a `StreamingPlan` | Stream model evaluates one window at a time; pairs with sliding-window framing (§"Window framing"). |

**Now supported (filled in by follow-up pushes since the
original SRD landed):** binary ops in plans, algebraic
reducers (`Avg`, `Stddev`, `Rate`, `Increase`, `Delta`),
HDR-sketch reducers (`QuantileOverTime`), vector-matching
modifiers, set ops (`and`/`or`/`unless`).

### Carve-out: `rate` and `increase` semantics diverge from batch

`rate(...[w])` and `increase(...[w])` are deliberately
**excluded from the streaming-equivalence test**. The batch
path implements full PromQL semantics: counter-reset
adjustment (walk samples, add back the pre-reset value when
`xs[i] < xs[i-1]`) and window-edge extrapolation (scale the
result when the first/last samples don't sit at the window
edges). Both behaviours assume a *fixed* window with known
edges and require walking the full sample sequence in
temporal order.

Streaming sees a *sliding* arrival window: samples land
incrementally, the "window" is whatever the runtime has
ingested so far, and partition order across merge isn't
strictly temporal. Both adjustments would oscillate as data
flows in. The streaming reducer therefore uses the simpler
`(last - first) / window_secs` algebra — which matches what
live consumers (TUI panels, dashboards) actually want:
trend, not exact window-fitted final number.

`delta` stays in the equivalence set — gauge semantics with
no reset detection or extrapolation, batch and streaming
match exactly.

---

## Data path: `ingest` and `snapshot`

### Sample feed

```rust
impl StreamingPlan {
    pub fn ingest(&mut self, samples: &[(Labels, Sample)]);
    pub fn snapshot(&self, anchor_ms: i64) -> Vec<Series>;
    pub fn reset(&mut self);
}
```

`ingest` takes `(Labels, Sample)` pairs because the streaming
runtime sits *upstream* of `DataSource::fetch` — the leaf
node knows its matchers, the runtime is responsible for
filtering samples through them before invoking ingest. This
keeps the plan independent of any specific `DataSource`
implementation; it works just as well against:

- A polling adapter that calls `DataSource::fetch` on a timer
  and diffs against the previous result (works against any
  `DataSource` impl today).
- A future `WatchableDataSource::watch` stream (deferred —
  see §"Followup roadmap").
- A test harness feeding hand-constructed samples (used by
  the property test).

### Snapshot semantics

`snapshot(anchor_ms)` produces the current result as a
`Vec<Series>`. Each series's `samples` is a single sample at
`anchor_ms` carrying the reducer's `snapshot()` output for
that group. **It does not consume or finalize state.** The
plan can be snapshotted any number of times between ingests;
each snapshot reflects the data ingested so far.

For `*_over_time` reducers: when the window for a series
closes (current sample's `ts >= window_start + window_ms`),
the accumulator is evicted from the `Window` node's map
before the new sample is ingested. This is the only "finalize"
point in the streaming model; everything else is read-only
snapshotting.

### `reset`

`reset` clears all accumulators in the plan back to identity.
Used by tumbling-window cadences that want to restart on a
fixed grid (e.g. "evaluate the 1-minute aggregate every minute,
then reset and start a new minute"). Sliding-window framing
(deferred) won't need it.

---

## The equivalence test (load-bearing)

A property test in `crates/nmbrs-metricsql/src/streaming.rs::tests`:

```rust
#[test]
fn streaming_equals_batch_for_supported_shapes() {
    let mut rng = StdRng::seed_from_u64(0xC0FFEE);
    for shape in supported_query_shapes() {
        for _trial in 0..ITERATIONS {
            let samples = random_samples(&mut rng, shape.cardinality_hint());
            let batch_result = batch_evaluate(&shape.expr, &samples);

            // Random partition of `samples` into K batches.
            let k = rng.random_range(1..=8);
            let partitions = random_partition(&mut rng, &samples, k);

            let mut plan = compile_streaming(&shape.expr).unwrap();
            for batch in partitions {
                plan.ingest(&batch);
            }
            let stream_result = plan.snapshot(anchor_ms_of(&samples));

            assert_series_eq(&batch_result, &stream_result, TOLERANCE);
        }
    }
}
```

### What it proves

For every supported reducer × every supported query shape ×
every random partition, the streaming-merge result equals the
batch result within numerical tolerance. This *is* the
property the user asked for, made executable.

### Tolerance

Floating-point ops are associative under the algebra but
**not bit-exact** under arbitrary order — Kahan summation
narrows the gap, but doesn't eliminate it. The test uses:

- Exact equality for `Min`, `Max`, `First`, `Last`, `Count`,
  `Group` (no FP accumulation).
- `|stream - batch| < TOLERANCE_F64` for `Sum`-derived
  reducers, with `TOLERANCE_F64 = 1e-9` per result entry.

If a future reducer can't meet this bound, the algebra needs
revisiting, not the bound relaxed.

### Run cost

Targeting ≤50 ms total wall time. Fixed-seed PRNG so failures
reproduce. If it ever exceeds budget, profile and shrink
inputs — don't gate it behind a feature flag or env var.
This test runs every CI build.

---

## Holistic-function policy (deferred design decision)

**Not in this push.** This section exists to fix the design
shape so that when holistic reducers land they slot into the
existing trait without re-litigation.

The choice is per-query (annotation) or per-installation
(config), with three modes:

| Mode | Memory | Result | Failure mode |
|------|--------|--------|--------------|
| **Exact** | `O(samples × cardinality)` | Bit-exact | OOM at high sample rate |
| **HDR-sketch** | `O(sigdigs)` per series, bounded | Approximate within HDR precision | None (precision is HDR's) |
| **Forbid** | n/a | Compile-time `CompileError::Unsupported` | n/a |

**HDR-sketch is the recommended default** for nmbrs given
`hdrhistogram = "7"` is already on the dependency path (see
SRD-40 §"Timer"). HDR histograms are mergeable, the precision
is configurable per session via the existing `hdr.sigdigs`
walk-up resolution, and the result is "approximate" only in
the sense that it's bucketized — not in the sense that
different runs produce different answers.

The reducer trait does not need to change for sketches:

```rust
struct QuantileSketchReducer { quantile: f64 }
impl Reducer for QuantileSketchReducer {
    type Acc = HistogramAcc;  // wraps hdrhistogram::Histogram
    // ...
}
```

The sketch satisfies all three monoid laws (`Histogram::add`
is associative, commutative, and HDR's empty histogram is the
identity).

---

## Window framing (deferred design decision)

**Not in this push.** Sliding-window rollups (`metric[5m]`
re-evaluated at every step in a range query) need either an
"evict on expiry" capability (works for sum/count, requires
monotonic deque for min/max, hard for quantile) OR a
window-bucket grid where windows align to a fixed cadence.

The pragmatic answer for nmbrs is the second: cadences are
already grid-aligned per SRD-42 (Windowed Metrics). The
streaming plan evaluates **one window at a time** under the
current SRD-42 cadence; range-query stepping over arbitrary
T is not supported in the streaming path.

When sliding-window framing lands, it'll be a layer over the
existing reducers — the algebra doesn't change.

---

## Anti-goals

The following are explicit non-goals for this push *and* for
the long-term design:

- **No parallel `streaming` evaluator copy.** Reducers reuse
  `match_key`, `labels_after_op`, sample alignment helpers
  from `eval.rs`. The streaming plan is a different scheduler
  over the same algebra; it is not a fork.
- **No `Box<dyn Trait>` for forward extensibility.** Plan
  nodes use erased accumulators (`AccCell`) because they have
  to — heterogeneous reducers in one tree. Public APIs use
  static dispatch.
- **No feature flag on the property test.** It runs every CI
  build. Skipping it is not a recovery path; fixing the
  algebra is.
- **No speculative hooks for sliding windows or watch
  streams.** Their absence is documented; their hooks aren't
  pre-wired.
- **No mutation of the `DataSource` trait shape.** SRD-section
  §References pins the trait's contract. The streaming plan
  composes with it; it does not change it.

---

## Push scope

### Capabilities landed by this push

- `Reducer` trait formalized with the three monoid invariants.
- Distributive reducer impls: `Sum`, `Count`, `Min`, `Max`,
  `Group`.
- `*_over_time` reducer impls: `SumOverTime`,
  `CountOverTime`, `MinOverTime`, `MaxOverTime`,
  `FirstOverTime`, `LastOverTime`.
- `StreamingPlan` struct + node types.
- `compile_streaming(&Expr) -> Result<StreamingPlan,
  CompileError>` for the supported query shapes (§"Plan
  compilation"); rejects everything else.
- `StreamingPlan::ingest`, `snapshot`, `reset`.
- The streaming-equivalence property test.
- Module-level rustdoc covering the algebra, the property
  guarantee, and the trait contract.

### Out of scope (deferred to follow-up pushes)

| Item | Why not now |
|------|-------------|
| Algebraic reducers (`avg`, `stddev`, `rate`, `increase`, `delta`) | Bundled together so the merge-time-ordering conversation happens once. |
| HDR-sketch reducers (`quantile_over_time` and friends) | Needs the policy decision in §"Holistic-function policy" formalized first. |
| Sliding-window framing | Needs the cadence-grid decision in §"Window framing" formalized first. |
| Binary ops in streaming plans | Adds a compiler node type; reducers unchanged. Bundles with vector-matching modifiers. |
| `WatchableDataSource` extension trait | No real producer to attach to yet; the polling adapter (any `DataSource`) is sufficient until the sqlite adapter lands. |
| Sqlite `DataSource` adapter | Independent track, gated on schema patches (indexes, PRAGMAs, primary key on `sample_value`). The streaming work proves itself against the in-memory test backend. |

### Task breakdown with acceptance criteria

| # | Task | Acceptance |
|---|------|------------|
| 1 | `Reducer` trait + 5 distributive impls | Per-impl unit tests verify identity / commutativity / associativity on hand-constructed accumulators. |
| 2 | 6 `*_over_time` reducer impls | Same monoid-law unit tests as Task 1 + per-reducer "samples in / accumulator out" check matching upstream semantics. |
| 3 | `StreamingPlan` struct + `compile_streaming` | One unit test per supported shape (compile succeeds, plan structure matches expectation) + one rejection test per unsupported AST node. |
| 4 | `ingest` + `snapshot` | End-to-end test per supported shape: ingest a known sample set, assert snapshot equals the expected `Vec<Series>` to numeric precision. |
| 5 | The equivalence property test | Test runs ≥1000 iterations across all supported shapes × reducers without divergence; total wall time ≤50 ms. |
| 6 | Documentation | `cargo doc` clean; this SRD checked in; memory entry capturing the design decision. |

### Exit criteria

The push is done when **all** of the following hold:

1. Tasks 1–6 land in a single coherent commit (or commit
   series, user-driven).
2. `cargo test --workspace` is green.
3. The equivalence property test passes for every supported
   reducer × every supported query shape.
4. `cargo build --workspace` is clean — no new warnings.
5. Module-level rustdoc names the three algebraic classes and
   points at the property test as the load-bearing artifact.
6. This SRD is referenced from `docs/SYSREF.md` and listed in
   `docs/SRD/00_index.md` under §"Metrics and
   Observability".
7. A new memory entry exists in
   `~/.claude/projects/.../memory/` capturing the design
   decision so future passes don't re-litigate.

### Risk log

| Risk | Mitigation |
|------|------------|
| `f64` summation order causes property-test flakes | Kahan summation in `Sum`/`SumOverTime`/`Avg` accumulators; documented tolerance bound. |
| Property test slow on every CI run | Fixed-seed PRNG (failures reproduce); cap iteration count via `const`; profile if it exceeds 50 ms total. |
| `StreamingPlan` API drifts from `eval::evaluate` | The property test pins them together: every shape's batch path is `evaluate(...)`, the streaming path runs in parallel, divergence is a test failure. |
| Type erasure on `AccCell` introduces panics | All conversions are reducer-private; `Reducer::ingest` / `merge` / `snapshot` see the concrete `Acc` type. The plan-node side is the only place erasure lives, and its lifetime is bounded by the reducer it was constructed against. |

---

## Followup roadmap

### Landed since this SRD was written

1. ✅ **Algebraic reducers**: `Avg`, `Stddev`, `Rate`,
   `Increase`, `Delta`, plus their `_over_time` variants.
2. ✅ **HDR-sketch reducer**: `QuantileOverTime` —
   `hdrhistogram::Histogram` as the `Acc` type. Negative /
   NaN samples dropped; values floored to `u64`. Covers
   nmbrs's nanosecond-latency use case.
3. ✅ **Binary ops in streaming plans**: `Binary { op,
   left_child, right_child }` plan node, per-timestamp
   value combine, reducers unchanged.
4. ✅ **Vector-matching modifiers** (`on`, `ignoring`,
   `group_left`, `group_right`) and set ops (`and` / `or` /
   `unless`) — mirror the batch implementation.
5. ✅ **Continuous-query runtime** (SRD-48): owns active
   `StreamingPlan` instances, drives from `SampleFeed`,
   exposes ArcSwap snapshots. First push complete; runtime
   feature flag.
6. ✅ **Sqlite `DataSource` adapter**: schema patches
   (indexes, PRAGMAs), stat-suffix resolution, INTERSECT-
   based matchers. Powers `nmbrs metrics query` /
   `nmbrs metrics watch`.

### Still deferred

1. **Sliding-window framing** for distributive ops — design
   parked in §"Window framing"; works for distributive ops
   but not min/max/quantile, which is why SRD-48 chose
   tumbling-window framing as the default.
2. **Streaming top-K / `count_values`** — would need
   Misra-Gries or GK-sketch. Batch handles these via
   `topk`/`bottomk`/`quantile`.
3. **`WatchableDataSource`** extension trait — gated on a
   real push producer (reporter wiring) wanting
   change-notification semantics. Polling adapter is
   sufficient until then.
4. **`tokio::sync::watch::Receiver` subscription** on the
   continuous-query runtime — gated on a real consumer (TUI
   panel) wanting change-notification semantics. Readers
   currently poll via `ArcSwap::load`.

### Batch evaluator — orthogonal additions

Not strictly part of SRD-47's streaming algebra, but worth
noting because they came out of the same surge of work and
shape what the streaming layer doesn't have to do:

- ✅ Counter-reset adjustment in `rate` / `increase` (batch).
- ✅ PromQL window-edge extrapolation for partial windows.
- ✅ Subquery `expr[w:s]` and `expr[w:]` (inherit-step).
- ✅ `@ start()` / `@ end()` modifier resolution.
- ✅ `topk` / `bottomk` / `quantile` parameterized
  aggregates (batch-only; holistic class).

---

## References

- **Code (current state)**: `crates/nmbrs-metricsql/src/eval.rs` —
  batch evaluator with six AST shapes covered (selector,
  rollup, aggregate, binary op, range query, rollup-consumer
  function); `crates/nmbrs-metricsql/src/ast.rs` — AST types;
  `crates/nmbrs-metricsql/src/parser.rs` — 100% parser parity
  against upstream; `crates/nmbrs-metricsql/src/prettifier.rs` —
  100% prettifier parity.
- **`DataSource` trait contract**: `nmbrs-metricsql::eval` —
  `__name__` in labels, samples sorted ascending, samples in
  `[start, end]` inclusive, no-match → empty-or-omitted.
  Returns `Result<Vec<Series>, DataSourceError>`. Single
  method wide; prefetch / streaming / pushdown extensions
  deferred until a real backend exposes the need.
- **Eval-storage boundary memory**:
  `~/.claude/projects/.../memory/project_metricsql_eval_boundary.md`.
- **OLAP algebra background**: Gray et al., "Data Cube: A
  Relational Aggregation Operator Generalizing Group-By,
  Cross-Tab, and Sub-Totals" (1997) coined the
  distributive/algebraic/holistic taxonomy. The metricsql
  function classification in §"Three algebraic classes"
  follows that vocabulary.
- **HDR sketch mergeability**: see `hdrhistogram` crate
  docs — `Histogram::add` is associative + commutative.
  SRD-40 §"Timer" specifies how `hdr.sigdigs` resolves
  through the component tree.
- **Cadence grid**: SRD-42 ("Windowed Metrics Access")
  defines the user-declared cadence list and intermediate
  bucket policy. Sliding-window framing in this design
  builds on the same grid, not a parallel one.
- **Upstream MetricsQL parser**: linked at
  `links/metricsql/`. The parity-test fixtures in
  `crates/nmbrs-metricsql/tests/fixtures/*.json` are harvested from
  it.
