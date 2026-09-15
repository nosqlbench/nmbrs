# Design Memo: Consolidate `Component`'s instrument storage

**Status:** blueprint (not yet implemented). Reviewer: Jonathan.

**Companion SRDs:** SRD-40 (metrics framework), SRD-40a (data
model), SRD-40b (synthetic metrics — the user of this
mechanism).

## Why we're doing this

Today `Component` carries metrics-related state in three
disjoint places:

1. **`instruments: Option<Arc<dyn InstrumentSet>>`** — a
   trait object whose `capture_delta` / `capture_current`
   methods return whole `MetricSet`s. Opaque: the cadence
   reporter calls these without knowing what's inside.
2. **`families: HashSet<String>`** — name-only collision
   guard added in SRD-40b §7. Empty for legacy components;
   populated by `register_family` at op-dispenser init.
3. **`MetricsDispenser` slots** (in `nmbrs-runtime::wrappers`)
   hold the *actual* `Arc<Counter>` / `Arc<ValueGauge>` /
   `Arc<Histogram>` for synthetic metrics, and a sibling
   `MetricsInstrumentSet` shim wraps them as a third copy
   to satisfy `set_instruments`.

That's three copies of the same Arc + one separate name
registry. The seam was visible in code review and the bridge
shipped broken (the cadence reporter doesn't walk arbitrary
descendants of `attach()`-ed components, so the synthetic
metrics never reached `metrics.db`).

The right shape is **one canonical store on `Component`**, in
which families and instruments are the same registration. The
upstream nosqlbench's `NBComponent` did this with a
`ConcurrentHashMap<String, NBMetric>`; we'll do it with a
`Vec<RegisteredInstrument>`, scanned for the rare name-lookup
case (justification in §3 below).

## 1. New types

In `nmbrs-metrics/src/component.rs`:

```rust
/// A typed instrument reference owned by a `Component`. The
/// kind discriminator is a structural property — every
/// instrument family produces samples in exactly one of
/// these shapes (per SRD-40a §1's MetricType axis).
#[derive(Clone)]
pub enum InstrumentRef {
    Counter(Arc<crate::instruments::counter::Counter>),
    Gauge(Arc<crate::instruments::gauge::ValueGauge>),
    Histogram(Arc<crate::instruments::histogram::Histogram>),
    Timer(Arc<crate::instruments::timer::Timer>),
}

impl InstrumentRef {
    pub fn labels(&self) -> &crate::labels::Labels { … }
    pub fn kind(&self) -> InstrumentKind { … }   // for diagnostics
}

pub enum InstrumentKind { Counter, Gauge, Histogram, Timer }

/// A single registration: the family name + the typed
/// instrument. The family name is what `metric_family.name`
/// will be in SQLite (after unit-suffix normalisation per
/// SRD-40a §4.3).
pub struct RegisteredInstrument {
    pub family: String,
    pub instrument: InstrumentRef,
}
```

## 2. `Component` changes

Drop both legacy fields:

```rust
// REMOVED:
//   instruments: Option<Arc<dyn InstrumentSet>>,
//   families: std::collections::HashSet<String>,
```

Replace with:

```rust
/// Instruments registered on this component. SRD-40 §"Per-
/// component instrument ownership" (consolidated 2026-05).
///
/// **Storage shape:** `Vec`, not `HashMap`.
///
/// Hot-path access pattern is **pre-bound** — every consumer
/// (a `MetricsDispenser` slot, an `ActivityMetrics` field,
/// the cadence reporter's per-tick walk) holds a typed `Arc`
/// captured at registration time and never looks up by name
/// per cycle. Name lookup (`find_instrument(name)`) is a
/// linear scan and is **expected to be rare** — used by
/// diagnostics (`dryrun=op`, `nmbrs describe polydat`),
/// introspection tooling, and ad-hoc tests. If we ever find
/// a hot path that does name lookup per cycle, that's a
/// design bug in the caller, not a reason to add a HashMap.
///
/// Maintaining insertion order also gives diagnostics a
/// stable, declaration-ordered view (matches the workload
/// author's source order).
pub instruments: Vec<RegisteredInstrument>,
```

New methods:

```rust
impl Component {
    /// SRD-40b §7 — register an instrument for the given
    /// family name on this component. Errors if the family
    /// is already registered (the dimensional cell is
    /// determined by the component's `effective_labels`;
    /// duplicates within one component would conflict).
    /// Atomic: the family-name check and instrument storage
    /// happen in one method, no two-step "claim then build".
    pub fn register_instrument(
        &mut self,
        family: impl Into<String>,
        instrument: InstrumentRef,
    ) -> Result<(), String> { … }

    /// Read-only view of every registered instrument.
    /// Walked by the cadence reporter on every tick.
    pub fn instruments(&self) -> &[RegisteredInstrument] {
        &self.instruments
    }

    /// Linear scan by family name. **Diagnostic / rare-path
    /// only**; per-cycle code must use the typed `Arc`
    /// captured at registration. Returns `None` when the
    /// family isn't registered on this component.
    pub fn find_instrument(&self, family: &str)
        -> Option<&InstrumentRef> { … }

    /// Capture all registered instruments into a
    /// `MetricSet`. Replaces the old
    /// `InstrumentSet::capture_delta`. Histograms drain;
    /// counters and gauges remain non-draining.
    pub fn capture_delta(&self, interval: Duration) -> MetricSet { … }

    /// Non-mutating capture (counters/gauges atomic load,
    /// histograms `peek_snapshot`). Replaces
    /// `InstrumentSet::capture_current`.
    pub fn capture_current(&self) -> MetricSet { … }
}
```

Removed methods:

```rust
// REMOVED:
//   pub fn set_instruments(&mut self, instruments: Arc<dyn InstrumentSet>);
//   pub fn register_family(&mut self, family: &str) -> Result<(), String>;
//   pub fn family_set(&self) -> &HashSet<String>;
```

The `InstrumentSet` trait itself **retires**. The cadence
reporter calls `component.capture_delta(interval)` directly
on every running component.

## 3. Why `Vec`, not `HashMap`

Per Jonathan's directive: **registration is once at init,
per-cycle access is pre-bound, name lookup is rare.**

Concretely:
- `ActivityMetrics` builds its counters/timers/histograms
  once at `Activity::new`. Per-cycle code increments through
  the typed Arc held in `ActivityMetrics`'s own struct fields
  — never `find_instrument("cycles_total")`.
- `MetricsDispenser` resolves its slots' Arcs at `wrap` time.
  Per-cycle execute records via `slot.instrument` — never
  `component.find_instrument(slot.family)`.
- The cadence reporter walks `component.instruments()` once
  per tick. The walk is sequential by definition; HashMap
  iteration order isn't useful here.
- `find_instrument(name)` exists for diagnostics
  (`dryrun=op` summaries, `nmbrs describe polydat` introspection,
  ad-hoc test queries). These paths run at most once per
  workload load, not per cycle.

A 10-instrument Vec scan is ~10 string compares, ~50 ns. A
HashMap probe on the same surface would save us ~40 ns once
per workload load and cost ~50 LOC of additional API + a
Hash bound on something that doesn't conceptually need it.

We don't add a HashMap unless / until profiling proves the
diagnostic surface is hot. The contract above makes "this is
not a hot path" the explicit norm; new callers tempted to
write `find_instrument(name)` in a per-cycle loop should
read this section first and reach for pre-binding instead.

## 4. Cadence reporter migration

Today `nmbrs-metrics::cadence_reporter` walks the component
tree, dispatching `Component.instruments.as_ref().map(|i|
i.capture_delta(...))`. New:

```rust
fn snapshot_running_components(&self, interval: Duration) -> Vec<(Labels, MetricSet)> {
    let mut out = Vec::new();
    walk_running(&self.root, |component| {
        out.push((
            component.effective_labels().clone(),
            component.capture_delta(interval),
        ));
    });
    out
}
```

The DFS walker (`crate::component::walk_running` or similar
— spelling depends on what's there today) is unchanged; only
the per-component capture call swaps shape.

## 5. `ActivityMetrics` migration

Today:

```rust
pub struct ActivityMetrics {
    cycles_total: Counter,
    cycles_servicetime: Timer,
    // … etc
}

impl InstrumentSet for ActivityMetrics {
    fn capture_delta(&self, interval: Duration) -> MetricSet {
        let mut s = MetricSet::new(interval);
        s.insert_counter("cycles_total", self.cycles_total.labels().clone(),
            self.cycles_total.get(), Instant::now());
        // … per-instrument inserts
        s
    }
}
```

New:

```rust
pub struct ActivityMetrics {
    cycles_total: Arc<Counter>,    // Arc, not bare — shared with Component registry.
    cycles_servicetime: Arc<Timer>,
    // … etc
}

impl ActivityMetrics {
    pub fn new(labels: &Labels) -> Self { … construct typed Arcs … }

    /// Register every instrument on the given component.
    /// Called once at attach time; subsequent per-cycle
    /// reads/writes go through this struct's own fields.
    pub fn register_on(&self, component: &mut Component) -> Result<(), String> {
        component.register_instrument(
            "cycles_total",
            InstrumentRef::Counter(self.cycles_total.clone()),
        )?;
        component.register_instrument(
            "cycles_servicetime",
            InstrumentRef::Timer(self.cycles_servicetime.clone()),
        )?;
        // … etc
        Ok(())
    }
}
```

The activity-side per-cycle code (`activity.metrics.cycles_total.inc_by(...)`)
remains untouched — it goes through the typed Arc held in the
struct.

## 6. `MetricsDispenser` simplification

Today:

```rust
pub fn wrap(inner, metrics, component) -> Result<Arc<dyn OpDispenser>, String> {
    // … allocate instruments, store in slots …
    component.register_family(&family)?;
    // … 30 LOC building MetricsInstrumentSet …
    let instrument_set = Arc::new(MetricsInstrumentSet::from_slots(&slots));
    component.set_instruments(instrument_set);
    Ok(Arc::new(Self { inner, slots }))
}
```

New:

```rust
pub fn wrap(inner, metrics, component) -> Result<Arc<dyn OpDispenser>, String> {
    let mut slots = Vec::with_capacity(metrics.len());
    for (name, spec) in metrics {
        let family = spec.family.clone().unwrap_or_else(|| name.clone());
        let arc_instrument: InstrumentRef = match spec.kind.unwrap_or_default() {
            MetricKind::Gauge => InstrumentRef::Gauge(Arc::new(ValueGauge::new(...))),
            MetricKind::Histogram => InstrumentRef::Histogram(Arc::new(Histogram::new(...))),
            MetricKind::Counter => InstrumentRef::Counter(Arc::new(Counter::new(...))),
        };
        // Single registration: name uniqueness check + storage in one call.
        component.register_instrument(family.clone(), arc_instrument.clone())?;
        slots.push(MetricSlot {
            family,
            value_expr: spec.value.clone(),
            format: spec.format.as_deref().map(parse_format_spec).transpose()?,
            instrument: arc_instrument,   // same Arc as component holds
        });
    }
    Ok(Arc::new(Self { inner, slots }))
}
```

Drops:
- `MetricsInstrumentSet` struct + impl.
- `families` separate registry.
- `set_instruments` call.

The `MetricInstrument` enum on the wrapper-side that I had
earlier collapses into the `InstrumentRef` enum from
`nmbrs-metrics`. One enum, used both in the wrapper's slot
and in the component's registry. They share the Arc.

## 7. Migration steps (phased)

Each step is independently testable.

| Step | What | Files | LOC est. |
|---|---|---|---|
| 1 | Add `InstrumentRef` + `RegisteredInstrument` types | `nmbrs-metrics::component` | ~80 |
| 2 | Add `register_instrument` / `instruments()` / `find_instrument` / `capture_delta` / `capture_current` on `Component` | `nmbrs-metrics::component` | ~120 |
| 3 | Update cadence reporter to call `Component::capture_delta` | `nmbrs-metrics::cadence_reporter` | ~20 |
| 4 | Migrate `ActivityMetrics` to register Arcs on its component | `nmbrs-runtime::activity` | ~60 |
| 5 | Migrate every other `InstrumentSet` impl (validation metrics, polling metrics, anything else) | various | ~50 |
| 6 | Retire `InstrumentSet` trait + `set_instruments` / `register_family` / `families` | `nmbrs-metrics::component` | -90 |
| 7 | Simplify `MetricsDispenser`: drop `MetricsInstrumentSet`, use `register_instrument` | `nmbrs-runtime::wrappers` | -60 |
| 8 | Update tests that constructed `MockInstruments` impls of `InstrumentSet` to use the registry | tests across crates | ~40 |

Net: ~+220 LOC added, ~-150 retired, **+70 LOC for a real
consolidation**, plus the structural win of one canonical
store.

## 8. Verification

- After step 6, the smoke test on
  `nmbrs/examples/workloads/metrics/synthetic_metrics.yaml` should populate
  `latency_curve_ms` / `load` / `step_counter` /
  `observation_dist` in `metrics.db`. The end-to-end
  synthetic-metrics path becomes observably correct.
- `cargo test --workspace` passes 0-failure.
- `dryrun=op` output gets richer: `find_instrument`-shaped
  diagnostics now have a real registry to read.

## 9. What changes shape, what stays the same

**Stays:**
- Per-cycle hot path. `metrics.cycles_total.inc_by(N)` works
  identically; the `Counter` type is unchanged.
- `MetricSet` / `MetricFamily` data model. `metric_family`
  schema unchanged. SRD-40a is unaffected.
- The per-component `effective_labels` machinery — the
  registry attaches to whichever component the caller hands
  in.
- Component lifecycle (Starting / Running / Stopped). Only
  Running components contribute to the cadence reporter walk.

**Changes:**
- Every `impl InstrumentSet for SomeMetrics` becomes
  `impl SomeMetrics { pub fn register_on(&self, c: &mut Component) }`.
- The cadence reporter walks `component.instruments()` (Vec)
  instead of dispatching `instruments.as_ref().map(|i|
  i.capture_delta(...))`.
- `MetricsDispenser` shrinks; the broken `MetricsInstrumentSet`
  bridge disappears.

## 10. Construction order — resolved

Considered the up-front variant
(`ActivityMetrics::new(labels, &mut component)` registers
inside `new()`) but `Activity::with_params_and_sigdigs`
(activity.rs:603-638) builds the activity with `metrics:
Arc::new(ActivityMetrics::with_sigdigs(...))` *before* the
component exists (`component: None`); the component arrives
later via `Activity::attach_component`. Flipping that order
forces the runner to build the component before the
activity, reshuffling several call sites, the `Default` impl
path, the test fixture, and the bench.

Per Jonathan's directive ("only if it comes with no
complications"), this is a complication. So:

- `ActivityMetrics::new(labels)` /
  `with_sigdigs(labels, sigdigs)` keep their current shape —
  they just construct `Arc<Counter>` / `Arc<Timer>` /
  `Arc<Histogram>` instead of bare types. Per-cycle code
  like `metrics.cycles_total.inc()` works unchanged through
  `Arc`'s `Deref`.
- `ActivityMetrics::register_on(&mut Component)` is called
  from `Activity::attach_component` once the component
  exists; it iterates every Arc field and registers it.

There IS a transient window — `ActivityMetrics` exists with
unregistered instruments — but it lives entirely inside
`Activity::with_params_and_sigdigs` → `attach_component`,
both runner-internal. No public API observes the half-state.
