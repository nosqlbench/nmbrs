# SRD-40b — Synthetic metrics declared by op templates

**Status:** normative (sketch — not yet implemented; tracked under
  SRD-98 deferred until phases A–E land)
**Owner:** nmbrs-workload (model + parser), nmbrs-runtime (wrapper +
  registration), nmbrs-metrics (component-level family registry),
  adapters (output-channel convention)
**Cross-refs:** SRD-40 (metrics umbrella), SRD-40a (data model),
  SRD-13c (GK scope model), SRD-18b (scenario tree),
  SRD-20 (workload model), SRD-46 (reports), SRD-52 (stdout model)

---

## What this SRD covers

This document defines the **mechanism** by which a workload op
template publishes synthetic, formula-driven metric values
through the standard data model (SRD-40a). It does **not** name
specific workloads or example formulas — those belong in design
memos under `docs/design/`. The contract here is the cross-cutting
shape: schema, wrapper, registration, invariants.

The motivating use case (analytical curves plotted alongside
measured latency) is documented in
`docs/SRD/notes/synthetic_metrics_cql_vector_demo.md`.

---

## Why the mechanism exists

Workloads need a way to publish synthetic metric values that are
**indistinguishable from measured metrics at the read side** —
same data model (SRD-40a), same catalog (SRD-49), same plot
directives (SRD-46). Without this, three lower-quality paths exist:

- Repurposing the runner's per-op `servicetime` histogram.
  Misnames the metric (it isn't a service time).
- Reusing a real metric family name. Produces ambiguous series
  in the metrics db.
- Pre-populating `sample_value` rows with SQL. Bypasses the
  cadence pipeline and the registry; metrics are invisible to
  `nmbrs metrics` and `metricsql`.

The mechanism in this SRD lets the workload **declare metrics
directly on an op template**, owned by the op dispenser, recorded
through the standard cadence pipeline.

---

## 1. Schema: `metrics:` discriminant on op template

A new field on [`ParsedOp`][parsed-op]:

```rust
pub struct ParsedOp {
    // … existing fields …
    /// Synthetic metrics published by this op per cycle. Map
    /// keyed by metric name (also the default family name).
    /// Empty when absent.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metrics: HashMap<String, MetricSpec>,
}

pub struct MetricSpec {
    /// `value:` — required. A Polydat expression evaluated in the
    /// op's bound scope. The expression may be a bare binding
    /// name (the canonical form when the formula belongs in a
    /// `bindings:` block) or any Polydat expression that produces a
    /// numeric result. See §2.
    pub value: String,

    /// `family:` — optional override of the metric family name.
    /// Defaults to the map key. Use this when two declarations
    /// want the same family at the same dimensional cell to
    /// produce a parse-time duplicate-family error (§7) — name
    /// the second one explicitly so the conflict is visible.
    pub family: Option<String>,

    /// `kind:` — optional. Defaults to `Gauge`: synthetic values
    /// are most often current-state observations (e.g. a
    /// formula's value at the current dimensional cell). Set
    /// `Histogram` when each cycle's value is a sample of a
    /// distribution; `Counter` when monotonic.
    pub kind: Option<MetricKind>,

    /// `unit:` — optional OpenMetrics unit suffix (`ms`,
    /// `bytes`, `ratio`, `seconds`, …). Defaults to "no unit"
    /// — the value is taken as-is, no normalisation, no
    /// suffix in the family name.
    ///
    /// When set, the unit lands in **both** places: it is
    /// concatenated onto the family name per OpenMetrics
    /// convention (`overscan` + `unit=ratio` →
    /// `overscan_ratio`), and it is stored in the
    /// `metric_family.unit` column per SRD-40a's data model
    /// for structured access from the read side. Both surfaces
    /// stay in sync because they're derived from the same
    /// declaration.
    pub unit: Option<String>,

    /// `format:` — optional **generation-time numeric
    /// sanitiser**. Stays in the numeric realm: the spec is
    /// translated at registration time into a round / truncate
    /// operation that runs *before* the value is recorded on
    /// the instrument. Storage holds the sanitised number, not
    /// a formatted string.
    ///
    /// Accepted syntax: **Excel-style hash patterns only**.
    /// `#` and `0` are interchangeable for precision purposes
    /// (Excel's render-time drop-trailing-zero distinction
    /// doesn't apply — we're rounding values, not rendering
    /// strings); the number of placeholder characters after
    /// the decimal point is the precision; the integer side is
    /// layout hint only and ignored at value-sanitiser time.
    /// Printf-style `%3.2f` syntax is **not** accepted —
    /// keeping one syntax avoids the "two paths" trap.
    ///
    /// Examples:
    /// - `"#.##"` → round to 2 decimal places before record.
    /// - `"0.000"` → equivalent to `"#.###"`; round to 3.
    /// - `"#"` → round to integer.
    pub format: Option<String>,
}

pub enum MetricKind { Histogram, Gauge, Counter }
```

The map key is the metric's identity in the op-template surface
and the **default family name**; `family:` overrides it. `HashMap`
(not `Vec`) so duplicate keys at parse time are a YAML syntax
error.

### Generic example

```yaml
phases:
  example:
    # Phase-level Polydat module (SRD-13c). The `value:` field on each
    # metric below is a Polydat expression evaluated in this scope —
    # bare binding names are the simplest form, but inline
    # expressions like `factor * 2.0` are equally valid.
    bindings: |
      example_factor := 1.0 + 2.5 * pow(<dim>, -0.5)

    ops:
      example_op:
        # The op's primary execution — whatever the adapter does.
        # The synthetic metric piggybacks on this op (§8); the
        # adapter, statement, and result handling are unchanged.
        prepared: …

        metrics:
          # Minimum form: just `value:`. Family = "example_factor"
          # (the map key); kind = gauge (default); no unit; no
          # format hint.
          example_factor:
            value: example_factor

          # Full form: every optional field set explicitly.
          example_low:
            value: example_factor * 0.9
            family: example_low_estimate     # override family name
            kind: gauge
            unit: ratio
            format: "%.3f"
```

Per cycle, the wrapper executes the inner op as normal, then
evaluates each metric's `value:` expression against the op's GK
state and records the result on the instrument. The metric lands
in `metrics.db` indistinguishably from any measured metric,
carrying the same label cell the real op produced (per §8).

---

## 2. Sugared forms

The full mapping form (§1) always works. Three sugared shapes are
also accepted; the parser dispatches on the YAML type of the
`metrics:` value.

### 2.1 Bare string — single metric

```yaml
metrics: overscan
```

Equivalent to:

```yaml
metrics:
  overscan:
    value: overscan
```

The name `overscan` is both the family name and the GK
binding/expression pulled per cycle. `kind` defaults to gauge,
`unit` and `format` default to none, `family` defaults to the
name. The bound name must already exist in the op's Polydat scope
(declared in `bindings:` or inherited from phase / workload).

### 2.2 List — sequence of declarations

```yaml
metrics:
  - overscan                                       # bare name
  - throughput_factor                              # bare name
  - latency_pred := 0.5 + 1.5 * pow(limit, -0.4)   # wire expression
```

Each list entry is one of two shapes:

- **Bare name string.** Same as §2.1 — family = name, value =
  name (the Polydat binding), kind = gauge.
- **Wire expression** of the form `name := <Polydat expression>`. The
  parser auto-injects `name := <expr>` into the op template's
  `bindings:` block (creating it if absent — and therefore
  contributing to the op's Polydat graph matter, see §3 on scope
  materialisation). The metric is registered with family = name,
  value = name, kind = gauge.

The wire-expression list form is sugar for "declare the binding
*and* declare the metric in one step." Use it when a metric is a
small inline computation that wouldn't otherwise be reused from
`bindings:`.

**Conflict rules — strict:**

- **`name` already declared in the op's `bindings:` block.**
  Workload parse error: `metric wire '<name>' collides with
  existing binding`. The user picks one site for the
  declaration; auto-injection never silently shadows.
- **Two list entries with the same `name`.** Workload parse
  error: `duplicate metric wire '<name>' in metrics list`.
  Last-wins is bad: a typo would produce the user's "winning"
  value silently.
- **`name` shadowing a parent-scope binding.** Same as a
  collision with the op's own bindings — workload parse error.
  Keep the surface small.

These rules fire at YAML parse time, before any Polydat compilation,
so the diagnostic names the offending YAML location.

### 2.3 Mapping — full form

The canonical shape from §1; supports all five `MetricSpec`
fields. Use the mapping when you need `unit:` / `format:` /
`family:` overrides, or when `kind` should be something other
than gauge.

```yaml
metrics:
  overscan:                       # full spec
    value: overscan
    kind: gauge
    unit: ratio
    format: "%.3f"

  latency_pred:                   # Polydat expression as value
    value: 0.5 + 1.5 * pow(limit, -0.4)
    unit: ms
```

Mixing forms across the workload is fine — different ops can use
different shapes. Within one op, the parser picks the form by
the YAML type of the `metrics:` value:

| YAML type             | Form    |
|-----------------------|---------|
| Scalar (string)       | §2.1    |
| Sequence (list)       | §2.2    |
| Mapping (object)      | §2.3    |

---

## 3. Polydat scope layering for op templates

Op templates compile to their own Polydat kernel scope, binding outer
to the phase kernel. The mechanism — including the
**scope-elision** optimisation that collapses trivial op
scopes into their parent — is specified by
[**SRD-13d — Op-template Polydat scope layer**](13d_op_template_scope.md).
Read that SRD for the full rules; the summary here is just enough
to read SRD-40b standalone.

Key contracts SRD-40b depends on:

- An op-template scope is a standard SRD-13c Polydat scope:
  `bind_outer_scope(phase_kernel)`, auto-extern resolution up the
  chain.
- The compiler decides per workload load whether each op's scope
  is **elided** (collapsed into the phase scope; no separate
  kernel) or **materialised** (own kernel, instanced at premap).
  The decision uses program-hash equivalence — see SRD-13d §3.
- Validation of every op template's Polydat source happens at
  workload load (SRD-13d §2.1); per-iteration kernel instances
  are produced at premap, descending to the op-template tier
  (SRD-13d §2.2). `dryrun=op` is the diagnostic level that
  exercises this path without running cycles (SRD-13d §2.3).

What SRD-40b adds on top: a `metrics:` block contributes Polydat graph
matter (an explicit `value:` expression, an auto-injected wire
expression, etc.). Per SRD-13d, that pushes the op into the
**materialised** category — the metric's wires need a kernel to
live in. Op templates with only bare-name `value:` references
that resolve to phase bindings do *not* push into materialised;
SRD-13d's elision pre-walk handles that case correctly.

When SRD-13d's machinery lands, SRD-40b's wrapper (§6) just
asks the dispenser for its kernel handle (which may be the
op-template's own kernel or, after elision, the phase
kernel) and pulls each metric wire through the standard GK
state API.

---

## 4. Value source is always Polydat — one path

The `value:` field is a **Polydat expression** evaluated in the op's
bound scope (workload → phase → op binding cascade, per SRD-13c).
A bare binding name (`overscan`) is the canonical form when the
formula's complexity belongs in a `bindings:` block, but any GK
expression is valid — the system has no preference between
`value: overscan` and `value: latency_factor * 2.0` other than
"keep complexity where it composes."

There is **no `from:`** discriminator distinguishing "result
body" from "GK context": the value is always GK. When a workload
wants the metric value to depend on the op's *result body*
(captures, returned columns), the dispenser exposes those fields
as Polydat bindings (via the existing capture mechanism), and the
metric's `value:` references the resulting binding name —
identical machinery, identical surface.

This rule is load-bearing: the registry, the wrapper, and the
parser all assume one evaluation context (GK). Adding a second
source later is a redesign, not a feature flag.

---

## 5. Result fields exposed as Polydat named wires

When a metric's value depends on the op's *result body*
(captured fields, returned columns, error flags), those values
must reach the Polydat scope through the same single path §4
demands: as **named wires** in the op-template's Polydat scope. The
metric's `value:` expression then references those wires like
any other binding.

The mechanism: a **result-as-GK adapter layer** owned by the
op dispenser. Per cycle, after the inner adapter returns its
`OpResult`, the dispenser's result adapter:

1. Reads the op-template's `captures:` / `expects:` declaration
   (the existing capture-extraction surface — SRD-34 Capture
   Points).
2. Pulls the named fields out of the result body.
3. Writes each field's value into the matching slot on the
   op's `PolydatState`.

Once written, the wires are visible to **every later Polydat eval
in this cycle** — including the metric wrappers (§6) and any
downstream op template that descends from this scope.

### 5.1 Schema: `result:` declaration on the op template

```yaml
phases:
  query:
    ops:
      ann_query:
        prepared: …
        result:                          # NEW — names result-derived wires
          rows_returned: count           # built-in: row count
          first_distance: rows[0].distance     # path expression on result
          recall: validate(ground_truth, rows)  # Polydat function over result + ctx
        metrics:
          query_recall:
            value: recall                # references result-derived wire
```

Each entry in `result:` declares one wire:

| Form                   | Source                                                           |
|------------------------|------------------------------------------------------------------|
| `<name>: count`        | Number of rows / records / elements in the result body.          |
| `<name>: ok`           | Boolean — true if the op succeeded (no `AdapterError`).          |
| `<name>: <path-expr>`  | Path into the result body's JSON-like view (`rows[0].field`).    |
| `<name>: <polydat-call>`    | A Polydat function call that takes captures + result + ctx as inputs. |

Path expressions and Polydat calls share Polydat's existing expression
language. The result-as-GK layer just dispatches on whether the
RHS is a built-in (`count`, `ok`), a path expression, or a GK
call.

### 5.2 Lifecycle within a cycle

The order is fixed:

1. Inner adapter executes; produces `OpResult { body, captures }`.
2. **Result-as-GK adapter** writes captured / declared wires
   into the op's `PolydatState`.
3. **MetricsDispenser** (§6) evaluates each metric's `value:`
   expression against the now-populated `PolydatState`.
4. Records on the instrument (kind dispatch per §6.1).

This is one path through one Polydat state. No `from:` flag, no
parallel evaluation surface — captures *are* Polydat bindings, and
metrics that depend on them are Polydat expressions.

### 5.3 Interaction with existing capture mechanism

SRD-34 already extracts named captures from result bodies into
the Polydat state for use by *subsequent ops* in the same scope.
SRD-40b's `result:` declaration uses the same machinery, with
two clarifications:

- **Same-cycle visibility.** Captures from this op are
  visible to this op's metric wrappers in the same cycle (the
  metric wrapper runs after the result adapter writes wires).
  Cross-op visibility within the cycle remains as SRD-34
  defines it.
- **Type homogeneity.** A wire named `rows_returned` set by
  the result adapter has the same `polydat::node::Value`
  shape as any GK-computed wire. Metric expressions can mix
  result-derived and formula-derived wires freely.

### 5.4 What this resolves

- Metrics that need result-body data go through a single GK
  evaluation surface, not a separate "result tap" path.
- The op-template scope (SRD-13d) is the only home for these
  wires; elision rules apply uniformly (an op with `result:`
  declarations contributes `Definitions` to its `polydat_matter()`,
  preventing elide — which is correct: result wires are
  per-op, not per-phase).
- Workloads that use captures + metrics together don't have
  to learn two surfaces; `result:` is just sugar over wiring
  the existing capture mechanism into Polydat at op-template scope.

---

## 6. Wrapper: `MetricsDispenser`

A new op-dispenser wrapper, sibling to the existing decorators in
[`crates/nmbrs-runtime/src/wrappers.rs`][wrappers]. Stacks **outermost**
in the op-dispenser construction chain:

```text
adapter
  → ConditionalDispenser   (if op.condition)
  → ThrottleDispenser      (if op.delay)
  → TraversingDispenser    (always)
  → EmitDispenser          (if emit / debug)
  → MetricsDispenser       (if op.metrics non-empty)  ← outermost
```

Outermost so `MetricsDispenser` observes the fully-bound
post-execute Polydat state — captures from `TraversingDispenser` are in
scope, condition-skipped ops produce no metric sample, throttling
has already paid its time.

**Init** (during op-dispenser construction):

1. Resolve `family` (default = key) and compile `value:` against
   the op's bound Polydat scope (or the parent scope when SRD-13d
   elides the op tier).
2. Look up the dispenser's `Component` (§7 — when the op
   materialises its own scope, the dispenser becomes its own
   component).
3. Allocate the instrument by `kind`, register on the component,
   and add the family to the component's instrument set. The
   collision check in §7 fires here.
4. Stash the instrument handle in the resolved spec.

### 5.1 `MetricKind` → instrument type → record API

Each `MetricKind` variant maps to a concrete instrument from
[`nmbrs_metrics::instruments`][nmbrs-instruments]:

| `MetricKind` | Instrument type    | Per-cycle call             | Stored shape                                             |
|--------------|--------------------|----------------------------|----------------------------------------------------------|
| `Gauge`      | `ValueGauge`       | `gauge.set(value as f64)`  | One `f64` slot, last-write-wins. Read-side reports the   |
|              |                    |                            | latest set value at each cadence tick.                   |
| `Histogram`  | `Histogram`        | `histogram.record(value as u64)` | Bucketed distribution. Per cadence tick: count,    |
|              |                    |                            | sum, p50/p99/min/max/stddev (SRD-40a `MetricPoint`).      |
| `Counter`    | `Counter`          | `counter.inc_by(value as u64)`   | Monotonic running total. Negative-or-zero values   |
|              |                    |                            | from Polydat are a runtime warning + skip-record (counters   |
|              |                    |                            | can't go down).                                         |

Type conversion at the wrapper:

- The Polydat expression returns a `polydat::node::Value`. The
  wrapper expects a numeric variant (`U64`, `F64`, or any
  bool/int that converts cleanly). Non-numeric variants
  (`Str`, vectors) are a per-cycle warning + skip-record.
- `Gauge` records the value as `f64` directly; `Histogram` and
  `Counter` cast to `u64` (truncating fractional parts after
  any `format:` rounding, §1).

`Gauge` is the SRD-40b default (per §1). The `format:` numeric
sanitiser (§1) runs *before* the kind-dispatched record so all
three instrument types see the rounded value.

**Per-cycle execute** awaits the inner dispenser, then for each
spec:

1. Evaluate the spec's compiled Polydat expression against the op's
   `PolydatState`.
2. Apply the `format:` sanitiser if set.
3. Dispatch to the kind-specific record method per the table.

**Concurrency:** instrument record APIs are atomic; multiple
fibers within one phase recording the same instrument is safe.
For `Gauge`, semantics are last-write-wins by definition (a
gauge holds one value); for `Counter` / `Histogram`, samples
aggregate. Workloads where every fiber computes the same
synthetic value at one dimensional cell are identity-safe under
last-write-wins; workloads where per-fiber values diverge (§5
result-derived metrics) should choose a kind whose semantics
match the intended aggregation.

[nmbrs-instruments]: ../../crates/nmbrs-metrics/src/instruments/

---

## 7. Family + dimensional uniqueness — strict at init

The nmbrs runtime aligns metric instruments with **components**:
each instrument is owned by the closest component for which its
label set is canonical. The component tree (SRD-40 / SRD-40a)
already does this for measured metrics; SRD-40b reuses the
same model.

### 7.1 The op-dispenser becomes a component

When an op template's Polydat scope materialises (SRD-13d), the **op
dispenser becomes a component** if it isn't already. Its
parent is the phase component; its `labels` carry the
op-template name plus whatever other label keys the op
iteration produces; its `effective_labels` is the standard
`parent.effective.extend(&own)`.

When the op template's scope is elided (SRD-13d §3),
declared metrics still need a component — the dispenser still
becomes a component for the duration of the cycle's metric
recording. (Elision is about the Polydat kernel layer, not the
metric component layer; the two decisions are independent.)

### 7.2 Duplicate-family check uses the component's registry

Each `Component` carries `Vec<RegisteredInstrument>`
(consolidated 2026-05; see SRD 40 §"Instrument registry").
`Component::register_instrument(family, instrument)` is the
single entry point — it scans the Vec for a name collision
and inserts atomically.

The check fires at op-dispenser construction, before any cycle
runs:

```
duplicate family name on dimensionally-same metric context:
  <family>{<effective_labels>}
```

If the same family name appears at a different op template
with a different label cell, the components differ — no
collision. The dimensional uniqueness comes from the component
tree's existing structure, not a separate registry.

### 7.3 Op-name is part of the dimensional context

The dispenser-as-component carries an **`op` label** with the
op-template name. Two ops in the same phase declaring the same
family produce different effective label sets (`phase=X,
op=foo` vs `phase=X, op=bar`), so the duplicate-family check
sees different components and accepts both registrations.

This is how the component tree has always worked for measured
metrics; SRD-40b inherits the convention. Workloads that
declare two ops in the same phase with the same family name
get two distinct families in the metrics db, distinguishable
by the `op` label.

### 7.4 Lifetime

Components drop on scenario end (existing behaviour, SRD-40
component lifecycle). Their instruments drop with them. SRD-40b
introduces no new lifetime; the dispenser-as-component is just
another component in the tree.

Per §11, the runtime emits a `scope_close` flush signal to the
cadence streamer at component teardown so partial windows
reach the streamer before the component drops.

Strict (not lazy) by spec: the user pays the validation cost once,
at workload load, rather than per-cycle. Workloads that legitimately
emit multiple values for the same family-on-the-same-cell must use
distinct family names, full stop.

---

## 8. Dimensional alignment — load-bearing principle

A synthetic phase that wants to be plottable alongside a measured
phase must produce metric samples whose **label cells match** the
measured phase's. This is structural, not advisory: SRD-46 plot
directives, `where=` filters, and `series=` axes act on label sets
— a synthetic family that lacks the right labels (or carries
extra ones) cannot be rendered uniformly with the measured family.

The mechanism for getting alignment right: **the synthetic phase
must read the same Polydat names** that produce the measured phase's
iteration variables. Examples (specifics live in workload-domain
design memos):

- A measured phase iterates over `comprehension_var in
  source_collection` (polydat clause syntax per
  `polydat/docs/design/comprehension_forms.md` §3.1 + §8.1)
  to produce its label cell. The synthetic phase iterates over
  the same expression, naming the variable the same way.
- Both phases descend from the same set of workload-level GK
  bindings. Phase-level overrides on either side break alignment;
  the SRD-13c bind-outer-scope rule keeps the binding cascade
  consistent if both phases sit at the same level of the scenario
  tree.

When the synthetic phase reuses the measured phase's Polydat names,
dimensional drift is **impossible by construction**: the iteration
that produces each op instance produces the labels with it, and
both phases inherit identical labels for matched cells.

A future syntactic helper for "produce one synthetic op per
measured cell" is plausible but **out of scope** for this SRD; the
explicit-iteration form is short and tight.

---

## 9. Adapter output-channel convention

Synthetic phases need an op-execution adapter, but the adapter's
*own* output is incidental — the metric is the point. Adapters
that participate in synthetic flows must offer a way to suppress
their normal output without suppressing op execution.

The convention:

- **Op-template parameter** `<adapter>: <channel>` — for example
  `stdout: eventlog`.
- Channel value `eventlog` (or equivalent) routes the adapter's
  rendered output into the standard event log instead of the
  user-facing terminal / file / network destination.
- Other adapters adopting this pattern use the same parameter
  *name* (the adapter name) with whatever channel set they
  support, so workloads stay portable across adapter swaps.

The stdout adapter (SRD-52) is the reference implementation for
this convention; other adapters extend it independently.

---

## 10. Read-side parity (SRD-46 plot directives)

Synthetic and measured metric families are **indistinguishable**
to plot, table, query, and metrics-list directives. A plot
directive over a synthetic family takes the same shape as one
over a measured family:

```yaml
plot <name>
  query: avg(<synthetic_family>_mean) by (<dim1>, <dim2>)
  x: <dim1>
  series: <dim2>
  …
```

The catalog (SRD-49) sees the family by the same name lookup; the
sqlite reporter writes it to the same `metric_instance` table; the
metricsql evaluator joins it on the same label set. No code in the
read side knows the family is synthetic; SRD-40b doesn't introduce
a "synthetic" flag anywhere downstream.

---

## 11. Scope-lifecycle flush to the cadence streamer

Synthetic metrics emit one sample per cycle, but the **cadence
pipeline** (SRD-40 / SRD-42) aggregates samples into windows
sized by the configured cadence (e.g. 1s buckets). When a
phase ends *between* two cadence pulses, every sample emitted
since the last pulse is in flight — held by the instrument's
delta accumulator, not yet flushed to the streamer.

For long-running phases this rarely matters; the trailing
pulse picks them up shortly. For **short or one-shot phases**
(`for_each` iterations producing a few cycles each, synthetic
phases with one cycle per dimensional cell), the trailing
samples can sit unflushed past the phase's end and never
appear in the stream — the instrument is dropped at component
teardown before the next pulse fires.

This SRD requires the runtime to **signal lifecycle close to
the cadence streamer** so partial windows are captured.

### 11.1 The flush signal

When a component ends (phase complete, scenario complete,
op-dispenser dropped), the runtime sends a **`scope_close`**
event to the cadence streamer carrying:

- The component's logical name + effective labels.
- A list of every instrument owned by the component.
- A "partial window" timestamp marker.

The streamer responds by:

1. Pulling the current delta from each named instrument
   (without resetting future windows — the instrument is
   ending, but the stream isn't).
2. Recording the partial window as a sample with a
   `partial=true` annotation in the sample metadata.
3. Aggregating that partial sample into the next full
   cadence window when one closes (sums for counters/
   histograms, last-value for gauges).

After the flush, the component is free to drop. The
instrument's data lives in the stream.

### 11.2 Aggregation rule for partial windows

A complete cadence window may be the sum of multiple partial
segments (one or more `scope_close`-flushed partials plus, if
the last cadence pulse falls inside the same window, the
pulse-closed remainder). The aggregation is per-instrument-
kind:

| Kind        | Aggregation across partials in one window           |
|-------------|------------------------------------------------------|
| `Counter`   | Sum of partials.                                    |
| `Gauge`     | Last `set()` value across partials in time order.   |
| `Histogram` | Merge of partial histograms (HDR-style merge).      |

This is the same aggregation the streamer already applies
within a single window for normal cadence-pulse-flushed
samples; partial windows just carry an extra annotation so
downstream tooling can tell them apart if needed.

### 11.3 Why this matters for synthetic metrics

A synthetic phase that emits one metric per dimensional cell
might run only a handful of cycles total. Without the flush
signal, those samples can be invisible in the metrics db —
the instrument drops before the cadence pulse that would
flush it. The lifecycle-close signal makes synthetic phases'
metrics observable regardless of phase duration.

### 11.4 Generality

The flush mechanism isn't synthetic-specific. Any phase whose
duration is shorter than the cadence interval benefits.
SRD-40b raises the requirement because synthetic phases are
the sharpest test case (often very short), but the
implementation belongs in the generic cadence-streamer
boundary — every phase consumes it, not just synthetic ones.

The contract is codified in [SRD-42 §"Component lifecycle:
`scope_close` flush"](42_windowed_metrics.md). This section
holds the SRD-40b-shaped justification for *why* the flush
matters; SRD-42 holds the cadence-reporter-shaped *how*.

---

## 12. Resolutions and codified working assumptions

This SRD has no remaining open questions. Every item that was
once tracked as "open" has been resolved or codified as a
working assumption — in this section or in the SRDs cross-
referenced from it.

### 12.1 Resolved

1. **Result-as-GK named-wire spec.** Specified in §5
   (result-fields-as-GK-wires via op-template `result:`
   declaration; one path, one Polydat state per cycle). Was
   previously deferred; pulled forward because the demo and
   any non-trivial synthetic workload needs it.

2. **Inline op templates and `metrics:`.** Inline op templates
   are treated identically to detailed templates. The schema
   is the same (`metrics:` discriminant), the parser flows
   through the same normalisation pass, and the wrapper sees
   no distinction. No special-case logic.

3. **Scope-lifecycle flush.** Specified in §11. Short phases
   (especially synthetic phases with a handful of cycles)
   cannot rely on the next cadence pulse to flush their
   instruments — the runtime sends a `scope_close` flush
   signal at component teardown, and partial windows
   aggregate into the enclosing cadence window. Generic
   mechanism; SRD-40 / SRD-42 will be amended to absorb the
   contract.

### 12.2 Codified working assumptions

4. **`unit:` in family-name suffix vs separate column.**
   Resolved as **both** (§1) — the value flows into the
   OpenMetrics-conventional family-name suffix *and* lands in
   the `metric_family.unit` column for SRD-40a-aligned
   structured access. SRD-40a may need a one-line
   confirmation that it accepts this dual storage.

5. **`MetricKind::Histogram` for synthetic metrics.** Kept in
   the enum at user discretion. Histograms are core
   operational tools; the SRD does not pre-judge meaningful
   uses. Workloads where every sample is the same value get a
   degenerate distribution — that's the workload author's
   call.

6. **Concurrency on `gauge.set()`.** Multiple fibers within
   one phase: last-write-wins. For synthetic metrics where
   all fibers compute the same formula at one dimensional
   cell, this is identity-safe. For metrics where the value
   depends on the cycle's result (§5), per-fiber values can
   diverge — last-write-wins still applies, and the user is
   responsible for choosing a metric kind whose semantics
   match (`Histogram` or `Counter` aggregate; `Gauge`
   over-writes).

---

## 13. Implementation phases (cross-cutting)

**Prerequisite:** SRD-13d (op-template scope layer + scope
elision + `dryrun=op`) must land before SRD-40b's dispenser
work can run cleanly. SRD-40b's wrapper consumes SRD-13d's
op-template kernel handle.

| Phase | What                                                                                | Where                                              |
|-------|-------------------------------------------------------------------------------------|----------------------------------------------------|
| A     | `ParsedOp.metrics` + `MetricSpec` + `ParsedOp.result` model + parsing (full + sugared) | `crates/nmbrs-workload/src/{model,parse}.rs`              |
| B     | `format:` numeric-sanitiser parser (Excel hash patterns → round/trunc op)           | `crates/nmbrs-workload/src/report.rs` or sibling           |
| C     | `unit:` flow into both family-name suffix and `metric_family.unit` column           | `crates/nmbrs-metrics/src/reporters/sqlite.rs`             |
| D     | Result-as-GK adapter layer (§5): dispenser-owned, writes captured wires to GkState  | `crates/nmbrs-runtime/src/wrappers.rs`                    |
| E     | `MetricsDispenser` wrapper + kind→instrument dispatch (§6.1)                        | `crates/nmbrs-runtime/src/wrappers.rs`                    |
| F     | Wrapper insertion at op-dispenser construction; op-dispenser as component (op label) | `crates/nmbrs-runtime/src/{runner,activity}.rs`           |
| G     | Component instrument-set duplicate check on registration (§7)                       | `crates/nmbrs-metrics/src/component.rs`                    |
| H     | `scope_close` cadence-streamer flush signal (§11) — generic, not synthetic-specific | `crates/nmbrs-metrics/src/scheduler.rs` (or sibling)       |
| I     | Adapter output-channel convention (stdout impl)                                     | `crates/nmbrs-adapter-stdout/src/...`                          |

Phases A, B, C, E, G are independently testable. Phase D
depends on SRD-34 (Capture Points) and on phase A. Phase F
depends on SRD-13d phases 4–6 (premap descent + dispenser
kernel handle). Phase H is generic — every short phase
benefits, not just synthetic ones; it should land alongside an
amendment to SRD-40 / SRD-42 documenting the flush contract.
Workload adoption (the actual phase / scenario / plot YAML)
belongs in a design memo and per-workload follow-up — not part
of this SRD.

[parsed-op]: ../../crates/nmbrs-workload/src/model.rs
[wrappers]: ../../crates/nmbrs-runtime/src/wrappers.rs
