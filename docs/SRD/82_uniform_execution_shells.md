# SRD-82 — Uniform Execution Shells (Outcomes, Error Handling, Stop Conditions)

**Status:** DRAFT — the canonical model for how execution levels
report results, handle errors, and decide stop conditions. Supersedes
the ad-hoc, per-level error mechanisms and folds SRD-03 (the error
router) and SRD-76 (phase outcome) into one recursive shape.

**Owner:** nmbrs-runtime (executor walker, `run_phase`, the activity
cycle/stanza loop), nmbrs-errorhandler (the router, generalised),
nmbrs-metrics (outcome persistence), nmbrs-workload (the per-level
`errors:` configuration surface).

**Cross-refs:**
- [SRD-02](02_concurrency_model.md) — One concurrency path; the
  single walker / fiber harness. SRD-82 says every level of that
  harness is the *same* execution shell.
- [SRD-03](03_error_handling.md) — The error router
  (`pattern:actions` cascade) and op/adapter/stanza error scoping.
  SRD-82 generalises the router to operate at every shell, matching
  on *outcomes*, not only op-error names.
- [SRD-18b](18b_scenario_tree_and_scheduler.md) — The scenario tree
  and the single walker. SRD-82 makes the walker's per-sibling error
  behaviour a configurable shell handler instead of an implicit
  `first_err` rule.
- [SRD-76](76_phase_outcome_disposition.md) — `PhaseOutcome`. SRD-82
  splits its single `PhaseStatus` into two orthogonal axes and makes
  the result the return type of *every* shell, not just phases.
- [SRD-77](77_working_sessions_and_refine.md) — `refine` re-uses a
  prior execution's outcomes. The Interrupted-but-Succeeded vs
  Interrupted-and-Failed distinction below is what tells `refine`
  which partial results are re-usable.
- [SRD-18c](18c_comprehension_syntax.md) — Cursor scope visibility:
  ops inside a phase share the phase's cursor base. Part 6 leans on
  this — it is *why* a daemon that needs its own base must be a daemon
  *phase*, not a daemon op.
- [SRD-83](83_stop_conditions.md) — Stop conditions as
  `(when: predicate over runtime-state, trigger, effect)`. Part 6's
  "stop the daemon when its group's foreground completes" is one such
  condition, not bespoke control flow.

---

## Why this exists

A workload runs through four nested **execution shells**:

```
scenario graph   →   phase   →   stanza   →   op
(execute_tree)       (run_phase)  (cycle loop)  (dispenser + adapter)
```

Today each shell has its *own* error mechanism and its own notion of
"result", and they don't compose:

| Shell | Code today | Error mechanism | "Result" today |
|-------|-----------|-----------------|----------------|
| Scenario graph | `executor::run_siblings_concurrently` | implicit `first_err`; halts *further* dispatch but never aborts in-flight siblings and never consults a stop signal | `Result<(), String>` |
| Phase | `executor::run_phase` | a `stop_flag: AtomicBool` boolean + `stop_reason` string | `PhaseOutcome { status }` |
| Stanza | the per-cycle loop in `activity::executor_task` | inline, per-cycle | none distinct |
| Op | `nmbrs_errorhandler::ErrorRouter` | the `pattern:actions` cascade (`count`/`warn`/`stop`/`retry`) | per-op error |

Two concrete failures fall out of this inconsistency:

1. **An invalid config completed the whole run.** Query cells with
   `errors: count` errored on 100% of ops but *completed* — no
   `stop_flag` was set, so `run_phase` returned `Ok`, the walker kept
   going, and the entire sweep ran producing garbage that *looked*
   finished. The QPS axis even rendered the all-error cells as
   ultra-high throughput. Nothing in the model says "a phase that is
   ~all-errors is Failed", and nothing says "a Failed phase stops the
   scenario."
2. **A failed phase does not stop the workload.** The scenario walker
   propagates the first child `Err` only far enough to halt *new*
   serial dispatch; concurrent or already-running siblings finish, and
   there is no "stop on error" default at the scenario level.

The fix is not four more patches. It is one recursive shape — a
**uniform execution shell** with a two-axis **outcome** and a
**handler** (the generalised router) that every level uses the same
way.

---

## Part 1 — Outcomes are two orthogonal axes

`PhaseStatus { Completed, Failed, Skipped, CursorSuspended }` conflates
two independent questions. Split them:

```rust
/// How much of the unit's work ran.
pub enum Disposition {
    Running,        // in flight
    Completed,      // ran to its natural end
    Interrupted,    // stopped before its natural end
    Skipped,        // never started (filtered / unreached)
}

/// Whether the produced result is usable.
pub enum Validity {
    Succeeded,      // result is trustworthy
    Failed,         // result is not trustworthy — do not rely on it
}

/// The result of ANY execution shell.
pub struct Outcome {
    pub disposition: Disposition,
    pub validity: Validity,            // meaningless while Running/Skipped
    pub errors: Vec<ErrorDetail>,      // chronological, SRD-76 shape
    pub resume: Option<ResumeCursor>,  // SRD-44 partial-progress handle
    // …identity / duration / hash carried as in SRD-76…
}
```

The two axes are independent. The meaningful combinations:

| | `Succeeded` | `Failed` |
|---|---|---|
| **`Completed`** | clean result | ran fully, result is garbage — the all-error query cells |
| **`Interrupted`** | partial but **re-usable** (keep what ran) | partial and **discard** |

Consequences the single status couldn't express:

- A user **Ctrl-C** is `Interrupted`; its `Validity` is whatever the
  shell had achieved — often `Succeeded` (keep the partial sweep), and
  a consumer (`refine`, replay) may re-use it. It is NOT inherently
  `Failed`.
- An **error-rate breach** is `Failed`; its `Disposition` is whatever
  it managed — usually `Interrupted` (we stopped it), occasionally
  `Completed` (a short phase finished before we judged it).
- `CursorSuspended` (SRD-44/76) collapses to
  `Interrupted + Succeeded` — re-usable partial progress.

`refine` and any result consumer key off **`Validity`** to decide
trust/re-use and off **`Disposition`** to decide completeness — never
a single overloaded enum.

> **Implemented in full 2026-07-10.** The axes are the STORED
> canonical on `PhaseOutcome` (`disposition` + `validity` fields);
> the conflated `PhaseStatus` enum is deleted. Display projections
> (`glyph` / `label` / `is_failure`) live on `Outcome`;
> `is_failure` keys on Validity alone, and `Interrupted+Succeeded`
> renders `…`/yellow (re-usable partial). `cursor_suspended` is
> retired — a resume cursor on the outcome carries the signal, and
> `PhaseOutcome::interrupted(...)` constructs that shape. Legacy
> compatibility is read-only: checkpoint JSONL records and sqlite
> rows written with the old single `status` string still
> deserialize/replay (`PhaseOutcomeWire` fallback; `nmbrs replay`
> accepts both label sets); new records write the axes only.

---

## Part 2 — One execution shell, recursively composed

Every level is the *same* shape:

```rust
/// One execution shell. Scenario graph, phase, stanza, and op are
/// all instances at different granularities.
trait ExecShell {
    /// Run the body (children, or ops) and return this shell's
    /// Outcome. The body feeds each child outcome / op error through
    /// `self.handler`, which decides the per-child action and thereby
    /// this shell's aggregate Disposition + Validity.
    async fn run(&self, ctx: &ShellCtx) -> Outcome;
}
```

A shell does three things, identically at every level:

1. **Body** — run its units (child shells, or the leaf ops). Units run
   serially or concurrently per the *one* concurrency path (SRD-02);
   the shell does not own a private concurrency mechanism.
2. **Handle** — feed each unit's result (a child `Outcome`, or an op
   error) through this shell's **handler** (Part 3). The handler emits
   an **action** that the shell applies to itself and to the rest of
   the body.
3. **Aggregate** — emit this shell's `Outcome`. Its `Disposition` is
   `Completed` if the body ran to its natural end, `Interrupted` if a
   handler action stopped it early. Its `Validity` is `Succeeded`
   unless a `fail` action fired (or a child's `Failed` propagated per
   policy). That `Outcome` is, in turn, one *unit result* fed to the
   parent shell's handler.

The recursion is the whole point: a phase is to its stanzas what the
scenario graph is to its phases. The op shell's "child results" are op
errors; every other shell's "child results" are `Outcome`s.

### The four shells as instances

| Shell | Body (its units) | Configured by |
|-------|------------------|---------------|
| **Scenario graph** | child scopes / phases | workload / scenario-level `errors:` |
| **Phase** | its stanza sequence + aggregate op-error conditions | per-phase `errors:` |
| **Stanza** | the linearized op chain (SRD-02) | per-stanza `errors:` |
| **Op** | the single adapter call | the op's resolved error class (leaf) |

This is exactly the hierarchy of handlers the design asks for: the
scenario-graph handler is configured at the scenario level, the phase
handler per phase def, the stanza handler per stanza def, the op
handler per op — each interpreting the results one level down.

---

## Part 3 — The handler is the generalised router

SRD-03's router (`"<pattern>:<actions>;…"`, first-match-wins) is
already the right abstraction. Generalise it from *op-error-name → op
verb* to *unit-result → shell action*.

### Match keys (what the left side matches)

A handler rule matches a **unit result**. The vocabulary widens as it
moves up the shells, but the grammar is identical:

- **Error class** (regex on `error_name`) — the op-leaf key, unchanged
  from SRD-03: `Timeout`, `cql_error`, `.*`.
- **Child validity** — `Failed` / `Succeeded` (matches a child shell's
  `Outcome.validity`). This is how a parent reacts to a failed child.
- **Aggregate condition** — evaluated over the body's running tally,
  not a single unit: `rate>0.1` (errored fraction of ops),
  `count>N`, `consecutive>K`. This is what makes the error-rate
  breach declarative rather than a hardcoded loop check.

### Actions (what the right side does)

| Action | Meaning | Effect on this shell |
|--------|---------|----------------------|
| `count` | tally to a metric; keep going | none |
| `warn` | log at Warn; keep going | none |
| `ignore` | explicit no-op (SRD-03 invariant: never a default) | none |
| `retry` | re-run the failing unit (op or child) | none, on success |
| `fail` | the result is not trustworthy | `Validity = Failed` |
| `stop` | halt the rest of the body now | `Disposition = Interrupted` |

`fail` and `stop` are orthogonal — the two-axis outcome demands it.
`fail` without `stop` lets a unit keep running but marks the result
untrustworthy; `stop` without `fail` is a clean early halt (a user
interrupt); `fail,stop` is the common "this is broken, abandon it."

### Defaults (the load-bearing part)

- **Scenario-graph handler default: `*Failed : stop`.** A child phase
  whose `Validity` is `Failed` halts the remaining scenario body and
  Interrupts it — *stop on error is the default scenario-graph
  behaviour*, expressed as one rule, not special-cased walker code.
- **Phase handler default** keeps SRD-03's op cascade
  (`.*:warn,stop`). The aggregate breach rule
  (`rate>{error_rate_max} : fail,stop`) is **OPT-IN ONLY** — see
  §"AggregateGuard retired as a default" below; it originally shipped
  with a silent `error_rate_max=0.1` default, which is withdrawn.
- **Stanza handler default** is the SRD-02 linearization response: an
  op error fails the dependent chain (`.*:fail` for the chain),
  surfaced to the phase handler.
- **Op handler** is the adapter's resolved error class — the leaf.

Both features the design started from collapse into the grammar:

```
# error-rate circuit breaker  →  a phase rule (opt-in)
phase.errors = "rate>0.1:fail,stop"

# stop-on-error                →  the scenario-graph default
scenario.errors = "*Failed:stop"        # implicit unless overridden
```

### AggregateGuard retired as a default (decision, 2026-07-09)

The aggregate error-rate breaker (`error_rate_max`, synthesized into
each phase's stop-condition set as
`op_count >= 50 && error_rate > {max}` with a `fail` effect) shipped
DEFAULTED ON at `0.1`. In practice the default was:

1. **Hidden** — nothing in the workload text, the startup output, or
   `describe` said it existed; it surfaced only when it tripped.
2. **Not optional in perception** — disabling required knowing the
   magic `error_rate_max=1` incantation for a knob the operator had
   never seen.
3. **Presumptive** — 10% is somebody's threshold, not necessarily the
   workload's. A probe phase that MEASURES query behaviour during
   ingest (expected-high error rates; its errors are data) was failed
   by a guard it never asked for.

The observed failure mode was the decisive one: **operators built
duplicate `stop_when` rate backstops because the built-in was
invisible** — the hidden default caused the very proliferation it was
meant to prevent. Per "no presumed features" and "never ignore
silently" (defaults must be visible), the DEFAULT is withdrawn:

- `error_rate_max` (session param / phase field) remains as an
  **explicit opt-in shorthand** that synthesizes the same stop
  condition. Setting it is visible in the workload/CLI by definition.
- Aggregate shell health canonically belongs to **workload-authored
  `stop_when:` conditions (SRD-83)** — visible, self-documenting,
  arbitrary predicates.
- The garbage-run protection story this default carried (the
  all-errors-completed-green incident above) is NOT abandoned: the
  agreed future shape is a **synthesized, VISIBLE default `stop_when`
  condition** — announced at phase start like other synthesized layers
  (the auto-cadence-logging discipline), shown in `describe`, and
  SUPERSEDED whenever the workload declares its own `error_rate`
  condition on that shell. That reintroduction is deliberate future
  work, not part of this decision.

---

## Part 3a — `ErrorPolicy`: the handler is its own resolver

The handler at a shell and the *resolver* that provisions its
children's handlers are the **same type** — an `ErrorPolicy`. There is
no separate dispenser service; consolidating them removes a parallel
object to thread.

```rust
pub struct ErrorPolicy {
    config: PolicyConfig,        // value-equality key (error_spec + error_rate_max)
    pub router: ErrorRouter,     // op-error handling
    pub guard: AggregateGuard,   // aggregate (rate>N) handling
    derived: Mutex<HashMap<PolicyConfig, Arc<ErrorPolicy>>>,  // breadth cache
}
```

Resolving a shell's policy is an **init-time** action with two
moments — both needed, both living in `resolve_child`:

- **Depth (on descend):** a child shell inherits its parent's policy
  unless it overrides. `resolve_child(None)` — or a config equal to the
  parent's — returns the parent `Arc` itself. Inheritance is sharing by
  reference; no new instance.
- **Breadth (within a layer):** sibling shells that override with the
  *same* config get one derived policy, deduplicated by the config's
  **value equality** (the `derived` cache on the parent). A 200-cell
  sweep that shares one `errors:` parses the spec once.

Crucially, resolution happens **once per shell, at scope-init**: the
shell binds the resolved `Arc<ErrorPolicy>` and holds it. There is no
re-resolution *within* a shell after init (no per-cycle, per-fiber
lookup). The session holds the **root** policy (the default); each
shell's policy is `parent.resolve_child(own_config)`.

This answers "one instance per unique configuration" with two
mechanisms rather than two caches: depth-inheritance shares the
ancestor's instance down an unbroken chain; breadth-dedup shares one
derived instance across a layer. The per-node "no re-init" is not a
cache at all — it is the shell holding its bound policy.

```text
session root  ── resolve_child ─▶  scenario policy  ── resolve_child ─▶  phase policy …
   (default)        (inherit / derive, value-equality shared, bound once at scope-init)
```

---

## Part 3b — Op-shell handler: verbs as aspects, not layers

*(Feasibility study, resolved 2026-07-08. Governs how the op-level
handler is implemented; SRD-32a owns the wrapper machinery it rides.)*

The op shell's handler lives in the op wrapper system — but the
question of **altitude** had to be settled: is each router verb
(`count`, `warn`, `ignore`, `retry`, `stop`, `fail`) its own wrapper,
or is the handler one wrapper with the verbs inside it?

### The measured cost of a wrapper layer

`OpDispenser::execute` returns `Pin<Box<dyn Future>>`. Every layer
therefore costs, per cycle, **on the happy path**: one vtable hop, one
heap-allocated future, one extra poll frame. Tens of nanoseconds
against ops costing 100µs–100ms — but it is *per layer*, so the design
question reduces to how many layers uniformity costs.

### Why per-verb wrappers are rejected

1. **Verb activation is a runtime decision.** Whether `warn` fires for
   a given failure depends on which *pattern* matched the error name
   (`Timeout:retry,warn;.*:stop`). A static `warn` layer would have to
   re-run the cascade match itself — N layers × pattern match per
   error.
2. **Verb order is per-rule; wrapper order is per-op.** The same verbs
   appear in different orders in different rules. A fixed stack cannot
   express rule-relative ordering.
3. **Happy-path cost scales with verb count** — every verb layer is a
   pure passthrough for the ~99.9% of ops that succeed.

Per-verb wrappers are both semantically wrong and slow. Rejected.

### The resolving taxonomy

| Kind | Verbs | Mechanism |
|---|---|---|
| **Control-flow** | `retry` / `retry(N)` | Must *re-invoke inner* — a loop around the call. Genuinely a wrapper (the only one): the **`tries` wrapper**, innermost. |
| **Terminal** | `count`, `warn`, `errlog`, `ignore` + effects `stop`, `fail` | A fold over the terminal outcome, in matched-rule order. Exactly ONE observation point — never on the call path. |

### The shape

1. **One `errors` wrapper, OUTERMOST**, registered in the SRD-32a
   registry (`owned_fields: ["errors"]`; activation = field present or
   an `errors` policy in scope — the session root's default seeds one
   everywhere, so stop-on-error stays the universal default). Full
   aspect citizenship: `nmbrs describe`, composition telemetry, per-op
   override.
2. **Its happy path is one branch**: `match inner.execute(..).await`
   — `Ok` passes through untouched; the pattern match, verb chain,
   tallies, error capture, and stop/fail effects run only in the `Err`
   arm. The verbs never needed to be on the call path: they consume a
   discriminant (`Result`) the caller already branches on.
3. **Verbs stay uniform INSIDE the compiled policy.** The
   `nmbrs-errorhandler` `ErrorHandler` chain is already the uniform verb
   abstraction at the correct altitude — pattern-routed, in-rule-order,
   extensible by registering one handler impl. Same precedent as the
   CQL `ModifierChain` (SRD 73): many uniform aspects, ONE applied
   object, no nested layers.
4. **`tries` is its own conditional wrapper** (innermost). `tries:` is
   its sigil — the TOTAL attempts an op may make — resolved in order
   from the op's `tries:` field, the inherited phase/root `tries` (the
   phase FIELD beats a scope wire: a root param also lands in GK scope
   as a constant and must not shadow an explicit phase pin), then an
   in-scope `tries` wire; never unconditionally. Absent or `tries: 1` →
   no wrapper (single attempt; the `errors` wrapper records the
   single-attempt `attempt_*` tallies). `tries: 0` → the op FAILS
   WITHOUT EXECUTING (a synthesised `tries_zero` error, routed like
   any terminal failure). `tries: N ≥ 2` → up to N total attempts.
   `errors:` and `tries:` are **orthogonal configuration surfaces**.
   The wrapper's companion knobs ride the same op-param excision as
   `tries` itself: retry pacing (`retry_backoff` /
   `retry_backoff_max` / `retry_backoff_ratio`, or the `tries:` map
   form's `backoff:`) and **retry-error counter-exemplars**
   (`retry_exemplar_rate`, a fraction ∈ [0,1] defaulting to 0.0 =
   off, and `retry_exemplar_max_hz`, an emission-frequency ceiling
   defaulting to 5/s). An error the wrapper retries never reaches
   the `errors:` policy — it exists only as an `attempt_failure`
   count — so at a non-zero rate the wrapper samples caught errors
   onto the structured event sink (`nmbrs-runtime::exec_events`, the
   opt-in `ExecEventSubscriber` decorator service): one WARN-level
   session-log line per sampled specimen carrying op, attempt/budget,
   cycle, error class, and message. Sampling is deterministic on
   (cycle, attempt) so replays reproduce it; admissions over the
   frequency ceiling are squelched AND counted, the tally reported
   on the next emitted line (leftovers flush at Debug when the
   sampler retires) — squelched, never silent. Both knobs are also
   SRD-23 **dynamic controls** (`retry_exemplar_rate` /
   `retry_exemplar_max_hz`, declared per activity, `DeclaredWhen::
   Always`), push-on-set: the applier is one atomic store into the
   activity's shared `ExemplarConfig` cell, which every tries
   wrapper that did NOT pin its own `retry_exemplar_*` params reads
   — only on its retry path, one atomic load. No control-registry
   traffic per op. An op with pinned params holds a private cell
   the controls deliberately do not move: authored matter wins, the
   live control moves the rest. Independent of sampling, the retry
   loop carries a DEFAULT-ON advisory: the first sighting of each
   error class per phase emits one actionable line (shared
   per-activity gate, capped at 3 classes; `retry_advisory: off`
   opts an op out) — a retry storm identifies itself even with
   every sampler off. User-facing walkthrough of the whole stack:
   `docs/guide/retry_visibility_and_throttle.md`; runnable
   demonstrations: `nmbrs/examples/workloads/controls/retry_visibility.
   yaml` and `throttle_backpressure.yaml`.
5. **The injection bridge**: the `errors` wrapper resolves *before*
   the tries wrapper's activation is evaluated. A `retry` / `retry(N)`
   verb in the compiled spec injects a `tries` budget (`N` additional
   → `N+1` total; bare `retry` → a small default) when the op declares
   none — so `errors: "Timeout:retry,warn"` still activates retries
   without coupling the surfaces. An explicit `tries` anywhere in
   scope WINS over the verb's budget.
6. **The AggregateGuard (`rate>N`) stays at the phase shell** — it is
   an aggregate over the body, not an op-terminal observation.
7. **Shell-level handler wiring is UNCHANGED.** This part governs only
   how the OP-level handler stacks into op dispatch. The
   `ErrorPolicy::resolve_child` chain (session → scenario → phase, Part
   3a), the phase shell's guard-driven drain decision, and the scenario
   shell's `ShellHandler` all keep their existing wiring; the op wrapper
   *consumes* the phase policy from that chain as its inheritance
   parent. Promoting a SHELL's handler into a wrapper-like layer is a
   separate, future exercise (the SRD-32a `WrapperLevel` metadata is the
   hook), not implied here.

### Cost accounting

Non-retrying op: `errors` (+1) replaces the previously-unconditional
retry wrapper (−1) — net zero layers; the fiber-loop inline error
block relocates, not duplicates. Retrying op (`tries ≥ 2`): +1 layer,
only where retries were asked for.

---

## Part 4 — Stop propagation and abort

When a handler emits `stop`/`fail`, the shell must (a) stop dispatching
new units and (b) abort in-flight concurrent units — *with the right
disposition*. This reuses the existing plumbing, corrected on two
points:

1. **A failed-stop carries `Failed`, not `Interrupted`.** Today the
   only session-wide stop is the Ctrl-C path (`session_signals::
   request_stop`), which marks the run interrupted. A handler `stop`
   from a `fail` rule must propagate a *Failed* disposition so the run
   is recorded `Interrupted + Failed`, distinct from a user Ctrl-C
   (`Interrupted + Succeeded`/policy). The stop signal therefore
   carries a `StopCause { Interrupt | Fault }` rather than a bare
   boolean.
2. **The walker consults the stop and aborts in-flight.**
   `run_siblings_concurrently` must, on a `stop` action (or an
   observed `StopCause`), both stop dispatching siblings *and* signal
   in-flight sibling shells to abort at their next cooperative
   boundary — the same `stop_flag` fibers already poll (activity.rs),
   now sourced from the shell handler rather than only the op router.

A shell never force-kills; abort is cooperative at the existing cycle /
sibling boundaries (SRD-02). In-flight units that reach their boundary
after the stop report `Interrupted`.

---

## Part 5 — Mapping to code + migration

Incremental; each step compiles and is independently testable.

1. **`Outcome` type.** ✅ DONE IN FULL (storage swap 2026-07-10).
   `disposition`/`validity` ARE the stored fields on `PhaseOutcome`;
   the conflated `PhaseStatus` and both bridge projections are
   deleted. Producers set the axes directly (`Completed+Failed` is
   expressible); legacy single-`status` records deserialize via the
   `PhaseOutcomeWire` fallback. See the Part 1 implementation note.
2. **Generalise the router.** Extend `nmbrs_errorhandler::ErrorRouter`
   to match `child validity` and `aggregate` keys and to emit `fail` /
   `stop` actions, alongside the existing op verbs. Keep first-match
   semantics.
3. **Phase shell.** ✅ DONE. `AggregateGuard`
   (`nmbrs-errorhandler/src/aggregate.rs`) holds the `rate>N:fail,stop`
   rule; `ErrorPolicy` (`nmbrs-runtime/src/error_policy.rs`, Part 3a)
   composes it with the op router; the phase shell's drain loop
   delegates the breach decision to `error_policy.guard.assess(cycles,
   errors)` rather than testing a hardcoded threshold. `run_phase`
   binds the phase policy via `ctx.error_policy.resolve_child(...)`; the
   runner seeds the session root. The breach is now a composed rule.
4. **Scenario shell.** ✅ DONE 2026-06-25. The default scenario-graph
   `*Failed:stop` is installed as the
   workload-shell stop condition `children_failed > 0` with a `fail`
   effect (`runner.rs`): any failed child latches the per-execution
   `walk_stop`, so `run_siblings_concurrently`'s `should_stop()` check
   halts not-yet-dispatched siblings *everywhere* (concurrent /
   cross-subtree), not just the local `Err` cascade's serial successors.
   `StopCause { Interrupt, Fault }` (`session_signals.rs`,
   `request_fault_stop` / `fault_stop_requested` / `request_shell_stop`)
   threads the cause: a `fail`-effect trip routes to a FAULT stop — the
   run exits non-zero (via the failing phase's own `Err`) and the
   deliberately-skipped tail is not reported as stranded
   (`unreached_phase_exit_code` stays quiet on fault, as for graceful) —
   distinct from a graceful trip's `request_graceful_stop`. Tested:
   `nmbrs/tests/workload_shell_e2e.rs::failed_phase_halts_walk_with_fault`.
   ABORT-IN-FLIGHT ✅ DONE: already-running `Bounded(N>1)` sibling phases
   abort cooperatively too. The per-execution `walk_stop`
   (`Arc<AtomicBool>` on the `WorkloadShell`, exposed via
   `walk_stop_flag()`) is cloned onto each `Activity` (`Activity::
   walk_stop`); fibers poll it at the same cooperative boundaries where
   they poll `stop_flag` / `stop_requested()` (`activity.rs`), exiting
   instead of draining. It is per-execution (one shell per `ExecCtx`), so
   a fault in one execution never aborts another's phases — chosen over a
   global fault flag precisely to avoid leaking across SRD-88 concurrent
   in-process executions; verified by the concurrent in-process example
   sweep. Tested:
   `…::inflight_concurrent_sibling_aborts_on_fault` (a `slow` phase
   concurrent with a fast-failing `boom` runs 1 of 20 ops, not 20).
5. **Stanza shell.** ⏳ PENDING. Fold the per-cycle stanza error
   handling into the same shape so the stanza's `errors:` configures it.
5a. **Op handler (per-op `errors:`).** ✅ DONE, WRAPPER FORM (Part 3b)
   ✅ DONE. Each op dispenser carries its own resolved `ErrorPolicy`:
   at activity build every op template's `errors:` override resolves a
   child of the phase policy — value-equality shared across ops that
   declare the same spec, inherited *by reference* (the phase policy
   `Arc` itself) when the op declares none. The terminal error of an op
   is ALWAYS routed through ITS OWN policy, so a lenient op
   (`errors: ".*:warn,counter"`) never softens its siblings and a strict
   one never hardens them. Only the op-error ROUTER is per-op; the
   aggregate rate breach (`error_policy.guard`) stays the phase
   shell's. The op-level `errors:` surface parses as a per-op
   activity-param (`nmbrs-workload` `parse.rs`), excised from op fields
   so it never reaches the adapter. Per Part 3b the handler is the
   OUTERMOST `ErrorHandlerDispenser` wrapper
   (`nmbrs-runtime/src/wrappers/errors.rs`, replacing the fiber
   loop's inline block), and the retry wrapper is the CONDITIONAL
   `tries` wrapper (`wrappers/tries.rs` — total-attempts sigil,
   orthogonal to `errors:`, bridged by `retry`/`retry(N)` verb
   injection; `nmbrs-runtime/tests/tries_wrapper.rs`). Tested:
   `nmbrs/tests/error_handlers.rs::op_level_errors_overrides_scope_policy`
   + `…::op_level_errors_does_not_leak_to_siblings`. (The op-*daemon* path
   keeps its Part 6 daemon-outcome lifecycle — a `Failed` bubbles to the
   parent handler — rather than the op router.)
6. **Daemon phase (Part 6).** ✅ DONE 2026-06-25 (phase-level; op-daemon
   collapse + `daemon: <N>` cap still pending). A phase `daemon: true`
   (`WorkloadPhase.daemon`, parsed bool/0|1/on|off) runs concurrently
   with its foreground siblings as one configured behaviour of the
   scenario shell's body: `run_scenario_body` partitions the children
   into foreground + daemon, spawns daemons FIRST onto their own
   `JoinSet` (no foreground permit — off-budget), runs the foreground,
   then latches a daemon-group `daemon_stop` flag and drains the daemons.
   `ExecCtx.daemon_stop` threads that flag onto the daemon phase's
   `Activity::daemon_stop`; its fibers poll it at the cooperative
   boundaries (alongside `stop_flag`/`stop_requested`/`walk_stop`) and
   exit — but `daemon_stop` is deliberately NOT in the `stopped` RETURN
   that decides failure, so a daemon stopped by foreground-completion is
   `Completed` (clean), while a daemon that ERRORS sets `stop_flag` →
   bubbles up through the handler. Its own phase scope gives it the
   independent cursor base (the motivating requirement). Tested:
   `nmbrs/tests/workload_shell_e2e.rs::daemon_phase_runs_concurrently_and_stops_with_foreground`
   + `…::daemon_phase_failure_bubbles_up`. PENDING refinements: the
   op-daemon collapsing into this same definition at the leaf; `daemon: N`
   (max-N activations); the until-stopped cursor primitive (today a daemon
   sizes its cursor — `until_elapsed` / a large cycle count — and
   `daemon_stop` cuts it when the foreground finishes); a scene-tree
   daemon marker.

7. **Scenario `ExecShell` shape.** ✅ DONE 2026-06-25. `executor.rs`:
   the `ExecShell` trait (Part 2 contract) + `ScenarioShell` (the first
   concrete instance) + an explicit `ShellHandler` / `ShellAction` (the
   scenario policy — the `*Failed:stop` default of Part 3). The dispatch
   (`run_scenario_body`, the former `run_siblings_concurrently`) is now
   structured BODY (the one SRD-02 concurrency path) → HANDLE (each
   joined child → `classify_child` → `handler.decide`) → AGGREGATE (a
   two-axis `Outcome` + the first failure reason, since `Outcome` carries
   no message); `execute_tree_at` projects that Outcome to the `Result`
   the rest of the walker still threads. Behavior-preserving (the
   handler reproduces the old `first_err` cascade; the Outcome→Result
   map matches the old Ok/Err). This is the shell hook daemon units
   (Part 6) partition.

Steps 1–2 (`Outcome` type, generalised router) and 5–6 remain. The
phase-shell breach is no longer a hardcoded stopgap — it is the composed
`ErrorPolicy`/`AggregateGuard` rule. Step 4 is complete: **a failed
phase now stops the workload** (a fault halt, non-zero exit), and both
not-yet-dispatched and already-in-flight concurrent siblings stop. The
scenario `ExecShell` shape (step 7) is in place; the remaining
unification is the **generalised router** (step 2 — wiring the scenario
`errors:` config into `ShellHandler` instead of the hardcoded
`*Failed:stop`), flowing a real child `Outcome` up through `execute_node`
/ `run_phase` (today the shell reconstructs it from each child's
`Result` via `classify_child`, faithful because the per-phase breaker
turns a soft-fail into an `Err`), and the **phase / stanza / op shells**
implementing `ExecShell`.

---

## Part 6 — Daemon units (a shell-unit lifecycle, level-agnostic)

A **daemon** is not a level — it is a *lifecycle property of one unit
within a shell's body*. The op-level `daemon:` (a long-running op that
runs alongside its sibling ops and is stopped when the phase's
foreground ops finish — `activity.rs`, `daemon_cancel_grace_ms`,
`daemon_cancelled_total`/`daemon_errors_total`) is the **leaf instance**
of a behaviour that, by the recursion of Part 2, belongs to *every*
shell. The same property on a **phase** unit is the "daemon phase": a
phase that runs alongside its sibling phases and is stopped when its
scope's foreground phases finish. One definition; the shell level is the
only variable. This is a *unification* — not a new feature, and not the
op-daemon copied down a level into a parallel implementation.

### The behaviour (identical at every shell)

A daemon unit:

1. **runs on the daemon concurrency group, off its parent's foreground
   budget.** It is dispatched through the *one* concurrency path
   (SRD-02) like every unit, but in a group with its own limit — it
   never holds a foreground permit, so it cannot block or starve the
   foreground body. The off-budget lane is **configuration of the one
   path, not a private mechanism**. (This is the rule the existing
   op-daemon's off-cycle-pool fiber must be reconciled to — see
   Rollout.)
2. **runs concurrently with the foreground units** of the same shell.
3. **is stopped when the shell's foreground body reaches its natural
   end** — the parent signals the daemon's `stop_flag` with
   `StopCause::Interrupt` (Part 4) within a `cancel_grace` window; past
   the window the parent records a daemon-shutdown fault. Cancelled /
   errored daemons are accounted (`daemon_cancelled_total` /
   `daemon_errors_total`), as at the op level today.

### The level is the scope you isolate

The level is not arbitrary. Each shell owns its own scope (SRD-18c):
ops inside a phase **share** the phase's cursor base, so an op-daemon
cannot read from a different base than its sibling ops — the isolation
boundary is the phase. A daemon **phase** owns its own scope, hence its
own cursor base. You therefore choose the shell whose scope gives the
isolation the daemon needs; "daemon at the phase shell" and "distinct
cursor base" are the same statement. (This is the motivating
requirement: a sparse read that must pull from a different base than the
phase it shadows.)

| | unit | parent shell | stopped when | scope it gets |
|---|---|---|---|---|
| op-daemon | op | stanza / phase | phase foreground ops done | shares the phase scope |
| daemon phase | phase | scenario / scope | scope foreground phases done | its own phase scope (own cursor base) |

### Termination is stop-driven, not body-exhaustion

A normal unit ends when its body runs out (cursor exhausts, cycles
complete). A daemon unit's body is **open**: its cursor is the *data
source* (it wraps — `mod_in` over its base), its `rate:` is the
cadence, and the loop runs until the stop signal arrives. This is the
op-daemon's `while + rate` lifted to the unit: the daemon iterates its
own base at its own pace and exits on the parent's foreground
completion, not on its cursor.

### Outcome of a daemon unit (Part 1)

A daemon stopped by its group is `Interrupted + Succeeded` by default —
halted before a natural end (there is none), but its work is clean and
re-usable; it is **not** `Failed`. A daemon that errors is
`Interrupted + Failed` and feeds the parent shell's handler like any
child `Outcome`, so the scenario-graph default `*Failed:stop` (Part 3)
applies to a failed daemon exactly as to a failed foreground phase. A
shell's own `Disposition` is decided by its **foreground** body —
daemons never keep a shell `Running`.

### Handler framing (Part 3 / SRD-83)

"Stop when the foreground body completes" is itself a **stop rule**, not
bespoke control flow: a daemon unit carries an implicit stop condition
keyed to its parent shell's foreground `Disposition` reaching
`Completed` (an SRD-83 predicate over runtime state). The parent shell
emits the `stop` action against its daemon group when its foreground
aggregate completes. Nothing in the dispatch loop special-cases
"daemon" beyond partitioning the body into a foreground group and a
daemon group; everything else is the existing handler + stop-cause
plumbing of Parts 3–4.

### Configuration surface

- `daemon: true | <N>` on a **phase** (parallels the op-level
  `daemon:`); `<N>` caps concurrent activations.
- `daemon_cancel_grace_ms` — per-unit override of the shutdown grace.
- **Lifetime binding (decision — recommended default):** a daemon unit
  is stopped by its *enclosing shell's* foreground completion. To bound
  it to a subset of siblings, nest them in a sub-scope — scenario
  includes are retained as `Scope` nodes (SRD-18b, *not* flattened), so
  a `scenario: rampup_window` containing `[rampup, sparse_read(daemon)]`
  stops the daemon at `rampup`'s end. *(Alternative considered: a
  `during: <phase>` binding to a named sibling — rejected as default;
  nesting already expresses it with the structure that exists, and a
  named binding reintroduces a second way to say "this group.")*
- **Failure (decision — recommended default):** a daemon's `Failed`
  Outcome bubbles to the parent handler (default `*Failed:stop`), like
  the op-daemon's "failures stay in scope." A best-effort daemon opts
  out per-unit with `errors: "*Failed:warn"`.
- **Concurrency budget (decision — recommended default):** daemon units
  carry their own `concurrency:` and run off the foreground limit; the
  scope's `concurrency_limit` governs the foreground group only.

### Rollout

The scenario shell now exists (Part 5 step 7): `ScenarioShell` /
`run_scenario_body` with the explicit body → handle → aggregate
structure. A daemon unit is one configured behaviour of that body:
partition the body into foreground + daemon groups; dispatch both
through the one concurrency path (daemon group: own limit, off the
foreground semaphore); await the foreground; signal the daemon group's
stop (`StopCause::Interrupt`) + grace; await the daemons; fold their
Outcomes through the handler. The existing op-daemon then **collapses
into this definition** at the leaf — the same `stop_flag`, grace, and
counters, with its off-cycle-pool fiber reframed as the stanza/phase
shell's daemon group (the SRD-02 reconciliation above). The partition
point is `run_scenario_body`'s dispatch loop (the foreground group is
today's semaphore-gated `JoinSet`; the daemon group is a sibling group
with its own limit + the walk-stop linkage already wired via
`Activity::walk_stop`).

The one genuinely new primitive is the **until-stopped loop** (a daemon
unit's open body): a unit that iterates its own cursor at its `rate:`
and terminates on the parent's foreground completion rather than on
cursor exhaustion. Mechanically it is the open-extent `until_elapsed`
cursor (SRD-71) with "until external stop" in place of "until time
budget."

---

## Error-rate guard: visible synthesis (2026-07-09, catchup D1)

The future shape recorded in §"AggregateGuard retired as a default"
is implemented. An opted-in `error_rate_max` no longer routes through
a hidden expression inlined in `build_for_phase`; it becomes a
first-class `StopConditionDecl` via the ONE canonical constructor
(`StopConditionDecl::error_rate_guard`), inserted at the head of the
phase's `stop_when` list during config assembly and announced at
INFO like every synthesized layer:

    phase 'X': error_rate_max=0.2 → stop_when: cycles_total >= 50 && to_f64(result_failure) > (to_f64(cycles_total) * 0.2) (synthesized)

`build_for_phase` is one uniform loop over declared conditions —
no special-cased first arm, no condition the operator can't see
before it trips. The trip keeps its established
`error_rate_exceeded` class via the decl's `reason` override.

## Panic reporting: one full render (2026-07-09, catchup C3)

A per-cycle panic used to print its full enriched diagnostic
(violation + original location + node + inputs) up to four times:
the std panic hook, the phase footer row, the `errors:` block, and
the `stopped by error handler:` headline. Every layer was correct
in isolation; together they made one root cause read as four.

The contract now:

- **The `errors:` block owns the full render.** The
  `PhaseErrorDetail.message` carries the complete enriched text,
  unconditionally on the failure path — it is the one place the
  full diagnostic is guaranteed to appear.
- **Headlines are first-line short form.** `stop_reason`
  composition (the per-op panic catcher, the fiber-boundary
  catcher) and the phase footer row render only the first line of
  the message. Multi-line text in a status row was a display bug
  in its own right.
- **The panic hook degrades to one short line when a downstream
  reporter exists.** The runtime declares
  `polydat::set_panic_reporting_downstream(true)` when it installs
  its fiber machinery; the enrich-and-re-raise hook then prints a
  single first-line notice instead of the full body + backtrace
  pointer. Bare polydat consumers (tools, tests, library callers)
  never set the hint, so the hook's full print — the guarantee
  that a panic is never invisible — is unchanged for them. The
  hint means "a reporter exists", not "suppress": the full text
  still travels in the panic payload, and the errors block renders
  it regardless of what the hook printed.

## Invariants (axioms this SRD adds)

- **Every execution level is the same shell.** Scenario, phase,
  stanza, op differ only in granularity and configured handler — never
  in mechanism. A new bespoke per-level error path is a regression.
- **Daemon is a unit lifecycle, not a level.** The off-budget,
  run-alongside, stop-with-the-group behaviour is defined once and
  specialised by shell; the op-daemon and the daemon phase are the same
  property at different granularities. A per-level daemon mechanism is a
  regression, same as a per-level error path. The shell level is chosen
  for the scope it isolates (SRD-18c).
- **Result = (Disposition, Validity), always two axes.** No single
  status enum mixing "how much ran" with "is it usable." Consumers key
  off the axis they care about.
- **Errors and stops are handler decisions, not control-flow
  specials.** "Stop on error", "fail on >N% errors", "retry timeouts"
  are all rules in one router grammar, configured at the shell whose
  children produced the result.
- **`stop` carries a cause.** `Interrupt` (clean, user) vs `Fault`
  (a `fail`-driven stop) are distinct so the run's `Validity` is
  recorded correctly and partial results can be re-used or discarded
  deliberately.
