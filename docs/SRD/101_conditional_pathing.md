# SRD-101 — Conditional Pathing: the `continue_if` Sweep Gate

**Status:** DRAFT (2026-06-29)
**Aligns with:** SRD-83 (Stop Conditions) — shares its terms (`when`, `each`) and
its mechanism (ScopedExpr predicate, two-axis `Outcome`, the `walk_stop` latch,
`PhaseOutcome` marker). `continue_if` is a member of the stop-condition family,
not a parallel structure.
**Extends:** SRD-18b (Single Walker), SRD-82 (Execution Shells / two-axis
Outcome), SRD-76 (Phase Outcome), SRD-81 (Event-Sourced Display). Instantiates the
parked "Conditional Pathing" design.

---

## 1. Problem

A sweep that iterates a comprehension (`for: "p in profile_partitions(...)"`)
sometimes needs to **stop iterating based on a property of where it is in the
iteration** — e.g. "stop the dataset-size sweep once a tier's cumulative
base-vector count would exceed `max_size`". That bound is:

- **not** a per-cycle op concern — today's `if:` gate (SRD-31) no-ops the op but
  keeps iterating every remaining tier as idle no-ops; and
- **not** a runtime-aggregate concern — an SRD-83 `stop_when` reads `op_count` /
  `error_rate` / `elapsed_ms`, and the predicate that *can* read the iteration
  coordinate (`end_of(p)`) is phase-scoped (stops only its own phase), while the
  one that *halts the walk* runs at the root shell where the coordinate does not
  exist. No single `stop_when` does both.

It is a **structural property of the iteration's coordinates**, knowable the
moment the iteration binds, and it belongs on the node — evaluated by the walker
as it decides whether to enter each iteration's body. It is the third member of
the gating family:

| construct | level | evaluated | reads | on trip |
|---|---|---|---|---|
| `if:` (SRD-31) | op | per **cycle** | op / cycle wires | no-op the op (keep iterating) |
| `stop_when:` (SRD-83) | phase / workload | per **tick** | runtime aggregates | stop phase / fail run |
| **`continue_if:`** (this SRD) | **shell node** | by the **walker, per iteration, pre-entry** | the **coordinate context** (comprehension / phase coordinates) | **skip the body; halt the sweep at `each`, gracefully** |

---

## 2. Alignment with stop conditions

`continue_if` reuses SRD-83's vocabulary and machinery; it differs only in the
three ways that constitute its thrust.

| | `stop_when` (SRD-83) | `continue_if` (this SRD) |
|---|---|---|
| predicate field | `when:` | `when:` *(shared)* |
| scope field | `each:` (`ScopeLevel`) | `each:` *(shared)* |
| predicate compile | `ScopedExpr` (`stop_conditions.rs`) | `ScopedExpr` *(shared)* |
| halt mechanism | `walk_stop` latch | `walk_stop` latch *(shared)* |
| outcome | two-axis `Outcome` | two-axis `Outcome` *(shared)* |
| marker | `PhaseOutcome` (SRD-76) | `PhaseOutcome` (SRD-76) *(shared)* |
| **polarity** | stop **when true** | continue **while true** (stop when false) |
| **evaluation** | per **tick** (during the body) | **pre-entry**, per iteration (before the body) |
| **context** | runtime-aggregate wires | **coordinate context** (§5) |
| **effect** | `stop` \| `fail` | always graceful `stop` (Interrupted+Succeeded) |

`continue_if` is the graceful pre-entry gate; the *fail* case (mark the run
failed on a condition) remains `stop_when … effect: fail`. Keeping `continue_if`
graceful-only preserves the clean split — both are the same machinery in
different roles, not two ways to spell one thing.

---

## 3. Surface

```yaml
# short form — predicate only; each defaults to `scenario`
continue_if: "end_of(p) <= effective_max_size"

# long form — explicit scope
continue_if:
  when: "end_of(p) <= effective_max_size"
  each: scenario        # self | phase | scenario | workload   (default: scenario)
```

- **`when`** — a polydat predicate over the coordinate context (§5). polydat
  predicates use bitwise `&` (no `&&`), exactly as `if:` and `poll: until:` do.
- **`each`** — the `ScopeLevel` whose sweep ends when the predicate goes false,
  reusing SRD-83's `each`. **Default `scenario`** (the enclosing comprehension
  scope). The halt **propagates up the scope tree to the specified level**:
  `each: workload` halts the whole run; `each: scenario` halts the nearest
  enclosing scenario sweep and the walk above it continues.

`continue_if` is **not** restricted to comprehension nodes (§5): it is a gate on
any shell node, expressed purely through the uniform expression context.

---

## 4. Semantics

`continue_if` is a **pre-entry gate** — an if-then on the node body, **not** a
do-while:

1. The walker reaches the iteration and binds its coordinates.
2. **Before the node body runs**, it evaluates `when` against the coordinate
   context (§5).
3. **True** → the body executes normally.
4. **False** → the body is **not executed** (the tripping node never runs), the
   sweep at `each` ends with `Outcome = Interrupted + Succeeded` (SRD-82, exit
   0), and a `PhaseOutcome` marker is recorded (§6).

Because it is a *gate*, the tripping iteration does no work; because the halt
latches the `each` scope, **no later iteration is dispatched**. For an ascending
sweep this is exactly "stop before the first tier that would exceed the cap, and
everything after it."

**Single Walker (SRD-18b).** `continue_if` is an *executional* gate evaluated at
depth ≥ `Op`, in the one walker path that gates all execution — no second walker,
no mode flag. The *structural* walk is unchanged: the scene node for the tripping
iteration is still pushed (structural work always happens) and flagged so the
display renders it as the cap point. At structural-only depth (pre-map /
`dryrun=phase`) the gate is a no-op, consistent with "structural always,
executional gated".

**Concurrency — algebraically uniform, no special case.** On a sequential
comprehension the gate is exact. On a concurrent comprehension (`Bounded(N>1)`),
iterations already in flight when the gate trips run to completion (they are not
interrupted mid-cycle); the halt stops *dispatching new* iterations. This is the
same drain-on-`walk_stop` behavior every stop already has — `continue_if` adds no
restriction. `when` is side-effect-free (it reads coordinates), so concurrent
evaluation is well-defined.

---

## 5. The coordinate context (uniform, unspecialized)

`continue_if` reaches the comprehension/scope **only** through the ordinary
expression-language context — the same resolver used for every other config
expression, with **no comprehension- or phase-specialized syntax**. The
expression language is unspecialized around comprehensions and phase scopes; it
simply resolves names against whatever the context exposes at the evaluation
point.

At the gate's pre-entry evaluation point the context is a **basic coordinate
context**:

- the **comprehension coordinates** — the bound iteration value(s) (`p`) and
  their pure accessors (`end_of(p)`, `start_of(p)`, `cardinality(p)`,
  `idx_of(p)`);
- the **phase / scope coordinate path** — the node's position in the scene tree;
- **outer-scope bindings already resolved** (consts such as
  `effective_max_size`).

It deliberately does **not** expose the gated node's own *inner* phase context —
that section is precisely what the gate decides whether to enter, so depending on
it would be circular. The gate sees *coordinates*, not the body's bindings.

Referencing a phase-scope coordinate must be **first-class and ergonomic** — a
condition author writes `end_of(p)` / `idx_of(p)` (or the coordinate-path
accessor) directly, with no boilerplate and no awareness of how the iteration is
wired. Mechanically the predicate is a `ScopedExpr` (the SRD-83 machinery) bound
to this coordinate context instead of to runtime-aggregate externs;
`stop_when` and `continue_if` are the same expression facility resolving
different values at different evaluation points.

---

## 6. Walker integration (single walker — no new path)

All inside the existing `execute_tree_at` (`nmbrs-runtime/src/executor.rs`):

```
execute_tree_at (executor.rs:825)            ── THE walker (SRD-18b)
  └─ dispatch_comprehension (executor.rs:2034)
       ├─ CountedSource loop (executor.rs:2118-2143)
       │    └─ should_stop() break (executor.rs:2140)   ── existing latch
       ├─ per-iteration scene push (executor.rs:2148)   ── structural, always
       └─ run_one_iteration (executor.rs:3140)
            ├─ bind iteration coordinates (line 3148)
            ├─ scope_enter (line 3160)
            ├─ ►► continue_if evaluated HERE — pre-entry, before terminal ◄◄
            └─ terminal: phase / children (line 3198)
```

- **Predicate is precompiled, not per-iteration.** `continue_if` compiles to an
  `Arc<ScopedExpr>` once at step materialization (`runtime_iterate`,
  executor.rs:~1918), carried on `IterationStep` (executor.rs:~1888). Per
  iteration we only *evaluate* — never recompile.
- **Halt** reuses `WorkloadShell::walk_stop` / `should_stop()`
  (`workload_shell.rs:81,182`) and the clean break the loop already honors at
  executor.rs:2140. `each` selects which scope's latch is set (the per-scope
  latch the daemon-stop mechanism already establishes per `run_scenario_body`);
  the halt propagates up to that level.
- **Marker** is a `PhaseOutcome` (§6 below), recorded via the existing outcome
  path — no parallel event type.

---

## 7. Marker — via `PhaseOutcome` (SRD-76), not a parallel structure

The halt is recorded as the sweep scope's `PhaseOutcome` with disposition
`Interrupted + Succeeded` and a reason carrying the **actual evaluated values**:

```
PhaseOutcome { disposition: Interrupted, validity: Succeeded,
               reason: "continue_if: end_of(p)=5000000 > effective_max_size=1000000" }
```

This flows through the same `PhaseOutcome` → sqlite/readout → replay path
(SRD-76) and the event-sourced display (SRD-81); the `StringSink` renders
`sweep halted at scenario: end_of(p)=5000000 > effective_max_size=1000000`, the
TUI shows it in the phase-outcome detail block. It is distinguishable from a
`stop_when` trip by the `continue_if:` reason prefix, while reusing the exact
outcome taxonomy. No new `EventType` / checkpoint variant is introduced.

---

## 8. First consumer — the incremental compaction sweep (out of tree)

Replace the three per-op `if: "end_of(p) <= effective_max_size"` gates with one
`continue_if` on the tier-sweep node:

```yaml
# on the for-bearing tier sweep (p in profile_partitions(...))
continue_if: "end_of(p) <= effective_max_size"   # each: scenario (the tier loop)
```

`effective_max_size` already defaults to the dataset's full base-vector count
(SRD-100 follow-up), so by default the sweep runs every tier; a smaller
`max_size` halts the sweep — gracefully, before the over-budget tier runs, with a
recorded `PhaseOutcome` — instead of no-op-iterating the tail.

---

## 9. Decisions (resolved 2026-06-29)

1. **`each` default `scenario`**, propagating up to the specified level
   (`workload` to halt the whole run). (Surface §3.)
2. **Not restricted to comprehension nodes.** The gate reaches state only through
   the uniform expression context (§5); the expression language is unspecialized
   around comprehensions/phase scopes. No "must be inside a comprehension"
   compile error.
3. **`PhaseOutcome` is the marker** (§7) — no parallel `SweepHalted` event.
4. **No concurrency restriction** — drain-in-flight is the uniform, correct
   behavior shared with every stop (§4), not a special case.
