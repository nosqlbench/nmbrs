# SRD-83 — Stop Conditions (Predicate-Triggered, Daemon-Scheduled)

**Status:** DRAFT — the stop-condition system for execution shells.
**Orthogonal to** SRD-82's error handling: error handling decides what
to do with an individual error; stop conditions decide whether to stop
a shell based on predicates over its accumulated state. Neither
subsumes the other.

**Owner:** nmbrs-runtime (executor shells, daemon scheduler, the
runtime-state wires), polydat (predicate compilation against shell
state), nmbrs-workload (the `stop_when:` configuration surface).

**Cross-refs:**
- [SRD-82](82_uniform_execution_shells.md) — Execution shells, the
  two-axis `Outcome`, and the cooperative abort mechanism
  (`stop_flag` / `StopCause`). A stop condition's *effect* is an
  `Outcome` and its enforcement is SRD-82 Part 4's abort. SRD-83
  supplies the *decision* layer that triggers it; it replaces the
  `AggregateGuard`-as-error-handler stopgap.
- [SRD-18b](18b_scenario_tree_and_scheduler.md) — One polydat kernel
  per scope node. Stop-condition predicates compile in the shell's
  own scope and read its runtime-state wires.
- [SRD-11](11_polydat_evaluation.md) — Const vs dynamic lifecycles.
  Runtime-state wires (`op_count`, `error_rate`, `elapsed_ms`) are
  volatile externs the executor injects, like the `phase_start` /
  `now_ms` pattern.
- [SRD-02](02_concurrency_model.md) — Daemon timers are async tasks;
  no blocking primitives in the firing path.
- [SRD-23](23_dynamic_controls.md) — Actor + ArcSwap async control
  surface; the daemon scheduler follows the same lock-free shape.

---

## Why this exists

Stopping a shell early has nothing to do with *how an individual error
is handled*. It's a function of the shell's accumulated state:
"too many ops have errored," "we've run long enough," "a child
failed," "the recall target was met." Folding that into the error
router (the `AggregateGuard` breach) conflated two orthogonal axes:

- **Error handling** (SRD-82, `ErrorPolicy.router`): per-op — match an
  error name, decide count / warn / retry / stop / fail.
- **Stop conditions** (this SRD): per-shell — evaluate a *predicate*
  over accumulated state at a *trigger*, and if true, stop the shell
  with a chosen *effect*.

They compose: the error handler increments error counters, which feed
the `error_rate` wire, which a stop condition reads. Neither owns the
other.

The motivating need, verbatim: *"I want to check whether the op count
is over 50 and the error rate is over 10%, but I only want to check
every 10 seconds."* That is one stop condition: a predicate, plus a
trigger that controls evaluation cadence.

---

## Part 1 — A stop condition is (predicate, trigger, effect)

```rust
pub struct StopCondition {
    /// A polydat expression over the shell's runtime-state wires,
    /// evaluating to bool. Compiled in the shell's scope.
    when: PolydatPredicate,
    /// When to evaluate `when` (Part 3).
    trigger: Trigger,
    /// The Outcome the shell adopts if `when` is true (Part 5).
    effect: StopEffect,
}
```

A shell carries a **list** of these (its `stop_when:`). On each
trigger firing, the matching conditions' predicates are evaluated; the
first true one stops the shell with its effect. Stop conditions are
resolved/inherited down the shell tree the same way an `ErrorPolicy`
is (SRD-82 Part 3a: depth-inherit, breadth-share) — a child inherits
its parent's conditions unless it declares its own.

---

## Part 2 — Predicates are polydat over runtime-state wires

The predicate is **polydat**, not a bespoke mini-language. When a stop
condition is declared for a shell layer, its predicate is compiled —
**at shell-layer construction, once** — as an SRD-84 **shape 2**
[`ScopedExpr`] *bound to that shell's scope kernel*: a scope-bound,
callable expression over the scope's runtime-state wires. Evaluating at
a trigger is then `RuntimeState::trips(&mut expr)` — inject the live
state, read the predicate's truthiness — never an ad-hoc per-trigger
compile.

> **Implemented 2026-06-09** (`nmbrs-runtime::stop_conditions`):
> `compile_stop_condition(phase_kernel, idx, when) -> ScopedExpr` builds
> the runtime-state externs as SRD-84 shape-1 `GraphMatter`
> (`extern_wire::<T: Wire>`, constructed not parsed), binds the
> `u64`-truthiness predicate stub, and `ScopedExpr::bind`s it as a
> sub-context of the phase kernel. **The predicate is not baked into the
> phase kernel's own matter** — it is a separate sub-context, so authored
> phase bindings and evaluated stop predicates stay orthogonal concerns.
> (This supersedes the interim `volatile __stop_cond_<i>` matter
> synthesis.) Tested: `compiles_and_trips_a_scoped_stop_condition`.

## The predicate vocabulary IS the instrument namespace (2026-07-10)

No magic variables. Every counter wire is named EXACTLY as its
registered instrument (`ActivityMetrics::register_on` — the same
names metrics.db and the status line carry), no derived
pseudo-counters exist, and no precomputed rates: a rate is written
in the predicate from the base counters, explicitly result- or
attempt-specific. The injected fast-path wires:

| Wire | Instrument / meaning |
|------|---------|
| `cycles_total` | the `cycles_total` counter — cycles completed so far |
| `result_failure` | the `result_failure` counter — TERMINAL failed ops (an op that exhausts its whole `tries` budget counts once; absorbed transients never do) |
| `attempt_total` / `attempt_success` / `attempt_failure` | the attempt counters (SRD-91), ALL counted at attempt resolution — `attempt_total == attempt_success + attempt_failure` at every read, so `attempt_failure / attempt_total` is exact. In-flight attempts are deliberately unrepresented |
| `elapsed_ms` | wall time since the shell started (shell state, not an instrument) |
| `children_total` / `children_failed` / `children_done` | child-shell outcomes (shell state; scenario/workload shells) |

Derived-in-the-predicate rates:

```text
# result-specific backstop
(cycles_total > 100) & (to_f64(result_failure) > (to_f64(cycles_total) * 0.05))
# attempt-specific sickness (sees through retries)
(attempt_total > 200) & (to_f64(attempt_failure) > (to_f64(attempt_total) * 0.10))
```

**Everything beyond the fast path reads through the metric reader
nodes** — predicates are full polydat, and `metric(...)` /
`metric_window(...)` (nmbrs-metrics polydat nodes, Nondeterministic,
never folded) reach ANY registered instrument by its own family
name: `metric('result_failure', 'count') > 3.0` is a valid guard.
The selection grammar is `"family, key=value, key~substring"` — a
bare token names the instrument family; labeled parts narrow the
series. Stats: `count` (counter/histogram), `value` (gauge),
`mean`/`p50`/`p99` (histograms — e.g.
`metric_window('cycles_servicetime', 'p99') > 50000000.0` guards a
latency collapse). Two consistency notes: the injected wires are
read directly from the live counters at the firing event
(result-before-cycles ordering keeps the derived fraction ≤ 1),
while the reader nodes go through the cadence pipeline's framed
views — coarser, potentially one frame stale, fine for backstops;
and reader selections are session-scoped, so multi-phase sessions
should narrow by label where families collide.

Authors get the full polydat surface — arithmetic, comparison,
`if(...)`, named intermediate consts. New shell state becomes a new
instrument (readable by name immediately) — never a new bespoke
wire vocabulary.

### Distribution — the `each:` selector (declared, never inferred)

A declaration carries an explicit **distribution selector** naming the
scope *levels* it rides along with — `each: <level | [levels]>` over a
closed set aligned to `ScopeKind`: `self | op | phase | scenario |
workload` (`self` = the declaring node). The declaration's written
location bounds the subtree; `each:` names the descendant levels.

The matter walk distributes by a purely **structural** rule — a node
matches when `node.level ∈ each` *and* the node is inside the declaring
subtree (two equality checks). It never inspects the predicate's
content. One declaration → one compiled handle **per matched node**,
each reading *that* shell's runtime-state, so `error_rate > 0.1` at a
phase node is the phase's rate and the same text at the enclosing
scenario node is the scenario's aggregate. **Placement is declared and
structural; binding is to the native scope; nothing is inferred.**
`error_rate_max: X` is sugar for `{ when: "error_rate > X", each:
[phase] }`.

> **Implemented 2026-06-09 (phase-level slice).** `ScopeLevel` +
> `StopConditionSpec.each` (`nmbrs-workload::model`); the executor filters
> each phase's applicable conditions (`each ∋ {self, phase}`) and binds
> them — via `StopConditionSet::build_for_phase` — against the phase
> node's **own** `cached_kernel` (`Activity.phase_kernel`, the structural
> walk's native scope; the conjured-root stopgap is gone). The
> drain-loop Tick evaluates them (`RuntimeState::trips`). Scenario /
> workload-level distribution needs shell-level evaluation
> (`children_*` aggregation + firing events at those levels) — the
> follow-up.

---

## Part 3 — Triggers and the firing-event enum

Stop conditions are a **phase- and scenario-layer concern only**. The
inner shells — stanza, op, cycle — deliberately raise **no**
stop-condition firing events: their SRD-82 error handlers already wrap
those scopes, and firing a lifecycle event per stanza or per cycle
would be needless overhead on the hot path. So the firing-event set is
small and lives at the phase and scenario layers:

```rust
/// Firing events the executor raises for stop-condition evaluation.
/// Phase- and scenario-layer only — never per stanza / op / cycle.
pub enum FiringEvent {
    PhaseStart,    // a phase shell begins
    PhaseEnd,      // a phase shell ends — also the scenario's
                   // "a child finished" signal, so a scenario's
                   // `children_failed > 0` is evaluated here (no
                   // separate per-child event)
    Tick(TimerId), // a timer daemon fired (Part 4); its schedule is
                   // Every (a fixed "tick timer") or Settle (periodic
                   // backoff). Hooked at the phase AND scenario layer.
}
```

A predicate is **not** evaluated continuously. `PhaseStart` / `PhaseEnd`
are raised inline by the walker at the (coarse, cheap) phase boundary;
`PhaseEnd` doubles as the scenario's child-finished signal. `Tick`
comes from a per-layer timer daemon and decouples evaluation cadence
from the work rate — the example's "only check every 10 seconds," and
the settle backoff. A `trigger:` defaults sensibly per condition kind
(rate predicates → the settle `Tick`; `children_failed` → `PhaseEnd`).

---

## Part 4 — Daemon-scheduled timings and named backoffs

Each execution layer owns a set of **daemon tasks** that manage trigger
timings **asynchronously**, off the work path (reusing the activity's
`DaemonPool`; SRD-02 — async, lock-free, no blocking in the firing
path). A timing daemon's only job is to raise `Tick(id)` on a schedule;
the shell evaluates the conditions bound to that timer.

Schedules are **named, symbolic backoffs** — incremental timings that
start eager and relax as a layer settles into its pattern:

```rust
pub enum BackoffSchedule {
    /// Fixed interval.
    Every(Duration),
    /// Eager → relaxed: start at `initial`, grow by `factor` each
    /// fire up to `max`. Resets on a state-change signal.
    Settle { initial: Duration, max: Duration, factor: f64 },
}
```

Named presets: `eager` (tight fixed), `settle` (the default for error
conditions: e.g. start ~1 s, grow to ~30 s), `lazy` (coarse fixed).

**The standard error-condition daemon.** Every shell gets, by default,
one `settle`-scheduled timing daemon that fires evaluation of its
rate/error stop conditions — **eagerly at first, then less often as the
shell settles**. This is exactly what catches a fast-failing config
(the all-error query cells) within the first second or two, without
paying a per-cycle predicate cost once a long phase has stabilized. The
initial / max / factor are configurable; `settle` is the default.

---

## Part 5 — Effects are two-axis Outcomes enforced cooperatively

A condition's `effect` is the `Outcome` the shell adopts when its
predicate fires — a `(Disposition, Validity)` pair (SRD-82 Part 1):

| `effect:` shorthand | Outcome | `StopCause` |
|---------------------|---------|-------------|
| `fail` | `Interrupted + Failed` | `Fault` |
| `stop` | `Interrupted + Succeeded` | `Interrupt` |

`fail` is the error-rate breach (the result is not trustworthy);
`stop` is a clean early halt (budget/time met — keep the partial
result). Enforcement is **not** new machinery: the firing sets the
SRD-82 `stop_flag` + `StopCause`, fibers drain at their next boundary,
and the phase-end outcome records the chosen axes. A scenario shell's
fired condition aborts in-flight siblings via the same cooperative path
(SRD-82 Part 4).

> **Implemented 2026-06-16** (`nmbrs-runtime::stop_conditions`,
> `workload_shell`, `activity`, `executor`). Each `StopCondition` carries
> an `effect: Outcome`; `StopConditionSet::evaluate` returns
> `Option<(Outcome, String)>`; `WorkloadShell::record_phase` threads the
> `Outcome` and exposes `stop_outcome()`. **Effect-less default is
> level-dependent** (preserving prior behaviour, codified): an internal
> error-rate breach and a **phase**-level trip default to `fail`
> (`Interrupted+Failed`); a **workload**-level trip defaults to a graceful
> `stop` (`Interrupted+Succeeded`). Explicit `effect: fail|stop`
> overrides. At the phase shell a `fail` records a `PhaseErrorDetail`
> (phase ends Failed) and a `stop` halts cleanly (phase ends Completed);
> at the workload shell a `fail` returns `Err` (session exits non-zero)
> and a `stop` requests the graceful walk-halt. Tested:
> `stop_conditions` unit set + `nmbrs/tests/{stop_conditions,workload_shell_e2e}`.
>
> **Completed 2026-08-04 — the phase shell adopts the declared effect.**
> The trip site latches the condition's Outcome on the activity
> (`Activity::stop_outcome`, first-stopper-wins alongside `stop_reason`),
> and `run_phase` adopts it: a `stop`-effect trip ends the phase
> **Interrupted + Succeeded** (per the Part 5 table — not `Completed` as
> the note above loosely said), keeps its partial result, emits its
> phase-level `metrics:` (emission now gates on Succeeded validity, not
> on the bare stop flag), records `status = interrupted` on the
> persisted outcome row, and the checkpoint logs `phase_completed`.
> Only a `stop_when` trip latches the outcome — an error-router `stop`
> verb, a walk-stop broadcast, a poll timeout, or Ctrl-C still derive
> failure. Tested: `nmbrs/tests/stop_conditions.rs`
> `phase_graceful_stop_exits_zero_and_keeps_metrics` over the
> `phase_graceful_stop` scenario in
> `nmbrs/examples/workloads/controls/stop_conditions_coverage.yaml`.
>
> **Completed 2026-08-05 — governance `timeout:` + reason classes (C3).**
> `WorkloadPhase.timeout` (duration / bare seconds / `{param}`)
> desugars at the phase gather into the synthesized, logged
> `StopConditionDecl::timeout_guard` (`elapsed_ms > <ms>`, effect
> `fail`, reason `timeout`) — the `error_rate_max` precedent. Expiry =
> Interrupted+Failed with reason class `timeout`: GOVERNANCE
> (disqualified at this tier), deliberately distinct from a budget
> (bounded cursor / `effect: stop` → Interrupted+Succeeded).
> `PhaseOutcome::reason_class()` derives the closed `ReasonClass`
> vocabulary (`timeout | stop_condition | error | panic | operator`)
> from the first error's class — never stored on the outcome; the
> sqlite `phase_outcomes.reason_class` column denormalizes it for
> report GROUP BY (legacy dbs read NULL via PRAGMA detection).
> `PhaseOutcome::protocol_class()` yields the testing-protocol
> three-way COMPLETED / OUT-OF-RANGE / FAILED (+SKIPPED). `nmbrs
> replay --json` emits `reason_class`. Tested:
> `phase_timeout_is_out_of_range_not_generic_failure`.
> The optimizer (SRD-86) reconfigures the per-point default so a failed
> probe is a feasibility datum, not a search abort.

---

## Part 6 — Orthogonality and reconciliation

Two systems, composing, neither owning the other:

```
ErrorPolicy (SRD-82)         StopConditions (this SRD)
  per-op error routing         per-shell state predicates
  count/warn/retry/stop/fail   when(polydat) @ trigger → effect(Outcome)
        │                              ▲
        └── increments error_count ────┘  (read as the error_rate wire)
```

Reconciling the landed SRD-82 work:

- **The error-rate breach moves out of `ErrorPolicy`.** The
  `AggregateGuard` (`nmbrs-errorhandler/src/aggregate.rs`) is retired;
  the breach becomes a default stop condition
  `when: "op_count > 50 && error_rate > 0.1"`, `trigger: settle`,
  `effect: fail`. `ErrorPolicy` keeps only its `router`; its `guard`
  field is removed.
- **Scenario "stop on error" is a default stop condition**, not an
  error-router rule: `when: "children_failed > 0"`, `trigger: PhaseEnd`
  (a child phase ending is the scenario's child-finished signal),
  `effect: fail` with `Fault` cause → `Interrupted + Failed` on the
  scenario. This closes SRD-82's Part 4 gap (a failed phase now stops
  the workload) as a stop condition.
- The drain-loop delegation to `error_policy.guard.assess(...)` is
  replaced by the shell's stop-condition evaluation on its settle
  daemon's `Tick`.

---

## Part 7 — Configuration surface

```yaml
phases:
  query_sweep:
    stop_when:
      - when: "op_count > 50 && error_rate > 0.1"   # polydat predicate
        trigger: settle        # named backoff; eager → relaxed
        effect: fail           # Interrupted + Failed
      - when: "elapsed_s > 600"
        trigger: { every: 5s }
        effect: stop           # Interrupted + Succeeded (budget met)
```

Defaults (inherited, overridable per shell): the session installs the
two default conditions above (rate breach + child-failed) so the
out-of-the-box behaviour is "fail fast on a broken config, stop the
workload on a failed phase" without any `stop_when:` in the workload.

---

## Part 8 — Migration

1. **Runtime-state wires.** ✅ DONE (`nmbrs-runtime/src/stop_conditions.rs`).
   `RuntimeState { op_count, error_count, elapsed_ms, children_* }` +
   `error_rate()` + the `wire::*` name vocabulary +
   `inject_into<D: Dataflow>` (per-wire `find_input` + `set_wire_idx`,
   skipping wires a predicate doesn't reference). Tested against a real
   compiled predicate kernel (inject by name → volatile re-evaluation).
   Step 2 declares these `op_count`/`error_rate`/… as volatile externs
   on the phase/scenario scope kernel at construction.
2. **Predicate compile (cohesive).** At shell-layer construction, fold
   each `when:` into the shell's scope kernel as an output binding —
   compiled once, with the runtime-state wires as its inputs — so
   evaluation is a single output pull, never an ad-hoc per-trigger
   compile. Phase and scenario layers only.
3. **Firing events + daemon scheduler.** Define the small `FiringEvent`
   set (`PhaseStart` / `PhaseEnd` / `Tick`); raise `PhaseStart` /
   `PhaseEnd` inline at the walker's phase boundary (no per-stanza /
   per-cycle events); add `Every`- and `settle`-scheduled timing
   daemons to the `DaemonPool` at the phase and scenario layers that
   raise `Tick`.
4. **Effects.** ✅ DONE (2026-06-16). `fail`/`stop` map to the two-axis
   `Outcome` (`Outcome::failed()` / `Outcome::interrupted()`) per
   condition, with a level-dependent default for effect-less conditions
   (phase → `fail`, workload → `stop`). See the Part 5 implementation note.
5. **Retire the `AggregateGuard` breach.** Replace the drain-loop
   `guard.assess` delegation with stop-condition evaluation; drop
   `ErrorPolicy.guard`; install the default conditions.

Step 3's event raising + 1's wires are the load-bearing new surface;
the rest reuses SRD-82's abort and SRD-18b's per-scope kernel.

---

## Invariants (axioms this SRD adds)

- **Stop conditions are orthogonal to error handling.** A stop
  condition never decides per-op error disposition; an error handler
  never decides shell termination. They communicate only through
  shared state wires (`error_rate`).
- **A stop condition is (polydat predicate, trigger, effect).**
  Predicates are polydat over runtime-state wires — no bespoke
  condition language. Effects are two-axis Outcomes.
- **Predicates compile cohesively into the shell's scope kernel at
  construction.** A `when:` is an output binding of the shell's scope
  (one kernel, built once, SRD-18b); evaluation is a single output
  pull against volatile state, never an ad-hoc per-trigger compile.
- **Stop conditions live at the phase and scenario layers only.** The
  stanza / op / cycle shells raise no stop-condition firing events —
  their error handlers suffice; firing per cycle would be needless hot-
  path overhead. The firing set is `PhaseStart` / `PhaseEnd` / `Tick`
  (fixed or settle), and `PhaseEnd` is the scenario's child-finished
  signal.
- **Evaluation is triggered, not continuous.** Rate/error conditions
  default to a `settle` backoff (eager → relaxed) so a broken config is
  caught fast and a settled phase pays little.
- **Timing is daemon-managed and async.** Trigger cadence lives in
  per-layer (phase / scenario) daemon tasks, never in the work path.

## Part 9 — `throttle:` — the adaptive backpressure governor

> **Implemented 2026-08-13** (`nmbrs-runtime::throttle`, phase field
> `throttle:`, e2e `nmbrs/tests/throttle_governor.rs`).

Stop conditions decide when a shell must END; the throttle governor
decides how hard a shell may PUSH. A saturated target converts client
overload into retry churn — the tries wrapper (SRD-82 Part 3b)
absorbs server rejections, `ok:%` stays green, and the only truthful
signal is the ATTEMPT plane: the windowed attempt-failure fraction
(`attempt_failure / resolved attempts`, the same see-through-retries
wires Part 2 exposes to predicates) climbing while wasted attempts
burn both sides.

A phase declares the governor:

```yaml
load_train:
  concurrency: "{concurrency}"
  throttle: true          # all defaults, or the map form:
  # throttle:
  #   high: 0.05          # back off above 5% windowed attempt failure
  #   low: 0.01           # recover below this (default high/5)
  #   control: concurrency  # or rate (needs a rate: on the phase)
  #   start: 1            # slow-start seed (default = floor); declare
  #                       # higher ONLY for known-robust targets
  #   floor: 1            # never throttle below
  #   window: "2s"        # evaluation window
```

Semantics — fragile-first, scaling to robust (TCP-style):

- The governor ticks on the same drain-loop cadence as the phase's
  stop conditions, computing the failure fraction from counter
  DELTAS per window — a true trailing window, never a lifetime
  average.
- **Slow-start.** The phase OPENS at `start` (default `floor`):
  the fiber pool spawns at the opening offer and the control is
  published to it, so the authored value is a ceiling the governor
  grows into, never an opening assault. While no congestion has
  been observed, each clean window DOUBLES the offer — a robust
  target reaches the authored ceiling in log2(ceiling/start)
  windows with zero failures; the most fragile target (a local
  single-node container) is never overdriven at all. (Reactive-only
  governance was measured against exactly that target: ten seconds
  of 100% attempt failure and ~20k server-side dropped mutations
  before the walk-down from the authored ceiling completed.
  Slow-start removes that entire regime.)
- **Severity-proportional back-off.** Above `high`, the offer
  multiplies by `clamp(1 − frac, 0.25, 0.9)`: a marginal breach
  trims ×0.9, total failure collapses ×0.25 — floored.
- **Congestion memory.** Each back-off records the offer at which
  failure was observed. Recovery climbs ×1.5 through the
  proven-safe zone (75% of that point), then probes ADDITIVELY
  (+max(1, 2% of it) per clean window) — never a multiplicative
  march back into the same wall, so a sensitive target sees gentle
  pressure waves, not sawtooth assaults. Three CONSECUTIVE clean
  windows at-or-above the remembered point clear the memory (the
  target got healthier — warm caches, finished compactions) and the
  doubling climb resumes; one clean window at a marginal congestion
  point is noise, and the additive probes keep stepping through the
  streak, so clearing means the target stayed clean across a rising
  run of offers.
- Writes ride the same push-on-set path as every control writer
  (`ControlOrigin::Governor`, confirmed-apply, spawned off the
  loop); reads are counter loads. No new machinery in the work
  path. External control writes (TUI, web, `control_set`) are
  honored — the governor steps from the committed value, never
  from private state.
- Every adjustment logs one line naming the signal, window, and
  movement (climbing / reclimbing / probing / back-off); the
  governor announces its slow-start and bounds at phase start —
  visible, never silent.
- Measurement honesty: a load figure taken at high attempt-failure
  is a saturation artifact. The throttled steady state IS the
  measurement — the target's capacity at the declared failure
  bound.

Companion default (SRD-82 Part 3b): the retry loop's first sighting
of each error class per phase emits ONE advisory line by default
(capped at 3 classes/phase, `retry_advisory: off` to silence), so a
retry storm identifies itself even with exemplar sampling off.

User-facing walkthrough of the layered system (counters → advisory
→ exemplars → tracing → governor → stop conditions):
`docs/guide/retry_visibility_and_throttle.md`. Runnable,
walker-pinned demonstrations:
`nmbrs/examples/workloads/controls/retry_visibility.yaml` and
`nmbrs/examples/workloads/controls/throttle_backpressure.yaml`.
