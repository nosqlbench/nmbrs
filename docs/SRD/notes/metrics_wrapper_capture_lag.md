# Metrics-wrapper capture-lag: ordering between wrapper stack and kernel write

**Status**: open design problem.
**Task**: #372.
**Affected paths**: `crates/nmbrs-runtime/src/wrappers.rs` (MetricsDispenser /
ResultDispenser), `crates/nmbrs-runtime/src/activity.rs:2235-2396` (per-cycle
execution sequence).

## The user-visible symptom

A workload writes:

```yaml
ann_query:
  ops:
    select_ann:
      prepared: "SELECT key FROM {table} ANN OF {q} LIMIT {limit}"
      result:
        row_count: count
      metrics:
        rows_per_op:
          kind: gauge
          value: row_count       # ← references a result-binding name
```

`row_count` is computed by the `result:` block from the inner adapter's
`OpResult.body`. The metrics declaration projects it to a gauge. But at
runtime the gauge reads `Value::None` (cycle 0) or the *previous* cycle's
stale value — never the current cycle's row_count. `audit.log` (driven by
`log_info` in a result-binding) shows the right value, so the data exists;
it just doesn't reach the metric.

Workaround: declare metric `value:` expressions in terms of bare GK
identifiers the cycle's kernel resolves directly (workload params,
iter-vars, magic externs) — NOT names produced inside the `result:` block.
That covers most metric workloads. The gap only bites when the desired
metric value is specifically computed in the result block.

## The wrapper stack at wrap time

The compile-time stack is a nested onion. Each wrapper holds an
`Arc<dyn OpDispenser>` of the layer below. `execute()` propagates
outermost→innermost, then results propagate back.

```text
┌─────────────────────────────────────────────────────────────┐
│ MetricsDispenser           ← outermost wrapper              │
│  ┌───────────────────────────────────────────────────────┐  │
│  │ ResultDispenser                                       │  │
│  │  ┌─────────────────────────────────────────────────┐  │  │
│  │  │ ConditionalDispenser (optional)                 │  │  │
│  │  │  ┌───────────────────────────────────────────┐  │  │  │
│  │  │  │ adapter (CQL / stdout / …)   ← innermost  │  │  │  │
│  │  │  └───────────────────────────────────────────┘  │  │  │
│  │  └─────────────────────────────────────────────────┘  │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## The cycle-time sequence — where the asymmetry lives

Time flows downward. `═════════` is the boundary between "inside
`dispenser.execute()`" and "back in the activity loop."

```text
┌─────────────────────────────────────────────────────────────────┐
│  STEP 1.  activity.rs:2243   pulls = fiber.resolve_pulls_for_idx│
│           ───────────────                                       │
│           SNAPSHOT of per-op kernel state — captured ONCE, used │
│           by every wrapper for the entire cycle. Frozen here.   │
│           At this moment, `body` / `count` / `ok` / any         │
│           result-binding wires are EMPTY (Value::None on cycle  │
│           0, or last cycle's stale values).                     │
└─────────────────────────────────────────────────────────────────┘
                                ▼
   ═════════════ dispenser.execute() begins ═════════════════════
                                ▼
┌─────────────────────────────────────────────────────────────────┐
│  MetricsDispenser::execute()                                    │
│   │                                                             │
│   │   let result = self.inner.execute(...).await?;              │
│   ▼                                                             │
│   ┌─────────────────────────────────────────────────────────┐   │
│   │ ResultDispenser::execute()                              │   │
│   │  │                                                      │   │
│   │  │   let result = inner.execute(...).await?;            │   │
│   │  ▼                                                      │   │
│   │  ┌──────────────────────────────────────────────────┐   │   │
│   │  │ adapter::execute() — runs the CQL stmt           │   │   │
│   │  │ Returns OpResult { body: Some(...), captures: {} │   │   │
│   │  └──────────────────────────────────────────────────┘   │   │
│   │  ▲                                                      │   │
│   │  Writes magic externs into result.captures:             │   │
│   │      result.captures["body"]  = Json(...)               │   │
│   │      result.captures["count"] = U64(N)                  │   │
│   │      result.captures["ok"]    = Bool(true)              │   │
│   │  PLUS user-declared captures from path-exprs etc.       │   │
│   │  ───►  THE KERNEL DOES NOT KNOW ABOUT THIS YET.         │   │
│   └─────────────────────────────────────────────────────────┘   │
│   ▲                                                             │
│   for slot in &self.slots {                                     │
│       let value = ctx.pulls.get(slot.pull_handle);    ◄───┐     │
│       //         ──────────                               │     │
│       // ❌ Reads the FROZEN snapshot from step 1.        │     │
│       // For `value: <result_binding>` this is stale.     │     │
│       slot.instrument.record(value);                      │     │
│   }                                                       │     │
│   Ok(result)  // captures still in OpResult only          │     │
└───────────────────────────────────────────────────────────┼─────┘
                                ▼                          │
   ═════════════ dispenser.execute() returns ══════════════│══════
                                ▼                          │
┌──────────────────────────────────────────────────────────┼──────┐
│  STEP 3.  activity.rs:2371                               │      │
│           ─────────────                                  │      │
│           for (name, value) in captures {                │      │
│               fiber.write_op_template_input_for_idx(     │      │
│                   template_idx, &name, value);           │      │
│               //  ──────────────────────────────         │      │
│               // ✅ Captures land in the kernel here.    │      │
│           }                                              │      │
│                                                          │      │
│  STEP 4.  activity.rs:2396                               │      │
│           ─────────────                                  │      │
│           fiber.pull_all_op_template_outputs_for_idx(    │      │
│               template_idx);                             │      │
│           // ✅ Result-binding compute chains evaluate.  │      │
│           //    log_info / log_debug / shared := …       │      │
│           //    write-throughs fire here.                │      │
│           //                                             │      │
│           //    AUDIT.LOG WORKS — because log_info       │      │
│           //    runs at step 4, after the kernel has     │      │
│           //    the values.                              │      │
│           //                                             │      │
│           //    METRICS DOESN'T — because it already     │      │
│           //    ran at step 2 against frozen pulls.   ◄──┘      │
└─────────────────────────────────────────────────────────────────┘
```

## The asymmetry in one sentence

> Result-binding Polydat code runs at **step 4** (after kernel-write), so
> `log_info(...)` in a result-binding sees fresh captures. The metrics
> wrapper runs at **step 2** (inside the stack), so its
> `ctx.pulls.get(...)` sees the snapshot from step 1.

## Three fix options

### (a) ResultDispenser writes through to the kernel mid-execute

```rust
// in ResultDispenser::execute, after computing each capture value:
result.captures.insert(name.clone(), value.clone());
cycle_wires.write(name, value);   // NEW — write through to kernel
```

Trade-offs:

- Requires giving wrappers a kernel-write API they don't currently have.
- `ctx.pulls` is a frozen snapshot from step 1; even with kernel writes,
  the MetricsDispenser would have to re-read via `ctx.wires` (the live
  kernel handle) rather than the snapshot — or the snapshot has to support
  mid-cycle invalidation.
- Couples ResultDispenser to the kernel write contract; if a future wrapper
  introduces a different write path, both implementations have to stay in
  sync.

### (b) MetricsDispenser falls back to OpResult.captures when kernel is empty

```rust
let value = ctx.pulls.get(slot.pull_handle);
if matches!(value, Value::None) {
    if let Some(v) = result.captures.get(&slot.value_expr) {
        value = v;
    }
}
```

**Rejected.** The project's "GK Is Canonical Scope" rule
(`feedback_polydat_canonical_scope` memory) forbids sidecar lookup paths.
Adding a captures-map fallback accumulates as design debt: each future
caller has to decide whether to consult the kernel, the captures, or both,
and the rules drift apart. The kernel is the one resolution surface.

### (c) Hoist metrics out of the wrapper stack — run after step 4

```text
┌────────────────────────────────────────────────────────────┐
│ STEP 1:  pulls = fiber.resolve_pulls_for_idx(...)          │
│ STEP 2:  dispenser.execute()   ← no MetricsDispenser here  │
│           ResultDispenser → Conditional → adapter          │
│ STEP 3:  fiber.write_op_template_input_for_idx(captures)   │
│ STEP 4:  fiber.pull_all_op_template_outputs_for_idx(...)   │
│ STEP 5:  NEW — for each metric_slot {                      │
│              v = cycle_wires.read(slot.handle);            │
│              slot.instrument.record(v);                    │
│           }                                                │
└────────────────────────────────────────────────────────────┘
```

The MetricsDispenser stops implementing `OpDispenser::execute` per-cycle
and instead exposes a `record_cycle(ctx)` the activity loop calls after
step 4. Wrap-time work (instrument registration, pull-handle binding) stays
where it is; only the per-cycle record method moves.

Trade-offs:

- One new line in the activity loop, one new method on the dispenser.
- Loses composability with other wrappers via the wrapper-resolver — but
  MetricsDispenser is always outermost by convention, so the loss is
  notional.
- Read goes through the live kernel via `ctx.wires`, matching the
  "GK Is Canonical Scope" rule cleanly.

## Recommendation

Option (c). The bug is a wiring problem, not an evaluation problem — the
metric values are computed correctly, they just get read at the wrong
moment. Moving the read to the right moment is the minimum correct change.
Options (a) and (b) both require new API surface (kernel-write contract
for wrappers, or fallback lookup) that doesn't pay back beyond fixing this
one bug.

## Code references

- Wrapper stack: `crates/nmbrs-runtime/src/wrappers.rs`
  - `MetricsDispenser` impl + execute: line 1087, 1361
  - `ResultDispenser` impl + magic-extern writes: line 669, 1005
- Per-cycle sequence: `crates/nmbrs-runtime/src/activity.rs`
  - Step 1 (pulls snapshot): line 2243
  - Step 2 (dispenser.execute): line 2275
  - Step 3 (captures → kernel): line 2369-2382
  - Step 4 (pull result-binding chains): line 2396
- Memory rule: `feedback_polydat_canonical_scope.md`
