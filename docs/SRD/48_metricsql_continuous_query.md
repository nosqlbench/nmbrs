# 48: MetricsQL Continuous-Query Runtime

The streaming layer (SRD-47) gives us a `StreamingPlan`
that compiles a metricsql query into a tree of incremental
reducers, ingests samples, and returns a snapshot at any
time. What's missing is the **runtime** that connects
plans to a sample feed, manages their lifecycle, and
exposes their snapshots to consumers (TUI panels, web
endpoints, programmatic API). This SRD specifies that
runtime.

The first consumer of this design is
[`nmbrs metrics watch`](../../crates/nmbrs/src/metricsql_cmd.rs) —
already shipping a polling-loop CLI demo of the streaming
engine. The runtime generalises that pattern to
multi-query orchestration: one sample feed fanning out to
N concurrent plans, each owned by a different consumer
(TUI panel, web subscriber, summary report).

---

## Motivation

The streaming layer is correct (SRD-47's load-bearing
property test) and exposed via a clean API
(`compile_streaming` → `ingest` → `snapshot`). What's
unaddressed:

1. **Who owns the plans.** A live TUI may have 20 panels
   each with a different metricsql query. Each is a
   separate plan, but they all consume the same sample
   feed. There's no orchestration layer today.
2. **Where samples come from.** The current `watch`
   command polls `metrics.db` on a timer and computes a
   per-leaf delta via watermark. Real-time ingest from the
   reporter would push samples directly — no poll latency,
   no double-fetch. But there's no plumbing for that
   shape.
3. **How consumers subscribe.** A TUI panel wants
   "snapshot when state changes"; a web endpoint wants
   "snapshot on request"; a summary report wants "final
   snapshot at run end". One abstraction has to serve all
   three.
4. **Window-framing policy.** Streaming accumulators grow
   lifetime. For continuous queries with sliding-window
   semantics (`rate(metric[5m])` always reflecting the
   *current* 5 minutes), the runtime needs a strategy —
   tumble-and-reset, sliding-subtract, or grid-aligned —
   from SRD-47 §"Window framing".
5. **Back-pressure and memory bounds.** N plans × M
   panels × infinite samples = unbounded memory if nothing
   prunes. The runtime is where pruning policy lives.

Each of these is a runtime concern, not a streaming-layer
concern. The streaming layer's algebra stays untouched
(SRD-47 invariants); this SRD adds the orchestration on
top.

---

## Architectural shape

Three layers, each with a clear contract:

```
┌──────────────────────────────────────────────────┐
│  Consumers (TUI panels, web, reports, CLI)        │
│  read snapshots; subscribe to updates              │
└────────────────────┬─────────────────────────────┘
                     │ Snapshot<Vec<Series>> + change events
                     ▼
┌──────────────────────────────────────────────────┐
│  ContinuousQueryRuntime                            │
│  - Plan registry (id → StreamingPlan + metadata)   │
│  - Subscription map (plan_id → Vec<Subscriber>)    │
│  - Sample-fanout: one ingest → all matching plans  │
│  - Window framing policy (when to reset/snapshot)  │
└────────────────────┬─────────────────────────────┘
                     │ ingest(sample) + watch_for_matchers(...)
                     ▼
┌──────────────────────────────────────────────────┐
│  Sample feed (push, pull, or hybrid)               │
│  - PullFeed: polls a DataSource on a cadence       │
│  - PushFeed: receives samples from the reporter    │
│  - WatchableDataSource trait (SRD-47 follow-up #6) │
└──────────────────────────────────────────────────┘
```

The runtime is the **only** stateful layer. Consumers are
stateless readers; sample feeds are stateless producers.
This means the runtime is the lifecycle owner, the
back-pressure point, and the place where memory bounds get
enforced.

---

## Public API surface

```rust
/// Runtime handle. Cheap to clone — internal state lives
/// behind `Arc<Mutex<...>>` or an actor channel (one of
/// the trade-offs decided in §"Concurrency model").
#[derive(Clone)]
pub struct ContinuousQueryRuntime {
    inner: Arc<RuntimeInner>,
}

impl ContinuousQueryRuntime {
    /// Construct a runtime backed by a sample feed.
    pub fn with_feed(feed: Box<dyn SampleFeed>) -> Self;

    /// Register a query. Returns a handle the consumer
    /// uses to read snapshots and unregister.
    pub fn register(&self, query: &str)
        -> Result<QueryHandle, RegisterError>;

    /// Force a tick: pull-and-ingest from the feed,
    /// then re-snapshot every plan. The CLI and tests
    /// drive this directly; the TUI/web wire it to a
    /// timer.
    pub fn tick(&self) -> Result<(), TickError>;
}

pub struct QueryHandle {
    runtime: ContinuousQueryRuntime,
    plan_id: PlanId,
}

impl QueryHandle {
    /// Read the current snapshot. Cheap — just a clone
    /// of the plan's last-computed series. NaN-aware.
    pub fn snapshot(&self) -> Vec<Series>;

    /// Subscribe to updates. Returns a receiver that
    /// yields a fresh snapshot every time the runtime
    /// re-evaluates this plan (after a tick or push
    /// event).
    pub fn subscribe(&self) -> SnapshotStream;

    /// Unregister explicitly. Drop also unregisters via
    /// the `Drop` impl, but explicit unregister is
    /// cheaper for short-lived consumers.
    pub fn close(self);
}
```

### Sample feeds

Two implementations ship in this push:

```rust
pub trait SampleFeed: Send + Sync {
    /// Pull all samples newer than `since_ms` for the
    /// given matchers. The runtime calls this once per
    /// tick per registered matcher set. May be called
    /// concurrently for different matcher sets.
    fn fetch_since(
        &self,
        matchers: &[Matcher],
        since_ms: i64,
    ) -> Result<Vec<Series>, FeedError>;

    /// Latest sample timestamp the feed has seen. Used
    /// by the runtime as the watermark advance target.
    fn latest_ts(&self) -> Result<Option<i64>, FeedError>;
}

/// Pulls from any `DataSource` on a cadence. Wraps the
/// polling logic from `nmbrs metrics watch`.
pub struct PullFeed { source: Box<dyn DataSource> }

/// Push-driven feed for runtime ingest from the reporter
/// path. Producer calls `ingest(sample)`; the runtime's
/// ingest loop drains a queue per tick. Built on
/// `crossbeam-channel` to match the rest of nmbrs's
/// lock-free metrics path (memory: §"Lock-Free Metrics").
pub struct PushFeed { /* mpsc channel, watermark */ }
```

A future `WatchableDataSource` extension (SRD-47 §
"Followup roadmap" #6) becomes a third feed type when
real backends start emitting events directly. The
`SampleFeed` trait absorbs it without API change.

---

## Concurrency model

The runtime sits at the meeting point of:
- **Many producers**: one or more sample feeds, possibly
  concurrent.
- **Many readers**: TUI panels and web subscribers, each
  reading at its own cadence.
- **Many plans**: each with mutable accumulator state.

Two viable shapes; pick one and stick with it:

### Option A: Actor + ArcSwap snapshots (recommended)

Mirrors SRD-40 §"Lock-Free Metrics" / `cadence_reporter`
exactly:

- A single owner thread owns the runtime's mutable
  state — plan map, accumulators, watermark.
- Producers send `IngestCommand` over a
  `crossbeam-channel` to the owner.
- Readers don't block: each plan exposes an
  `arc_swap::ArcSwap<Vec<Series>>` snapshot. The owner
  re-publishes after every tick.
- Subscribers get `tokio::sync::watch::Receiver` clones
  of the same snapshot; they see the latest value
  whenever they poll.

**Pros**: zero contention on the read path; matches an
established nmbrs pattern; readers and producers never
block each other.

**Cons**: mutating a plan (e.g. unregistering) requires a
round-trip to the owner.

### Option B: `Arc<RwLock<RuntimeInner>>`

- Reader-friendly when read-mostly; standard library
  `RwLock` types directly.
- Loses on Linux's writer-preferring std `RwLock`
  (memory: §"No std::RwLock for Render State"). Would
  need `parking_lot` or `tokio::sync::RwLock`.

**Pros**: simpler for a first pass; less ceremony than an
actor.

**Cons**: contention under high subscriber count; writer
starvation risk; departs from the established lock-free
pattern.

**Decision**: Option A. The runtime is owned by an actor,
sample feeds and consumers communicate via channels +
ArcSwap snapshots. This is the established nmbrs pattern;
adopting it here keeps the dispatch layer uniform across
metrics, dynamic controls, and continuous queries.

---

## Lifecycle

Each plan goes through a documented sequence:

| State | Trigger | Effect |
|-------|---------|--------|
| **Register** | `runtime.register(query)` | Parse → compile_streaming → assign PlanId. Backfill the plan's accumulators by pulling `[anchor - warmup, anchor]` via every leaf's matchers. Publish first snapshot. |
| **Active** | (idle) | Snapshot is cached; readers see whatever the last tick produced. |
| **Tick** | `runtime.tick()` (CLI / TUI timer / web request) | Watermark-advance fetch: each leaf pulls `[watermark+1, latest_ts]` from the feed. Ingest into the plan. Recompute snapshot. Notify subscribers. |
| **Reset** | Window-framing policy fires (§"Window framing") | `plan.reset()`, watermark reset to current `latest_ts`. Subscribers notified; first post-reset snapshot may be empty. |
| **Unregister** | `handle.close()` or `drop(handle)` | Plan removed from registry; accumulators dropped; subscribers see a final `None` on the watch channel. |

### Backfill at register time

When the runtime is connected to historical data (e.g.
running queries against an existing `metrics.db`), a new
plan should ingest the warmup window so its first
snapshot reflects real data, not "(no series matched)".
Same logic `nmbrs metrics watch` already uses — lifted
into the runtime so every consumer benefits.

---

## Window framing policy

SRD-47 §"Window framing" parked this as a deferred
decision. The runtime is where it gets resolved.

Three concrete policies, configurable per-plan or globally:

| Policy | Memory | Semantics | Use case |
|--------|--------|-----------|----------|
| **Lifetime** (default) | `O(samples observed)` | Accumulators grow without bound; snapshot reflects everything ingested. | Counters where you want cumulative; not for live dashboards over wide windows. |
| **Tumbling** | `O(samples per window)` | Reset at fixed cadence (e.g. every minute). Snapshot reflects the *current* window only. | Per-minute throughput panels; resets are visible to consumers as a final snapshot then a fresh start. |
| **Grid** | `O(grid_size × series cardinality)` | Bucket by timestamp into N grid cells; ageing cells get pruned as new samples arrive past them. Approximates a sliding window without per-sample subtraction. | Dashboards with `[5m]` rollups that should reflect the trailing 5 minutes at any time. |

The streaming layer's algebra is unchanged under any of
these: tumbling is `reset()` calls; grid is multiple
sub-plans, one per bucket, summed at snapshot time.

**Sliding-subtract** (the "true" sliding window per
sample) is harder — works for distributive ops like sum
via subtraction, but not min/max (need a monotonic
deque) or quantile (need sketch-with-subtract).
Deliberately left out of this push; grid is the
pragmatic answer that covers the common case.

---

## TUI binding model

Each TUI panel that wants metricsql output:

1. Holds a `QueryHandle` it registered at panel
   construction.
2. Subscribes via `handle.subscribe()` to a
   `SnapshotStream`.
3. On each render, reads the latest snapshot from the
   watch receiver (non-blocking, returns the most recent
   value).
4. Drops the handle on panel teardown — runtime cleans
   up automatically.

The runtime lives in the TUI's data plane, not the render
plane (memory: §"Display Actor Decoupling"). Renders
read snapshots; they don't drive the runtime.

---

## Web endpoint shape

A simple HTTP API:

```
GET /api/v1/query?expr=<metricsql>&engine=streaming|batch
GET /api/v1/query/stream?expr=<metricsql>  (SSE)
```

- `/query` does a single batch eval — same semantics as
  `nmbrs metrics query`.
- `/query/stream` registers a continuous query and
  emits snapshots over Server-Sent Events. Closes the
  query when the client disconnects.

The web layer is a thin wrapper on the runtime; same
register/subscribe/close lifecycle as the TUI consumes.

---

## Memory bounds and back-pressure

Two failure modes to engineer against:

### 1. Unbounded accumulator growth

A plan with no window-framing policy and lifetime
ingestion grows monotonically. For `Aggregate` (per-(group,
ts) keying), that's `O(group_cardinality × distinct
timestamps)`. For HDR-backed quantile plans, accumulator
size is fixed but per-series cardinality matters.

**Mitigations**:
- Default window-framing policy is **Tumbling 5-minute**
  for continuous-query-mode plans. Lifetime is opt-in.
- Per-runtime memory budget; plans whose accumulator size
  exceeds budget get a warning logged and (configurably)
  unregistered.
- Operators can inspect plan memory via a runtime
  introspection API.

### 2. Slow consumer drag

If one TUI panel renders much slower than the runtime
ticks, the watch channel buffers grow. `tokio::sync::watch`
keeps only the latest value (not a queue), so the
slow-consumer pathology is naturally bounded — slow
consumers just see staler data.

For SSE web subscribers we use a bounded mpsc; on
overflow we drop the oldest snapshot for that subscriber.
Logged as a metric.

---

## Configuration surface

Configurable per-runtime (constructor argument or workload
config):

| Setting | Default | Purpose |
|---------|---------|---------|
| `tick_interval` | 5s | How often `tick()` runs in driver-managed mode (TUI / web). |
| `warmup_window` | 5m | Backfill window at plan registration. |
| `default_window_policy` | Tumbling 5m | Per-plan override possible at register time. |
| `max_plans` | 64 | Reject `register()` past this count to bound memory. |
| `max_plan_memory_bytes` | 16 MiB | Per-plan accumulator-size cap. |

---

## Anti-goals

- **Not a query optimiser.** The runtime doesn't dedup
  identical queries from different consumers (yet). Same
  query registered N times = N independent plans. Easy to
  add later via canonical-query keying; not in this push.
- **Not a query language extension.** No new metricsql
  syntax. Everything compiles via the existing
  `compile_streaming` and falls back to batch on
  `CompileError`.
- **No persistence.** The runtime is in-memory. Plans are
  re-registered on restart; accumulators don't survive.
  Persistence is a separate concern that would couple the
  runtime to specific storage.
- **No remote query distribution.** All plans evaluate in
  the same process. Cross-process / cross-host
  coordination is out of scope.

---

## Push scope

### What this design enables

The first push lands a minimal viable runtime — enough
to power one TUI panel against a `PullFeed`. That's the
shortest path to user-visible value.

| Capability | First push | Status |
|-----------|-----------|--------|
| `ContinuousQueryRuntime` struct + actor | ✅ | done |
| `register` / `tick` / `QueryHandle::snapshot` | ✅ | done |
| `PullFeed` (wraps `DataSource`) | ✅ | done |
| `Lifetime` + `Tumbling` window policies | ✅ | done |
| Backfill at register time | ✅ | done |
| `PushFeed` (channel-based ingest) | — | deferred (gated on reporter wiring) |
| `WatchableDataSource` trait | — | deferred (gated on real producer) |
| Subscription via `watch::Receiver` | — | **deferred** — readers currently poll via `ArcSwap::load`. Lands when a real consumer (TUI panel) needs change-notification semantics. |
| Grid window policy | — | deferred (sliding-window design parked in SRD-47 §"Window framing") |
| TUI panel binding | — | separate push (TUI work) |
| Web endpoint | — | separate push (web work) |
| Memory budget enforcement | minimal (cap on plan count) | followup |

### Tasks

| # | Task | Acceptance |
|---|------|------------|
| 1 | `ContinuousQueryRuntime` skeleton + actor | Owner thread starts on construction; `register` sends a command; receives a PlanId. |
| 2 | `PullFeed` wrapping `DataSource` | `fetch_since(matchers, since)` returns post-watermark series; `latest_ts()` works. |
| 3 | Tick loop: watermark-advance ingest + snapshot publish | After `tick()`, `handle.snapshot()` reflects new data. |
| 4 | `QueryHandle::subscribe` via `tokio::sync::watch` | **Deferred.** Subscribers can poll via `QueryHandle::snapshot()` (ArcSwap clone) or `snapshot_handle()` (zero-copy guard) until a real consumer needs change-notification semantics. |
| 5 | Backfill at register time | First snapshot after `register()` contains historical data within `warmup_window`. |
| 6 | Tumbling window policy | After `tumble_window` elapses, accumulators reset; subscribers see the change. |
| 7 | Property test: streaming-via-runtime equals batch | Random partition of input samples → ingest through runtime → snapshot equals `evaluate` result. Same harness pattern as SRD-47's load-bearing test. |
| 8 | Documentation | This SRD + module-level rustdoc on the runtime + memory entry. |

### Exit criteria

1. All 8 tasks land in a coherent commit (or commit
   series).
2. `cargo test --workspace` is green.
3. The new property test passes for every supported
   streaming shape.
4. `cargo build --workspace` is clean.
5. A new memory entry captures the design choices in
   §"Concurrency model" and §"Window framing policy" so
   future passes don't re-litigate.
6. SRD-48 is referenced from `docs/SYSREF.md` and
   `docs/SRD/00_index.md` under §"Metrics and
   Observability".

### Risk log

| Risk | Mitigation |
|------|------------|
| Actor pattern adds latency vs. RwLock for read-heavy workloads | Profile after first push; the write path is the rare one (one tick → one publish); read path is `ArcSwap::load`, comparable to a relaxed load. |
| Watermark drift across feeds means missed samples | Watermark is **per-leaf**, not global. Each matcher set advances its own watermark. Documented. |
| Plans that register after a long backfill see stale data first | Documented in `register()` rustdoc; tests verify backfill happens before the first snapshot is published. |
| Tumbling-window resets surprise consumers | The `watch::Receiver` yields a snapshot containing the empty post-reset state immediately after every reset. Subscribers see the transition explicitly. |

---

## Followups already mapped

After this push, the runtime composes with its surrounding
infrastructure cleanly. Followups are ordered by code dep:

1. **Grid window policy** (within runtime) — covers
   sliding-window semantics for distributive ops without
   needing per-sample subtraction.
2. **`PushFeed` wired to the reporter path** — turns
   real-time ingestion into runtime input. Closes SRD-47
   followup #6 (`WatchableDataSource`) as a third feed
   type.
3. **TUI panel using metricsql** — first user-visible
   consumer of the runtime; depends on (1) for sliding
   semantics and (2) for real-time updates.
4. **Web endpoint** — `/api/v1/query` (one-shot) +
   `/api/v1/query/stream` (SSE).
5. **Multi-query dedup** — when one query is registered
   N times by different consumers, share one plan. Pure
   optimisation, doesn't change semantics.
6. **Plan memory budget enforcement** — config-driven
   cap on per-plan accumulator size.
7. **Persistence / hot reload** — out of scope for this
   SRD; revisit if user demand emerges.

---

## References

- **SRD-47 (MetricsQL Streaming Aggregation)**: the
  algebra this runtime sits on top of. The `Reducer`
  trait's commutative-monoid invariants are what makes
  partial-result-equals-batch correct here.
- **SRD-40 §"Lock-Free Metrics"**: the actor + ArcSwap
  pattern this runtime adopts.
- **SRD-42 (Windowed Metrics Access)**: the user-declared
  cadence list that the grid window policy aligns to.
  Same grid; not a parallel one.
- **SRD-23 (Dynamic Controls)**: precedent for runtime
  state held in an actor with command-channel writes and
  ArcSwap reads; runtime wiring should look familiar to
  anyone who's read SRD-23.
- **`nmbrs metrics watch`**: the polling-loop CLI in
  `crates/nmbrs/src/metricsql_cmd.rs::watch` is the proto-runtime;
  this SRD generalises that pattern.
- **OLAP algebra background** (Gray et al. 1997) — same
  reference SRD-47 cites; the partition law that makes
  partial-result equivalence possible is the foundation
  for runtime correctness too.
