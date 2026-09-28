# Metric dimensions from query data

*Proposal — not yet an SRD. Written 2026-07-28; implemented the same day.*

**Status: implemented.** Verified end to end — a cell-placed metric emits one
series per dimension value, each carrying its registration site's labels plus
the dimension:

```text
bytes_out{exec_id="1",op="probe",phase="t",tier="even",workload="w"} = 12
bytes_out{exec_id="1",op="probe",phase="t",tier="odd",workload="w"}  = 13
```

`op=` retained, `tier=` added: the cell refines the identity rather than
replacing part of it.

## What we want

A workload reads rows whose values name instances of a dimension — a compaction
tier, a keyspace, a node — and wants one series per instance of a single
measurement:

```
compaction_bytes_out{tier="24"}
compaction_bytes_out{tier="25"}
```

Today the label values a metric can carry are fixed at component-attach time
(`component.effective_labels()`, read in `wrappers/metrics.rs:277`). Nothing in
`MetricSpec` (`crates/nmbrs-workload/src/model.rs:2234`: `value`, `family`, `kind`,
`unit`, `format`) can bind a label to captured data.

## What already works, and is not the problem

Verified on 2026-07-28, so the design does not need to solve any of it:

- `Str` is a first-class `PortType` (`polydat/src/ast.rs:1036`).
- Nodes take **runtime** (non-`Const`) string arguments — `str_eq(a: &str, b:
  &str)` (`polydat/src/library/compare.rs:118`) is a real node over two String
  wires.
- A query's text column captures into a `Str` wire and flows into a node
  argument. Verified end to end against a live node: `extern kname: str = ""` +
  `capture: kname: "/0/keyspace_name"` → `str_eq(kname, "system")` → `1`, with
  the negative comparison returning `0`.
- Storage already models one instance per label set — `metric_instance` +
  `instance_label` (`crates/nmbrs-metrics/src/reporters/sqlite.rs:368,400`).

The metrics/metricsql nodes use `Const<&str>` **by choice** — they pre-parse the
pattern via `#[poly_const(...)]` — not because polydat lacks runtime strings.

## The invariant this must not weaken

`Component::register_instrument_with_unit` (`crates/nmbrs-metrics/src/component.rs:232`)
rejects a second registration of the same family on one component:

> The component's `effective_labels` define the dimensional cell; the same
> family on a different component is a different cell and produces no collision.

That is metric identity, not an obstacle. **A per-value series is a per-value
cell** — a child component carrying the dimension value, with the family
registered exactly once on it. The uniqueness check is untouched and keeps
catching the error it exists to catch: two different instruments claiming one
name in one dimensional context.

`Component::attach` (`component.rs:805`) enforces the other half:

> component label-ownership violation: child re-declares label `{k}` already
> owned by an ancestor. Each label name must be set on exactly one tier and
> inherited downward.

So a dimension **name** is a structural declaration made at exactly one tier;
only the **value** varies per cell.

### The two legitimate patterns

| pattern | mechanism in tree today | correct when |
|---|---|---|
| new family, same cell | `errors.<type>` — `Counter::new(labels.with("name", format!("errors.{error_name}")))` (`activity.rs:566`) | the values name **different measurements** |
| same family, new cell | one component per `k` / `profile` sweep value | the values are **instances of one dimension** |

This proposal is the second. `compaction_bytes_out` is one measurement and
`tier` is a dimension; `compaction_bytes_out.tier24` would be the first pattern
misapplied.

### Why not `DynamicCapture`

`DynamicCapture` (`component.rs:102`) emits samples straight into the
`MetricSet` from `capture_into`, carrying labels on the instrument itself. It is
how per-error counters reach the wire — and it **bypasses the component registry
entirely**, which is how it sidesteps cell identity.

That is acceptable for an engine-internal concern with a fixed shape. As the
general mechanism for workload-authored dimensions it would put an unbounded,
uncheckable namespace outside the type system: label sets assembled by runtime
string formatting, invisible to the compiler, unvalidated. It is the "not
reified" design, and it is the argument against itself.

## Design

### 1. Declaration — the dimension is compiled matter

A dimension is declared once, at the tier that owns the label name, beside
`bindings:` / `metrics:`:

```yaml
finalize_index:
  dimensions:
    tier: { type: str }
```

The declaration names the label key and types its value wire. It does **not**
enumerate values — those come from data.

### 2. Placement — a metric names the cell it lands in

`MetricSpec` grows one optional field, `cell:`, mapping dimension name to an
**expression** (same grammar as `value:`, not a bare wire name):

```yaml
ops:
  read_compaction_history:
    capture:
      tier_name: "/0/tier"
    metrics:
      compaction_bytes_out:
        kind: gauge
        value: history_bytes_out
        cell: { tier: tier_name }
```

Read as: *this measurement, in the cell where `tier` = the captured value.*

### 3. Compiled representation — the reification

Scope synthesis emits a typed binding per cell coordinate, exactly as phase
metrics already emit `volatile __metric_<name> := <expr>`
(`crates/nmbrs-runtime/src/scope.rs`):

```
volatile __cell_tier := tier_name       # typed Str by the declaration
```

So a cell coordinate is a **compiled kernel output**, type-checked and pulled
through the normal wire path — the same standing as a metric value. The
program's manifest carries the declared dimension names, which is what makes
every check below possible.

### 4. Runtime

Implemented in `wrappers/metrics.rs` as `CellPlacement`. A metric declaring
`cell:` does **not** register on the dispenser's own component at wrap time:
that would claim the family for the un-refined identity, and the first cell to
materialise would then collide with it on the very duplicate-family check this
design preserves. Its slot carries no instrument; instruments are materialised
per coordinate instead.

**The parent is the component the metric registers on** — never an ambient one.

Identity is the label set with the family name promoted into it (this crate
already works that way: `split_name_label` requires a `name` label, and the
wrapper builds `component_labels.with("family", …)`). Distinct label set ⟺
distinct identity, 1:1 and closed. A cell therefore **refines** an identity: the
coordinate ADDS dimensions to the set its registration site already contributes.

Sourcing the parent from ambient context — the running fiber's component, say —
composes identity from wherever the code happens to be executing, which silently
drops what the registration site owns (`op=`, most obviously) and yields a
different identity wearing the same family name. It is also structurally wrong:
one fiber runs many ops, each with its own `op=` component, so no fiber-level
component can be the right parent for all of them.

`cells::resolve_under(parent, coord)` takes the parent explicitly for exactly
this reason.

At attach time the declaring tier's component records the dimension **name**
(no values).

Per cycle, the metrics wrapper pulls `__cell_<dim>` alongside
`__metric_<name>`, then:

1. resolve-or-create the child cell for that value — memoized, so the same value
   maps to the same cell for the life of the activity;
2. on first materialization only, register the family on the new cell — the
   uniqueness check runs there, per cell, which is where it belongs;
3. steady state: hash lookup on the value → cell → instrument handle →
   `gauge.set()`. No registry write and no component write lock after first
   sight.

The memoization lives on the parent component's `CellMap`, so it is scoped by
the same containment that scopes the cells themselves — not a separate cache
with a lifetime of its own.

### 5. Compile-time checks

Because the dimension is in the program, validation moves ahead of execution —
the same class of check that just caught the per-op program bug in
`validate_bind_points` (`synthesis.rs:354`):

1. a metric's `cell:` names a dimension declared at some enclosing tier;
2. a dimension name is declared at exactly **one** tier in the chain — this
   turns `Component::attach`'s runtime **panic** into a compile error;
3. the cell expression types as `Str`, by ordinary polydat type checking once
   the binding is synthesized.

### 6. Report side

No change. The value lands in `instance_label` exactly as a sweep label does, so
`group_by: tier` works as it does for `k` / `profile`. The manifest additionally
lets a report distinguish *"dimension declared, no data yet"* from *"typo"* —
today a `group_by:` over a label that never materialized silently yields
nothing.

## Explicitly not changing

- **`register_instrument_with_unit` keeps rejecting duplicate families per
  cell.** No intern-or-return, no relaxation.
- **No cardinality cap.** The number of series is a modeling decision the author
  makes by declaring a dimension, exactly as a sweep over `k` is. A cap would
  also silently discard data.
- **`DynamicCapture` stays** for engine-internal dynamic metrics.

## Open questions

1. **Which tiers may declare.** Phase-level is clearly needed. Op-level
   declaration would let one op own a dimension its siblings never see — useful,
   or a footgun?

## Settled

- **Concurrent siblings may not share a label set; sequential reuse may.**
  The label-ownership check in `attach` is *vertical* (child vs. ancestors). The
  horizontal case — two siblings composing byte-identical `effective_labels`,
  each able to register the same family and produce two instruments wearing one
  identity — is now checked, but only for components alive AT THE SAME TIME.

  An unconditional version was implemented first and rejected: it panics on
  `comprehension_gen_fib_count`, where a sequence containing 1 twice attaches
  two `emit_n` phases labelled `n="1"`. Those are sequential — the first reaches
  `Stopped` before the second attaches — and re-materialising an identity later
  is one identity sampled again over time, not a second identity.

  Liveness rides on a token each component holds until `ComponentState::Stopped`
  (cleared in `set_state`, the single transition choke point); the parent indexes
  `Weak` clones per own-label set and prunes the one bucket it touches. So the
  check needs no lock on any child, costs a lookup in that bucket, and expires
  claims with no teardown pass — a component that is dropped without stopping
  releases its claim too, which a test pins.

  This closes the hazard at the tree rather than leaving it to whichever resolver
  created the child, so a future data-derived child path cannot reopen it.

- **Multi-dimension coordinates are ONE cell** carrying the whole set. Identity
  is the resulting label set, so composition order is not something identity
  distinguishes; nesting would add tree structure with no identity meaning.
- **Lifetime is containment, not a new rule.** A cell lives in its parent's
  subtree and goes when that subtree does. Where a coordinate binding is
  declared, reified, or subset is already governed by polydat's scope semantics
  — this proposal introduces no scope rules of its own.

## Unverified

The pieces are each proven (above), but the synthesized `__cell_<dim>` form
specifically has not been compiled yet — that is the first thing to build and
the first place this design could be wrong.
