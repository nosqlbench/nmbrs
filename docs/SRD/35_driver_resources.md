# SRD-35 — Driver Resource Lifecycle and Sharing

**Status:** design (not yet implemented)
**Owner:** runtime / executor + adapters
**Implementation target:** `crates/nmbrs-runtime/src/resource_pool.rs` (new),
  `crates/nmbrs-runtime/src/adapter.rs` (extension), per-adapter
  driver-instance impls
**Cross-refs:** SRD-30 (adapter interface), SRD-31 (op pipeline),
  SRD-04 (umbrella options), SRD-19 (component tree),
  SRD-50 (CQL adapter — primary prototype)

---

## What this SRD is for

Adapters today conflate two concepts in one type: the *driver
shell* (op-template analysis, dispenser construction, statement
preparation, field mapping) and the *driver instance* (the
heavyweight network-side object — a CQL `Session`, an HTTP
client with its connection pool, an OpenAPI runtime). Today
the executor calls `DriverImpl::create(...)` once per phase,
which builds a fresh shell **and** a fresh instance. For drivers
where the instance is cheap or stateless, that's free; for
drivers like the Cassandra C++ engine where the instance carries
a libuv loop, a worker-thread pool, and dozens of TCP sockets,
that's a per-phase open/close storm. Even short workloads can
exhaust per-process limits (`RLIMIT_NOFILE`, `RLIMIT_NPROC`)
when the driver's teardown is asynchronous and lags behind the
phase open rate.

This SRD specifies the **driver resource pool**: a generic
runtime layer that lets adapters declare which params identify
"the same instance," a policy knob that lets users elevate
isolation per workload, refcount-based lazy lifecycle bound to
the pre-mapped scenario graph, and explicit start/end events
for every shared-instance lifecycle transition. The CQL adapter
is the prototype consumer, but nothing about the layer is
CQL-specific.

The load-bearing rule this SRD establishes:

> **Resource sharing is the default, isolation is opt-in, and
> every shared instance has a bounded lifecycle marked by
> debug-level start/end events.**

---

## Vocabulary

- **Driver shell.** The cheap, per-phase object that knows how
  to take an op template and produce an `OpDispenser`. Holds an
  `Arc<dyn SharedResource>`. Constructed per phase; cheap to
  recreate.
- **Driver instance.** The heavy, long-lived object — a CQL
  `Session`, an HTTP client, etc. Owns network resources.
  Constructed lazily, shared across phases when policy allows,
  torn down explicitly when no further phase will use it.
- **Resource key.** A typed value that identifies "this
  instance" — equal-by-value keys produce the same instance
  under the default share policy. Adapters synthesise the key
  from their params.
- **Share policy.** A user-facing knob that elevates isolation:
  `Shared` → `PerScenario` → `PerPhase` → `PerFiber`. The
  adapter declares the strictest *capability* it supports; the
  user picks any policy at or above the adapter's floor.
- **Pool.** The runtime structure (one per session) that holds
  a `(ResourceKey, Arc<dyn SharedResource>)` map plus refcounts
  walked from the pre-mapped scenario graph.

---

## Why generic, not CQL-specific

The same lifecycle problem applies to:

- **HTTP adapters** with shared connection pools and TLS sessions
  (re-establishing TLS per phase is observable in latency).
- **Vectordata** prebuffer and catalog handles already use a
  process-global cache (SRD-53); the adapter-pool layer is the
  same shape pulled into a typed contract.
- **OpenAPI / testkit** adapters whose runtime carries
  authentication state.
- **Future driver families** (Kafka producers/consumers, gRPC
  channels, custom protocol stacks) whose instance creation
  cost is measured in 100 ms+ and whose teardown is
  asynchronous.

Putting the layer at the adapter-interface boundary, not inside
each adapter, also makes the contract *auditable*: every shared
instance the workload touches appears in one place in the log
stream, with one consistent vocabulary for sharing decisions.

---

## Two-layer split: shell vs instance

```
                         ┌────────────────┐
                         │  DriverImpl    │  registered via
                         │  (factory)     │  inventory at link time
                         └───────┬────────┘
                                 │
                                 │ resource_key(params)
                                 ▼
                       ┌──────────────────┐
            (one)──────│ SharedResource   │──────(many)
                       │  (heavy object)  │
                       └────────┬─────────┘
                                │
                                │ Arc<dyn SharedResource>
                                │
              ┌─────────────────┼─────────────────┐
              ▼                 ▼                 ▼
       ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
       │ DriverAdapter│  │ DriverAdapter│  │ DriverAdapter│
       │  (shell, p1) │  │  (shell, p2) │  │  (shell, p3) │
       └──────────────┘  └──────────────┘  └──────────────┘
```

The pool owns instance lifetime. Each shell is owned by its
phase iteration and dropped at phase end (cheap). Multiple
shells sharing one instance is the default; each shell holds an
`Arc` clone of the underlying instance.

The split must be visible in the trait surface — not hidden as
an implementation detail — so the executor can ask the right
question at the right time:

```rust
pub trait DriverImpl: Send + Sync + 'static {
    /// Adapter-name surface (`cql`, `http`, …) and driver-name
    /// surface (`cassandra-cpp`, `scylla`, `reqwest`, …).
    fn adapter(&self) -> &'static str;
    fn driver(&self) -> &'static str;

    /// Synthesise the resource key from the adapter's params.
    /// The adapter decides which params are identity-bearing.
    /// Two calls with the same `params` MUST return equal keys;
    /// the manager dedupes by `==`. Pure function — no side
    /// effects, no I/O.
    fn resource_key(&self, params: &Params) -> ResourceKey;

    /// Strictest sharing this driver supports. Most drivers
    /// return `ShareCapability::Shared`. Drivers whose
    /// instance is fundamentally per-something return a
    /// stricter floor.
    fn share_capability(&self) -> ShareCapability {
        ShareCapability::Shared
    }

    /// Build the heavy instance. Called by the pool on first
    /// attach for a key, exactly once per key per session
    /// (modulo `const` failure → poison; see §"Failure isolation").
    /// The pool emits `resource.init.{started,completed,failed}`
    /// events around this call.
    fn create_instance(&self, params: &Params)
        -> Pin<Box<dyn Future<Output = Result<Arc<dyn SharedResource>, String>> + Send>>;

    /// Build the per-phase shell against a (possibly shared)
    /// instance. Cheap. Called once per phase activation (per
    /// adapter name in that phase). The pool emits
    /// `resource.attach` around this call.
    fn create_shell(&self,
        instance: Arc<dyn SharedResource>,
        params: &Params,
    ) -> Result<Arc<dyn DriverAdapter>, String>;

    /// Field inventory (existing surface, retained).
    fn known_params(&self) -> &'static [&'static str];
}
```

Backwards compat: the existing `create` factory remains as a
thin default that does both halves — adapters not yet migrated
keep working, with the pool treating each phase as a singleton
key (`PerPhase` policy) so behaviour is unchanged.

---

## Resource key — value equality, no derived hashing

A resource key is *structural value identity*, not a derived
digest:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceKey {
    pub driver: &'static str,
    pub fields: BTreeMap<String, String>,
}
```

Adapters populate `fields` with **only** the params that
distinguish one instance from another. Anything per-statement,
per-phase, or per-fiber that the adapter handles in its shell
must NOT appear in the key.

For the CQL/Cassandra-cpp adapter:

| Field             | In key? | Reasoning |
|-------------------|---------|-----------|
| `hosts`           | yes     | Different cluster → different session |
| `port`            | yes     | Different cluster → different session |
| `keyspace`        | yes     | Cluster session is keyspace-bound at connect |
| `consistency`     | yes     | Cluster default; per-statement override happens in the shell |
| `username`        | yes     | Auth identity |
| `password`        | yes     | Auth secret (hashed when logged; equality on raw value) |
| `request_timeout_ms` | no   | Per-statement; shell applies it |
| `trace_rate`      | no      | Live dynamic control on the shell |
| `cassandra_log_level` | no  | Process-global one-shot, not per-instance |

The pool `Hash`-implements `ResourceKey` for internal
`HashMap` lookup, but the *contract* the adapter sees and
the *user-visible* identity rule is value equality. There is
no separate "key digest" function; two adapters that build
the same `BTreeMap` get the same instance, period.

Param normalisation (case-folding `consistency`, deduping
`hosts`, sorting host lists, …) is the **adapter's**
responsibility before populating the key. The pool is
ignorant of param semantics.

---

## Share capability and share policy

Two distinct concepts, often conflated:

- **`ShareCapability`** — declared by the *driver*. The
  strongest sharing the driver implementation can technically
  support without race conditions, undefined behaviour, or
  state corruption.
- **`ResourceSharePolicy`** — chosen by the *user*. The actual
  sharing strategy applied at runtime. Must be at or above the
  capability floor. Defaults to the highest sharing the
  capability allows.

```rust
/// What a driver implementation can technically tolerate.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ShareCapability {
    /// Safe to share one instance across every adapter shell
    /// in the workload that produces an equal key. CQL,
    /// HTTP-with-pool, Scylla, OpenAPI typically.
    Shared,
    /// One instance per scenario subtree. Use when state
    /// changes during the run that mustn't bleed across
    /// scenarios (auth tokens, schema versions per scenario).
    PerScenario,
    /// One instance per phase. Use when each phase
    /// legitimately wants a fresh state object (chaos / fault
    /// injection drivers, drivers that intentionally exhaust).
    PerPhase,
    /// One instance per fiber. Use only when the driver type
    /// is `Send` but not `Sync`, or when the underlying
    /// library mandates single-threaded use.
    PerFiber,
}

/// What the user wants applied. Higher values increase
/// isolation; lower values increase sharing. Must satisfy
/// `policy >= capability.floor()`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResourceSharePolicy {
    Shared,       // = ShareCapability::Shared
    PerScenario,
    PerPhase,
    PerFiber,
}
```

Because both enums are ordered, the rule "policy at or above
capability" is a single `PartialOrd` comparison.

### CLI / workload surface

Following SRD-04's umbrella pattern:

```
--resource-share <kv-list>
NMBRS_RESOURCE_SHARE=<kv-list>

--resource-share <policy>            # bare-token: applies to all adapters
--resource-share 'cql:per-phase,http:shared'
```

| Item                       | Meaning |
|----------------------------|---------|
| bare token                 | Apply to every adapter |
| `<adapter>:<policy>`       | Apply to one adapter |
| policy values              | `shared`, `per-scenario`, `per-phase`, `per-fiber` |

Workload-level override:

```yaml
params:
  resource_share: per-phase                    # global
  resource_share_cql: shared                   # per-adapter override
```

CLI wins over workload params (SRD-04 Rule 5). Setting a
policy below the adapter's capability floor is a hard error at
session start, not a silent clamp.

### Why isolation must be elevatable

User-driven reasons to elevate isolation even when the driver
is `Shared`-capable:

- **Reproducible per-phase warmup.** A workload measuring
  cold-cache latency wants every phase to start with a fresh
  connection pool.
- **Workload-side debugging.** "Does the leak still happen
  if every phase has its own instance?" is a useful triage
  experiment without code changes.
- **Resource-cap testing.** Force the open/close churn that
  triggers `LIB_UNABLE_TO_INIT` on purpose, to validate
  diagnostic output.
- **Side-effect isolation.** A scenario that runs schema
  migrations alongside read tests doesn't want migration
  control state visible to read-test sessions.

The policy lever satisfies all four without leaking adapter
internals into the workload.

---

## Two trait methods on the live resource: `can_share` and `can_support_more_load`

Capability and policy together answer "*can* this instance be
shared, and *should* it be?" — but neither answers "*given
that sharing is allowed, is the active instance under enough
contention right now that another would help?*" That's the
job of two boolean queries the resource API itself carries:

```rust
#[async_trait]
pub trait SharedResource: Send + Sync + 'static {
    // ...

    /// **Capability** — does this instance support being
    /// shared by multiple adapter shells concurrently? A
    /// `true` return is a positive declaration that the
    /// implementation is thread-safe AND has been designed
    /// for concurrent multi-shell access (the two are not
    /// the same — `Send + Sync` covers data-race safety,
    /// but a driver can hold internal state that races
    /// invisibly under concurrent multi-shell load even
    /// when its types are `Sync`).
    ///
    /// Default `true` — most modern adapters are designed
    /// for sharing. Drivers whose underlying library
    /// mandates serial access, or whose state model breaks
    /// under shared use (single-threaded GUI clients,
    /// some legacy protocol stacks), override to `false`.
    /// A `false` return forces the pool to apply at minimum
    /// `PerPhase` policy regardless of the user's
    /// `ResourceSharePolicy` choice — the user can isolate
    /// further but cannot share what the driver says it
    /// cannot share.
    fn can_share(&self) -> bool { true }

    /// **Live capacity** — does the driver, knowing its
    /// own internals, judge that *another concurrent
    /// caller can attach to this instance right now*
    /// without substantial contention?
    ///
    /// `true` (the default) means "yes, route the next
    /// attach to me — I have capacity." `false` means
    /// "no, I'm saturated; the pool should spawn a sibling
    /// for the new attach." Parallel to `can_share()`:
    /// `can_share()` says *whether* sharing is
    /// structurally possible at all,
    /// `can_support_more_load()` says *whether sharing one
    /// more caller is OK at this moment*.
    ///
    /// The driver has access to its own command-queue
    /// depth, in-flight request count, connection-pool
    /// occupancy, attached-shell count, and any other
    /// implementation-specific signal that would inform
    /// the call. A driver can implement this as a real
    /// capacity metric (e.g. cassandra-cpp's pending
    /// request count vs. queue cap → return false when
    /// pending > 80% of cap) or as a simple count
    /// threshold (e.g. "return false once attached shells
    /// exceed 100") — that choice is local to the
    /// driver's capacity envelope.
    ///
    /// The pool consults this on each attach. It picks
    /// the first existing generation whose
    /// `can_support_more_load()` returns `true`. When
    /// every existing generation returns `false`, the
    /// pool spawns a sibling under the same
    /// `ResourceKey` and routes the new attach there.
    ///
    /// Default `true` — drivers that don't track capacity
    /// always say "yes, share me," and the pool keeps a
    /// single shared instance for the whole workload.
    /// MUST be cheap (atomic read or short metric query);
    /// MUST not block.
    ///
    /// Drivers that override `can_support_more_load()`
    /// MUST document their decision criterion in the
    /// type's docstring so an operator reading a
    /// `reason=capacity-declined` lifecycle event can
    /// interpret what triggered it.
    fn can_support_more_load(&self) -> bool { true }
}
```

The pair is deliberately complementary:

- `can_share()` is the **design declaration** the pool reads
  to decide *whether sharing is on the table at all*.
- `can_support_more_load()` is the **runtime opinion** the
  pool reads to decide *whether routing one more caller to
  this instance is OK right now*.

### Validity rules for `can_support_more_load()` implementations

A `can_support_more_load()` body is correct only when its
answer reflects **current, ongoing usage** of the
instance, not accumulated history or stale state. The pool
calls it at attach time to ask "is *this* instance OK to
take one more concurrent caller right now?" — answering
based on a metric that's already-paid-for (e.g. a ring
buffer that filled during an earlier phase but is now
idle) gives wrong answers.

**Concrete failure modes to avoid:**

- **Historical-state leakage.** Watching a metric like
  "max queue depth ever observed" or "total writes since
  startup" and reporting capacity from it. These reflect
  the past, not the present; an instance whose previous
  user just stopped will look saturated (return `false`
  from `can_support_more_load()`) when in fact it has full
  capacity now.
- **Lagging accumulators that don't drain.** If the driver
  tracks a moving-average occupancy with a long window,
  the average stays elevated for tens of seconds after
  load disappears. `can_support_more_load()` would keep
  reporting `false` (refusing new callers) during those
  tens of seconds even though no user is active.
- **Counters that reset on close, not on user departure.**
  A queue depth that increments on submit and decrements
  on completion *does* reflect active load. A counter
  that increments on submit and only resets on
  `cass_session_free` reflects history, not load.

**Required properties:**

- **Liveness-bounded.** A `can_support_more_load() ==
  false` return is *only* meaningful when the active
  generation has shells attached right now. At quiescence
  (`live_attaches == 0`) there is no contention to refuse,
  so a `false` return cannot be correct — there's no load
  to be saturated by. Conforming drivers therefore return
  `true` (has capacity) whenever no shell is attached. The
  pool enforces this structurally — see "Pool-level guard"
  below — so a buggy driver can't trigger a wasted spawn,
  but the guard logs at `Warn` when it has to act, and
  that Warn is the signal that the driver's
  `can_support_more_load()` body is reading the wrong metric.
- **Reflects in-flight work only.** The metric the body
  consults must drop to its idle value when no
  user-driven request is currently in flight. A pending
  request count that goes back to 0 after the last
  request completes meets this bar; a "lifetime peak
  pending" or "average over last minute" does not.
- **Cheap and bounded.** Atomic read or short metric
  query; MUST NOT block. Called on every attach.

**Pool-level guard (mandatory):** the runtime checks that
`can_support_more_load()` returning `false` is consistent
with non-zero current usage. Specifically: the pool never
spawns a sibling generation when the active generation's
`live_attaches` count is 0. A `can_support_more_load()`
body that returns `false` at quiescence is logged at
`Warn` (one entry, naming the resource key) and the attach
is routed to the existing idle generation instead. This
protects against accidental historical-state leakage in
driver implementations without silently losing the spawn
signal when the driver IS under live load.
Repeated `quiescent-decline` events indicate a buggy
`can_support_more_load()` implementation that the operator
should report.

**Worked counter-example.** A driver tracks a ring buffer
of completed-request timestamps for latency histograms.
At the end of phase A — the moment phase A's last shell
detaches — the ring is full and the recent-window
high-water mark looks busy. If `can_support_more_load()`
read off "ring buffer size > 80% of cap → no capacity",
the next phase's first attach would force a wasted
sibling spawn even though the instance is fully idle by
then. Correct: the body should consult
`pending_in_flight` (currently submitted, not-yet-completed
requests), which dropped to 0 the instant the last
completion landed — so `can_support_more_load()` correctly
returns `true` at the start of phase B.

### Worked examples

A cassandra-cpp instance returns `can_share() == true`
unconditionally (the C++ driver's `CassSession` is documented
thread-safe and its API was designed for many-clients-per-session)
and computes `can_support_more_load()` from
`cass_session_get_metrics()`-derived pending-request count
against `cass_cluster_set_queue_size_io` (default 8192) —
returns `true` while pending stays below 80% of cap, drops
to `false` once it crosses. A simpler prototype could just
count attached shells and return `false` once the count
exceeds 100; the trait makes no demand about which signal
the driver uses.

An HTTP-with-pool instance returns `can_share() == true`
and computes `can_support_more_load()` from in-flight-request
count against the connection-pool max-idle-per-host
setting (reqwest's default is 10) — returns `true` while
the pool has free slots, `false` once it's saturated.

A hypothetical single-threaded driver returns
`can_share() == false`; the pool collapses its policy to
`PerPhase` and the user's `Shared` request is rejected at
session start with an actionable error ("driver X declares
itself non-shareable; the highest sharing policy available
is `per-phase`").

A trivially-concurrent driver (`stdout`, `testkit`) leaves
both methods at default — `can_share() == true`,
`can_support_more_load() == true` — so the pool keeps one
shared instance for the whole workload and never spawns a
sibling.

### Sibling-instance spawning

When `can_support_more_load()` returns `true` on the active generation
during an attach, the pool spawns a fresh sibling instance
under the same `ResourceKey`. The pool keys siblings
internally as `(key, generation)`; from the user's
perspective there's still "one shared resource for this
config," but the runtime has multiplied it transparently to
relieve contention. Each generation has its own lifecycle:
own init, own refcount, own close. They share nothing
operationally — the "sibling" framing is only for accounting.

When multiple generations exist for one key, the pool routes
each new attach as follows:

1. **Capacity-first** — pick the first generation whose
   `can_support_more_load()` returns `true` (has capacity).
   Among generations with capacity, prefer the one with
   the lowest current `live_attaches`, breaking ties by
   lowest generation index.
2. **Spawn-on-saturation** — if *every* existing generation
   returns `false` (all saturated), spawn a new sibling
   under the same `ResourceKey` and route the attach to
   it. The spawn fires only after the
   [`needs_sibling_spawn`] guard confirms the decline is
   live (live_attaches > 0); a quiescent decline is logged
   as a `quiescent-decline` driver bug and the attach is
   routed to the existing idle generation instead.
3. **Pinned** is an opt-in policy bit
   (`--resource-share cql:pin-by=phase`) that pins a phase's
   generation choice for repeatability. Off by default;
   useful for benchmark reproducibility where you want the
   same shell-to-instance mapping run-over-run.

### Why the spawn decision belongs at the implementation boundary

Drivers are the only layer that knows their own internals
well enough to make this call:

- **The user can't.** "Should I let CQL handle 50 or 200
  concurrent shells?" requires knowing the driver's
  command-queue depth, its worker-thread count, its
  socket-multiplexing strategy. Pushing this to the user
  makes every workload author research every driver.
- **The pool can't.** It sees only `live_attaches`. It has
  no model of what an attach *costs* on an instance, and
  the right answer changes when the driver's settings
  change (change `num_threads_io` from 2 to 8 and the right
  threshold moves with it).
- **A static config table can't.** Same reason — the right
  number depends on the driver's current configuration, not
  a one-shot SRD value.

Putting the decision in the live `can_support_more_load()` body keeps
the knowledge co-located with the driver code that
materialises it.

### User overrides

Users can shape sharing behaviour without overriding the
driver's capacity logic:

```
--resource-share 'cql:per-phase'        # full isolation, no sharing
--resource-share 'cql:force-single'     # ignore can_support_more_load(); always route to gen 0
```

Workload param form:

```yaml
params:
  resource_share_cql: per-phase
  resource_force_single_cql: true
```

Validation:

- Setting `Shared` policy against a driver whose
  `can_share()` returns `false` is a hard error at session
  start — the pool consults `can_share()` on first-attach
  init and rejects the policy with an actionable message
  before any phase runs.
- `force-single` is meaningful only when the driver
  overrides `can_support_more_load()`; harmless no-op for
  drivers that leave it at the default.

The user does NOT have a knob for *overriding*
`can_support_more_load()`'s return value — that decision is
the driver's by definition. Users who want strict isolation
use `per-phase`; users who want a single shared instance
regardless of the driver's capacity signal use
`force-single`.

---

## Lifecycle

### Lazy init bound to the scenario graph

At session start the pool walks `pre_map_tree` (already
computed for the SceneTree display) and builds, for each
distinct `(ResourceKey, generation)` projection, an
**`Entry`**:

```rust
struct Entry {
    key:            ResourceKey,
    policy:         ResourceSharePolicy,
    generation:     usize,            // 0 for the first instance under a key; ≥1 for siblings spawned via `can_support_more_load()`
    instance:       OnceCell<Arc<dyn SharedResource>>,
    inflight_init:  Mutex<()>,        // serialises concurrent first-attach races
    pending_uses:   AtomicUsize,      // remaining attaches predicted by pre-map (this generation only)
    live_attaches:  AtomicUsize,      // currently in use (this generation only)
    poisoned:       AtomicBool,       // true if init failed and we won't retry
}
```

`pending_uses` starts at the count of phases the pre-map
projects will attach this entry. Each `attach` decrements it
once *eagerly* (the slot is consumed); each `detach` is a
no-op against `pending_uses`. When `pending_uses == 0` and
`live_attaches == 0`, the pool calls `close()` and removes
the entry. Other generations under the same key are
unaffected — generation 1 can close while generation 0 is
still live.

This makes "stays cached if a future phase will use it" a
direct consequence of refcounting against the pre-map — no
heuristic, no clock, no LRU.

For `PerPhase` and `PerFiber` policies the pre-map count
*equals* the predicted attach count exactly (one new
instance per phase / fiber); for `Shared` a single
generation-0 entry covers all matching attaches unless
`can_support_more_load()` returns `false` and triggers a
sibling spawn.

The pre-map deliberately does NOT predict sibling counts.
Specific sibling instancing is not fully knowable ahead of
time *by design* — `can_support_more_load()` is a runtime decision
the driver owns, not a planner-side projection. Generation 0
gets the full predicted attach count; siblings spawned at
runtime appropriate `pending_uses` from generation 0's
remaining count when they enter (the pool re-divides the
residual prediction across the now-larger generation set).
The arithmetic is approximate; the lifecycle event surface
carries the generation index on every line so post-hoc
divergence is visible.

### Init concurrency

Two phases attaching the same key simultaneously must not
double-init. The pool uses `OnceCell` + an inflight mutex:
both attach futures contend for the mutex; the winner runs
`create_instance`; the loser observes the populated cell on
its next read. Init failure poisons the entry: every
already-pending attach for that key returns the cached error;
the entry is not retried during this session.

### Symmetric close

`close()` is an explicit `async fn` on `SharedResource`, not
just `Drop`. The pool awaits it. For the cassandra-cpp
adapter the implementation calls `cass_session_close()` and
awaits the resulting `CassFuture` — the whole reason this SRD
exists is that `Drop`'s synchronicity contract isn't strong
enough for resource pools that need to bound teardown.

```rust
#[async_trait]
pub trait SharedResource: Send + Sync + 'static {
    fn resource_key(&self) -> &ResourceKey;

    /// Optional one-time init beyond what `create_instance`
    /// already did. Most drivers do all of init in
    /// `create_instance` and leave this as the default no-op.
    /// Provided for drivers whose underlying library
    /// distinguishes "construct" from "start."
    async fn init(&self) -> Result<(), String> { Ok(()) }

    /// Block until network resources are *actually released*.
    /// No async-Drop races; no sockets in TIME_WAIT counted as
    /// "closed"; no libuv worker thread races. The pool
    /// awaits this; the next pool tick observes resources
    /// released.
    async fn close(self: Arc<Self>) -> Result<(), String>;
}
```

---

## Lifecycle event surface

Every state transition emits a debug-level event under the
`driver` log family. The event names are stable; the field
list is open-ended (more fields can be added without breaking
log consumers).

Every event carries `key`, `generation`, and `policy`
unconditionally; the *Required fields* column below names
the additional fields specific to that event.

| Event                     | When                                | Additional required fields |
|---------------------------|-------------------------------------|---------------------------|
| `resource.attach`           | Phase begins, attaches to entry     | `phase`, `pending`, `live`, `target_gen` |
| `resource.init.started`     | First attach forces lazy init       | `reason` (`first-attach` / `capacity-declined`) |
| `resource.init.completed`   | `create_instance` returned Ok       | `elapsed_ms` |
| `resource.init.failed`      | `create_instance` returned Err      | `error`, `elapsed_ms` |
| `resource.detach`           | Phase ends, releases entry          | `phase`, `pending`, `live` |
| `resource.share.spawn`      | Pool spawned a sibling generation   | `from_gen`, `to_gen`, `live_in_from_gen` |
| `resource.close.started`    | `pending == 0 && live == 0`         | `reason` (`refcount-zero` / `session-end` / `idle-sibling`) |
| `resource.close.completed`  | `close()` returned                  | `elapsed_ms` |
| `resource.close.failed`     | `close()` returned Err              | `error`, `elapsed_ms` |

`reason` on `close.started` distinguishes three paths:

- **`refcount-zero`** — the pre-map predicted N uses, all N
  phases have detached, nothing else will use this entry. This
  is the common path; if it fires *before* the workload ends,
  the operator can correlate "driver X gen=2 closed at 14:32"
  with "phase Y completed at 14:32" by phase name.
- **`idle-sibling`** — a `can_support_more_load()`-spawned sibling has
  drained while the key's generation 0 (or earlier siblings)
  is still alive. The pool MUST close idle siblings eagerly
  rather than hold them open until session end — otherwise
  one transient contention spike at phase 5 leaves the
  sibling alive for the rest of the workload, doing nothing.
- **`session-end`** — the executor is shutting down and is
  flushing the pool. Entries with `pending > 0` here are bugs
  in the pre-map walk (the SceneTree promised attaches that
  never happened) and the event line carries the residual
  count for diagnostic purposes.

`reason` on `const.started` distinguishes:

- **`first-attach`** — the first phase to need this key has
  arrived. Generation 0 spinning up.
- **`capacity-declined`** — a sibling spawned because the
  active generation's `can_support_more_load()` returned `true` —
  the driver judged that another instance would relieve
  knowable, substantial contention.

Operator-facing note: when a sibling is spawned via
`can_support_more_load()`, its close timing (driven by *its* drained
attaches, not the workload's overall progress) need not
align with any scenario boundary. Reading "gen=1 closed at
14:32" in the lifecycle log is NOT a workload-progress
milestone — it's the runtime releasing a transient
contention-relief instance.

Routing follows SRD-41 (logging): events go through the
session log at `Debug` level; an explicit `--diag drivers`
umbrella flag elevates the family to `Info` so a normal run
shows the lifecycle without manual log filtering.

---

## Failure isolation

Three failure shapes, three policies:

1. **`create_instance` fails.** The entry is poisoned; every
   phase predicted to use it surfaces `resource.init.failed` and
   the executor halts that phase. Other entries are
   independent. The pre-map's attach count tells the executor
   *up front* how many phases this will affect — it can either
   stop the run early or continue and aggregate failures
   depending on the strict-mode setting.
2. **`close` fails.** Logged at `Warn`. The entry is removed
   from the pool either way. The session log carries the
   error; the run continues. Fail-soft on close because
   teardown errors don't change the user's data.
3. **`create_shell` fails.** The shell call is per-phase, not
   pool-global; behaves like a phase-local error and follows
   the SRD-03 phase failure policy. The instance is unaffected
   and stays cached.

---

## Interaction with existing subsystems

### SRD-23 dynamic controls

Adapter-level controls (`adapter.cql.consistency`, …) are
declared today on the *shell*. With shared instances, controls
that conceptually live on the instance (e.g. cluster-wide
trace-rate sampling) move to the instance. The shell's
`declare_controls` becomes the union of "controls declared on
my instance" plus "shell-local controls (per-phase timeouts,
per-statement consistency overrides)." `declare_controls` must
be idempotent on the instance side — a second phase attaching
to the same instance gets the existing controls.

### SRD-19 component tree

Each shared instance attaches to the **session-root**
component, not the per-phase component. Per-phase shells
attach as today (under the phase node). Metrics emitted by an
instance carry session-scope labels (no `phase=` label),
which matches the data semantics: an instance's connection
counters describe the cluster, not the phase.

### SRD-44 checkpointing

A resumed run rebuilds the pool from the resumed scenario
graph. Pre-map runs against the (possibly truncated) remaining
phase set, so refcounts are correct on resume. Instances are
re-initialized from scratch — checkpoint state is workload
progress, not driver-internal connection state.

### SRD-50 CQL adapter (prototype consumer)

The CQL adapter's existing `common`/`cassandra_cpp`/`scylla`
split is the right boundary:

- `common` defines the `CqlConfig` value type that becomes
  the `ResourceKey`'s `fields` source via a
  `to_resource_key()` method.
- `cassandra_cpp::CassSharedResource` wraps the
  `cass::Session`, libuv loop, and trace log. Implements
  `SharedResource`. `close()` awaits the cpp-driver's session
  shutdown future.
- `scylla::ScyllaSharedResource` wraps the
  `scylla::Session` symmetrically.
- The existing `CqlAdapter` shell becomes the
  `DriverAdapter` impl, holding an
  `Arc<dyn SharedResource>` (downcast as needed in dispenser
  paths) and constructing dispensers as today.

The `DriverImpl` registration in
`crates/nmbrs-adapter-cql/src/cassandra_cpp/mod.rs:1166-1188` migrates
from the single `create` closure to the split
`create_instance` + `create_shell` shape. The current `create`
becomes a default-impl that calls both — adapters that
haven't migrated yet keep working under `PerPhase` policy
without code changes.

---

## Decisions made

These were design questions during drafting; resolutions are
folded into the body but called out here so readers can see
*why* the body looks the way it does.

1. **Param normalisation lives on the adapter.** The
   `ResourceKey` accepts whatever the adapter populates; the
   pool stays ignorant of param semantics. No `KeyBuilder`
   helper in the runtime; adapters know their own params best.
2. **`PerFiber` is lazy, not eager.** The pool indexes per-fiber
   entries by `(key, fiber_id)` and creates instances on
   first attach for that fiber. Pre-allocating N instances
   at phase start is rejected — pre-allocation of "just in
   case" resources is the bug shape this SRD is *fixing*, not
   adopting. Cached-once-created behaviour is the design
   contract; eager fan-out is not.
3. **Cross-session sharing is a future feature with identical
   semantics.** A future SRD can extend the pool's lifetime
   beyond one session. The sharing-and-scaling shape carries
   forward unchanged; the only added surface is user
   overrides for lifecycle boundaries between runs (which
   close, which persist). Out of scope for this SRD.
4. **No CLI introspection of predicted instance counts.** The
   specific instance mapping is intentionally *not* fully
   knowable from the pre-map — `can_support_more_load()` is a runtime
   decision the driver owns, not a planner-side projection.
   `nmbrs describe drivers` can show the projected
   `(key, policy)` table without trying to predict sibling
   counts.
5. **Instance-shaping params and shell-shaping params are
   strictly separated** (§"Instance-shaping vs shell-shaping
   params"). Per-op defaults set at the instance level —
   "this CQL session's default consistency for ops that don't
   specify their own" — is a *recognised future scope* but
   intentionally murky for now. Today every op-time knob
   lives in the shell; instance-level defaults that flow into
   per-op behaviour are deferred.
6. **`can_support_more_load()` decision criterion is per-driver, full
   stop.** The SRD doesn't and won't prescribe a canonical
   shape — internal resource shapes vary too much for a
   prescriptive contract to be useful. Each driver picks the
   signal that makes sense for its own model and documents
   it in the type's docstring (normative requirement —
   §"Two trait methods on the live resource").
7. **Fanout cap retired.** An earlier draft introduced
   `ShareFanout` (Static / Dynamic / Bounded) as a separate
   axis declaring "max concurrent shells per instance."
   Retired in favour of `can_support_more_load()` alone — a driver
   that wants count-based sibling spawning simply implements
   `can_support_more_load()` to return `true` once attached shells
   exceed its threshold. The trait pair `can_share()` /
   `can_support_more_load()` covers every shape the fanout enum
   covered, with strictly less surface to learn. The fanout
   axis can return if a real need surfaces it.
8. **`can_share()` lives on the live instance; the type-level
   `share_capability()` is what the pool uses for planning.**
   Path (a) from the original framing. The live `can_share()`
   is a runtime safety net that aborts the session if it
   disagrees with the type-level declaration — no
   sanctioned-disagreement story. The pool MUST be able to
   plan the entry layout before instantiating any resource;
   forcing a configure-and-instantiate just to learn
   instancing limitations would defeat the lazy-init
   contract.

---

## Instance-shaping vs shell-shaping params

The adapter's `Params` map carries values that MUST be
partitioned into two strictly disjoint, exhaustive sets:

- **Instance-shaping params** — values that change which
  underlying low-level driver instance is right for the call.
  These appear in `ResourceKey::fields`. Two phases with the
  same instance-shaping params share an instance; differing
  values produce different instances. The CQL example table
  in §"Resource key" enumerates these for the
  cassandra-cpp / scylla adapters.
- **Shell-shaping params** — values that change how the
  per-phase shell uses an instance. These are NOT in the
  resource key; the shell consumes them at construction and
  applies them per-op or per-statement (`request_timeout_ms`,
  `trace_rate`, …).

The partition is normative: every adapter that adopts the
resource pool MUST publish the partition for its params in
the adapter's SRD section, and `ResourceKey::fields` MUST be
populated from exactly the instance-shaping set.

### Future scope: instance-level defaults flowing into per-op behaviour

A natural extension is "use the instance's default
consistency for ops that don't specify one" — instance-level
defaults that the shell consults per-op rather than baking
in at construction. This is a recognised future capability
but the scope is murky enough today that we keep the two
classes strictly separate: a value either shapes the
instance OR shapes the shell, never both. A later SRD can
introduce the bridge if a workload pattern motivates it; the
current SRD's contract is "the partition is exhaustive and
exclusive."
---

## Migration plan

Three pushes, each independently shippable:

### Push A — pool layer + `PerPhase` default for everyone

- Add `nmbrs-runtime::resource_pool` with the new traits, the
  multi-generation `Entry` machinery, the pre-map-driven
  refcount walker, and the lifecycle event emitters
  (`attach` / `const.{started,completed,failed}` /
  `share.spawn` / `detach` / `close.{started,completed,failed}`).
- Add `ShareCapability` + `ResourceSharePolicy` enums and the
  CLI / workload-param surface (SRD-04 umbrella).
- Existing `DriverImpl::create` factories get a default
  `share_capability = PerPhase` and a synthetic per-phase
  `ResourceKey`. The default trait impls of `can_share()`
  (true) and `can_support_more_load()` (false) leave sibling-spawning
  inactive. Behaviour is byte-identical to today (a fresh
  instance per phase) but every phase boundary now emits the
  full lifecycle event sequence, giving operators visibility
  into the open/close churn that motivated this work.
- Tests: pool unit tests against a `MockSharedResource` that
  records its lifecycle calls and can be configured to return
  `can_support_more_load()=true` after N attaches, exercising the
  sibling-spawn path against synthetic load.

### Push B — CQL adapter migrates to `Shared`

- Split `CqlAdapter` into `CassSharedResource` (cassandra-cpp)
  / `ScyllaSharedResource` (scylla) plus the existing shell.
- Implement `SharedResource` on each, including async `close()`
  that awaits the cpp-driver's session shutdown future.
- Vendored `cassandra-cpp` gains the `cass_session_close`
  → `CassFuture` Rust async surface (small fork addition;
  follows the existing `cass_future_tracing_id` precedent in
  `vendor/cassandra-cpp/CHANGES.local.md`).
- `share_capability` returns `Shared`; `resource_key`
  populated from `CqlConfig`. `can_support_more_load()`
  initially stays at the default `true` — Push D adds the
  contention-aware capacity logic that drops to `false`
  under saturation.
- Smoke test: 50-phase sequential workload should emit one
  `const.started` / `const.completed` (gen=0) and one
  `close.started` / `close.completed` for the whole run,
  with `attach` / `detach` events at every phase boundary
  and no `share.spawn` events.

### Push C — HTTP, OpenAPI, stdout migrate

- Same shape as Push B for each adapter.
- `stdout` instance is trivially shareable (`println!` is
  thread-safe); migration mostly just adopts the new trait
  shape with default `can_support_more_load() = false`.
- HTTP migration captures the connection pool / TLS-session
  reuse benefit that was always possible but not realised.
  `can_support_more_load()` could later be implemented against
  reqwest's pool stats once the unstable feature stabilises;
  not on the critical path.

### Push D (optional) — contention-aware `can_support_more_load()` for CQL

- CassSharedResource overrides `can_support_more_load()` with a
  documented decision criterion (e.g.
  `cass_session_get_metrics()`-derived pending request count
  vs. cluster `queue_size_io`, or a simple attached-shell
  count threshold — driver's choice; per open question 6 the
  criterion is per-driver).
- New smoke test: synthetic high-concurrency workload should
  fire `share.spawn` at the load level where the driver
  judges contention substantial, with each generation closing
  independently as its attaches drain.
- Defer until after Push C; the single-instance shape from
  Push B already captures most of the benefit (one CQL
  session for the whole workload). Adding contention-driven
  multi-instancing is a quality refinement, not a fix for the
  underlying lifecycle problem.

---

## Out of scope

- Actual *connection pooling inside* a driver instance. That's
  the driver's internal business (cassandra-cpp, reqwest, etc.
  all do it). This SRD is about pooling driver *instances*, not
  pooling connections within an instance.
- Cross-process sharing (shared memory, daemonised driver
  pool). nmbrs is single-process; if that ever changes, that
  change subsumes this SRD's pool, not the other way around.
- Tenancy / quota enforcement on shared instances. A shared
  instance is shared transparently — phases don't compete for
  it. If quota becomes a concern (e.g. a phase that wants a
  guaranteed slice of the connection pool), that's a future
  extension orthogonal to the lifecycle layer.
