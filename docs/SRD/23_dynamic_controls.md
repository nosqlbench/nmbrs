# 23: Dynamic Controls

> **Status.** Design committed and shipping end-to-end across
> the runtime. The following integration points are all in code:
>
> - `crates/nmbrs-metrics/src/controls.rs` — `Control<T>`,
>   `ControlBuilder` (`final_at_scope`, `branch_scope`,
>   `from_f64`), `ControlRegistry`, `ErasedControl::set_f64`.
> - `crates/nmbrs-rate/src/applier.rs` — `RateLimiterApplier` +
>   `RateLimiter::reconfigure`.
> - `crates/nmbrs-runtime/src/fiber_pool.rs` — `FiberPool` +
>   `ConcurrencyApplier`; wired into
>   `Activity::run_with_adapters`, declared on the activity's
>   component at `attach_component` time.
> - `polydat/src/nodes/runtime_context.rs` — `control`,
>   `control_u64`, `control_bool`, `control_str`, `control_set`,
>   `rate`, `concurrency`, `phase`, `cycle`. The Polydat compiler
>   threads the enclosing DSL binding name into `ControlSet`
>   for attribution (`ControlOrigin::Gk { binding }`).
> - `crates/nmbrs-runtime/src/runner.rs` — `dryrun=controls` renders the
>   component tree after phase construction.
> - `crates/nmbrs-tui/src/app.rs` — inline `e` keybind + `ControlEditPrompt`
>   with validator / final-scope error surfacing.
> - `crates/nmbrs-web/src/routes.rs` — `GET /api/controls` and
>   `POST /api/control/{name}` with structured error bodies.
>
> Outstanding: control-value types richer than `f64`/`u64`/
> `String`/`bool` (e.g. enum-valued `errors` policies) rely on
> the info-family metric pattern (§"Non-numeric controls: info
> family"); the info-family emit path is documented but not yet
> implemented.

Some workload parameters need to move at runtime. Concurrency has
to follow load, a rate limiter has to respond to SLO breaches, an
operator has to throttle a runaway phase without restarting the
run. The SRD-21 `params:` block covers *static* configuration —
values set at launch and read once. This document sketches a
complementary mechanism: **dynamic controls** that a workload,
operator, or automation can change while a phase is running, with
live consumers picking up the new value on their next read.

Controls are the control-plane instance of the broader GK
reification pattern (SRD 10 §"GK as the unified access surface"):
a runtime-mutable value is reified as a GK-addressable name
(`control("rate")`) so workloads, operators, and scripts all
read and write through one surface instead of inventing
per-subsystem access paths. The read-side of the same pattern
(`metric(...)`, `rate`, `concurrency`, `phase`) is catalogued in
SRD 12 §"Runtime context nodes".

North stars:

- **Typed**, not string-map. We have the type system; use it.
- **Scoped to the component tree**, not a global registry.
- **Multi-observer by default**. Multiple consumers subscribe to
  one control; one writer pushes updates.
- **Safe under concurrent writers**. Two sources trying to set
  the same control converge on a well-defined last-write-wins
  order with a monotonic revision number.
- **Reactive-friendly**. A Polydat feedback loop that reads
  `metric("error_rate")` and writes `control("rate")` is a
  first-class use case, not an add-on.
- **Idiomatic Rust**. Leverages `tokio::watch`, `Arc<Atomic*>`,
  and the component tree already on the floor. Avoids
  reinventing a `Send + Sync` event bus.

---

## Prior art — nosqlbench (Java)

The Java engine handles runtime parameter mutation with a single
pair of abstractions:

- **`ParameterMap`** — a `ConcurrentHashMap<String, Object>` with
  an `AtomicLong changeCounter` and a `LinkedList<Listener>`.
  Every `put`/`remove`/`set` increments the counter and fires
  every registered listener.
- **`ActivityDefObserver`** — a visitor interface. The
  `ActivityExecutor` is the primary listener; on each parameter-
  map update it calls `activity.onActivityDefUpdate(def)` and
  walks its motors calling `observer.onActivityDefUpdate(def)`.
  Motors, inputs, and actions optionally implement the interface
  to re-read their own parameters.

Mutation surface:

- **Scenario scripts** (Groovy / JavaScript) call
  `activities.<alias>.threads = N`, which round-trips through
  `ActivityDef.setThreads(int)` → `parameterMap.set(...)` →
  listeners.
- **`NBEvent::ParamChange`** — a typed event carrying
  `SetThreads`, `CycleRateSpec`, `StrideRateSpec`, etc. Activity
  consumes it in `onEvent` and dispatches to rate-limiter
  re-creation / motor count adjustment.

What's good in that design:

- One atomic source of truth per activity; observers pull from
  it when poked.
- The listener pattern lets rate limiters, thread-pools, and
  error handlers all react to the same events without knowing
  about each other.
- The change counter gives polling consumers a "what's new?"
  question without a subscription.

What's worth reconsidering for Rust:

- **Everything is `String`** in `ParameterMap` — values, keys,
  the whole wire format. Consumers parse on read. Type-safety is
  only enforced downstream.
- **Listener list is unbounded** and synchronous; a slow
  listener blocks the writer. `ConcurrentHashMap` is
  thread-safe, but the `LinkedList<Listener>` isn't — the code
  races if listeners are added during notification.
- **No scope other than activity.** A phase-scoped control
  (e.g., concurrency that applies only to the currently-running
  phase) requires punching through to the per-phase component
  externally.

The nmbrs component tree + tokio primitives give us better
building blocks. Core pattern carries over; the implementation
doesn't.

---

## What's idiomatic for nmbrs

A control is a **named, typed, observable cell** attached to a
component node. Operators / scripts / automation read and write
it by component path; consumers subscribe for reactive updates.
Components that own their current values already (e.g., the
`Component::props` map, the phase bindings context) continue to
do so — controls are the live wire for the subset of values that
change over a run.

### Confirmed-apply delivery (no lossy notification)

A control write is not a broadcast-and-forget — it's a
**confirmed apply**. The caller that sets a new value does not
proceed until every registered consumer has acknowledged the
change is in effect. If any consumer fails, stalls, or times
out, the write surfaces as an error and the control's
committed value is not advanced. This is deliberately
different from a `tokio::sync::watch` channel, which would
drop intermediate values when a subscriber lags: under this
model, a lagging or broken consumer is a signal that the
control input was ineffectual, and the operator needs to know.

Consequences:

- Subscribers don't "observe" a control — they **register an
  applier**, an async function from `(new_value) -> Result<(),
  ApplyError>`. The control coordinator holds the list and
  drives it on each set.
- A `set()` call runs every applier (concurrently, with a
  per-applier timeout) and gathers results. All-or-nothing:
  if any applier returns `Err` or times out, `set()` returns
  `Err` with the aggregated failure list. The committed value
  remains the previous rev.
- The caller has a simple postcondition: when `set()` returns
  `Ok(rev)`, the new value is live in every consumer that was
  registered when the write started. Subsequent writes observe
  that state as the baseline.

### Three primitives

1. **`Control<T>`** — a coordinator cell that owns the
   committed value, its revision, the list of registered
   appliers, and an optional validator. All writes go through
   `set(value, origin) -> Result<Rev, SetError>`.

2. **`ControlTarget`** — a label-based identifier for the
   component (and thus the control registry) a write is aimed
   at. Canonical component identity at runtime is dimensional:
   labels like `type=phase, name=rampup, profile=label_00`
   — the same labels the metrics system uses for series
   selection. A target is a `Selector` (SRD 24) paired with a
   control name inside the matched component. All selector
   grammar — exact, glob, presence predicates, scope-aware
   lookup from any subtree — is defined in SRD 24; this
   document consumes that facility rather than redefining it.

3. **`ControlRegistry`** — the per-component store of locally-
   declared controls. A read for `name` within a component
   walks up the parent chain until the name resolves,
   mirroring `Component::get_prop`. Writes address a specific
   component (resolved via a `ControlTarget` selector) and
   affect only that component's registry — they never
   accidentally punch through to a parent.

### Sketch (subject to the component-lookup prerequisite)

```rust
pub struct Control<T: Clone + Send + Sync + 'static> {
    committed: RwLock<Versioned<T>>,
    appliers: Mutex<Vec<Arc<dyn ControlApplier<T>>>>,
    validator: Option<Box<dyn Fn(&T) -> Result<(), String> + Send + Sync>>,
    apply_timeout: Duration,
}

pub struct Versioned<T> {
    pub value: T,
    pub rev: u64,                 // monotonic, session-unique
    pub updated_at: Instant,
    pub origin: ControlOrigin,
}

#[async_trait]
pub trait ControlApplier<T>: Send + Sync + 'static {
    /// Make the new value take effect. Return `Err` if the
    /// apply could not be completed — the set() call that
    /// drove this will propagate the failure back to its
    /// caller. May not mutate the subscriber's state if it
    /// returns `Err`; at minimum, state should be consistent
    /// with either the old or the new value.
    async fn apply(&self, value: &T) -> Result<(), String>;
}

pub enum ControlOrigin {
    Launch,                       // initial seed from params
    Cli,                          // `nmbrs ctl ...` (future)
    Tui,                          // keybind / input
    Polydat { binding: String },       // scripted feedback loop
    Api { source: String },       // web endpoint caller id
}
```

A rate limiter's applier is illustrative:

```rust
#[async_trait]
impl ControlApplier<Option<RateSpec>> for RateLimiterApplier {
    async fn apply(&self, value: &Option<RateSpec>) -> Result<(), String> {
        match value {
            Some(spec) => self.limiter.reconfigure(spec).await
                .map_err(|e| format!("rate reconfigure: {e}")),
            None => self.limiter.disable().await
                .map_err(|e| format!("rate disable: {e}")),
        }
    }
}
```

The writer-side contract is simple: either the new rate is in
effect when `set()` returns `Ok`, or no visible state changed
and `set()` returned `Err`.

---

## Controllable parameters (initial scope)

Controls live at the smallest component scope they can
meaningfully affect. A value can be declared at a higher scope
and inherited downward via the walk-up.

| Control | Scope | Type | Notes |
|---|---|---|---|
| `concurrency` | phase | `u32` | Fibers the executor maintains. Re-balanced on every change (scale up by spawning, down by cooperative-exit at the next op boundary). |
| `rate` | phase | `Option<RateSpec>` | Cycle-rate limiter. `None` disables the limiter entirely. |
| `errors` | phase | `ErrorSpec` | Error-handler spec (`stop`, `retry:N`, `ignore`, …). Re-parsed on each write. |
| `max_retries` | phase | `u32` | Retry cap. Today compiled into the dispenser; hoist to a control so it can track `errors` changes without a phase restart. |
| `log_level` | session | `LogLevel` | Live log-filter tuning. |

**Shipped status:** `concurrency` and `rate` are wired end-to-end
(declared via `control_catalog::{CONCURRENCY, RATE}`); `errors`,
`max_retries`, and `log_level` remain planned. Adapters contribute
their own beyond this core set — e.g. `cql_trace_rate` (the
`cassandra-cpp` driver). The live, build-accurate inventory is
`nmbrs describe controls`; this table is the core design scope.

Metric cadences (SRD 42) are not part of this v1 because they
are a universal platform-level concern rather than a dynamic
control — the cadence tree is declared by the runner and
consumed by every sink (SQLite, VictoriaMetrics push, TUI,
summary report). Making cadences mutable at runtime is a
larger change that belongs in its own proposal.

Non-goals for v1:

- Changing `workload=` / `scenario=` — these are startup-only
  identity decisions, not controls.
- Changing Polydat kernel program source — immutable per phase
  (dispenser compiles once at phase start). If a control affects
  something inside the kernel, the kernel re-reads via a
  `control(name)` node, not by recompilation.
- Adding new phases mid-run.

---

## Mutation entry points

Five prospective writers, each with its own
`ControlOrigin` variant:

1. **Launch (`ControlOrigin::Launch`).** Initial values from
   `params:` + CLI overrides at scenario start. Not really
   "dynamic" — but it's how each control is seeded.

2. **TUI (`ControlOrigin::Tui`).** Keybinds — `+` / `-` to
   nudge concurrency, a `/`-style input for typing `rate=500`,
   etc. Needs display chrome so the user sees the current value
   and where the next edit would go. Ties directly into the
   per-phase tree detail block (SRD 62) since that's where
   each phase's live controls will be surfaced.

3. **GK-driven feedback loops
   (`ControlOrigin::Gk { binding }`).** A new Polydat node family —
   `control_set(name, value)` and `control_get(name)` — lets a
   binding reach into the active phase's control registry.
   Combined with the existing metric-reading nodes
   (`metric()`, `metric_window()`), this is the path for
   self-adjusting scenarios:

   ```polydat
   err_rate := metric("errors/s") / metric("cycles/s")
   target_rate := control_get("rate") * if(err_rate > 0.05, 0.8, 1.05)
   capture nonce = control_set("rate", target_rate)
   ```

   Needs a guard against the loop: control writes are
   rate-limited per GK-node invocation, and a kill-switch
   disables Polydat writes if the runner detects thrash.

4. **Web API (`ControlOrigin::Api`).** When `nmbrs-web` is built
   in, `POST /control/:path/:name { value, origin }`. Mirrors
   the component path resolution the TUI uses. Authentication
   out of scope for v1.

5. **Programmatic / test
   (`ControlOrigin::Cli`).** Test harnesses and the CLI's
   existing `key=value` mutation at CLI-parse time. No
   `nmbrs ctl` command yet — that's a follow-up.

---

## Scoping and inheritance

Controls live on components. A phase component declares
`concurrency`; the phase executor reads it via the phase's
control registry. If the session component defines
`concurrency` and the phase doesn't, reading from the phase
walks up to the session's definition. Same rules `get_prop`
uses today — but with the dynamic/observable layer.

Scope at a glance:

```
Session
├── control "log_level"        scope: session-wide
├── control "concurrency"      scope: default for every phase
└── Activity (phased)
    ├── Phase "rampup" (running)
    │   ├── control "concurrency"  ← OVERRIDES session default
    │   └── control "rate"
    └── Phase "ann_query" (pending)
        └── (no own controls — inherits "concurrency" from session)
```

Writers address a control by a `ControlTarget` (label
predicate + control name) resolved against the live component
tree; the writer chooses the scope by matching the intended
component. Readers never see the scope explicitly — they ask
their own component for `name` and the walk-up returns the
nearest declaration.

---

## Branch-scoped and final controls

Two declaration modifiers shape how a control value
propagates and whether it can be overridden at runtime.

### Branch scope — "configure this subtree"

Some settings are naturally a property of a subtree rather
than a single component. HDR histogram significant digits
(SRD 40) are the canonical example: every timer instantiated
anywhere below the session root should pick up the session's
configured precision. The same pattern applies to any
cross-cutting knob that affects how components in a branch
are constructed or operate — default capture mode for a
wrapper chain, an adapter's default timeout, a log-level
override for one phase's subtree.

A **branch-scoped control** is declared on the root of the
subtree it governs. When set, its applier semantics are
"configure every component currently in the subtree, and seed
every component instantiated in the subtree after this write
from the new value." This means:

- **Existing components in the subtree** apply the new value
  the same way a non-scoped control's applier would — via a
  registered applier per component that owns the relevant
  piece of state.
- **New components** created after the write read the current
  committed value at construction time, so they start out
  already configured. No retroactive reconstruction.
- **Components that have moved past the point where they can
  accept a change** (e.g. an HDR histogram whose bucket
  layout is fixed once the reservoir is created) are not
  retrofitted. The control's contract is "this applies from
  here forward for new reservoirs" — callers that need a
  live swap must restart the affected component, as with any
  other immutable-at-construction property.

Branch scope is a property of the control declaration, not of
the write. A control declared branch-scoped at the session
root governs the whole session; one declared branch-scoped on
a phase governs that phase only. Writers still address the
control by `ControlTarget`; the scope decides how far the
applier walks.

### Final declarations — "this scope fixes the value"

A control can be declared `const` at a parent Polydat scope. A
final declaration pins the committed value for the scope and
**rejects runtime writes targeting descendants of that scope
with a logical error**, not a silent drop. The write surfaces
as `SetError::FinalViolation` (parallel to `ValidationFailed`
and `ApplyFailed`) and the writer sees the scope where the
final declaration lives.

This is the counterpart to Polydat's existing compile-time
strictness for workload params: if an author says "this
phase runs at `concurrency=1`, period", a runtime control
write that tries to override it is an operator error, not a
silently-accepted nudge. The error message names the
final-declaring component so the operator knows where the
pin lives.

Testability: the final-violation path is a required test
case — concurrency, rate, and log-level controls each need a
test that declares `const` at a parent, attempts a runtime
write at a descendant, and asserts the specific
`SetError::FinalViolation` outcome plus an unchanged
committed value. Otherwise a regression could silently
convert final pins to ordinary defaults.

---

## Consistency model

- **Monotonic revisions, session-wide.** Every successful
  write to any control allocates a new `rev` from a session-
  wide `AtomicU64`. Consumers can log or compare by rev;
  replay tools can order events. Within one control, rev is
  strictly increasing.
- **Writes are serialized per control.** Each `Control<T>`
  serializes concurrent writers through an internal mutex
  (`Control::set` holds the lock across validate → fan-out
  → acks → commit). Two concurrent callers see their calls
  completed in whatever order they acquired the lock; neither
  overlaps with the other's fan-out.
- **All-or-nothing apply.** A write fans out to every
  registered applier concurrently, with a per-applier
  timeout. If every applier returns `Ok`, the value is
  committed and `set()` returns the new `rev`. If any applier
  returns `Err` or times out, `set()` returns an aggregated
  error and the committed value is NOT advanced — the old
  value remains authoritative for readers and for the next
  write. Appliers that already succeeded during a failed
  write continue to hold the new value locally; the
  next successful write re-synchronizes them, and a failed
  write is a correctness signal to the operator that the
  control input was ineffectual.
- **Validation at write.** `Control<T>` carries an optional
  validator run before fan-out. Bad values return `Err` to
  the writer without disturbing any applier. Example: a
  concurrency validator rejects `0` and `> 10_000`; a
  rate-spec validator parses the string.
- **No automatic rollback across applies.** If one applier
  out of N fails after the others succeeded, we do not
  attempt to un-apply the successful ones. That would require
  a second full fan-out with the same failure surface, and
  deadlocks when the reason the first applier failed is
  persistent. The caller sees the error, inspects which
  appliers failed, and decides whether to re-set with a
  different value or roll back explicitly.

---

## Integration points

### Rate limiter

`RateLimiter` today starts once and runs immutably until `stop`.
Add a `reconfigure(&self, spec: &RateSpec) -> Result<(), String>`
(and `disable(&self)`) method, then register the limiter as a
`ControlApplier<Option<RateSpec>>` on the phase's `rate`
control. The applier's body calls `reconfigure` / `disable` and
returns the result; `Control::set` awaits it before returning
to the writer.

### Fiber executor

The executor maintains a pool sized by the phase's
`concurrency` control. On every change it:

- **Scales up** by spawning new fiber tasks.
- **Scales down** by signaling the tail-end fibers to exit at
  their next op boundary (no mid-op termination — a fiber
  always finishes its current op).

Keeps the change graceful and bounded in time to one cycle.

### Dispensers and op wrappers

Any component — including dispensers, op wrappers, and
adapter-provided machinery — is allowed to declare and
subscribe to controls. The rule isn't "dispensers don't
subscribe"; the rule is "controls can't change a component's
identity or compiled form without restarting that component".

Concretely:

- **Compiled op structure.** Changes that would require
  recompiling the op template (e.g., swapping a CQL
  `consistency` baked into a prepared statement) need a
  phase restart. Those don't get registered as controls.
- **Wrapper behavior.** An `explain` op wrapper that can be
  toggled on or have its sampling rate dialed — or any
  similar knob that affects *how* a dispenser runs without
  changing *what* it produces — is exactly the kind of thing
  a dynamic control is for. The wrapper registers the
  control when the dispenser (or the wrapper instance) is
  constructed and ties its own applier to the new value.
- **Per-adapter extensibility.** Adapters
  registered via the `inventory` crate can contribute their
  own controls at adapter-registration time, at op-template-
  instance time, or at dispenser-instance time. The
  registration API is the same simple tuple every other
  control uses: `(component instance, name, value type,
  applier)`.

The guiding principle is that a control is attached to the
component whose behavior it affects — and any component is
welcome to declare one.

---

## TUI surface (sketch)

Under SRD 62's per-phase detail block, each running phase shows
its current control values with a subtle edit affordance. Exact
chrome left for the TUI design pass; the data plumbing is:

- Detail rows read current values via
  `phase.component.control::<T>(name).borrow()`.
- An edit keybind (e.g., `e`) opens an inline prompt anchored
  to the selected control.
- Submitting the prompt calls `control.set(value,
  ControlOrigin::Tui)`. Validator errors render inline.

Control-value changes flow through the gauge reification
(see §"Reification as metrics" below), so the TUI picks up
new values on its next metrics-drain tick — same path as any
other live stat. No custom notification wiring needed at the
panel level.

---

## Enumeration: controls are structural

Controls are a **runtime structural property of a component**,
declared when the component (or a wrapper on top of it — a
dispenser, an op wrapper, an adapter-registered bit of
machinery) is instantiated. They are not created implicitly
by a writer submitting a value; writers only look up and
write. A target that doesn't exist produces an immediate
`LookupError::NotFound` on the write — no silent side effects.

Because controls live on components, they are enumerable
through the component tree. Two direct consequences:

- **`dryrun=controls`** — a new flag that walks the session
  tree after component construction, prints every control
  declared by every component, along with its type, current
  (seeded) value, and scope-inherited behavior. This is the
  discovery UX that lets a user see "what can I turn the
  knobs on?" without having to read code.
- **Listing API** — `Component::list_controls()` on any
  node returns the declared controls, used by the web API
  (`GET /controls`), the TUI (edit affordance
  surfacing), and anything else that needs to render the
  catalog.

### Two enumeration tiers — instance vs capability

The component-tree enumeration above is the **instance tier**:
it shows what *this run* declared. That tier alone is not
enough for discovery, because declaration is *conditional* —
`concurrency` is always declared, but `rate` only when a phase
sets `rate:`, and `cql_trace_rate` only when the
`cassandra-cpp` driver backs the `cql` adapter. So a user who
never sets `rate:` can't learn from a live tree that `rate` is
servo at all. That is the discoverability asymmetry between
`concurrency` and `rate`.

The complementary **capability tier** closes it. Each control
has a static [`ControlDesc`](../../crates/nmbrs-runtime/src/control_catalog.rs)
— `name`, value-type, default, range, unit, doc, and a
`declared_when` condition — that is the **single source of
truth**: the imperative `declare` *derives* the live control
from it (`ControlDesc::build_u32` / `build_f64` / `build_rate`
supply the name / range / gauge; only the instance-specific
applier is wired separately), and the discovery surface reads
the same descriptor, so the two cannot drift. Owners contribute
their descriptors where they already live:

- **core** controls (`concurrency`, `rate`) — `control_catalog::core_controls()`.
- **adapter** controls — each adapter's
  `AdapterRegistration::supported_controls` (e.g. the `cql`
  adapter contributes `cql_trace_rate`, feature-gated so a
  scylla-only binary doesn't advertise a knob it can't declare).

`control_catalog::all_controls()` unions them. Surfaced by:

- **`nmbrs describe controls [<name>]`** — the static capability
  catalog: every control the binary *can* declare, each with
  its `declared_when` condition, listed without running
  anything. `describe adapter <name>` shows that adapter's
  controls too.

The capability tier answers "what knobs does this binary
support, and when do they appear?"; the instance tier
(`dryrun=controls`) answers "what knobs are live in *this*
run?". Both read controls as structural — neither invents one.

Because declaration is structural, the type of a control's
value is known at registration time — applier types are in
sync with written values without runtime casting. Writers
that supply the wrong type fail at validation, not at apply.

---

## Reification as metrics

Every control's current value is **also published as a gauge
metric** with dimensional labels drawn from the declaring
component. This means:

- Controls appear in the SQLite metrics database, the
  VictoriaMetrics push stream, the summary report, and any
  other metric sink — with no extra plumbing. One control
  per sink, not two.
- Long-running scenarios produce a time series of control
  values that sinks can graph, aggregate, and query alongside
  throughput and latency data.
- The summary report's table can surface the "final value"
  for each control by the same mechanism that surfaces
  `mean(recall) over profile~label` — control-value gauges
  live in the same label space as everything else.
- The replay / post-run question "what was the rate set to
  between minute 12 and minute 20?" becomes a standard
  metric-window query.

Each `Control<T>` registers a gauge on its owning component
at declaration time; on every successful write the gauge's
value updates.

### Non-numeric controls: info family

Gauges are `f64`-valued. Enum-valued controls
(`errors=stop|retry|ignore`), bool-valued, and string-valued
controls don't have a natural numeric projection, so they
reify as an **info family** rather than as a numeric surrogate
with a side-channel label.

Specifically: the control publishes a companion metric named
`control_info.<name>` whose value is a constant `1.0` and
whose label set includes the symbolic value under a
`value="..."` label (plus the usual component labels).
Readers filter / group by `value` the same way they would
any other label dimension. This matches the OpenMetrics
"info" pattern — a metric that exists only to project a
categorical attribute into the label space so every downstream
sink (SQLite, VictoriaMetrics push, summary report, TUI) can
consume it through the same filter/group machinery it uses
for every other metric.

Rationale for info family over a numeric encoding:

- **No synthetic ordinal.** An enum's integer surrogate is a
  schema you have to invent, version, and keep in sync with
  the code. The info family carries the symbolic name
  verbatim and stays correct across control enum changes.
- **Queryable as an attribute.** "Which phases were running
  with `errors=stop`?" is a `group by value` question over
  `control_info.errors`; no external decoder lookup.
- **Uniform fallback for missing projections.** When a
  numeric-typed control has a variant with no meaningful
  numeric projection (e.g. a `Mode::Off` case), the
  reified-gauge code simply emits no sample for that
  variant (SRD 23 §"Reification as metrics" already permits
  this via `to_f64` returning `None`). A companion
  `control_info.<name>` sample still fires, so the
  "what's the control set to?" question always has an
  answer.

---

## Non-goals (resolved from prior open questions)

- **Grouped / transactional writes across multiple controls.**
  Every `Control<T>::set` commits one control's value. There
  is no group-commit form. Controls are independent by design:
  each one owns its own validate → fan-out → commit flow, and
  two separate control writes are two separate revs. Callers
  that need multi-control correctness (e.g. "change
  `concurrency` and `rate` together") coordinate externally —
  sequence the writes, check each result, roll back or re-try
  by calling `set` again. The applier protocol stays
  single-shot; no two-phase extension.