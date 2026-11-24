# 11: Polydat Evaluation Model — nmbrs-side framing

> **Planned (SRD-84):** `&&` / `||` land as **eager** truthiness
> combinators (both operands evaluate → `u64` 1/0); short-circuit is a
> deferred optimization (needs conditional-pull — out of scope now).
> The `<expr> as <type>` cast (Part 1b) is an **optional, alignment-only**
> type-fusion infill into the SRD-79 layer. Part 3 formalises the
> **truthiness contract**: predicates resolve to `u64` 1/0 (comparisons
> already do), an indeterminate value takes a stub's declared
> truthy/falsy default, and Rust-generic stubs bind their return type
> via the SRD-80b `Wire` trait. See [SRD-84](84_grammar_safe_matter.md).

The substrate half of this SRD (program/state split,
provenance-based invalidation, the two evaluation lifecycles,
the const binding contract Plan A/B, non-deterministic node
exclusion, input spaces, capture context, compilation
levels) has moved into the polydat crate:

- [polydat/docs/design/evaluation_model.md](https://github.com/nosqlbench/polydat/blob/main/crates/polydat/docs/design/evaluation_model.md)
  — moved 2026-05-30 as part of the import-first reorganization
  (see [docs/polydat_srd_audit.md](../polydat_srd_audit.md))

This file retains the nmbrs-runtime surface: the FiberBuilder
bridge and the cursor-driven evaluation pump.

---

## Planned: type-driven name resolution

The current name-resolution flow (Pass 1 auto-infers every
unknown identifier as a `u64` coordinate input, Pass 3 binding
compilation resolves against the resulting input set) is
type-agnostic. This is the cause of three operator-visible
symptoms:

- The audit-log `boundary adapter: no catalog entry for X → ext`
  warning class (workloads that pass u64 / str / handle values
  into auto-extern'd slots typed as the catch-all `ext`).
- The workload-author surface ambiguity around `set: { mode:
  outer }`: today the bare token lowers to a wire reference
  and silently auto-extern's as a `u64` coordinate, even when
  the operator intended a str literal.
- The YAML array / object workload param round-trip through
  stringified text: `mnc_values: [8, 128]` and `opts: {a: 1}`
  flatten to `"[8,128]"` and `"{\"a\":1}"` at the polydat
  boundary, forcing every consumer to re-parse the structure.
  `Value::Json` / `PortType::Json` already exists as a first-
  class carrier; the resolution pass doesn't currently use it
  as the deferred-typing fallback it's well-suited to be.

[SRD-79](79_type_driven_name_resolution.md) plans the refactor
that makes name resolution type-aware end-to-end. **Primary
goal**: primitive type alignment outside of `Value::Json` —
the type-expectation graph collapses workload-author
polymorphism into runtime specialization wherever the
alignment can be proved safe (`u64`, `f64`, `str`, `bool`,
`vec_u64`, `vec_f32`, …) so producer and consumer wire types
agree by construction. **Type fusion / polyfill** is the
auto-bridge that inserts conversion adapters where primitive
mismatches DO exist (widening, narrowing, parse-from-string,
etc.) or materializes a primitive from `json` at the
receiver. **`Value::Json` is the interstitial bridge and
last-resort carrier** — used only when the graph genuinely
can't decide a primitive alignment OR when the operator
explicitly defers typing for late binding. json is never
the default when the graph could have committed. When SRD-79
ships, this section will be replaced with the new resolution
contract; until then, the existing flow stands and the audit-
log warning surface is the right place to look when the
boundary-adapter surface fires.

---

## FiberBuilder

The per-fiber bridge between Polydat and the execution engine:

```rust
pub struct FiberBuilder {
    program: Arc<GkProgram>,   // shared, immutable
    state: GkState,            // per-fiber, mutable
}

impl FiberBuilder {
    pub fn new(program: Arc<GkProgram>) -> Self;
    pub fn set_inputs(&mut self, inputs: &[u64]);
    pub fn resolve_with_field_pulls(
        &mut self, template: &ParsedOp, field_pull_names: &[String]
    ) -> ResolvedFields;
    pub fn resolve_pulls(&mut self, plan: &PullPlan) -> ResolvedPulls;
    pub fn capture(&mut self, name: &str, value: Value);
    pub fn reset_captures(&mut self, cycle: u64);
    pub fn apply_captures(&mut self);
}
```

No separate params argument — workload params are injected into
the Polydat source as constant bindings before compilation and resolve
as normal Polydat outputs. No globals mechanism needed.

`resolve_with_field_pulls` iterates the op's field map, substitutes
`{name}` bind points from Polydat outputs and captures, and additionally
pulls each name in `field_pull_names` (the union of bind-point
names referenced by op fields) into `ResolvedFields` for the inner
adapter's name-keyed reads.

`resolve_pulls` materializes a [`PullPlan`] (sealed at init from
the per-template `ScopeFixture`, SRD 32 §"Init-Time Fixture and
Consumer Self-Registration") into a `ResolvedPulls` keyed by
`PullHandle`. This is the wrapper-side read path — distinct from
`ResolvedFields` and bundled alongside it in `ExecCtx` (SRD 31
§"Pull plan vs bind plan").

---

## Cursor-Driven Evaluation

When a Polydat program declares `cursor` bindings, the evaluation
model extends from counter-driven to cursor-driven iteration.
A cursor is a Polydat node whose output is a `u64` ordinal. The
runtime advances the cursor externally; downstream accessor
nodes re-evaluate via provenance-based invalidation.

### Advance / Access Separation

The cursor model separates **advance** (moving the position
forward) from **access** (reading data at the current position):

1. **Advance**: The runtime calls `Cursors::advance()` to move
   each targeted cursor to its next position. This is a pull
   from the underlying `DataSource` reader.
2. **Inject**: `Cursors::inject_into_state()` writes the new
   ordinal into the Polydat state's input slot for the cursor.
3. **Access**: The Polydat DAG re-evaluates. Accessor functions
   (e.g., `vector_at(base, ...)`) read the updated cursor
   ordinal and produce typed values. Provenance-based
   invalidation ensures only nodes downstream of the changed
   cursor are re-evaluated.

```
loop {
    if !cursors.advance() { break }  // cursor exhausted
    cursors.inject_into_state(&mut state);
    let fields = fiber.resolve_with_field_pulls(template, &field_pulls[idx]);
    let pulls  = fiber.resolve_pulls(&pull_plans[idx]);
    let ctx = ExecCtx::new(&fields, &pulls);
    dispenser.execute(cycle, &ctx).await;
}
```

### Cursors Type

`Cursors` is a provenance-driven advancer that targets only
the cursor nodes relevant to a specific set of output fields:

```rust
pub struct Cursors {
    targets: Vec<CursorTarget>,  // (DataSource reader, Polydat input index)
    last_items: Vec<Option<SourceItem>>,
    advances: u64,
}
```

Built at phase setup via `Cursors::for_fields()`, which traces
GK provenance from the op template's referenced field names
back to root cursor nodes. Only those cursors advance on each
iteration — unused cursors are left untouched. This enables
phases with multiple cursors where different ops consume
different data sources independently.

### Lazy Evaluation After Cursor Advance

After cursor advance and injection, the Polydat DAG does not
eagerly re-evaluate all nodes. Values are pulled lazily when
`resolve_with_field_pulls` (or `PullPlan::resolve` for wrapper
reads) requests specific outputs. Only nodes in the provenance
chain of the requested output are evaluated. Combined with
per-node caching, this means accessor functions for unrequested
fields are never called.

### DataSource API

The underlying data readers implement the `DataSource` trait:

```
DataSource (per-cursor, stateful)
  ├── next() → Option<SourceItem>     — pull next item
  ├── next_chunk(n) → Vec<SourceItem> — pull up to n items
  ├── extent() → Option<u64>           — known size
  └── consumed() → u64                 — progress
```

All source API surface (`DataSource`, `SourceItem`,
`SourceSchema`, `DataSourceFactory`, `Cursors`) lives in
`polydat::source`. The runtime crates consume these types
but do not define them.
