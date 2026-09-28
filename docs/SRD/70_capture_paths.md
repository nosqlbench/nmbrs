# SRD-70 — Capture via JSON Path Expressions (First Wave)

**Status:** Draft, 2026-05-14.
**Relationship:** Practical-shipping subset of [SRD-69 Capture
Semantics](69_capture_semantics.md). The unified
adapter-trait-extension contract, `captures:` YAML block, and
per-element-transform pipeline that SRD-69 sketches are parked as
future work. This SRD specifies what we do *now* with the
mechanisms already in place.

## Scope

Two things to nail down for the current push:

1. **Multi-row column projection from a JSON result body to a typed
   wire** — the vector-recall use case: SELECT N rows, capture
   `key` column as `VecI32`, hand to the evaluator.
2. **Wire-reference reads in `evaluations.relevancy.actual:`** —
   the evaluator reads the captured wire via `ctx.wires.get`, no
   string round-trip, same shape `expected:` already uses.

What's **explicitly out of scope** for this SRD:

- `ResultBody::capture(name)` adapter-side hook (SRD-69 §"The
  adapter-side capture hook").
- New `captures:` YAML block (SRD-69 §"Capture declarations").
- Per-element Polydat transform during column projection (SRD-69
  §"Multi-row column projection... Path B / Path C").
- Adapter-direct capture namespace convention.

We address those in a future push when there are concrete
use-case shapes that justify the additional surface.

## The mechanism

Today's `ResultDispenser` already supports path-expression
result-bindings (`crates/nmbrs-runtime/src/wrappers.rs:~720`):

```rust
enum ResultSource {
    Count,
    Ok,
    Path(Vec<PathSeg>),     // existing — single-element extraction
    GkCall(String),         // deferred
}

enum PathSeg {
    Field(String),
    Index(usize),
}
```

`parse_path_expr` handles `rows[0].field`, `rows.0.field`,
`field`, etc., and `resolve_path` walks a JSON value through the
segments, returning a single `&serde_json::Value` or `None`.

**The SRD-70 extension is additive: one new `PathSeg` variant.**

```rust
enum PathSeg {
    Field(String),
    Index(usize),
    Wildcard,        // NEW — `[*]`
}
```

And the corresponding resolve-path evolution: when `resolve_path`
hits a `Wildcard` segment, it forks into a *column-projection*
mode that collects every matching value from every element of the
current array, applies the remaining segments to each, and emits
a typed vector.

## Path-expression grammar

The full first-wave grammar:

```
path        := segment (("." segment) | ("[" index "]"))*
segment     := identifier | index | wildcard
identifier  := [a-zA-Z_][a-zA-Z0-9_]*
index       := [0-9]+
wildcard    := "*"
```

Parser-allowed shapes:

- `field` — top-level field. Single value.
- `field.nested` — nested object access. Single value.
- `rows[0].id` — indexed array access. Single value.
- `rows.0.id` — equivalent dotted form. Single value.
- `rows[*].id` — **NEW** — column projection. Returns a typed vector.
- `rows[*].nested.field` — projection with further per-element
  walk. Each row's `nested.field` collected.
- `[*]` — top-level body-as-array projection (the body IS the
  array; capture every element). Returns a typed vector.

**Disallowed for the first wave:**

- Multiple wildcards in one path (`rows[*].items[*].id`).
  Two-level nested projection is parking-lot — first use case
  drives that extension.
- Wildcard on objects (`config.*`). Not needed for the recall
  case; parking-lot.
- Conditional / predicate selectors (`rows[?key == "foo"].value`).
  jq territory; very explicitly out of scope.

## Output typing

When a wildcard appears, the resolve step collects a sequence of
JSON values (one per array element). The output type follows
uniform-element coercion:

1. If every element coerces to `i32` → `Value::VecI32`.
2. Else if every element coerces to `i64` (out-of-range for `i32`)
   → `Value::VecI32` with saturation per `json_value_as_i32` (same
   contract `body_column_i32` uses).
3. Else if every element coerces to `f64` → `Value::VecF32` via
   `as f32` cast.
4. Else if every element is a string → `Value::Str` with
   newline-joined leaves (matches `json_text` for downstream
   regex / parse paths).
5. Else (mixed types) → `Value::Str` with JSON-serialized array
   form. The legacy "stringify and let downstream parse" fallback.

Coercion rules per element are the same as
`json_value_as_i32`/`json_value_as_f32`/`json_value_as_str` —
numbers parse as-is, string-numeric forms parse via
`<T>::from_str`, bool/null/non-numeric → skip / drop the row from
the output vector.

When a wildcard appears but no rows match (empty body), the output
is the empty form of the inferred type (e.g. `Value::VecI32` with
a zero-length slice).

## Validator integration

`evaluations.relevancy.actual:` accepts:

- **Bare wire-name** (new canonical form): `actual: keys`.
  The validator reads the wire via `ctx.wires.get("keys")` at
  cycle time, expects a typed `Value::VecI32` / `Value::VecF32` /
  `Value::Str(CSV-or-bracket)` (the same shapes
  `resolve_expected_from_value` already accepts for
  `expected:`).
- **Legacy column-name** (transitional): `actual: key`.
  Continues to work via the legacy JSON walk path for one
  release. **TODO:** flag as deprecated in `--strict` mode.

The wire `keys` is populated by a path-expression result-binding
the workload author declares:

```yaml
result:
  keys: rows[*].key
evaluations:
  relevancy:
    actual: keys
    expected: ground_truth
    k: k
    r: limit
```

## Compatibility with `body_column_i32`

The `body_column_i32(body, "name")` node added under SRD-69's
draft already covers the recall use case end-to-end. SRD-70's
path-expression form is the **canonical workload-author surface**
because it stays inside the existing `result:` block grammar —
the workload author doesn't need to know about Polydat function names
to get a column-to-Vec capture.

`body_column_i32` stays as a power-user / GK-author surface for
cases where a workload wants to compose the capture inline with
other Polydat operations (e.g. `result: { weighted_keys:
weighted_pick(body_column_i32(body, "key"), some_seed) }`).
Path-expression captures and GK-function captures coexist; they're
different surface levels.

## Implementation steps

Estimate: small, contained.

1. **Add `PathSeg::Wildcard`** to the `PathSeg` enum
   (`wrappers.rs:~736`).
2. **Extend `parse_path_expr`** to recognise `[*]` and bare `*`
   between dots. Reject multi-wildcard paths with a clear
   diagnostic.
3. **Refactor `resolve_path`** into two paths:
   - `resolve_path_scalar` — current single-value walk (no
     wildcard).
   - `resolve_path_vector` — when the segment list contains a
     wildcard, fork into column collection. Returns
     `Vec<&serde_json::Value>`.
4. **Add typed-vector coercion** in `ResultDispenser::evaluate` —
   when the path has a wildcard, run the coercion ladder above
   and emit the appropriate `Value` variant.
5. **Validator change** (`validation.rs`):
   - Add `actual_wire_name: Option<String>` alongside the
     existing `actual_field`. Populate from the same `actual:`
     YAML field.
   - Cycle-time read: try `ctx.wires.get(actual_field)` first;
     if `Some(typed)`, hand to `resolve_expected_from_value`-style
     extractor. If `None`, fall back to the legacy JSON walk
     against `actual_field` as a column name.
   - Once all in-tree workloads migrate (this push), the legacy
     fallback gets removed.
6. **Migrate `full_cql_vector.yaml`** to declare:
   ```yaml
   result:
     keys: rows[*].key
   evaluations:
     relevancy:
       actual: keys
       expected: ground_truth
   ```
7. **Migrate `recall_e2e` test fixtures** to declare the same
   capture pattern in their fake bodies. The fake adapters
   return body shaped like `[{key: 1}, {key: 2}]`, the
   result-binding extracts via the new wildcard path, the
   validator reads the typed wire.

## Test plan

- **Unit:** `parse_path_expr` accepts `rows[*].key`, rejects
  `rows[*].items[*].key`.
- **Unit:** `resolve_path_vector` against `[{k:1},{k:2}]` returns
  `[Value::VecI32([1,2])]`.
- **Unit:** uniform-type coercion ladder — integers, floats,
  strings, mixed.
- **Integration:** existing recall_e2e tests pass with the new
  capture shape.
- **Integration:** workload-author shape change documented in
  `docs/guide/workload_field_contexts.md`.

## Naming-collision rules

The result-binding LHS is the wire name (e.g. `keys`). The
validator reads `keys` via `ctx.wires.get`. If a workload writes
`result: { keys: ... }` AND a regular `bindings:` block has
`keys := <something>`, that's an op-template-kernel-level
binding collision — the existing compile path surfaces it as a
duplicate-port error. Match SRD-69's open question: this should
be a clean compile error with a clear "you've defined `keys`
twice" diagnostic.

## What this SRD does not address

Repeating for clarity, all of these stay parked under SRD-69:

- Adapter-typed capture (`ResultBody::capture(name)`). Today's
  flow still goes through `to_json()` + JSON walk.
- Captures: YAML block as a unified declarative surface.
- Per-element Polydat transform during column projection (e.g.
  `rows[*].key | hash(value) % shard_count`).
- Captures landing on result bodies that aren't JSON-serializable
  (binary blobs, opaque handles).
- Multi-level wildcards.
- Skipped-op capture semantics.

## See also

- [SRD-34 — Capture Points](34_capture_points.md) — `[name]`
  bind-point form (one of the four sources in SRD-69; orthogonal
  to this SRD's path-expression form).
- [SRD-66 — Result Bindings](66_runtime_feature_detection.md)
  + result-block schema.
- [SRD-69 — Capture Semantics](69_capture_semantics.md) — unified
  contract; parked work that this SRD's first-wave shape feeds into.
- `docs/guide/workload_field_contexts.md` — workload-author
  reference.
