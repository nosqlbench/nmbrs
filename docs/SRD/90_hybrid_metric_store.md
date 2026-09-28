# 90: Cadence-Aware Hybrid Metric Store    (owning crate: nmbrs-metrics; modules: cadence_reporter, queryapi/{mod,live,sqlite,hybrid,sample_view}, scheduler; new traits: MetricSink (write) + MetricAccess (read); tests: cadence_reporter, queryapi/*, crates/nmbrs/tests/example_workloads_in_process)

Windowed metric reads (`rate(errors_total[400ms])`, `increase(...[W])`, every
`metricsql_*` reader, the SRD-86 optimizer settle objective) source their data
from the live cadence store. Under SRD-88 concurrent in-process execution that
store loses the temporal resolution a window needs, and a windowed read becomes
nondeterministic. This SRD specifies the storage + read-trait changes that make
a windowed read **deterministic, lock-free, and uniformly served from memory
(with sqlite spill-over)** — *within* the SRD-42 cadence model, not beside it.

This is the load-bearing artifact behind the SRD-89 servo-example failure: the
servo retargets a control (now SRD-89 per-exec) and reads a *windowed objective*;
control rooting + synthetic saturation made the write and the signal correct, but
the **read** still collapsed under a contended cadence. SRD-90 is that read.

## Motivation — the defect

The cadence reporter is a streaming fold (SRD-42 §"Streaming coalesce semantics").
Every scheduler-tick `MetricSet` enters `CadenceWindow::ingest` (`cadence_reporter.rs`)
and is immediately merged into the in-flight prebuffer via
`MetricSet::coalesce` (`snapshot.rs`), which collapses N inputs to **one** snapshot
whose `captured_at = inputs.map(captured_at).max()` and whose per-metric counter
point keeps only the latest cumulative. Closed windows land in a ring bounded to
`HISTORY_RING_CAP = 32` slots.

For the **smallest** cadence the fold is a no-op per tick (a tick's interval equals
the cadence, so each tick closes immediately) — so the smallest tier already holds
one immutable point per tick. What it loses is **(a)** history beyond 32 slots and
**(b)** sub-tick **partial flushes** (`scope_close` / `quiesce` / phase-end), which
*are* folded into the prebuffer instead of retained as distinct timestamps.

Under SRD-88 concurrency the single metrics scheduler starves: tick spacing
**drifts and jitters**, so a 400 ms window alternately holds enough points, one
point, or none. metricsql's range rollup needs ≥ 2 samples — fewer yields the
no-data sentinel (SRD-89 `nodes::no_data_value` → NaN). The settle then either
holds forever, latches a fabricated value, or catches a transition transient, and
the optimizer mis-converges run-to-run. It works **solo** because the cadence keeps
up. The storage has *cadence-pulse* resolution; windowed reads demand
*sub-interval* resolution; the slot bound + partial-flush fold destroy exactly the
timestamps the window needs.

## Contract

- **Exports (the store abstraction).** A **store** is the pair
  `MetricSink` (write — accept a labeled, timestamped snapshot) **+** `MetricAccess`
  (read — answer a query). `MetricAccess` (`queryapi/mod.rs`) is preserved verbatim;
  `MetricSink` is new and formalizes the *already-existing* ingest/subscribe surface.
  New impls/compositions: `HybridStore` (composes the **reads**, mem-first/cold-tail),
  a fan-out sink (composes the **writes**), the smallest-tier retention accessors on
  the reporter, and an optional `HorizonAware` capability trait.
- **Consumes (inbound contract):** the SRD-42 cadence cascade (smallest-cadence
  ticks + partial-flush boundaries) and its subscription fan-out, the SRD-45
  per-session sqlite store (`queryapi/sqlite.rs`), the SRD-89 read-scoping hook
  (`current_read_exec_id`).
- **Allowed edges:** nmbrs-metrics internal; nmbrs-runtime installs the backend via
  `install_live_access`. No new cross-crate edge (see SRD-05 Contract Registry).

## Axioms

- **A1 — One path, cadence-native.** Windowed reads source the **same cadence
  data** the pipeline already maintains. There is no parallel "sample log": the
  smallest `CadenceWindow` is enriched in place. Cadence remains the sole user
  surface for timing configuration and point alignment (operators configure
  cadences, not a second concept).
- **A2 — Sub-interval retention is time-bounded, generous for counters, tight for
  distributions.** The smallest tier retains distinct timestamped points over a
  **wall-clock horizon**, not a slot count. *Design refinement (landed):* the
  counter/gauge horizon is a **generous fixed floor** (`COUNTER_RETAIN_FLOOR`,
  60 s) rather than demand-derived *down* to the declared query windows — because
  cumulative counters cost ~nothing to retain (a point is `(timestamp,
  cumulative)`) and a generous floor is precisely what serves **short-timeframe
  trending**, the capability this tier exists for; deriving down to a `[400ms]`
  query would defeat it. HDR-distribution series carry a **separate, tighter**
  bound (`cadence × HISTORY_RING_CAP`, today's footprint) — distributions are the
  memory-heavy part. Any read wider than the in-memory horizon is served correctly
  (coarser) from the sqlite tail (A6), so the floor bounds memory without bounding
  correctness. (Raising the floor to cover a workload that *declares* a window
  larger than the floor — to keep such a query in-memory rather than spilling — is
  a thin future refinement; the spill already serves it.)
- **A3 — Stack, never fold, the sub-interval points.** Per-tick closes and
  sub-tick partial flushes are appended as **distinct** `(captured_at, interval,
  value, partial)` points. `captured_at` is never `max()`'d away; `partial` is
  per-point, never sticky-OR'd. A window of any size sees every constituent sample
  that arrived inside it.
- **A4 — Lock-free readers.** Readers take an `ArcSwap` snapshot
  (`state.load_full()` — one atomic load, one `Arc` clone) and binary-search an
  immutable slice. N concurrent readers never block the owner or each other. The
  residual `LIVE` install-mutex collapses to an `ArcSwap`/`OnceLock` cell.
- **A5 — The cascade *fold* is untouched (A1 byte-identity); only the finest
  ring's *retention bound* changes.** The two halves of SRD-42's "bounded state"
  invariant age differently under the cumulative-counter model
  (`notes/cumulative_counter_model.md`): the **promotion-work** half stays `O(1)`
  per tick (time-coalesce is now `combine_coalesce` = keep-latest, even cheaper
  than the old delta-sum); but "one closed snapshot per layer is sufficient state"
  was **never** the operative model for windowed reads and the cumulative move is
  exactly why — a windowed `rate()` differences the running total at *distinct
  timestamps*, which keep-latest cannot provide. The codebase already concedes
  this: "the finest-cadence ring (cap 32) is retained **independently of the
  cascade fold**, so a continuous/sliding lookback … is always derivable from the
  running totals" (`cumulative_counter_model.md:17`). SRD-90 changes **only that
  ring's bound** (from the fan-in-derived 32 slots to a demand-derived time
  horizon) and stacks sub-tick partials into it. Coarse-tier prebuffer+ring
  coalescing, subscription fan-out, the sqlite writer, `session_lifetime`, the TUI
  feed are unchanged; single-run durable output is byte-identical.
- **A6 — In-memory first, sqlite as the older tail.** A read served entirely from
  the in-memory horizon **never touches sqlite**. Only the `[start, horizon)`
  portion of a longer read spills to the durable store, stitched into one smooth
  timeline. Each logical sample window is used distinctly (no re-coalescing).
- **A7 — A store is `MetricSink` + `MetricAccess`; the read dual has a write dual.**
  Submitting a snapshot to the cadence pipeline and submitting one to sqlite are the
  **same trait** (`MetricSink`), with two impls; the read side (`MetricAccess`) is
  likewise two impls. The hybrid composes the reads; a fan-out composes the writes.
  Two invariants keep this from collapsing the tiers: **(i)** sqlite stays the
  **coarse, downstream** tier — it is fed the *coalesced cadence windows* via the
  cadence pipeline's fan-out, **never** the raw sub-interval ticks (the hybrid read's
  mem=fine / cold=coarse split depends on this; submitting fine ticks to sqlite would
  blow up disk and dissolve the cold tier); **(ii)** the ingest **hot path stays
  lock-free** — `MetricSink::submit` is a non-blocking enqueue, and durability /
  single-writer serialization is an impl detail of the sqlite sink, never imposed on
  the contract or the cadence inlet. The trait is deliberately **thin** ("accept a
  labeled snapshot at its captured time") — it does not pretend the reporter (run the
  whole cascade) and sqlite (append rows) are interchangeable peers.

## Mechanism

### M1 — Re-size the finest ring's retention (it already exists, independently)

The finest-cadence ring is **already** retained independently of the cascade fold
so a sliding lookback is derivable from the running totals
(`cumulative_counter_model.md:17`); SRD-90 changes its **bound**, not its
existence. `CadenceWindow` for the **smallest declared cadence** swaps its
`HISTORY_RING_CAP`-slot (fan-in-derived) cap for a **time-bounded** retention
denominated in the declared query horizon (M3). Because a counter point is a
**cumulative running total** at its timestamp, a `RetainedPoint` is just
`(captured_at, cumulative)` and `rate([W])` is the difference of the two points
bracketing the window — no delta-summing, no "the fold lost the window total"
problem (that problem belonged to the old self-contained-delta model; the
cumulative move makes per-point retention the natural representation):

```
struct RetainedPoint {          // per (component, series-identity)
    captured_at: Instant,        // the real, distinct sub-interval timestamp
    interval:    Duration,       // this point's own coverage (real elapsed, never summed)
    value:       MetricValue,    // counter cumulative / gauge / hdr-handle (Arc for hist)
    partial:     bool,           // a sub-tick flush (scope_close/quiesce/phase-end)
}
```

On each smallest-cadence close the point is **appended** (not coalesced into a
prior point); on append the front is evicted while
`front.captured_at < now − horizon`. Series identity is the same
`(family, label_set)` `coalesce` uses today, kept **distinct across time**. Coarser
cadences are unaffected — they keep folding the smallest tier's closes into their
`O(1)` prebuffer+ring exactly as SRD-42 specifies; the retention buffer is the
smallest tier *also* keeping its closes around longer, not a new structure or a new
writer.

### M2 — Partial-flush stacking

`scope_close` / `quiesce` / `force_close` (phase- and session-boundary flushes)
append a `RetainedPoint{partial:true}` carrying their **own** real-elapsed interval
(`component.rs::capture_delta_auto` already computes it) into the smallest tier's
retention buffer, *in addition to* their existing cascade behavior. They no longer
lose their distinct timestamp to the prebuffer fold. This is the "wrapping within
each cadence pulse" the requirement names: a cadence pulse's interior sub-samples
are retained, not melted into the pulse.

### M3 — Horizon derivation (the carrying-cost guarantee)

The retention horizon is `max(declared windows) × SAFETY`, where *declared windows*
are the `[W]` lookbacks of every compiled metricsql range-selector plus any
`past(span)` / `increase_over` span the workload declares. Discovered at
query-compile time and installed on the reporter, **raised monotonically** as
queries are seen. A small floor (a few × smallest cadence) gives brand-new series a
little history; the coarser HDR-distribution horizon is bounded separately. A
workload with no windowed queries retains essentially nothing extra. Reads wider
than the horizon are correct via sqlite spill (A6), at the cadence's coarse
granularity.

### M4 — Lock-free reader view

`ReaderState` (published through the existing `ArcSwap`, `cadence_reporter.rs`)
gains one field — the immutable per-component smallest-tier view:

```
samples: HashMap<ComponentPath, Arc<SmallestTierView>>
struct SmallestTierView { series: HashMap<SeriesId, Arc<[RetainedPoint]>> }
```

The owner mutates its private retention deques on each `Ingest`/`ClosePath`, then
republishes a fresh view whose changed series get a rebuilt `Arc<[RetainedPoint]>`
spine while unchanged series **share their previous `Arc`** by clone. A windowed
read: `load_full()` → O(1) component + series lookup → O(log P) binary-search for
the `[start,end]` slice → materialize into a `Vector`. Zero locks; readers each hold
their own snapshot `Arc`.

### M5 — HybridStore + the time-boundary handshake

`select_range` is served by a new `HybridStore: MetricAccess`:

```
mem:  Arc<dyn MetricAccess>   // the smallest-tier reader (M4)
cold: Arc<dyn MetricAccess>   // the existing SqliteDataSource, unchanged

fn select_range(m, start_ms, end_ms):
    let h = mem.earliest_ms()                  // in-memory horizon boundary
    let hot = mem.select_range(m, start_ms.max(h), end_ms)
    if start_ms >= h: return hot               // fully in memory — sqlite untouched
    let cold = cold.select_range(m, start_ms, h-1)
    return stitch(cold, hot)                    // one timeline; exact-dup dedup at the seam
```

**Instant ↔ absolute-ms reconciliation.** The reporter captures one reference pair
`(ref_instant, ref_unix_ms)` at construction. In-memory `captured_at` → ms is
`ref_unix_ms + (captured_at − ref_instant)` — *stable across queries*, unlike
today's per-call `now_ms − elapsed()` (`live.rs`) which re-samples the clock each
read. sqlite rows already carry absolute ms on the same clock, so the same sample
written cold and held hot map to the **same** ms — making the seam dedup exact and
the timeline seamless at any window size.

### M6 — Dimensional scoping (SRD-88/89)

`exec_id` / `session` scope rides as a label `Matcher` applied identically to `mem`
and `cold` (the sqlite reader translates label→column internally). SRD-89's
`current_read_exec_id` is folded into the matcher set the engine passes down, so a
concurrent neighbour's series never enters either tier — no cross-talk, uniform on
both sides of the seam.

### M7 — Write side: the `MetricSink` dual (A7)

The read trait gets a write dual so a *store* is uniformly `(MetricSink, MetricAccess)`:

```
trait MetricSink: Send + Sync {
    fn submit(&self, labels: &Labels, snapshot: MetricSet);   // non-blocking enqueue
    fn flush(&self);                                          // scope/quiesce/shutdown boundary
}
```

This **formalizes the topology that already exists** — it does not re-route data:

- `CadenceReporter` implements `MetricSink` at its *inlet*: `submit` is its existing
  lock-free actor-channel `ingest` (the scheduler already calls this); `flush` is the
  existing `scope_close`/`quiesce`/`force_close` cascade. Internally it still retains
  (M1), cascades (SRD-42), and **fans out** to downstream sinks.
- The sqlite writer implements `MetricSink`: `submit` enqueues a row-append onto its
  existing async, single-writer path; `flush` consolidates the WAL. It is registered
  as a **fan-out target of the cadence pipeline's cadence-level subscription** — i.e.
  it receives the *coalesced cadence windows*, exactly as today, **not** the
  smallest-tier sub-interval ticks (A7-i). The in-memory fine retention is never
  persisted at fine resolution; once it ages past the horizon the coarse cadence
  window covering that span in sqlite is the cold-read source (A6).
- A `FanOutSink { Vec<Arc<dyn MetricSink>> }` composes writes (the dual of
  `HybridStore` composing reads) so a future backend (VictoriaMetrics push, an
  in-test capture sink, a replay sink) is one `(MetricSink, MetricAccess)` pair added
  at the edge, with no change to the producer or the cadence core.

**Hot-path invariant (A7-ii):** `submit` must never block or take a lock on the
scheduler's call. The cadence sink enqueues onto its actor channel; the sqlite sink
enqueues onto its async writer; serialization/durability live behind the enqueue.
This is the same non-blocking discipline the lock-free-metrics axiom already mandates
— the trait makes it explicit, it does not weaken it.

## How this fixes the defect

`rate(errors_total[400ms])` under N concurrent executions, scheduler drifting:

1. Engine → `HybridStore::select_range(errors_total{exec_id=K}, now−400ms, now)`.
2. `now−400ms ≥ horizon` ⇒ **fully in memory, sqlite untouched.**
3. The smallest-tier view is binary-searched: the window holds **every** point that
   actually arrived in those 400 ms (per-tick closes + any partial flushes) — say 3
   points at +0/+130/+280 ms even though the "pulse" drifted, where today the slot
   model would surface one coalesced point or none.
4. ≥ 2 points ⇒ metricsql computes a real rate. **Deterministic:** identical
   retained samples ⇒ identical result, independent of scheduler drift, because
   retention is time-denominated and timestamps are real.
5. `{exec_id=K}` matcher scopes the read ⇒ no neighbour leakage.

The SRD-89 NaN-no-data hold remains, now firing only for a genuinely empty window
(true cold start), not for "the scheduler didn't tick" — a correct guard rather
than a contention artifact.

## Scope / non-goals (honest boundaries)

SRD-90 makes the **windowed read** deterministic and lock-free and unifies the
in-memory/durable view. It does **not**, by itself, resolve two adjacent axes:

- **Settle transition latching (SRD-86).** A settle can still stabilize while the
  window straddles a just-changed coordinate (the old setting's samples not yet
  aged out). That is a settle-timing concern, not storage. **Note (found via
  `cumulative_counter_model.md:155-166`):** a window-sized settle **viability gate**
  — sized to the objective's widest rollup window via
  `PolydatProgram::max_temporal_window_ms` / the reader node's `temporal_window_ms`,
  which held a `rate(...[W])` read from settling until its window had cleared the
  prior coordinate — *used to exist and was removed* (the `temporal_window_ms`
  removal: "settle now relies on stability detection alone"). Its removal is the
  most likely cause of transition latching. Restoring a window-aware gate is the
  coupled SRD-86 fix; SRD-90's retention makes the gate cheap to honor (the window's
  samples are always present once cleared). Reassess after M1–M2 land.
- **Scheduler sample density under extreme starvation.** Retention surfaces every
  sample that *was* captured; if a low-thread host genuinely fails to tick for a
  full window, there is nothing to retain. This degrades windowed *fidelity*
  (noisier rate), not *determinism/correctness*. A scheduler-robustness follow-up
  (per-component capture budgeting / decoupled ticking) is tracked separately and is
  out of scope here.

Also out of scope: cross-session history beyond sqlite; making the
`metric_window()` coarse-ring polydat path sub-interval-accurate (follow-up if
wanted).

## Staged implementation

Each stage is independently testable; Stage 1 is the smallest slice that makes the
3-copy `control.yaml` repro deterministic. **Stages 1–4 are the read-side
determinism fix and land first; Stage 5 (the `MetricSink` write-side dual) is a
separate, later refactor of the hot ingest spine, deliberately decoupled so the
fix that must land first carries no write-path risk.**

1. **Time-bounded sub-interval retention (LANDED 2026-06-22).** The
   `CadenceWindow` ring (`cadence_reporter.rs`) is now **time-bounded**, not
   `HISTORY_RING_CAP`-slot-bounded: counter/gauge points are kept to a
   `COUNTER_RETAIN_FLOOR` (60s, raised never below the distribution bound),
   while HDR-reservoir distribution families are stripped
   (`MetricSet::without_distributions`) once a window ages past
   `cadence × HISTORY_RING_CAP` — so distribution memory matches the prior bound
   while cheap cumulative history runs ~37× longer for short-timeframe trending.
   `evict_and_compact` replaces the slot eviction; reads stay lock-free through
   the existing `ArcSwap<ReaderState>` (the published ring just carries more
   counter windows). Tests: `ring_retains_by_time_not_slot_count`,
   `ring_evicts_windows_past_the_counter_horizon`,
   `distributions_stripped_past_hist_horizon_but_counters_kept` (cadence_reporter);
   288 nmbrs-metrics tests + the full walker green (no regression).
   **Scope note:** this delivers req #2 (sub-window trending) and confirms req #1
   (lock-free), but is — by the §non-goals analysis — **inert for the servo's
   short 400ms window** (the smallest ring already covered it; the servo lever is
   the bracket-rate + settle gate, tracked separately). Demand-derived horizons
   (vs the fixed floor), a per-series binary-search view, and the explicit
   `samples`/`SampleView` reader are deferred to Stage 4 / a perf follow-up — the
   existing ring + read path already deliver the capability lock-free.
2. **Partial-flush stacking.** `scope_close`/`quiesce`/`force_close` append
   `partial:true` points. Test: a mid-cadence phase-end flush appears as a distinct
   timestamped sample in `select_range`; the cascade ring is byte-identical.
3. **HybridStore + sqlite spill + uniform exec scoping (LANDED 2026-06-22).**
   `queryapi/hybrid.rs`: `HybridStore` over an **ordered tier list** (finest
   first) + `Tier` + `HorizonAware`. Read model = **coverage-aware walk** (query
   a tier only while the query isn't yet covered back to `start`; skip a tier
   whose data starts after the window) → **same bounds to each chosen tier,
   concurrently** (`thread::scope`, only when >1 needed) → **fold finest-first by
   union-minus-overlap** (per-series; the finer tier wins every span it covers).
   `MetricsQueryAccess::earliest_ms` advertises the in-memory horizon for the
   walk's fast-path (a recent windowed read never opens sqlite). The runner
   installs `ExecScopedAccess(HybridStore[mem, sqlite-tail])`.
   **Uniform exec scoping (SRD-89 §3b / §M6):** `exec_id` is one dimensional
   label injected once by `ExecScopedAccess` as a matcher; the mem tier applies
   it to its label set (the bespoke post-filter is **deleted**) and the sqlite
   tier to its `exec_id` column (no `CurrentReadExec` selection special-case) —
   one mechanism, both tiers. Tests: 4 hybrid (fast-path / spill-stitch /
   coverage-skip / cold-only series); 293 nmbrs-metrics + full walker green (exec
   scoping intact, no regression). Single-run injects nothing ⇒ A1.
   **Note:** the cold tier opens a WAL read-only connection to the session's own
   `metrics.db`; the spill is rare (fast-path) and the optimizer (400ms) never
   spills, so this is exercised mainly by long live reads.
4. **Horizon + histogram split + de-mutex (LANDED 2026-06-22).** Resolved per A2:
   the counter/gauge horizon is the **generous fixed `COUNTER_RETAIN_FLOOR`**
   (60 s — the trending floor; demand-deriving *down* is rejected as it defeats
   trending, and counters are cheap), with the **separate tighter distribution
   bound landed in Stage 1**. The `LIVE` service cell is now an **`ArcSwapOption`**
   (sized `LiveHolder` over the `dyn` service) — `live_access()` is one lock-free
   atomic load on the metricsql read hot path, no mutex. Tests:
   `distributions_stripped_past_hist_horizon_but_counters_kept` (Stage 1) covers
   the split; the de-mutex is exercised by every read through `live_access`.
   Deferred (thin, optional): raising the floor to cover a *declared* window
   larger than it (to avoid a spill the sqlite tail already handles).
5. **`MetricSink` write-side dual (A7, M7) — LANDED 2026-06-22.** `MetricSink {
   submit, flush }` added (`cadence_reporter.rs`); `CadenceReporter` implements it
   at its **inlet** (`submit` = `ingest`, no behavior change); `FanOutSink`
   composes writes (the dual of `HybridStore` composing reads). **Key recognition:
   the durable/coarse fan-out SINK role A7 envisioned already exists** as the
   `scheduler::Reporter` trait (`report` + `flush`), implemented by
   sqlite/console/csv/VM and fed *coalesced cadence windows* by the subscription
   dispatch — so A7-i (sqlite stays downstream/coarse, never raw ticks) and A7-ii
   (lock-free async dispatch) already hold. SRD-90 formalizes the **inlet** as the
   matching trait so a producer can fan one snapshot to multiple stores. The
   scheduler still calls `ingest` directly (no hot-path rewire — the trait is
   additive public API), so single-run output is byte-identical. Test:
   `metric_sink_fans_out_and_cadence_reporter_is_a_sink` (FanOutSink delivers to
   every sink; `CadenceReporter::submit` ingests, queryable). Deferred (cosmetic):
   routing the scheduler's submit through the trait, and unifying `Reporter` with
   `MetricSink` (two roles — inlet vs fan-out-target — genuinely differ in shape).

## See also

- crate `crates/nmbrs-metrics/src/lib.rs`; modules `cadence_reporter`, `queryapi/*`,
  `scheduler`, `snapshot`, `component`; tests as named above.
- **SRD-42** Windowed Metrics — the cadence cascade this builds on (M1 extends the
  smallest tier; A5 preserves the rest).
- **SRD-40c** Metric Query API — the `MetricAccess` contract `HybridStore` implements.
- **SRD-45** Sessions — the per-session sqlite store used as the cold tier.
- **SRD-47 / 48** MetricsQL streaming / continuous-query — the smallest-tier view is
  the realized live "watchable" sample feed those SRDs anticipated.
- **SRD-88** Concurrent executions — the contention that exposed the defect.
- **SRD-89** Execution-scoped resolution — the per-exec read scoping M6 threads
  through; the NaN-no-data hold this SRD makes a true cold-start guard.
- **SRD-86** Optimization — the servo settle, the first beneficiary and the owner of
  the residual transition-latching concern noted under non-goals.
