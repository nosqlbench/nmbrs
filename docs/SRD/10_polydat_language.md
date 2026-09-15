# 10: Polydat Language and Compilation — nmbrs-side framing

> **Planned (SRD-84):** add `&&` / `||` boolean operators (eager
> truthiness combinators; short-circuit deferred) at the **lowest**
> precedence — two-char lexer tokens distinct from bitwise `&`/`|` — and
> a uniform `<expr> as <type>` type-coercion cast (an optional,
> idempotent type-fusion infill into the SRD-79 layer). See
> [SRD-84](84_grammar_safe_matter.md) Parts 1 + 1b.

The Generation Kernel (GK) is a deterministic data generation
engine. It transforms named u64 input tuples into typed
output variates via a directed acyclic graph (DAG) of composable
node functions.

The DSL syntax, type system, node contract, wiring model, and
compilation pipeline now live in the polydat crate:

- [polydat/docs/design/polydat_grammar.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/polydat_grammar.md)
  — **the definitive Polydat surface-language specification and guide**
  (2026-06-28). For any grammar matter — lexical rules, statement and
  expression productions, modifiers, cursors (including the `over`
  clause), `as` casts, type-name vocabulary, projection/round-trip
  behaviour — this is authoritative. Its examples are machine-verified on
  every `cargo test` run, with a programmatic AST-builder companion at
  [polydat_grammar_programmatic.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/polydat_grammar_programmatic.md).
- [polydat/docs/design/language_spec.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/language_spec.md)
  — the substrate half of this SRD (moved 2026-05-30 as part of
  the import-first reorganization; see
  [docs/polydat_srd_audit.md](../polydat_srd_audit.md))

This file retains only the **nmbrs-side framing** — why GK
is the unified access surface for nmbrs workloads, how nmbrs
selects outputs, how the Polydat kernel acts as the unified
state holder for inter-op flow, and op-level binding
conventions. Its grammar-facing material (cursor declarations,
modifiers) is now specified definitively in
[polydat_grammar.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/polydat_grammar.md); the
prose below is retained for the host-integration framing and resolves to
that spec on any grammar-structural conflict.

---

## Polydat as the unified access surface

GK is not only the data-generation engine — it is also the
**assumed interface for reading any runtime value a workload
might want to reference**. If a value is visible at cycle time
(a parameter, a metric reading, the current value of a
[dynamic control](23_dynamic_controls.md), a phase name, a
captured result from a prior op), the way to get at it from a
workload is a Polydat binding, not an ad-hoc side channel.

This gives workloads one resolution model, one type system,
one compile-time name check, one set of diagnostics, and one
set of folding / JIT passes across everything they read. The
bind-point syntax (SRD 21) — `{bind:name}`, `{capture:name}`,
`{input:name}`, `{param:name}` — and the unqualified shorthand
`{name}` all resolve to the same underlying Polydat graph; the only
difference is which *source* a binding attaches to.

### Reification: runtime state → Polydat wire

When the engine has a runtime value that workloads might want
to read, it **reifies** that value as a GK-visible binding or
node output. Three patterns, all interchangeable from the
workload's side:

- **Input**. An external value the runner pushes on every
  cycle (e.g. `cycle`). Declared via `input ...: u64` and
  wired via `{input:name}`.
- **Binding output**. A named Polydat expression compiled into the
  kernel (e.g. `dim := vector_dim("example")`). Read
  via `{bind:name}` or `{name}`.
- **Context node**. A stdlib node that reaches into a stable
  runtime surface and returns its current reading — `metric(...)`
  for a live metric, `control(...)` for a dynamic control
  (SRD 23), and similar nodes for fiber-pool state, phase
  identity, etc. See
  [polydat/docs/design/library_catalog.md §"Runtime context nodes"](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/library_catalog.md).

The three share one implication: **a workload never has a
"second" way to read a value**. If a new runtime quantity is
worth exposing, it is exposed as a reification the DSL can
reference by name — not as a special-case templating hook, an
ad-hoc environment variable, or a global lookup function
wired into the adapter layer.

### Why this is the default

- **One name-resolution story.** `strict` mode (SRD 15) can
  enforce that every reference in every op field is resolvable
  against a Polydat source. There is no "but some parameters are
  read through a different path" exception to audit.
- **Natural composition.** Reified values compose with every
  other Polydat node — a feedback loop that reads `metric("errors/s")`
  and writes `control("rate")` is just a binding, not a plugin.
- **Uniform observability.** The `--explain` / compile-event
  stream (SRD 41) describes every input a workload reads in
  one place. Values that weren't reified would be invisible
  to this accounting.
- **Uniform testing.** `strict` and `dryrun=controls`
  (SRD 23 §"Enumeration: controls are structural") can both
  walk the tree of what a workload will read before anything
  runs, because everything-it-can-read is in the Polydat graph.

Subsystems that introduce new mutable state (a fiber pool's
concurrency, a rate limiter's target, a metric cadence's
current bucket duration) **are expected to reify their
relevant fields as a GK-addressable name** — through a
control, a context node, or both. "I need to observe this
from a workload" is the designer's cue to reify, not to build
a bespoke reader.

---

## Output Selection

Not all bindings become program outputs. Only bindings referenced
by consumers are included:

- Op field bind points: `{user_id}` in a statement
- Param bind points: `{ground_truth}` in `relevancy.expected`
- Extra bindings: validation layer declarations

The compiler scans op fields AND params for `{name}` references.
Unreferenced bindings are dead code — compiled into the DAG but
never pulled, so constant folding may eliminate them entirely.

This is an nmbrs-runtime concern: the activity layer is what
defines "consumer" (which fields and which params count). The
polydat compiler's Output Selection step (see
[polydat/docs/design/language_spec.md §Compilation Pipeline](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/language_spec.md#compilation-pipeline))
operates on whatever consumer set the host hands it.

---

## Polydat as Unified State Holder

The Polydat kernel should be the single state holder for all inter-op
data flow, not just input-driven generation. Captured values
from op results are already injected via ports, but the
current model treats them as second-class inputs. The target
design unifies captures with Polydat inputs:

- Captured values write into named Polydat buffers (the same buffers
  that nodes write to)
- Downstream nodes that depend on captured values re-evaluate
  when the capture changes — the same invalidation mechanism
  that handles input changes
- Complex derived values from captures (e.g., parsing a captured
  JSON string into structured fields) are expressed as Polydat nodes,
  not as special-case logic in the executor

This means the Polydat kernel acts as general-purpose named registers
for inter-op state, with the DAG providing derived-value
computation on top.

The polydat substrate mechanism that supports this
(`ctx.wires` read/write, capture-aware embedding) is in
[polydat composition_substrate.md L3](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/composition_substrate.md).

---

## Op-Level Bindings

Ops may declare their own `bindings:` block as syntactic
convenience — a way to define bindings close to the op that
uses them. Op bindings do NOT create a new scope. They are
merged into the enclosing scope's DAG at compile time.

**Rules:**
1. Op bindings augment the enclosing scope's kernel. They
   add nodes to the same DAG that all ops in the scope share.
2. Op bindings that shadow a name from the enclosing scope
   are a **compile error**. The enclosing scope owns the DAG;
   ops contribute to it but cannot override it.
3. Each op dispenser holds a reference to the enclosing
   scope's Polydat context (program + state). There is no
   per-op kernel.
4. `shared`/`const` constraints from outer scopes are
   enforced — an op binding cannot redefine a `const` name.

If different ops need incompatible bindings, they belong in
different phases. Phases are the scope boundary; ops are not.

**Strict mode** additionally detects cross-op binding
references: if op A declares a binding and op B uses it in
its template, that's an error in strict mode. Each op should
only reference the enclosing scope's bindings or its own.
Cross-op coupling via bindings is a code smell — promote the
shared binding to the enclosing scope instead.

---

## Cursor Declarations

A cursor is a named `u64` position tracker that drives data
access. The `cursor` keyword declares a cursor and wires it
to a constructor that determines its data source:

```
cursor users = range(0, 1000000)
cursor base  = vectordata_base("example", "label_00")
cursor q     = vectordata_query("example", "label_00")
cursor any   = vectordata_source("example", "label_00", "base")
```

The cursor itself is just a position value (a `u64` ordinal).
It does not carry fields or schema — it is a pure position
tracker. Data access happens through **accessor functions**
that take the cursor's ordinal as input and return typed values:

```
cursor base = range(0, vector_count("example:label_00"))
const prebuffer := dataset_prebuffer("example:label_00")
id := format_u64(base, 10)
train_vector := vector_at(base, "example:label_00")
```

Here `base` resolves to the cursor's current ordinal value.
Accessor functions like `vector_at` use that ordinal to look up
the corresponding data. This separation means the cursor is a
simple Polydat node with a `u64` output, and all data access is
expressed through standard Polydat function composition.

#### Cursor-Constructor Sugar

Every vectordata-backed phase otherwise repeats the same
boilerplate: a `range(...)` extent computed from `vector_count`,
a `dataset_prebuffer` const binding, and a per-vector projection
via `vector_at_bytes`. The compiler exposes a generic
*cursor-sugar* registry — any node module can register a
handler that recognizes a non-`range` constructor and rewrites
it into a synthetic `range(...)` plus a list of auxiliary
bindings. The core compiler stays agnostic; nothing in
`dsl::compile` knows what sugar names exist.

##### Vectordata sugar (built-in)

The vectordata module registers three forms:

| Form | Equivalent to |
|------|---------------|
| `vectordata_base(d, p)` | `range(0, vector_count("d:p"))` + prebuffer + `<cursor>__vector := vector_at_bytes(<cursor>__ordinal, "d:p")` |
| `vectordata_query(d, p)` | same, but with `query_count` / `query_vector_at_bytes` |
| `vectordata_source(d, p, "base"\|"query")` | explicit-facet form for tooling that needs the facet as a parameter |

##### Before / after

Verbose form a workload would write today without sugar:

```
cursor row = range(0, vector_count("example:label_00"))
const prebuffer := dataset_prebuffer("example:label_00")
id := format_u64(row, 10)
train_vector := vector_at_bytes(row, "example:label_00")
```

Equivalent sugared form:

```
cursor row = vectordata_base("example", "label_00")
id := format_u64(row.ordinal, 10)
// row.vector is auto-published as a Bytes projection;
// op templates can reference {row.vector} directly.
```

In a complete phase:

```yaml
phases:
  rampup:
    bindings: |
      cursor row = vectordata_base("example", "label_00")
      id := format_u64(row.ordinal, 10)
    ops:
      insert:
        max_batch_size: 64KB
        prepared: "INSERT INTO vectors (id, value) VALUES ('{id}', {row.vector})"
```

The cursor's extent (`vector_count`) drives phase auto-sizing — no
`cycles:` declaration needed; the runtime exhausts the cursor
across all fibers via the source-dispatch model (SRD 18 §"Source
dispatch").

##### What stays explicit

Facet-specific projections like `metadata` / `ground_truth` /
`predicate` stay manual:

```
cursor row = vectordata_base("example", "label_00")
// auto: row.ordinal (U64), row.vector (Bytes), prebuffer init
meta := metadata_value_at(row.ordinal, "example:label_00")
```

Their existence is dataset-conditional (not every dataset has a
metadata column or a predicate facet), so the sugar can't safely
auto-emit them.

##### Adding a new sugar form

Sugar registration is by `inventory::submit!` in any node
module. A handler matches a constructor name, validates its
args, and returns a [`CursorSugar`] describing the rewrite:

```rust
fn my_source_sugar(
    source_name: &str,
    constructor: &Expr,
) -> Result<Option<CursorSugar>, String> {
    let Expr::Call(call) = constructor else { return Ok(None); };
    if call.func != "csv_source" { return Ok(None); }
    let path = positional_str_lit(call.args.first()).ok_or_else(|| ...)?;
    Ok(Some(CursorSugar {
        // Synthesized `range(0, csv_row_count("..."))` drives the
        // cursor's extent through the standard extent path.
        effective_constructor: /* range(0, csv_row_count(path)) */,
        aux_bindings: vec![
            // `<cursor>__row := csv_row(<cursor>__ordinal, path)`
            // Promoted to a `row` projection: `<cursor>.row` works.
            AuxBinding {
                name: format!("{source_name}__row"),
                value: /* csv_row(...) call */,
                projection: Some(("row".into(), PortType::Str)),
            },
        ],
    }))
}

inventory::submit! {
    CursorSugarRegistration {
        handler: my_source_sugar,
        name: "csv",
    }
}
```

`AuxBinding.projection` is the key field: when set, that aux
binding's wire is registered on the cursor's `SourceSchema` and
exposed as a kernel output, so workload op templates can
reference it via `<cursor>.<field>` field-access syntax.

The full surface — `CursorSugar`, `AuxBinding`,
`CursorSugarRegistration`, and the `dispatch` walker — lives in
`polydat::dsl::cursor_sugar`.

**Cursor-to-accessor wiring**: the compiler resolves `base`
in accessor function arguments to the cursor node's output.
The cursor node is an input to the Polydat graph — the runtime
advances it externally, and downstream accessor nodes
re-evaluate via standard provenance-based invalidation.

**Phase completion** is determined by cursor exhaustion. When
all cursors in a phase are exhausted (no more positions to
advance to), the phase completes. This replaces `cycles:` for
cursor-driven phases.

**Planned: auto-cursors.** When a phase references accessor
functions that imply a data source but no explicit cursor is
declared, the compiler will auto-generate a cursor. This
reduces boilerplate for single-source phases.

**Planned: cardinality discovery.** The cursor's extent
(total number of positions) is discovered at init time by
interrogating the constructor's data source. This enables
automatic cycle count derivation and progress reporting.

**Cursor constructor types:**
- `range(start, end)` — finite ordinal sequence (replaces `cycles:`)
- `dataset_source(spec, facet)` — dataset vectors, queries, metadata
