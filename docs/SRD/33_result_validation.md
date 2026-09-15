# 33: Result Validation and Recall Measurement

Validation verifies operation results and computes information
retrieval metrics. Implemented as a composable dispenser wrapper
with zero overhead when not declared.

**Status:** Core implementation complete and verified (recall@100
= 0.94 on the 1.2M-vector, 25-dim reference dataset). Relevancy should be refactored into
a pluggable validator (see sysref 32 design note).

---

## Relevancy Functions

Pure compute functions for vector/ANN search quality:

| Function | Formula | Description |
|----------|---------|-------------|
| `recall@k` | \|relevant ∩ actual\| / k | Fraction of ground truth found |
| `precision@k` | \|relevant ∩ actual\| / \|actual\| | Fraction of results that are relevant |
| `f1@k` | 2·(R·P)/(R+P) | Harmonic mean of recall and precision |
| `reciprocal_rank` | 1/(first relevant position + 1) | How quickly a relevant result appears |
| `average_precision` | Mean precision at each relevant position | Order-sensitive quality metric |

All operate on sorted `i64` slices. Set intersection uses
two-pointer O(n+m) scan with no allocation.

---

## Workload Declaration

```yaml
ops:
  select_ann:
    prepared: "SELECT key FROM vectors ORDER BY v ANN OF {query} LIMIT {k}"
    relevancy:
      actual: key              # column name in result rows
      expected: "{ground_truth}" # Polydat binding for neighbor indices
      k: 100
      functions:
        - recall
        - precision
        - f1
```

`relevancy:` is routed to `ParsedOp.params` by the parser (it's
in the `activity_params` list). The adapter never sees it.

---

## Ground Truth Flow

```
GK bindings:
  ground_truth := neighbor_indices_at(cycle, "{dataset}")

    ↓ (compiled because relevancy.expected references it)

Bind-point scanner walks the full op template (op fields + params),
sees `{ground_truth}` inside `relevancy.expected`, and adds it to
the kernel's required-outputs set (SRD 13c §"Auto-Extern Generation").
    ↓
ValidatingDispenser::fixture(template, &mut scope_fixture)
    → scope_fixture.register_pull("ground_truth")
    → returns PullHandle, stored on the dispenser
    ↓
At cycle time:
    PullPlan::resolve(state) → ResolvedPulls
    ↓
ValidatingDispenser reads ground_truth via stored PullHandle:
    let gt = ctx.pulls.get(self.expected_handle);
```

**Key**: `ground_truth` is NOT an op field. It's a Polydat binding
needed only by validation. The validation wrapper self-registers
the name with the scope fixture at init, receives a `PullHandle`,
and reads from `ResolvedPulls` at cycle time. There is no
"extras" side channel — see SRD 32 §"Init-Time Fixture and
Consumer Self-Registration" for the contract.

**Missing ground truth is a hard error**, not a silent zero:
```
error: [op] [relevancy_error] relevancy: no ground truth for
'ground_truth'. Available fields: ["prepared"]. Ensure the
binding exists in the Polydat program.
```

---

## Result Extraction

The engine provides two access modes for reading an op's
result (and, by the same rules, an op template, an op
dispenser, or any product an op produces). Both modes are
always valid; the rules below decide which to prefer.

### Universal JSON access

Every `ResultBody` renders as JSON via `to_json()`. The JSON
projection is the **default access mode** for validation,
diagnostics, and any consumer that doesn't have a
performance-critical hot path:

- Validation assertions read via JSON paths
  (`json_field_as_i64` / `json_field_as_str`).
- Diagnostics (`--explain`, stdout adapter, test harnesses)
  consume the same JSON.
- Captures whose consumers don't know the adapter-specific
  type go through JSON too.

JSON access is uniform across adapters — no adapter-specific
branches in validation code — and its cost is negligible for
the cold path where it's used.

```rust
fn extract_indices_from_json(json: &Value, field: &str) -> Vec<i64> {
    // Array of row objects → extract field from each
    // json_field_as_i64: tries as_i64(), then as_str().parse()
}
```

The `json_field_as_i64` coercion handles text columns containing
numeric keys (e.g., CQL `text` key `"544844"` → `i64 544844`).
Without this, string keys would silently produce empty vectors.

### Typed accessors / traversers for hot paths

Some readers operate on the per-cycle hot path and can't
afford a round-trip through JSON. The canonical example is a
stateful cursor over CQL rows: the row iterator is already
live, each row exposes columns by native type, and
re-serializing to JSON per row would dominate the op's cost.
For these cases, `ResultBody` exposes **typed accessors** or
**traversers** the consumer may use instead of `to_json()`:

- Downcast fast path (`as_any().downcast_ref::<CqlResultBody>()`)
  for consumers that know the concrete adapter type and want
  zero-copy column access.
- Adapter-provided traverser protocols (e.g. row-by-row
  iterators with typed column readers) when the result is
  streamed and a whole-structure JSON view would be
  materializing data the consumer never needs.

Typed access is an **optimization**, not a semantics change.
Anything the typed path can read, the JSON path can also
read; the hot-path consumer picks the faster route.

### When to use which

- **Validation, assertions, predicates, cold paths.** Use
  JSON. No adapter-specific code in validation means a new
  adapter is trivially validatable the day it ships.
- **Captures with a known static target type.** If the
  capture's destination is typed (e.g. feeding a CQL prepared
  statement's typed parameter on the next cycle), use a typed
  accessor; it's both faster and more precise.
- **Captures whose destination is a Polydat wire.** The wire's
  declared type decides: `String → String` captures go
  through the typed accessor if the adapter exposes one,
  otherwise through `to_json()` and a string render.
- **Streaming traversal over large results.** Use the
  adapter's traverser. Materializing the whole structure to
  JSON defeats the purpose of streaming.

The rule of thumb: **JSON is the universal language;
typed access is the fast path you opt into when the cost of
the universal path shows up in a profile.** Every reader
that needs the fast path explicitly asks for it; readers
that don't know or don't care get JSON and it works.

---

## Metrics

### HDR Histograms

Relevancy scores in [0.0, 1.0] stored as [0, 10000] in HDR
histograms (4 decimal place fixed point). Reported as:

```
recall@100: mean=0.9385 p50=0.9503 p99=1.0000 min=0.7800 max=1.0000 (n=100)
precision@100: mean=0.9385 p50=0.9503 ...
f1@100: mean=0.9385 ...
```

### Pass/Fail Counters

```
validation: 100 passed, 0 failed
```

Printed at end of activity run. Assertion failures count as
failures. Relevancy computation (even with low scores) counts
as a pass.

---

## Assertion Failures

Assertion failures are measurements, not execution errors:

| Outcome | Counter | Behavior |
|---------|---------|----------|
| Op success, verify pass | passed++ | Normal |
| Op success, verify fail | failed++ | Count, continue |
| Op error | (error path) | Skip validation |

With `strict: true`, assertion failures become
`ExecutionError::Op` for correctness regression testing.

---

## Verified Results

Tested end-to-end against Cassandra 5.0 SAI:
- Dataset: the 25-dim public reference set (1,183,514 training vectors)
- recall@100 = 0.94 mean across 100 ANN queries
- reciprocal_rank@100 = 1.00 (first result always relevant)
