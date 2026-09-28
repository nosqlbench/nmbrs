# SRD-69 — Capture Semantics

**Status:** Draft, 2026-05-14.

## Why this exists

"Capture" has been an implicit concept across SRD-66 (result-bindings),
SRD-67 (subcontext construction), SRD-68 (dispenser-owned Polydat context),
and the recent wrapper-stack-canonical-scope follow-up. The *sink* side
got unified — every capture lands on the op-template kernel via
`ctx.wires.write`. The *source* side is still scattered: bind-points
in stmt strings, result-bindings, magic externs, adapter-direct
writes. The lifecycle rules (when does a capture fire, who triggers,
how does slot allocation work, what happens on name collision) are
nowhere stated as a single contract.

This document is that contract.

## Definition

A **capture** is any typed value written to an op-template kernel
input slot during op execution. It is observable through
`ctx.wires.get(name)` from any wrapper above the writing layer in the
dispenser onion. Captures have the same shape and lifecycle as any
other kernel input — they are not a separate namespace, they are not
on a separate read API, they are not stored in a sidecar. The
distinction "capture vs binding" is about *source*, not about
storage or visibility.

This is deliberate. We do not extend the Polydat kernel surface with a
capture-specific API. Captures are inputs the kernel doesn't know
came from outside the Polydat matter walk; the matter walk treats them
identically to any other input slot. Encapsulation stays at the
`ResultBody` / `WireSource` boundary.

## The four sources

Every capture in the project today comes from one of four sources.
The source determines who fires the write and when; once landed on
the kernel, all four are indistinguishable.

### (1) Bind-point captures from op-field strings

**Shape:** `[name]` or `[name as alias]` inside `stmt:` / `prepared:`
strings.

**Parsing:** `nmbrs_workload::bindpoints::parse_capture_points`.

**Site:** `TraversingDispenser` (`crates/nmbrs-runtime/src/wrappers.rs:131`).
After the inner adapter returns, the wrapper walks
`result.body.to_json()` for each declared capture point and writes via
`ctx.wires.write(name, value)`.

**Typing:** Today, untyped JSON walk (`extract_captures_from_json`).
Numbers become `Value::U64` / `Value::F64` / `Value::Str` depending on
JSON form; everything else is stringified.

### (2) Result-binding captures (`result:` block)

**Shape:**
```yaml
result:
  name1: expr1
  name2: expr2
```
or the equivalent string-form Polydat source.

**Parsing:** SRD-66 `ResultSpec`, flattened to a Polydat source by
`collect_result_bindings_source` (`crates/nmbrs-runtime/src/scope.rs`).

**Site:** Compiled INTO the op-template kernel by
`SubcontextBuilder::add_result_bindings`. Each LHS becomes a kernel
output (and possibly a Rule 2 write-through to a parent shared cell).
The RHS may reference magic externs (which the closure-binding
economy walks for slot allocation).

**Typing:** Whatever the Polydat expression evaluates to. Fully typed
through the eval cone.

### (3) Magic-extern captures (`body` / `count` / `ok`)

**Shape:** Implicit — these names are reserved. Referencing any of
them in a result-binding RHS (or under `KernelOptLevel::Diagnostic`,
unconditionally) allocates an input slot on the op-template kernel.

**Site:** `ResultDispenser` (`crates/nmbrs-runtime/src/wrappers.rs:~970`).
After the inner adapter returns, writes the standard three values
via `ctx.wires.write`:
- `body` → `Value::Json(body.to_json())`
- `count` → `Value::U64(body.element_count())`
- `ok` → `Value::Bool(true)`

**Typing:** Fixed by SRD-66 §"Surface 4": Json / u64 / bool.

### (4) Adapter-direct captures

**Shape:** No explicit declaration in YAML. The adapter just writes.

**Site:** Inside the adapter's own `OpDispenser::execute`. Examples:
- CQL batch dispenser writes `rows_inserted` after a successful
  batch (`crates/nmbrs-adapter-cql/src/cassandra_cpp/mod.rs:~1640`).
- Await/Poll dispenser writes `poll_count` and `poll_elapsed_ms` on
  the condition-met cycle (`wrappers.rs:~537`).

**Typing:** Adapter-chosen. Native types preserved.

## The sink contract

Every source writes through the same call:

```rust
let _ = ctx.wires.write(name, value);
```

Returns `WriteOutcome::Stored | NoSlot`. The result is ignored — a
`NoSlot` return is the closure-binding economy's DCE signal (the
workload didn't reference this name; the kernel doesn't have a slot;
the value is silently dropped). Sources do not gate their write on
whether the slot exists; the kernel decides.

## Slot allocation: who triggers it

Op-template kernel synthesis is the single decision point. It walks
all *use sites* (places where a name is *read*) and allocates input
slots for any name a magic-extern injector or cross-scope wiring
phase claims.

Current use-site walkers:

- Result-binding RHSs → walked by `add_result_bindings` for magic-extern
  injection (`subcontext/builder.rs:~265`).
- Metric `value:` expressions → walked by the op-template synthesiser
  via the `__metric_<name> := <value_expr>` synthesised bindings
  (`scope.rs:~1620`). Same walker as result-bindings.
- Evaluator inputs (`evaluations.relevancy.expected:`) → not currently
  a use-site walker input; the wire is expected to already exist on the
  kernel via some other source. Wrap-time `register_pull` validates.

**The closure-binding economy is the slot-allocation authority.**
A source can write to a name; whether the write lands depends on
whether some use site referenced that name.

Under `KernelOptLevel::Diagnostic`, magic externs are
force-allocated regardless of reference. Other slots still follow
the use-site rule.

## Collision rules

When multiple sources target the same name:

- **Magic-extern names are reserved.** `body` / `count` / `ok` as
  result-binding LHSs is a hard compile error
  (`add_result_bindings` lines 274-281). No collision is possible
  with result-bindings; adapter-direct writes to these names would
  overwrite the ResultDispenser writes (don't do that).

- **Result-binding LHS == bind-point capture name** is permitted
  but ambiguous. Order of execution:
  1. `TraversingDispenser` runs at its layer of the onion, writes
     bind-point captures.
  2. `ResultDispenser` runs (outer), writes magic externs and any
     `result:` LHS computed dispenser-side (path-expr legacy form).
  3. Polydat eval cone fires for result-binding LHSs that are kernel
     outputs.

  Net result: last writer wins. Today's TraversingDispenser is the
  inner-most capture writer, so a result-binding overrides it.

- **Result-binding LHS == kernel binding (regular `bindings:` LHS)**
  is currently undefined. The synthesiser doesn't reject it; the
  compile may surface a duplicate-binding error. **TODO:** make this
  a deliberate compile error.

- **Adapter-direct write == any declared capture name** is the most
  invisible collision. The adapter writes whatever it writes; the
  workload may or may not know. Convention: adapters should namespace
  their direct writes by adapter name (e.g. `cql_rows_inserted` rather
  than just `rows_inserted`) to avoid colliding with workload-declared
  names. **TODO:** prefix-convention not enforced today.

## Lifecycle per cycle

For one cycle:

1. **Pre-execute:** `cycle_wires = CycleWires::new(fiber.per_op_kernel_mut(template_idx))`.
   `ExecCtx::with_wires(...)` binds it for the stack's duration.

2. **Stack runs:** inner → outer.
   - Adapter executes, returns `OpResult`. Native body in `result.body`.
   - Adapter may write adapter-direct captures via `ctx.wires.write`.
   - `ConditionalDispenser` skips path: no captures fire.
   - `TraversingDispenser` extracts bind-point captures from
     `body.to_json()`, writes via `ctx.wires.write`.
   - `ResultDispenser` writes magic externs (`body`/`count`/`ok`) plus
     any dispenser-side path-expr result-bindings via `ctx.wires.write`.
   - `MetricsDispenser` reads `ctx.wires.get(__metric_<name>)` —
     pulls fresh through the eval cone, sees every write above.

3. **Post-execute (activity loop):**
   - `fiber.commit_op_template_write_throughs_for_idx(template_idx)`
     fires Rule 2 write-throughs from result-binding LHSs to parent
     shared cells.
   - `fiber.pull_all_op_template_outputs_for_idx(template_idx)`
     evaluates every output so side-effecting nodes (`log_info` etc.)
     in result-binding RHSs fire.

The dispenser stack is single-threaded per cycle per fiber. No race
between writers.

## The adapter-side capture hook

The current `ResultBody` trait exposes `to_json` + `as_any` —
adapter-aware code can downcast for typed access, but no
standardised "give me this field as a typed `Value`" path exists.

This SRD specifies adding:

```rust
pub trait ResultBody: Send + Sync + fmt::Debug {
    // ... existing methods ...

    /// Extract a named field from this body as a typed Polydat `Value`.
    /// Adapters with native typed columns (CQL row metadata, HTTP
    /// header maps, etc.) override to return the underlying typed
    /// data without a JSON detour. The default implementation walks
    /// `self.to_json()` with best-effort typing — matches the legacy
    /// `extract_captures_from_json` shape so adapters that don't
    /// override behave as they do today.
    ///
    /// `None` when the field doesn't exist in the body.
    fn capture(&self, name: &str) -> Option<polydat::node::Value> {
        capture_from_json(&self.to_json(), name)
    }
}
```

`capture_from_json` is a framework-provided helper that does the JSON
walk + best-effort typing. The CQL adapter overrides `capture` for
its native body type to produce `Value::VecI32` / `Value::VecF32` /
`Value::Str` directly from typed row data without serializing to
JSON first.

This is the API-specific-capture hook the project needs. Adapter
knows its types; framework calls the trait method; value flows
through `ctx.wires.write`.

## Multi-row column projection with per-element transforms

The user's specific concern: SELECT returns N rows, each with a
`key` column. We want all keys as `Value::VecI32` for the recall
comparison. But the workload author may need to apply a per-element
GK transform — cast, modulus, lookup table, hash — before the values
land in the wire.

### What works today

The trivial-projection case (no transform) is supported by the
`body_column_i32` node + adapter `capture` override:

```yaml
result:
  keys: body_column_i32(body, "key")
```

Or, with the adapter-side hook:

```yaml
captures:                    # NEW shape, see "Capture declarations" below
  keys: { from: body, column: "key" }
```

Either way, `keys` lands as `Value::VecI32` on the kernel; the
evaluator reads it via `ctx.wires.get("keys")`.

### What's missing

A per-element transform during projection. The workload author can't
say:

```yaml
captures:
  keys: { from: body, column: "key", map: hash(value) % shard_count }
```

…because Polydat doesn't have closures and the per-element binding name
`value` isn't a thing in the current language.

### The shape of the eventual answer

Three viable paths, in increasing scope:

**Path A — Pre-baked variants.** For each common transform, build a
specialised node: `body_column_i32`, `body_column_u64`,
`body_column_str`, `body_column_i32_mod(body, name, base)`,
`body_column_i32_hash(body, name)`, … Covers the practical use cases
but doesn't scale to arbitrary transforms.

**Path B — Capture-pipeline declaration.** Extend the YAML schema
with a capture-pipeline form:

```yaml
captures:
  keys:
    from: body
    column: "key"
    per_element: hash(value) % shard_count
```

The synthesiser compiles `per_element` as a Polydat micro-program with a
free variable `value` that each row's value gets bound to. The
compiled node iterates the column, applies the micro-program, collects
into a typed vector. No language extension; pure synthesis-layer work.

**Path C — Polydat closures / for-each.** Add closure literals to the GK
language: `body | for_each(value -> hash(value) % shard_count) | collect_i32`.
Real language extension. Powerful, but a bigger surface to maintain.

**SRD position:** Path A is the canonical-for-now (write specialised
nodes as use cases land). Path B is the next-step recommendation if a
second use case shows up with a different transform shape (the
canonical "rule of three"). Path C is a parking-lot item for when
closures earn their keep across multiple node families.

### The vector-ground-truth example

```yaml
# Ground truth source (already works today via prebuffered dataset):
bindings: |
  ground_truth := neighbor_indices_at(prebuffered, q % base)

# Actual source (proposed):
result:
  keys: body_column_i32(body, "key")     # Path A — no per-element transform

evaluations:
  relevancy:
    actual: keys
    expected: ground_truth
```

If a workload needs to shard the ground-truth indices into local
indices before comparison, today's options are: (a) write a
specialised node like `body_column_i32_mod` (Path A); (b) wait for
Path B/C. The SRD records the requirement; the implementation
follows usage.

## Capture declarations: the YAML surface

The current implicit surface is fragmented:

- Bind-points: `[name]` inside stmt strings.
- Result-bindings: `result: { ... }`.
- Magic externs: implicit (the names body/count/ok).
- Adapter-direct: not declared in YAML at all.

This SRD specifies a unifying `captures:` block as the
forward-looking surface for adapter-driven captures:

```yaml
captures:
  keys: { from: body, column: "key" }
  status_code: { from: headers, name: "status" }
```

Semantics:
- The wrapper layer between adapter and ResultDispenser walks the
  `captures:` map per cycle.
- For each entry, calls `body.capture(column)` (or the appropriate
  source-specific hook) and writes the result via `ctx.wires.write`.
- The closure-binding economy walks `captures:` entries for slot
  allocation, same as it does for result-binding RHSs.

The bind-point syntax (`[name]`) stays as the inline convenience form
for the common case (single string capture from stmt result). It
desugars into a `captures:` entry at parse time.

**Migration:** existing workloads with `[name]` continue to work
unchanged. New workloads can use the explicit `captures:` block for
multi-column or typed captures.

## Open questions parked here

- **Result-binding LHS vs. regular binding LHS** in the same op-template
  scope: should it be a compile error or last-write-wins? Today it's
  undefined.
- **Adapter-direct namespace prefix.** Should adapters be required to
  prefix their direct writes (`cql_rows_inserted` vs `rows_inserted`)?
  No enforcement today; would prevent silent collisions.
- **Capture lifecycle on skipped ops.** `ConditionalDispenser` skip
  path bypasses all capture writes today. Is that the right default,
  or should skipped ops still write their declared captures with a
  sentinel value? (Argues for: stable kernel state across cycles.
  Argues against: skipped means "nothing happened.")
- **Cross-cycle persistence.** Captures landing on the op-template
  kernel persist across cycles until the next write (or the cycle
  reset). Workloads currently assume per-cycle freshness; the
  contract isn't stated. Should `set_inputs` clear capture slots, or
  preserve them?
- **Path B implementation cost.** Capture-pipeline declarations
  with `per_element` Polydat micro-programs — sketch the compiler step.

## Code references

- `crates/nmbrs-runtime/src/adapter.rs:34` — `ResultBody` trait.
- `crates/nmbrs-runtime/src/wrappers.rs:131` — `TraversingDispenser` (bind-point captures).
- `crates/nmbrs-runtime/src/wrappers.rs:~970` — `ResultDispenser` (result-bindings + magic externs).
- `crates/nmbrs-runtime/src/wires.rs:41` — `WireSource` trait (sink contract).
- `polydat/src/subcontext/builder.rs:~220` — `add_result_bindings`
  (slot allocation walker).
- `polydat/src/nodes/json.rs::BodyColumnI32` — first column-projection node.

## See also

- SRD-34 — Capture Points (pre-wires-architecture shape of source #1,
  the `[name]` bind-point syntax). This SRD-69 supersedes its
  unified-flow framing; SRD-34 remains the canonical reference for
  the `[name]` syntax itself.
- SRD-66 — `result:` block schema and magic externs.
- SRD-67 — Subcontext construction, Rule 2 write-throughs.
- SRD-68 — Dispenser-owned Polydat context, `WireSource` trait.
- `docs/guide/workload_field_contexts.md` — workload-author reference
  for field evaluation contexts.
