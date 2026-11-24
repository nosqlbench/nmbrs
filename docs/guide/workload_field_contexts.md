# Workload field evaluation contexts

A workload YAML mixes several distinct evaluation contexts. Most
fields look the same on the page — a string with maybe a few
`{name}` placeholders — but the same syntax means different things
depending on which field it appears under. This page is the
canonical reference for "given this YAML, what's the runtime
contract?"

## Two axes

Every workload field falls on two axes:

**Axis 1 — Source of value:**
- *Literal* — the YAML text IS the value (e.g. `kind: gauge`,
  `concurrency: 100`).
- *Wire reference* — a name the Polydat kernel resolves at cycle time
  (e.g. `expected: ground_truth`).
- *Polydat expression* — a full expression evaluated at cycle time
  (e.g. `value: count + 1`).

**Axis 2 — Type of value:**
- *String-via-template* — the field produces a string built by
  interpolating `{name}` placeholders (e.g. `prepared: "SELECT
  key FROM {table}"`).
- *Typed value* — the field produces a typed `Value` (number,
  bool, vector, json) read straight off the kernel (e.g.
  `expected: ground_truth` → typed `vec_i32`).

A field's *schema* pins which combination applies. The YAML
itself doesn't carry a marker — the field name does.

## The sigils

Inside a *string-via-template* field:

- `{name}` — replaced by `wires.get(name).to_display_string()`.
- `{{ expr }}` — evaluates `expr` as a Polydat expression, then
  stringifies the result.
- A *pure-token* field (the entire content is `{name}` with no
  surrounding text) preserves the typed value — useful for
  binding typed params like vectors to adapter ops without
  CSV stringification.
- Bare braces inside CQL maps or JSON object literals (`{'k':
  'v'}`, `{"a": 1}`) pass through verbatim.

Inside a *GK-expression* or *wire-reference* field:

- Bare identifiers are Polydat wire names.
- Strings need `"..."` (GK syntax).
- Polydat's own string templating (`"id_" + format_u64(cycle, 10)`)
  produces typed `Value::Str`.

## Field-by-field reference

| Field | Source | Type | Example |
|---|---|---|---|
| `bindings:` (string or map sugar) | Polydat source block | per-statement | `cursor q = range(0, n)` |
| `result:` (string or map sugar) | Polydat source (`count`/`ok`/calls) or path expression | per-statement | `row_count := count`, `keys: rows[*].key` |
| `metrics.value:` | Polydat expression | typed numeric | `count + 1` |
| `if:` | Polydat expression | typed bool | `cycle > 0` |
| `evaluations.relevancy.expected:` | wire reference | typed (vec_i32 / vec_f32 / str) | `ground_truth` |
| `evaluations.relevancy.actual:` | result-binding wire (SRD-109) or legacy result-body column | wire name / column name | `keys` (projected), `key` (legacy) |
| `evaluations.relevancy.k:` / `.r:` | literal int OR wire reference (bare or `"{name}"`) | usize | `100`, `k`, or `"{k}"` |
| `evaluations.relevancy.functions:` | literal list of strings | `Vec<RelevancyFn>` | `[recall]` |
| `stmt:` / `prepared:` / `uri:` / op fields | text-template | string (or pure-token typed) | `"SELECT … {q} LIMIT {k}"` |
| `delay:` | literal duration | `Duration` | `"100ms"` |
| `concurrency:`, `cycles:`, `rate:`, … | literal scalar (or `"{param}"` text-template) | u64 / f64 | `100` |
| `kind:`, `unit:`, `format:` (metric) | literal enum / string | structural | `gauge` |

## Map-form sugar for Polydat text blocks

Any field that accepts a Polydat text block accepts either form:

```yaml
# String form — raw Polydat source.
bindings: |
  cursor q = range(0, n)
  query := query_at(prebuffered, q)

# Map form — sugar that desugars to one `name := source` line per entry.
bindings:
  query: query_at(prebuffered, q)
  cursor_q: range(0, n)
```

Both compile to the same kernel. Use whichever reads more naturally
for the entries at hand. Same rule for `result:` — with one
addition there: a map entry whose source is a PATH EXPRESSION
(`keys: rows[*].key`, `first: rows[0].id`) is not Polydat source;
it is evaluated against the result body by the result wrapper
each cycle. The SRD-70 `[*]` wildcard projects a column across
every row into a typed vector (at the `results:` interface type
when the op declares one — see SRD-109 Part 3 — else inferred:
all-integer → `vec_i64`, all-float → `vec_f64`, mixed → `json`).
One wildcard per path.

## Use sites trigger slot allocation

When a GK-expression or wire-reference field names a wire, that's a
*use site*. The op-template synthesiser walks every use site to
decide what input slots to allocate on the kernel:

- Result-binding RHSs (`result: { foo: count }`) — the magic-extern
  walker injects `extern count: u64` if any RHS references it.
- Metric-value expressions (`metrics.value: count + 1`) — same
  walker, same magic-extern injection. So `metrics: rows_per_op:
  { value: count }` works without a throw-away `result:` block.
- Evaluator inputs (`expected: ground_truth`) — validated at
  wrap-time; the wire must be declared somewhere in the
  op-template kernel's scope.

This is why you don't need a "force-allocate" mode for the
common cases — just referencing a magic extern in a use site is
enough to wedge its slot open.

## Quick reference: when to use which sigil

- "I want to embed a wire value as a string fragment" → text-template
  `{name}` inside a stmt / prepared / uri field.
- "I want to bind a typed value (vector, json) to an adapter param" →
  pure-token form, the entire field value is `{name}`.
- "I want a typed value read straight off the kernel for an evaluator
  / metric / conditional" → bare wire name, no braces.
- "I want a one-shot Polydat computation inline" → `{{ expr }}` inside a
  string-template field, or write a Polydat expression directly in a
  GK-expression field.

## What changed (post-SRD-68 follow-up, 2026-05-14)

- `metrics.value:` accepts arbitrary Polydat expressions, not just bare
  binding names. The op-template synthesiser compiles each
  expression as `__metric_<name> := <expr>` so the magic-extern
  walker sees its identifier references.
- `evaluations.relevancy.expected:` reads through `ctx.wires.get`
  (typed, snapshot-free). The `"{name}"` text-template form still
  works (braces stripped) but the bare `name` form is canonical.
- `evaluations.relevancy.k:` and `.r:` accept bare wire-names
  (e.g. `k: k`, `r: limit`) in addition to integer literals and
  the legacy `"{name}"` text-template form. The value is resolved
  once at wrap-time against the canonical kernel — `k` and `r`
  are phase-constants by contract.
- The wrapper stack's `ctx.wires.write` / `ctx.wires.get` replace
  the prior HashMap-of-captures intermediary across
  ResultDispenser, MetricsDispenser, TraversingDispenser, and
  ValidatingDispenser.
