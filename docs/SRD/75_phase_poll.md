# SRD-75 — Phase-Level Poll

**Status:** DRAFT — design for Option A in
`docs/design/phase_poll_design.md`'s "build a synchronizer
without violating Polydat invariants" line. No code lands until
this SRD is reviewed against the Polydat invariants listed in
§"Load-bearing invariants this SRD honours."

> **⚠️ OUT OF DATE — NEEDS UPDATE TO MATCH SHIPPED CODE (2026-06-08).**
> The synchronizer example in §"Workload surface" shows `trigger_compact`
> as a *conditional regular op* (`if: …`, re-evaluated every poll
> iteration). The shipped `ensure_compacted`
> (`nmbrs/workloads/cql/full_cql_vector.yaml`) instead fires
> `trigger_compact` as a **daemon op** (SRD-79).
>
> The load-bearing correction: a daemon op does **not** "fire once at
> phase init". It dispatches **at its position in the cycle/op walk** —
> `nmbrs-runtime/src/activity.rs`'s cycle loop spawns the daemon fiber
> when the stanza walk reaches that op (pinned by the
> `daemon_op_dispatches_at_cycle_pool_position` test). So daemon ordering
> follows normal op declaration order: a regular op declared *before* a
> daemon op runs to completion first.
>
> This ordering is what lets a synchronizer phase run a prerequisite op
> (e.g. a synchronous `forceKeyspaceFlush`) to completion *before* the
> daemon compaction trigger dispatches — i.e. it makes consolidating
> flush + compact + await into a **single** phase (`finalize_index`)
> correct, not just a multi-phase scenario group.
>
> TODO when this SRD is next revised: (1) rewrite §"Workload surface" +
> §"Runner integration" around the daemon-based `trigger_compact`;
> (2) state the daemon dispatch-at-op-position semantics explicitly and
> cross-ref SRD-79; (3) fix the stale module doc in
> `nmbrs-runtime/src/daemon_pool.rs` ("spawned at phase init" → "spawned
> when the cycle-pool stanza walk reaches the daemon op"). Until then,
> the shipped `ensure_compacted` phase is the source of truth, not this
> draft's example.

**Owner:** nmbrs-workload (model), nmbrs-runtime (synthesis,
runner / executor), workloads (consumers under
`nmbrs/workloads/cql/`).

**Cross-refs:**
- [SRD-11](11_polydat_evaluation.md) §"Two Evaluation
  Lifecycles" (the predicate is dynamic).
- [SRD-13c](13c_polydat_scope_model.md) §"Mutability Rules:
  Shared Mutable" + §"Implementation: SharedCell-backed
  input slots" (captures land in shared cells on the phase
  kernel).
- [SRD-13f](13f_cross_scope_wire_materialization.md) §"Wire-
  reference classification" (synthesizer rule for capture
  names referenced from op-template kernels).
- [SRD-18b](18b_scenario_tree_and_scheduler.md) §"Single
  Walker Contract" (phase-poll is a runtime concept at
  `depth=Cycle`, not a new tree-walk modality).
- [SRD-32](32_wrappers.md) (wrapper cascade; the existing
  per-op `PollingDispenser` is the model for the new
  phase-level controller).
- [SRD-67](67_polydat_subcontext_construction.md) §"Cross-binding
  rules" Rules 1–3 (capture cells are spawn-time wirings of
  the phase scope's `shared` exports).
- [SRD-68](68_dispenser_owned_polydat_context.md) (captures flow
  through `WireSource`; no parallel resolution path).
- [SRD-69](69_capture_semantics.md) (parked; this SRD picks
  the narrowest workable shape — declarative `capture:` map
  + JSON-Pointer paths from SRD-70 / shipped in P1).
- [SRD-74](74_none_propagation.md) (predicate sees
  `Value::None` on capture-miss; predicate stays false-y
  via None propagation through `&&` / `==`).

---

## What this SRD covers

Phase-level `poll:` — a block on a `WorkloadPhase` that
loops the phase's *cycle execution* (all ops in the
phase's `ops:`, run once per iteration) until a
predicate over captures returns true, with a wall-clock
deadline as the safety net.

Driver use case: the synchronizer pattern for Cassandra
compaction, where one op reads observable state via a
Jolokia bulk POST and a second op fire-and-yields the
compact trigger conditionally on that state. Per-op
`PollingDispenser` (SRD-32) doesn't cover this — it
wraps a SINGLE op; we need the loop to wrap MULTIPLE
ops with cross-op state visibility.

The phase-poll is *not* a new scenario-tree shape (do-loop
in YAML is the scenario-tree-level loop; phase-poll is
inside one phase). It is *not* a separate walker (per
SRD-18b §"Single Walker Contract"). It is a runtime
concept at the phase's cycle-execution layer.

---

## Workload surface

```yaml
phases:
  ensure_compacted:
    cycles: 1            # outer cycle count — usually 1 for a synchronizer phase
    concurrency: 1
    poll:
      until: "sstables == 1 && active_for_cf == 0 && pending_for_cf == 0"
      interval_ms: 5000
      timeout_ms: 14400000
      max_error_retries: 3
      metric_name: ensure_compacted_wait_s
      on_timeout: abort     # workload invalidation if compaction stuck
    ops:
      read_table_state:
        adapter: http
        method: POST
        uri: "http://{host}:8778/jolokia/"
        body: '[{...}, {...}, {...}]'
        capture:
          sstables:       "/0/value"
          active_for_cf:  "/1/value:count"
          pending_for_cf: "/2/value"
        request_timeout_ms: 10000
        metrics:
          cassandra_table_sstables:        { type: gauge, value: sstables }
          cassandra_compactions_active:    { type: gauge, value: active_for_cf }
          cassandra_compactions_pending:   { type: gauge, value: pending_for_cf }
      trigger_compact:
        adapter: http
        if: "sstables > 1 && active_for_cf == 0 && pending_for_cf == 0"
        method: POST
        uri: "http://{host}:8778/jolokia/"
        body: '{...forceKeyspaceCompaction...}'
        request_timeout_ms: 30000
        on_timeout: accept     # fire-and-yield (SRD-73-style modifier; shipped in P2)
        strict: false
```

### `poll:` block fields

| Field | Required | Default | Meaning |
|---|---|---|---|
| `until` | yes | — | Polydat expression returning a boolean. Compiles into the phase scope kernel as `__poll_until := <until>`. Re-evaluated after each iteration. |
| `interval_ms` | no | `1000` | Sleep between iterations, milliseconds. |
| `timeout_ms` | no | `300000` (5 min) | Overall wall-clock cap. Loop returns `poll_timeout` error if exceeded. |
| `max_error_retries` | no | `0` (strict) | Cap on consecutive retryable inner-op errors before propagation. Mirrors per-op `PollingDispenser` semantics. |
| `metric_name` | no | `None` | Named metric (gauge in seconds, with unit-suffix decode per existing `duration_value_for_metric_name`) written via `ctx.wires.write` when the loop terminates successfully. Same contract as per-op poll. |
| `on_timeout` | no | `error` | What happens when `timeout_ms` expires without satisfying `until`. `error` = phase fails, scenario walker continues per its error-routing policy. `abort` = `session_signals::request_stop()` is called in addition; the scenario walker observes the global stop signal and terminates the whole run. Use `abort` when the predicate's satisfaction is a precondition for any downstream phase being meaningful (e.g., the synchronizer phase before query benchmarks — a stuck compaction invalidates the rest of the sweep). |

### Op-level fields used by the synchronizer pattern

- `capture:` map — declarative JSON-Pointer captures
  (shipped in P1; SRD-70 path-expr direction). Each
  entry's name is the wire that gets written via
  `TraversingDispenser`.
- `if:` — existing condition wrapper (SRD-32's IF_COND).
  Reads from `ctx.wires.get(name)` via PullHandle; sees
  captures from prior ops in the same iteration because
  captures land on the *phase scope* shared cells (see
  §"Captures are phase-scope shared wires" below).
- `on_timeout: accept` — HTTP-adapter modifier
  (shipped in P2). Converts request-timeout firings to
  empty-body success without failing the phase.

---

## Load-bearing invariants this SRD honours

Every clause below was checked against the referenced SRD
before drafting. A change that violates one of these is
wrong, not the rule.

1. **Polydat kernels are the canonical state holder**
   (SRD-13c, SRD-18b). No sidecar `HashMap<String,
   Value>` for capture storage. Captures land on the
   phase scope kernel as `shared` wires; the same kernel
   answers `__poll_until` reads, `if:` reads, and metric
   `value:` reads — all through the standard
   `WireSource` / `lookup` surface.
2. **One walker, one tree** (SRD-18b §"Single Walker
   Contract"). Phase-poll plugs into the existing
   `execute_tree_at` at `depth=Cycle` — it does NOT
   create a new walker, NOT add a "poll mode" flag, NOT
   duplicate cycle execution. The single change is
   inside `run_phase`: when the phase has `poll:`, the
   existing cycle dispatcher is invoked from a poll
   controller that decides whether to re-enter.
3. **Read invariant is uniform** (SRD-13f). The
   predicate's reads of capture names go through the
   phase kernel's standard `lookup` chain, picking up
   the latest cell value. No `with_fallback` or
   external chain composition. The wires layer takes
   one kernel handle (the phase kernel for predicate
   eval; the op-template kernel for op execution as
   today).
4. **`shared` is the write-permission flag** (SRD-13c).
   Captures are declared `shared <name>: <type>` on the
   phase scope; ops write through via the standard
   SharedCell mechanism. No new write mechanism.
5. **SRD-67 construction protocol** (SRD-67). Each
   phase scope module is built through
   `SubcontextBuilder` and spawned via
   `parent.spawn(name, module)`. Capture wires are
   declared via `builder.export(ExportSpec::shared(name,
   type))`; op-template kernels declare them as
   `builder.import(ImportSpec::shared(name, type))`
   (Rule 1: shared import → cell-attached). Predicate
   binding added via
   `builder.body(BodyFragment::GkSource("__poll_until := <until>"))`.
6. **Closure-binding economy** (SRD-67 Rule 5). Capture
   names that are never read by any op (no `if:`, no
   metric `value:`, no predicate reference) are dropped
   at spawn — no slot, no cell, no per-cycle cost. The
   synthesizer walks the predicate's free identifiers +
   each op's `if:` free identifiers + each op's metric-
   spec free identifiers and emits cells only for the
   union.
7. **Two evaluation lifecycles** (SRD-11). The
   predicate is **dynamic** — it re-evaluates per pull
   because its upstream changes per iteration (capture
   writes). `__poll_until` is NOT a `const` binding;
   the synthesizer emits it as a regular cycle-binding.
8. **TraversingDispenser writes via ctx.wires.write**
   (SRD-68 §I-1, SRD-69). The op-template kernel's
   import slot for the capture name is cell-attached
   (Rule 1: shared import); `wires.write` calls
   `state.set_input` on the slot, which writes through
   to the shared cell (SRD-13c §"Implementation:
   SharedCell-backed input slots" §4 "Write through").
   The phase kernel's slot picks up the update on next
   read (intrinsic per-read cell access; no refresh
   step).
9. **`Value::None` propagates** (SRD-74). If a capture's
   JSON-Pointer doesn't resolve (e.g. first iteration
   before any read completes, or a malformed response),
   the cell carries `Value::None`. The predicate's
   `sstables == 1 && active_for_cf == 0 && ...`
   propagates `None` through `==` and `&&`, so the
   overall predicate stays None → the loop reads it as
   "not true" → continues iterating. The
   `timeout_ms` wall-clock is the failsafe; the
   predicate doesn't need explicit "wait until first
   read" gating.

---

## Architectural shape

### Scope-tree integration

A phase with `poll:` doesn't change its scenario-tree
node kind — it's still a `Phase`. The scope tree's
`ScopeNode` for the phase scope gains additional
*synthesized matter* before its `ScopeModule` is
finalized:

- For each capture name in the union of (predicate
  free idents, op-`if:` free idents, op-metric free
  idents): `shared <name>: <inferred-type> := <zero>`
  added to the phase scope's bindings source.
- Final binding added: `__poll_until := <until>`.

Type inference for capture wires:
- If the capture's `path:` is a `:count` form → `u64`.
- Otherwise → start with `Value::None`-friendly typing;
  the engine carries whatever the runtime writes (Str /
  U64 / F64 / Bool / Json). For now we declare the cell
  as `u64` for `:count` captures and `Str` for the rest,
  with future work tracked as a type-inference
  refinement once we have multiple captures per
  workload to validate against.

The op-template scope kernels (one per op in the phase)
already import names referenced in their text via the
SRD-13f Rule 2 cascade-on-read mechanism — capture
names are no different from any other parent-scope
visible name. The cascade naturally cell-attaches when
the parent export carries `shared` (SRD-67 Rule 1
"shared import → share cell").

### Runner integration

The change is bounded to one site:
`nmbrs-runtime/src/runner.rs::run_phase` (or its current
equivalent — the function that drives a phase's cycles).
When `phase.poll.is_some()`, the existing cycle loop is
invoked from a `PollController` that:

1. Records `start = Instant::now()`.
2. Loops:
   a. Run the phase's normal cycle execution once
      (all ops, in declared order, per existing
      semantics). Catches inner-op errors per the
      `max_error_retries` budget.
   b. After cycle completion, pull `__poll_until` from
      the phase scope kernel. (Standard `lookup` —
      reads through the cell-backed slot, evaluates
      the binding against the latest capture values.)
   c. If the pulled value is `Value::Bool(true)`:
      - Write `metric_name` (if set) via
        `phase_kernel.set_input("__poll_metric_<name>", value)`
        — or, more naturally, append a
        `__metric_<name> := <metric_name>` binding at
        synthesis time and let the existing metrics
        path emit. (Implementation detail; aligns with
        SRD-40b §6.)
      - Return success.
   d. If `start.elapsed() > timeout_ms`: return
      `poll_timeout` error (mirrors per-op
      `PollingDispenser` error name + message
      structure).
   e. Sleep `interval_ms`. Loop.
3. The phase's outer `cycles: N` (if > 1) is honoured
   ABOVE the poll — each outer cycle is a fresh poll
   loop. (Most synchronizer phases will be `cycles: 1`,
   but this composes correctly without special-case.)

### Concurrency

Phase-poll is sequential within one phase activation:
the predicate's evaluation depends on a serial sequence
of capture writes. If `concurrency: > 1` is set on a
phase with `poll:`, the synthesizer ERRORS at
workload-load (per SRD-15 strict mode + the
load-bearing rule "no silent misconfiguration"). The
shape `poll: + concurrency > 1` doesn't have a
meaningful semantic; the wall-clock loop is the unit
of work, not a stream of independent cycles.

This is enforced in the workload parser when `poll:` is
detected: `concurrency` must be unset, `None`, or `1`.

### Pre-map walk visibility

Per SRD-18b §"Single Walker Contract" the pre-map walk
is the same walker at `depth=Phase`. It SEES the
`phase.poll` field and records it on the SceneTree
node for display ("polling, until: …"). No special
pre-map machinery; the structural walk runs as today.

The depth discriminant doesn't gate the poll loop —
the poll loop is at `depth=Cycle` and only runs there.
Pre-map and dryrun observe the predicate text + interval
+ timeout via the scene-tree node's annotations; they
don't execute the loop.

---

## What this is NOT

- **Not a new scenario-tree node kind.** No
  `ScenarioNode::PhasePoll`. The phase is the phase;
  `poll:` is a phase configuration field.
- **Not a new walker.** Per SRD-18b §"Single Walker
  Contract" — phase-poll is bounded to inside
  `run_phase` at `depth=Cycle`.
- **Not a wrapper.** No new entry in
  `wrapper_registrations.rs`. The wrapper cascade is
  per-op; phase-poll is per-cycle-batch (loops the
  whole cycle).
- **Not a do-loop.** Do-loops are scenario-tree
  iteration over child scenarios with a counter and
  condition; phase-poll is one phase's cycle execution
  in a wall-clock loop. They could be unified in a
  future SRD (both are "shared-kernel-for-the-whole-
  loop, predicate-driven iteration"), but the
  unification isn't in scope here.
- **Not a generalization of per-op `PollingDispenser`**
  (SRD-32). The per-op poll wraps a SINGLE op with
  row-count / json-path emptiness termination; phase-
  poll wraps MULTIPLE ops with predicate-over-captures
  termination. They coexist; per-op poll is the right
  tool when a single op's response is sufficient to
  signal completion.

---

## Workload-load validation

The synthesizer enforces:

1. **`poll.until` parses and compiles** against the
   phase scope kernel's known wire space (including
   the synthesized capture wires + any phase
   `bindings:` outputs). Unresolved identifiers fail
   per SRD-13f §"Case 4 — Unresolved → matter
   validation error".
2. **Each op's `if:` parses and compiles** against the
   same wire space. Same case-4 rule.
3. **Each capture name's referenced type matches** the
   inferred phase-scope shared cell type. If the
   predicate writes `sstables == 1` (u64 compare) but
   the capture is declared `Str`, the
   matter-validation pass surfaces the type mismatch.
4. **`concurrency`** is `None` / `1` when `poll:` is
   set. Otherwise: workload-load error.
5. **At least one op** is present. A phase with
   `poll:` but no `ops:` is a misconfiguration —
   nothing produces capture writes, nothing to do.

---

## Diagnostics

Per-iteration logging mirrors `PollingDispenser`'s
existing surface:

- INFO: phase start
- DEBUG: per-iteration "awaiting: <captured snapshot>,
  predicate=<bool>"
- INFO: phase end with elapsed wall-clock + iteration
  count

The `metric_name` (if set) lands as a gauge on the
phase's metrics component with the conventional unit-
suffix decode (`_s`, `_ms`, etc.) — same as per-op
poll's `metric_name`.

If captures emit metrics (per the workload's
`metrics:` block), those metrics carry the phase's
scope-coord labels automatically per SRD-40b §6 —
same machinery the existing per-op metrics use.

---

## Migration / shipping plan

### Push 1 — Workload model surface

- `nmbrs-workload/src/model.rs::WorkloadPhase` gains
  `pub poll: Option<PhasePollSpec>`.
- `PhasePollSpec` defined with the fields above.
- Parser accepts `poll:` block; validates the
  `concurrency` exclusion and the "at least one op"
  rule.
- Tests: parser accepts canonical shape, rejects the
  invalid combinations.

### Push 2 — Synthesizer extension

- Phase scope synthesizer (the M3.4 `build_subscope`
  call site for a phase) detects `phase.poll`, walks
  predicate / `if:` / metric-value free idents,
  declares the union as `shared <name>: <type> :=
  <zero>` and appends `__poll_until := <until>`.
- Op-template synthesizers' existing cascade-on-read
  picks up the new phase-scope shared exports
  automatically (Rule 2 cascade) — no op-side change.
- Tests: a phase scope module with `poll:` shows the
  expected exports + binding; op-template kernel
  reads land on the cell.

### Push 3 — Runner integration

- `runner.rs::run_phase` consults `phase.poll`;
  invokes the existing cycle dispatcher inside a
  `PollController` loop when set.
- `PollController` holds the predicate handle, the
  interval / timeout, the metric-name. Reads
  `__poll_until` via the standard pull surface.
- Tests: end-to-end run of a synthetic workload with
  a synchronizer phase using a testkit adapter that
  fakes capture writes per iteration; predicate
  fires after N iterations; metric emitted; loop
  exits.

### Push 4 — Documentation + workload migration

- `docs/guide/workload_field_contexts.md` adds a
  `poll:` row under the phase-level fields.
- `nmbrs/workloads/cql/full_cql_vector.yaml`
  migrates `jolokia_compact` + `jolokia_await_compaction`
  to a single `ensure_compacted` phase using the new
  pattern (was P6 in the planning task list).
- Integration coverage: workload runs end-to-end on a
  testkit-stubbed Jolokia and on a real cluster.

Each push is independently shippable; downstream pushes
gate on the prior push landing.

---

## Open questions

- **Capture-type inference granularity.** The "u64 for
  `:count`, Str for everything else" rule is a starting
  point. A workload that captures a JSON path that
  resolves to a numeric scalar without `:count`
  currently lands as Str — the predicate's `==` compare
  would coerce via Str equality. Tracked for a future
  type-inference refinement; out of scope for the
  initial ship.
- **Re-trigger nuance in the synchronizer pattern.**
  If two ops in the phase both write to overlapping
  captures (rare in practice, but possible — e.g. two
  bulk reads providing alternate views of the same
  state), the LAST write wins per the SharedCell
  last-write-wins contract (SRD-13c §"Concurrent
  semantics: last-write-wins"). Synchronizer
  workloads should structure ops so that capture
  writes don't conflict; the validator could surface
  same-name captures across ops as a warning under
  `--strict`.
- **Composition with `if:` skipping.** If an op's
  `if:` evaluates to false → op skipped → its captures
  not written → predicate may stay in the prior
  iteration's state. The synthesizer should preserve
  the prior-iteration value (cell semantics already
  do this — skipped op = no write = cell carries
  prior value), and the predicate's design must
  account for this. Documented as a workload-author
  contract.

---

## Why this isn't Option B yet

Option B — predicate evaluated via MetricsQL against
the in-process metrics store — is architecturally
cleaner (single canonical observation plane for both
the synchronizer and dashboards) but depends on
SRD-47 (streaming) + SRD-48 (continuous query) being
ship-ready as evaluators. SRD-48 is partially landed;
the runtime evaluator path for ad-hoc workload
queries is not yet a first-class surface.

Option A (this SRD) is the right initial cut: it ships
the synchronizer behavior end-to-end on existing GK
machinery, validates the workload-author surface,
generates the metric stream that Option B will later
query, and leaves the migration path open without
deferring the use case.

A subsequent SRD will specify `until_metricsql:` as a
peer of `until:` once the evaluator surface is ready.

---

## See also

- `docs/SRD/47_metricsql_streaming.md` — evaluator
  algebra Option B will sit on.
- `docs/SRD/48_metricsql_continuous_query.md` —
  runtime path for `until_metricsql:`.
- `docs/SRD/73_op_field_modifiers.md` — the
  `on_timeout: accept` modifier shipped in P2 fits
  this taxonomy (HTTP-adapter, fire-and-yield).
- `docs/SRD/70_capture_paths.md` — JSON-Pointer
  shipped in P1 provides the declarative `capture:`
  form this SRD's workload examples use.

## C5 amendment — `require:` strict-gate selectors (2026-08-05)

`poll.require: [<selector>, …]` — each `metric()`-style selector must
resolve to a registered instrument **visible to the framed
session-lifetime view the `until:` reader consumes** (an instance with
a readable point). While any selector is unresolved the gate HOLDS
(the predicate is not consulted — an unregistered family reads 0.0
silently, so a `>=` gate would pass spuriously and a `<` gate would
hang). Grace window = one smallest-cadence frame + one poll interval
from loop start; past it, an unresolved selector fails the phase with
error class `poll_require` naming the selectors — loud, never a
hang-to-timeout. Deliberately poll-only: `stop_when`'s lenient reads
stay as SRD-83 sanctions. Probe: `nmbrs-metrics::polydat_nodes::
metric_selector_resolves`. Caveat discovered in coverage: a phase
reading its OWN activity metrics from inside its poll loop sees no
fresh frames while holding — cross-phase reads (the coordination-gate
pattern) are the supported shape. Tested:
`nmbrs/tests/poll_require.rs` over
`nmbrs/examples/workloads/controls/poll_require_smoke.yaml`.
